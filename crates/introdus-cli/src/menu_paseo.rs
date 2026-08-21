//! The paseo orchestrator control-panel actions, split out of
//! [`crate::menu_actions`] to keep that file within its line budget. Owns the
//! **(re)install + relay⇄direct mode switch** (`install_paseo`), the safe daemon
//! package update (`update_paseo`), the "connect" action (`paseo_qr` — a pairing
//! QR in relay mode, the paste-ready `tcp://host:port?password=…` URL in direct
//! mode), and the shared opt-in cores the headless [`crate::cli_actions`] reuses.

use std::time::Duration;

use anyhow::{bail, Result};
use introdus_core::config::PaseoMode;
use introdus_core::podman::{self, exec_interactive};
use introdus_core::{agents, hostnet, Config};

use crate::context::LaunchContext;
use crate::frontend::Frontend;
use crate::menu_actions as act;
use crate::panel::Ui;
use crate::util::shell_quote;

/// Oldest pnpm major that supports every supply-chain control used by the
/// paseo updater (strict release-age handling, missing-time rejection,
/// trustPolicy, and exotic-subdependency blocking).
const MIN_SAFE_PNPM_MAJOR: u64 = 11;

/// A seven-day quarantine, expressed in pnpm's minutes.
const PASEO_MIN_RELEASE_AGE_MINUTES: &str = "10080";

/// Root-owned, empty pnpm config directory used by the updater. The unprivileged
/// workload cannot seed it with a global pnpmfile, registry override, or weaker
/// policy before the user invokes an update.
const PASEO_UPDATE_CONFIG_DIR: &str = "/run/introdus-pnpm-update-config";

/// Arguments for a deliberately fail-closed global Paseo update. Keep these as
/// separate argv tokens: no project or package-controlled text is evaluated by
/// a shell.
///
/// `--ignore-scripts` blocks package lifecycle scripts, while
/// `ignore-pnpmfile` closes the separate executable-hook path that pnpm's docs
/// explicitly say `ignoreScripts` does not cover. The remaining settings make
/// the seven-day delay strict (including registries that omit publish times),
/// reject trust downgrades and exotic transitive sources, pin the public npm
/// registry for both the default and @getpaseo scope, and prevent less-safe
/// user/project settings from overriding those choices.
const PASEO_UPDATE_ARGS: &[&str] = &[
    "update",
    "--global",
    "--latest",
    "--yes",
    "--ignore-scripts",
    "--config.ignore-scripts=true",
    "--config.ignore-pnpmfile=true",
    "--config.dangerously-allow-all-builds=false",
    "--config.minimum-release-age=10080",
    "--config.minimum-release-age-strict=true",
    "--config.minimum-release-age-ignore-missing-time=false",
    "--config.trust-policy=no-downgrade",
    "--config.block-exotic-subdeps=true",
    "--config.userconfig=/dev/null",
    "--config.registry=https://registry.npmjs.org/",
    "--config.@getpaseo:registry=https://registry.npmjs.org/",
    "--config.strict-ssl=true",
    "--config.verify-store-integrity=true",
    "--config.strict-store-pkg-content-check=true",
    "--config.side-effects-cache=false",
    "--config.prefer-offline=false",
];

/// Shell snippet that ensures the paseo daemon is running, for prefixing an
/// interactive window command. `paseo daemon status` exits 0 whether the daemon
/// is running OR stopped, so `status || start` never starts it (and `start` errors
/// noisily when already up); gate on the `localDaemon` field of the JSON status
/// instead. Without a running daemon the relay never connects and phone pairing
/// times out.
///
/// A non-zero `paseo daemon start` is NOT authoritative. Its readiness gate can
/// report "Daemon failed to start in background (exit code 1)" even though the
/// worker actually came up and is serving on :6767 — e.g. a slow first relay
/// handshake trips the gate. So we never treat `start`'s exit code as fatal:
/// after attempting it we re-probe the daemon status and treat a running daemon
/// as success. Only if it is genuinely still down do we make one more start
/// attempt (the warm retry that succeeds by hand).
pub(crate) const PASEO_ENSURE_DAEMON: &str = concat!(
    r#"_paseo_up(){ paseo daemon status --json 2>/dev/null | grep -Eq '"localDaemon":[[:space:]]*"running"'; }; "#,
    r#"_paseo_up || paseo daemon start || true; "#,
    r#"_paseo_up || paseo daemon start || true; "#,
    r#"_paseo_up || echo 'paseo: daemon still not up after two attempts — see ~/.paseo/daemon.log'"#,
);

