//! Headless implementations of the control-panel utilities, exposed as one-shot
//! `introdus` subcommands so everything you can do from the TUI you can also
//! script. Each resolves the current project's context, then reuses the shared,
//! [`Frontend`](crate::frontend::Frontend)-generic action cores from
//! [`crate::menu_actions`] / [`crate::menu_tunnel`] — the panel supplies its
//! prompts interactively, these supply the same decisions from CLI flags.
//!
//! The `--recreate` flag on the frozen-env commands (ntfy / expose-webapp /
//! install-paseo) applies the change by recreating the container immediately,
//! exactly like `introdus recreate` (it execs into podman and does not return);
//! without it the change is saved and takes effect on the next recreate.

use anyhow::{bail, Result};
use introdus_core::agents;
use introdus_core::podman::{self, exec_interactive, podman};

use crate::context::LaunchContext;
use crate::frontend::StdioFrontend;
use crate::launch::{self, LaunchOpts};
use crate::lifecycle::Lifecycle;
use crate::menu_actions as act;
use crate::menu_paseo;
use crate::menu_tunnel;

/// Resolve the current project's launch context (from the working directory).
fn ctx() -> Result<LaunchContext> {
    launch::load_context()
}

// ---- read-only / notification utilities -------------------------------------

pub fn tunnel_url() -> Result<()> {
    act::tunnel_url(&ctx()?, &mut StdioFrontend)
}

pub fn blocked_egress() -> Result<()> {
    act::blocked_egress(&ctx()?, &mut StdioFrontend)
}

pub fn test_notify() -> Result<()> {
    act::test_notify(&ctx()?, &mut StdioFrontend)
}

pub fn notify_log() -> Result<()> {
    act::notify_log(&ctx()?, &mut StdioFrontend)
}

pub fn restart_notify() -> Result<()> {
    act::restart_notify(&ctx()?, &mut StdioFrontend)
}

// ---- container lifecycle ----------------------------------------------------

pub fn restart() -> Result<()> {
    act::restart(&ctx()?, &mut StdioFrontend)
}

pub fn stop(yes: bool) -> Result<()> {
    let ctx = ctx()?;
    if !yes {
        bail!("stopping the container halts your session — pass --yes to confirm");
    }
    if podman::container_running(&ctx.container_name) {
        podman().args(["stop", &ctx.container_name]).run()?;
        println!("  container {} stopped.", ctx.container_name);
    } else {
        println!("  container {} is already stopped.", ctx.container_name);
    }
    Ok(())
}

// ---- terminals & agents (foreground exec) -----------------------------------

/// Open an interactive shell in the container, replacing this process. `root`
/// picks the root user; otherwise `dev`.
pub fn shell(root: bool) -> Result<()> {
    let ctx = ctx()?;
    act::require_running(&ctx)?;
    let user = if root { None } else { Some("dev") };
    let never = exec_interactive(&ctx.container_name, user)
        .arg("bash")
        .exec()?;
    match never {}
}

/// Launch an installed agent in the foreground, replacing this process. `--yolo`
/// launches with the agent's skip-permissions / auto-approve flag (unattended).
pub fn agent(id: &str, yolo: bool) -> Result<()> {
    let ctx = ctx()?;
    act::require_running(&ctx)?;
    if !ctx.config.install_agents.iter().any(|a| a == id) {
        bail!(
            "agent '{id}' isn't in this project's INSTALL_AGENTS — add it with \
             `introdus install-agent {id}`"
        );
    }
    let agent = agents::find(id);
    let cmd_name = agent.map(|a| a.cmd).unwrap_or(id);
    if !act::container_has_cmd(&ctx, cmd_name) {
        bail!(
            "`{cmd_name}` isn't installed in the container (the earlier install may have \
             been blocked) — run `introdus install-agent {id}` first"
        );
    }
    let flag = if yolo {
        agent.and_then(|a| act::yolo_flag(a.yolo))
    } else {
        None
    };
    let mut cmd = exec_interactive(&ctx.container_name, Some("dev"));
    if id == "claude" {
        // claude launches through run-claude (repo cd + remote-control session);
        // pass the bypass flag or `--safe` just like the panel does.
        cmd = cmd.arg("run-claude").arg(flag.unwrap_or("--safe"));
    } else {
        cmd = cmd.arg(cmd_name);
        if let Some(f) = flag {
            cmd = cmd.arg(f);
        }
    }
    let never = cmd.exec()?;
    match never {}
}

// ---- egress allowlist -------------------------------------------------------

/// Append hostnames to `WHITELIST_HOSTS` and rewrite the proxy allowlist;
/// `--restart` restarts the container so the running proxy picks them up.
pub fn allow(hosts: &[String], restart: bool) -> Result<()> {
    let ctx = ctx()?;
    let mut config = ctx.config.clone();
    let added = act::append_whitelist(&mut config, &hosts.join(" "));
    let mut f = StdioFrontend;
    if added.is_empty() {
        println!("  nothing new to add.");
        return Ok(());
    }
    act::save_and_write_allowlist(
        &ctx,
        &mut f,
        config,
        &format!("added {} host(s): {}", added.len(), added.join(", ")),
    )?;
    apply_restart(&ctx, &mut f, restart, "the new allowlist")
}