/// The panel's **(Re)Install paseo** action. If paseo isn't opted in yet, this is
/// a first install: confirm, pick the connection mode (relay vs direct), and
/// recreate to wire it. If it's already installed, this instead offers to **switch
/// to the other connection mode** (relay⇄direct). Both the relay nft bypass and
/// the direct published port + env are frozen at container-create, so applying
/// either mode needs a recreate — which keeps the `/home/dev` volume.
pub fn install_paseo(ctx: &LaunchContext, ui: &mut Ui) -> Result<()> {
    act::require_running(ctx)?;
    if ctx.config.install_paseo {
        return switch_mode(ctx, ui);
    }
    first_install(ctx, ui)
}

/// Panel action: confirm, safely update the global Paseo package, then restart
/// its daemon so the running service uses the new code.
pub fn update_paseo(ctx: &LaunchContext, ui: &mut Ui) -> Result<()> {
    require_paseo_installed(ctx)?;
    if !ui.confirm(
        "Update paseo to the newest release that is at least 7 days old, then restart its daemon?",
        false,
    )? {
        return Ok(());
    }
    run_update_paseo(ctx, ui)
}

/// Safely update Paseo and restart its daemon. Shared by the panel and the
/// headless `update-paseo` subcommand.
pub(crate) fn run_update_paseo(ctx: &LaunchContext, f: &mut impl Frontend) -> Result<()> {
    require_paseo_installed(ctx)?;

    let pnpm_version = act::exec(ctx, Some("dev"))
        .args(["pnpm", "--version"])
        .stdout()?;
    require_safe_pnpm(&pnpm_version)?;

    // Isolate pnpm from mutable global config in /home/dev. A compromised
    // workload is the threat model, so the directory must be created by root
    // under root-owned /run, then only read by the dev-user pnpm process.
    act::exec(ctx, None)
        .args([
            "install",
            "-d",
            "-m",
            "0755",
            "-o",
            "root",
            "-g",
            "root",
            PASEO_UPDATE_CONFIG_DIR,
        ])
        .run()?;

    f.log(format!(
        "  updating paseo with pnpm {pnpm_version}: lifecycle/pnpmfile scripts blocked; \
         releases <7 days ({PASEO_MIN_RELEASE_AGE_MINUTES} minutes) old rejected"
    ));
    f.run_task(
        "safe paseo package update",
        act::exec(ctx, Some("dev"))
            .args([
                "env",
                "HOME=/home/dev",
                "XDG_CONFIG_HOME=/run/introdus-pnpm-update-config",
            ])
            .arg("pnpm")
            .args(PASEO_UPDATE_ARGS)
            .arg(agents::paseo::SPEC),
    )?;
    f.run_task(
        "restart paseo daemon",
        act::exec(ctx, Some("dev")).args([agents::paseo::CMD, "daemon", "restart"]),
    )?;
    f.log("  paseo updated and its daemon restarted.");
    Ok(())
}

fn require_paseo_installed(ctx: &LaunchContext) -> Result<()> {
    act::require_running(ctx)?;
    if !ctx.config.install_paseo || !act::container_has_cmd(ctx, agents::paseo::CMD) {
        bail!("paseo isn't installed — run `introdus install-paseo` first");
    }
    Ok(())
}

/// Fail closed when an old pnpm would silently ignore one of the updater's
/// security settings.
fn require_safe_pnpm(version: &str) -> Result<()> {
    let major = version
        .trim()
        .split('.')
        .next()
        .and_then(|part| part.parse::<u64>().ok());
    if major.is_some_and(|major| major >= MIN_SAFE_PNPM_MAJOR) {
        return Ok(());
    }
    bail!(
        "safe paseo updates require pnpm >= {MIN_SAFE_PNPM_MAJOR} (found {version:?}); \
         run `introdus update` to refresh the container toolchain first"
    )
}