// ---- agents / paseo install (config edit + in-container install) ------------

/// Add agents to the config and install them into the running container.
/// `--restart` applies the new egress hosts to the running proxy.
pub fn install_agent(ids: &[String], restart: bool) -> Result<()> {
    // Validate ids up front so a typo fails fast (before we need a container).
    for id in ids {
        if !agents::is_known(id) {
            bail!(
                "unknown agent '{id}' — pick from: {}",
                agents::AGENTS
                    .iter()
                    .map(|a| a.id)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    let ctx = ctx()?;
    act::require_running(&ctx)?;
    let mut config = ctx.config.clone();
    let added = act::select_agents(&mut config, ids);
    if added.is_empty() {
        println!("  nothing to install — all given agents are already selected.");
        return Ok(());
    }
    let mut f = StdioFrontend;
    act::save_and_write_allowlist(
        &ctx,
        &mut f,
        config.clone(),
        &format!("selected: {}", added.join(", ")),
    )?;
    act::run_install_agents(&ctx, &mut f, &config.install_agents.join(" "))?;
    apply_restart(&ctx, &mut f, restart, "new egress hosts")
}

/// Opt into paseo: record `INSTALL_PASEO=true` + its relay host, then either
/// recreate (which wires the relay bypass needed for phone pairing) or install
/// the CLI into the current container.
pub fn install_paseo(recreate: bool) -> Result<()> {
    let ctx = ctx()?;
    act::require_running(&ctx)?;
    if ctx.config.install_paseo && act::container_has_cmd(&ctx, agents::paseo::CMD) {
        println!("  paseo is already installed and enabled.");
        return Ok(());
    }
    let mut config = ctx.config.clone();
    menu_paseo::paseo_opt_in(&mut config);
    let mut f = StdioFrontend;
    act::save_and_write_allowlist(&ctx, &mut f, config, "enabled paseo (INSTALL_PASEO=true)")?;
    if recreate {
        return recreate_now("recreating to wire paseo's relay access (phone pairing)");
    }
    menu_paseo::run_install_paseo(&ctx, &mut f)?;
    if !act::container_has_cmd(&ctx, agents::paseo::CMD) {
        bail!("paseo isn't installed — check `introdus blocked-egress` for a blocked host, then retry");
    }
    println!("  paseo installed. NOTE: phone pairing needs the relay bypass, which only a");
    println!("  container recreate wires — run `introdus install-paseo --recreate` to enable it.");
    Ok(())
}

/// Print the paseo pairing URL (the QR's payload) for the running daemon, so you
/// can open it on a phone/desktop without scanning a QR from a terminal.
pub fn paseo_url() -> Result<()> {
    let ctx = ctx()?;
    act::require_running(&ctx)?;
    if !act::container_has_cmd(&ctx, agents::paseo::CMD) {
        bail!("paseo isn't installed — run `introdus install-paseo` first");
    }
    // Direct mode has no relay pairing URL — print the port + password instead.
    if ctx.config.paseo_mode.is_direct() {
        for line in agents::paseo::direct_connection_help(
            ctx.config.paseo_port,
            ctx.config.paseo_password.as_deref(),
        ) {
            println!("{line}");
        }
        return Ok(());
    }
    // Ensure the daemon is up, then print the pairing details and pull the URL
    // out of them. `2>&1` folds any diagnostics into the captured stream.
    let script = format!(
        "{}; paseo daemon pair 2>&1",
        menu_paseo::PASEO_ENSURE_DAEMON
    );
    let out = act::exec(&ctx, Some("dev"))
        .args(["bash", "-lc", &script])
        .stdout_quiet()?;
    match extract_pairing_url(&out) {
        Some(url) => {
            println!("{url}");
            Ok(())
        }
        None => bail!("couldn't find a pairing URL in paseo's output:\n{out}"),
    }
}

/// Allow a browser origin to reach the direct-mode paseo daemon (CORS), applying
/// it live to the running daemon. Mirrors the panel's "Add a paseo client origin".
pub fn paseo_allow_origin(origin: &str) -> Result<()> {
    menu_paseo::allow_origin(&ctx()?, &mut StdioFrontend, origin)
}

// ---- config toggles ---------------------------------------------------------

/// Enable ntfy.sh push for the given topic. `--recreate` applies it now.
pub fn ntfy(topic: &str, recreate: bool) -> Result<()> {
    let ctx = ctx()?;
    if topic.trim().is_empty() {
        bail!("a topic is required");
    }
    let mut config = ctx.config.clone();
    config.enable_notify_sh_alerts = true;
    config.ntfy_sh_topic = Some(topic.trim().to_owned());
    let mut f = StdioFrontend;
    act::save_config(&ctx, &mut f, &config)?;
    apply_frozen_change(recreate, "ENABLE_NOTIFY_SH_ALERTS=true")
}

/// (Re)expose the in-container app via a Cloudflare quick tunnel. When already
/// exposed and running, this refreshes the tunnel in place (no recreate);
/// otherwise it flips `EXPOSE_WEBAPP` and, with `--recreate`, applies it now.
pub fn expose_webapp(recreate: bool) -> Result<()> {
    let ctx = ctx()?;
    let mut f = StdioFrontend;
    if !ctx.config.expose_webapp {
        let mut config = ctx.config.clone();
        config.expose_webapp = true;
        act::save_config(&ctx, &mut f, &config)?;
        return apply_frozen_change(recreate, "EXPOSE_WEBAPP=true");
    }
    act::require_running(&ctx)?;
    if !menu_tunnel::container_has_tunnel_holes(&ctx) {
        println!("  this container predates the Cloudflare tunnel egress rules —");
        println!("  a recreate is needed to (re)expose the app.");
        return apply_frozen_change(recreate, "EXPOSE_WEBAPP=true");
    }
    menu_tunnel::refresh_running_tunnel(&ctx, &mut f)
}

// ---- helpers ----------------------------------------------------------------

/// Restart the container (for allowlist-style changes the proxy re-reads on
/// start) when `restart` is set, else print how to apply it later.
fn apply_restart(
    ctx: &LaunchContext,
    f: &mut StdioFrontend,
    restart: bool,
    what: &str,
) -> Result<()> {
    if restart {
        act::restart(ctx, f)
    } else {
        println!(
            "  {what} applies after a restart — run `introdus restart` (or `introdus recreate`)."
        );
        Ok(())
    }
}

/// Apply a frozen-env change: recreate now if asked (like `introdus recreate` —
/// this execs into podman and does not return), else note that it takes effect
/// on the next recreate.
fn apply_frozen_change(recreate: bool, changed: &str) -> Result<()> {
    if recreate {
        recreate_now(&format!("{changed} saved — recreating to apply it"))
    } else {
        println!(
            "  {changed} saved — it applies only after a container recreate (run `introdus recreate`)."
        );
        Ok(())
    }
}

/// Recreate the container now: drop it and re-run the full launch (keeps the
/// volume). Execs into podman on success and does not return.
fn recreate_now(note: &str) -> Result<()> {
    println!("  {note}…");
    launch::run_launch(Lifecycle::Recreate, LaunchOpts::default())
}

/// Pull the pairing URL out of `paseo daemon pair` output — the first
/// `https://` token (preferring a paseo.sh one), with trailing punctuation
/// trimmed. It's the same payload the QR encodes.
fn extract_pairing_url(out: &str) -> Option<String> {
    let is_url = |t: &&str| t.starts_with("https://");
    out.split_whitespace()
        .find(|t| is_url(t) && t.contains("paseo"))
        .or_else(|| out.split_whitespace().find(is_url))
        .map(|t| t.trim_end_matches(['.', ',', ')']).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ta155_extract_pairing_url_prefers_paseo_https_token() {
        // A QR dump with a preamble line and the URL embedded mid-text.
        let out = "Scan this to pair:\n\
                   Or open https://app.paseo.sh/pair#abc123 on your phone.\n";
        assert_eq!(
            extract_pairing_url(out).as_deref(),
            Some("https://app.paseo.sh/pair#abc123")
        );
        // Trailing punctuation is trimmed.
        assert_eq!(
            extract_pairing_url("pair at https://app.paseo.sh/x.").as_deref(),
            Some("https://app.paseo.sh/x")
        );
        // Falls back to any https URL when none mention paseo.
        assert_eq!(
            extract_pairing_url("go to https://example.test/p now").as_deref(),
            Some("https://example.test/p")
        );
        // Nothing URL-shaped -> None.
        assert_eq!(extract_pairing_url("no url here"), None);
    }

    #[test]
    fn ta158_ensure_daemon_does_not_treat_nonzero_start_as_fatal() {
        let s = menu_paseo::PASEO_ENSURE_DAEMON;
        // Every `paseo daemon start` is followed by `|| true`, so its exit code
        // (e.g. the readiness-gate "exit code 1" reported even when the worker is
        // actually listening on :6767) never aborts the snippet.
        assert_eq!(
            s.matches("paseo daemon start").count(),
            s.matches("paseo daemon start || true").count(),
            "every start attempt must be `|| true` so a non-zero exit is not fatal"
        );
        // Readiness is decided by re-probing the daemon status, not by `start`'s
        // exit code: the running-daemon gate appears before AND after starting.
        assert!(
            s.matches(r#"grep -Eq '"localDaemon":[[:space:]]*"running"'"#)
                .count()
                >= 1,
            "must gate on the localDaemon=running status probe"
        );
        assert!(
            s.matches("_paseo_up").count() >= 4,
            "must re-probe readiness after attempting start (not trust its exit code)"
        );
    }
}