/// First-time paseo opt-in: choose the connection mode, persist it, then recreate
/// (or, if the user declines, install the CLI locally with a note that a recreate
/// is still needed to wire the mode).
fn first_install(ctx: &LaunchContext, ui: &mut Ui) -> Result<()> {
    if !ui.confirm(
        "Install paseo? It runs your agents and lets you drive them from a phone/desktop.",
        true,
    )? {
        return Ok(());
    }
    let mode = prompt_mode(ui, PaseoMode::Relay)?;
    let mut config = ctx.config.clone();
    config.set_paseo_mode(mode);
    if mode.is_direct() {
        config.set_paseo_origins_all(prompt_origins(ui)?);
    }
    act::save_and_write_allowlist(
        ctx,
        ui,
        config,
        &format!("enabled paseo ({} mode)", mode.as_str()),
    )?;
    if offer_recreate(ctx, ui, mode)? {
        return Ok(());
    }
    // Declined (or no container running): install the CLI so paseo works locally,
    // but be explicit that the mode's inbound/relay wiring needs a recreate.
    run_install_paseo(ctx, ui)?;
    if !act::container_has_cmd(ctx, agents::paseo::CMD) {
        bail!("paseo isn't installed — check `egress-log` for a blocked host, then retry");
    }
    ui.log("  paseo installed, but its connection wiring only applies after a container");
    ui.log("  recreate (Recreate in Container lifecycle) — until then remote pairing/access won't work.");
    Ok(())
}

/// Already opted in: offer to switch to the *other* connection mode and recreate.
/// Switching relay→direct drops the relay egress host and lets the next launch
/// re-provision a published port + password; direct→relay re-adds the relay host.
fn switch_mode(ctx: &LaunchContext, ui: &mut Ui) -> Result<()> {
    let current = ctx.config.paseo_mode;
    let target = current.other();
    ui.log(format!(
        "  paseo is installed in {} mode.",
        current.as_str()
    ));
    if !ui.confirm(
        &format!(
            "Switch paseo to {} connection mode instead? (recreates the container)",
            target.as_str()
        ),
        false,
    )? {
        ui.log("  left unchanged.");
        return Ok(());
    }
    let mut config = ctx.config.clone();
    config.set_paseo_mode(target);
    if target.is_direct() {
        config.set_paseo_origins_all(prompt_origins(ui)?);
    }
    act::save_and_write_allowlist(
        ctx,
        ui,
        config,
        &format!("switched paseo to {} mode", target.as_str()),
    )?;
    if !offer_recreate(ctx, ui, target)? {
        ui.log("  saved — recreate later (Recreate in Container lifecycle) to apply the new mode.");
    }
    Ok(())
}

/// If the container is running, offer a recreate to apply paseo `mode` (default
/// yes — neither mode's frozen-at-create wiring, the relay bypass or the direct
/// published port, exists until one runs). Returns whether the recreate ran.
fn offer_recreate(ctx: &LaunchContext, ui: &mut Ui, mode: PaseoMode) -> Result<bool> {
    if !podman::container_running(&ctx.container_name) {
        return Ok(false);
    }
    if !ui.confirm(
        &format!(
            "Recreate the container now to apply paseo {} mode? (keeps your /home/dev volume)",
            mode.as_str()
        ),
        true,
    )? {
        return Ok(false);
    }
    act::remove_container_task(ctx, ui, "recreating the container")?;
    act::respawn_dev_window(ctx, ui)?;
    Ok(true)
}

/// Ask which connection mode to use (Yes = a direct VPN/zero-trust connection,
/// No = the paseo relay), defaulting to `default`.
fn prompt_mode(ui: &mut Ui, default: PaseoMode) -> Result<PaseoMode> {
    let direct = ui.confirm(
        "Use a DIRECT connection over your own VPN/zero-trust network instead of the paseo relay? \
         (No relay contacted; the daemon port is published on the host, protected by a generated \
         password.)",
        default.is_direct(),
    )?;
    Ok(if direct {
        PaseoMode::Direct
    } else {
        PaseoMode::Relay
    })
}

/// Ask whether the direct-mode daemon accepts any browser origin (Yes) or is
/// restricted to this machine's client (No). Password-gated either way — this is
/// browser hygiene, not the access control.
fn prompt_origins(ui: &mut Ui) -> Result<bool> {
    ui.confirm(
        "Allow ANY browser origin to reach the paseo daemon? (No = restrict to this machine's \
         client; add other devices later with 'Add a paseo client origin'. Password-gated either \
         way — browser hygiene, not access control.)",
        false,
    )
}

/// The `node -e` program that adds `$PASEO_NEW_ORIGIN` to the daemon's
/// `config.json` CORS allowlist (dropping any `*`), so a live add survives a plain
/// restart (setup.sh unions it back). Single-quoted; contains only double quotes.
const NODE_ADD_ORIGIN: &str = "node -e 'const fs=require(\"fs\");const f=process.env.HOME+\"/.paseo/config.json\";const c=JSON.parse(fs.readFileSync(f,\"utf8\"));c.daemon=c.daemon||{};c.daemon.cors=c.daemon.cors||{};const s=new Set((c.daemon.cors.allowedOrigins||[]).filter(o=>o!==\"*\"));s.add(process.env.PASEO_NEW_ORIGIN);c.daemon.cors.allowedOrigins=[...s];fs.writeFileSync(f,JSON.stringify(c,null,2)+\"\\n\");'";

/// Panel action: prompt for a browser origin and allow it (direct mode only).
/// The guidance goes to the output pane so the prompt line stays short enough to
/// keep the typed URL on screen.
pub fn add_origin(ctx: &LaunchContext, ui: &mut Ui) -> Result<()> {
    ui.log("  Allow a browser origin to reach the direct-mode paseo daemon.");
    ui.log("  Enter the URL a browser loads the paseo UI from, e.g. http://<host-ip>:6767");
    ui.log("  — the HOST's IP + the client's port, NOT the phone's IP.");
    let raw = ui.text("Client origin (scheme://host:port):", false)?;
    allow_origin(ctx, ui, &raw)
}

/// Add one browser `origin` to the direct-mode CORS allowlist and apply it live:
/// persist to config (so a recreate re-applies) and patch the running daemon's
/// `config.json` + restart the daemon — no container recreate. Shared by the panel
/// prompt and the headless `paseo-allow-origin`.
pub(crate) fn allow_origin(ctx: &LaunchContext, f: &mut impl Frontend, origin: &str) -> Result<()> {
    act::require_running(ctx)?;
    if !ctx.config.paseo_mode.is_direct() {
        bail!("client origins only apply to paseo DIRECT mode — this project uses relay mode");
    }
    if ctx.config.paseo_allows_all_origins() {
        f.log("  paseo already allows ALL origins — nothing to add.");
        return Ok(());
    }
    let mut config = ctx.config.clone();
    if !config.add_paseo_origin(origin) {
        f.log("  nothing added (blank, duplicate, or already allowing all).");
        return Ok(());
    }
    let origin = origin.trim().to_owned();
    act::save_and_write_allowlist(
        ctx,
        f,
        config,
        &format!("allowed paseo client origin {origin}"),
    )?;
    // Apply to the running daemon without a container recreate: patch config.json
    // (dropping any prior `*`) and restart just the daemon.
    let script = format!(
        "PASEO_NEW_ORIGIN={} {NODE_ADD_ORIGIN}; paseo daemon restart",
        shell_quote(&origin)
    );
    act::exec(ctx, Some("dev"))
        .args(["bash", "-lc", &script])
        .run()?;
    f.log(format!(
        "  applied to the running daemon (restarted) — reconnect from a client at {origin}."
    ));
    Ok(())
}

/// How long the direct-connection URL stays in the panel's output pane before it
/// erases itself. Long enough to copy or type into a client, short enough that
/// the daemon password isn't left sitting on a screen (or in a scrollback
/// someone shoulder-surfs later).
const DIRECT_URL_TTL: Duration = Duration::from_secs(15);

/// What replaces the URL block once [`DIRECT_URL_TTL`] is up.
const DIRECT_URL_HIDDEN: &str = "  (Paseo direct url hidden after 15 seconds)";

/// The host address a client should dial for direct mode, preferring this host's
/// tailscale/VPN address (see [`hostnet`]) and falling back to a placeholder when
/// the host has no routable IPv4 address to name. Returns `(address, source)`.
pub(crate) fn direct_host() -> (String, String) {
    match hostnet::detect() {
        Some(a) => {
            let source = a.source();
            (a.ip, source)
        }
        None => (
            "<host-ip>".to_owned(),
            "no routable address found — substitute this host's address".to_owned(),
        ),
    }
}

/// The paseo "connect" panel action. In relay mode it opens a tmux window that
/// starts the daemon and prints the pairing QR (the daemon dials out to the relay;
/// nothing is exposed inbound). In direct mode there is no QR — the daemon is
/// reached over plain TCP on your VPN — so it shows the paste-ready
/// `tcp://host:port?password=…` URL, which self-erases after [`DIRECT_URL_TTL`].
pub fn paseo_qr(ctx: &LaunchContext, ui: &mut Ui) -> Result<()> {
    act::require_running(ctx)?;
    if !act::container_has_cmd(ctx, agents::paseo::CMD) {
        bail!("paseo isn't installed — run '(Re)Install paseo' from the Agents menu first");
    }
    if ctx.config.paseo_mode.is_direct() {
        let (host, source) = direct_host();
        ui.log(format!("  address from {source}"));
        let lines = agents::paseo::direct_connection_help(
            &host,
            ctx.config.paseo_port,
            ctx.config.paseo_password.as_deref(),
        )
        .into_iter()
        .map(|l| format!("  {l}"))
        .collect();
        ui.log_transient(lines, DIRECT_URL_TTL, DIRECT_URL_HIDDEN);
        return Ok(());
    }
    // Relay mode: ensure the daemon is up, print the pairing QR (paseo renders it
    // natively), then drop to a shell so the code stays on screen long enough to
    // scan.
    let inner = format!("{PASEO_ENSURE_DAEMON}; paseo daemon pair; exec bash");
    let cmd = exec_interactive(&ctx.container_name, Some("dev"))
        .args(["bash", "-lc", &inner])
        .shell_line()
        .to_owned();
    ui.log("  opening the pairing QR — scan it from the paseo app to connect your phone.");
    act::spawn_window(ctx, ui, "paseo-qr", &cmd)
}

/// Stream `install-agents` with only the paseo opt-in set. `INSTALL_AGENTS=` is
/// passed empty so the installer touches paseo alone and leaves the agent list
/// untouched (an unset `INSTALL_AGENTS` would default to installing claude).
pub(crate) fn run_install_paseo(ctx: &LaunchContext, f: &mut impl Frontend) -> Result<()> {
    f.run_task(
        "install-agents (paseo)",
        act::exec(ctx, Some("dev"))
            .arg("env")
            .arg("INSTALL_AGENTS=")
            .arg("INSTALL_PASEO=true")
            .arg("install-agents"),
    )
}

/// Record the paseo opt-in on `config` in its currently-set mode (`INSTALL_PASEO=true`
/// plus, in relay mode, paseo's proxy-allowlist host — see [`Config::set_paseo_mode`]).
/// Shared by the CLI opt-in flow; the caller sets `paseo_mode` first and persists
/// afterwards.
pub(crate) fn paseo_opt_in(config: &mut Config) {
    let mode = config.paseo_mode;
    config.set_paseo_mode(mode);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ta180_paseo_update_is_fail_closed_and_script_free() {
        let minimum_age = format!("--config.minimum-release-age={PASEO_MIN_RELEASE_AGE_MINUTES}");
        let required = [
            "--ignore-scripts",
            "--yes",
            "--config.ignore-pnpmfile=true",
            "--config.dangerously-allow-all-builds=false",
            minimum_age.as_str(),
            "--config.minimum-release-age-strict=true",
            "--config.minimum-release-age-ignore-missing-time=false",
            "--config.trust-policy=no-downgrade",
            "--config.block-exotic-subdeps=true",
            "--config.userconfig=/dev/null",
            "--config.@getpaseo:registry=https://registry.npmjs.org/",
            "--config.strict-ssl=true",
            "--config.verify-store-integrity=true",
            "--config.strict-store-pkg-content-check=true",
            "--config.side-effects-cache=false",
            "--config.prefer-offline=false",
        ];
        for arg in required {
            assert!(
                PASEO_UPDATE_ARGS.contains(&arg),
                "missing safe pnpm arg: {arg}"
            );
        }
    }

    #[test]
    fn ta180_paseo_update_rejects_pnpm_without_required_controls() {
        assert!(require_safe_pnpm("11.0.0").is_ok());
        assert!(require_safe_pnpm("12.3.1").is_ok());
        assert!(require_safe_pnpm("10.26.0").is_err());
        assert!(require_safe_pnpm("not-a-version").is_err());
    }
}
