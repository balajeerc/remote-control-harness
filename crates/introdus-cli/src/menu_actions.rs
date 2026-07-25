//! Implementations of the control-menu utilities. Each takes the resolved
//! [`LaunchContext`] and the panel [`Ui`], performing one action and streaming
//! its progress into the output pane (`ui.log`) — external commands it runs are
//! captured into the same pane automatically (see [`crate::panel`]). These run
//! on the host, so they can edit `.env`, drive podman, spawn tmux windows, and
//! open a root shell — the operations an in-container TUI can't do.

use anyhow::{bail, Context, Result};
use introdus_core::podman::{self, exec_interactive, podman};
use introdus_core::{agents, session as session_names, tmux, Config};

use crate::context::{env_path, LaunchContext};
use crate::frontend::Frontend;
use crate::panel::Ui;
use crate::util::{expand_tilde, shell_quote};

/// How many trailing lines of the notify-host log the menu shows.
const NOTIFY_LOG_TAIL: usize = 40;

// ---- read-only / runtime utilities -----------------------------------------

/// Show the cached cloudflared quick-tunnel URL from inside the container. Its
/// output is captured into the pane automatically (no explicit `ui.log`).
pub fn tunnel_url(ctx: &LaunchContext, _f: &mut impl Frontend) -> Result<()> {
    require_running(ctx)?;
    let _ = exec(ctx, Some("dev")).arg("tunnel-url").run();
    Ok(())
}

/// Show the hostnames the egress proxy recently blocked (captured into the pane).
pub fn blocked_egress(ctx: &LaunchContext, _f: &mut impl Frontend) -> Result<()> {
    require_running(ctx)?;
    let _ = exec(ctx, Some("dev")).arg("egress-log").run();
    Ok(())
}

/// Fire a test notification event from inside the container.
pub fn test_notify(ctx: &LaunchContext, f: &mut impl Frontend) -> Result<()> {
    require_running(ctx)?;
    f.log("  sending a test 'done' notification via rc-notify…");
    let _ = exec(ctx, Some("dev")).args(["rc-notify", "done"]).run();
    // Surface where the config says the event should land — the usual "no popup"
    // cause is a forward mismatch, so name it here.
    match &ctx.config.rc_forward_addr {
        Some(addr) => {
            f.log(format!(
                "  config: forwarding to {addr} (a remote dev machine over the ssh reverse tunnel)"
            ));
            f.log("  if you changed this recently, use 'Restart the notification service' so");
            f.log("  the running notify-host picks it up (its env froze at session start).");
        }
        None => f.log("  config: rendering locally on this host (RC_FORWARD_ADDR unset)"),
    }
    f.log("  (delivery is handled by the detached notify service — see its log below)");
    Ok(())
}

/// Restart the detached `notify-host` so it re-reads the project's notification
/// config (e.g. a just-added `RC_FORWARD_ADDR`) without recreating the container
/// or bouncing the tmux session.
pub fn restart_notify(ctx: &LaunchContext, f: &mut impl Frontend) -> Result<()> {
    f.log("  restarting the notification service (notify-host)…");
    crate::session::restart_notify_host(&ctx.config, &session_of(ctx))?;
    match &ctx.config.rc_forward_addr {
        Some(addr) => f.log(format!("  restarted — now forwarding to {addr}.")),
        None => f.log("  restarted — now rendering locally on this host."),
    }
    Ok(())
}

/// Show the tail of the detached notify-host service's log. The service has no
/// tmux window of its own; this is how you inspect what it has delivered.
pub fn notify_log(ctx: &LaunchContext, f: &mut impl Frontend) -> Result<()> {
    let path = introdus_core::paths::notify_log(&session_of(ctx))?;
    match std::fs::read_to_string(&path) {
        Ok(s) if !s.trim().is_empty() => {
            let lines: Vec<&str> = s.lines().collect();
            let start = lines.len().saturating_sub(NOTIFY_LOG_TAIL);
            if start > 0 {
                f.log(format!(
                    "  notification log (last {} lines of {}):",
                    NOTIFY_LOG_TAIL,
                    lines.len()
                ));
            } else {
                f.log("  notification log:");
            }
            for line in &lines[start..] {
                f.log(format!("    {line}"));
            }
        }
        _ => f.log(format!(
            "  no notifications logged yet ({})",
            path.display()
        )),
    }
    Ok(())
}

// ---- terminals & agent windows ---------------------------------------------

/// Open a shell in a new tmux window: `root-bash` (root) or `dev-bash` (dev).
pub fn open_terminal(ctx: &LaunchContext, ui: &mut Ui, user: Option<&str>) -> Result<()> {
    require_running(ctx)?;
    let window = if user.is_some() {
        "dev-bash"
    } else {
        "root-bash"
    };
    let cmd = exec_interactive(&ctx.container_name, user)
        .arg("bash")
        .shell_line()
        .to_owned();
    spawn_window(ctx, ui, window, &cmd)
}

/// Launch an installed agent in its own tmux window (Claude via `run-claude`,
/// with remote control already on; others via their own command). When the
/// agent supports a skip-permissions / auto-approve flag, offer to launch with
/// it so the agent runs unattended.
pub fn launch_agent(ctx: &LaunchContext, ui: &mut Ui) -> Result<()> {
    require_running(ctx)?;
    let installed = ctx.config.install_agents.clone();
    if installed.is_empty() {
        bail!("no agents configured to launch");
    }
    let id = ui.select("Launch which agent?", installed)?;
    let agent = agents::find(&id);
    let label = agent.map(|a| a.label).unwrap_or(id.as_str());
    let cmd_name = agent.map(|a| a.cmd).unwrap_or(id.as_str());

    // The menu lists what was *selected* in `.env`, but install-agents is
    // deliberately never-fatal: a blocked-egress or pnpm failure leaves the
    // agent absent while it stays in the config. Launching a missing binary just
    // flashes a tmux window that exits 127 ("command not found"). So verify the
    // binary is actually present first, and offer to (re)install it if not.
    if !container_has_cmd(ctx, cmd_name) {
        ui.log(format!(
            "  {label} is selected but its `{cmd_name}` command isn't installed"
        ));
        ui.log("  (the earlier install likely failed — e.g. blocked egress to the registry).");
        if !ui.confirm(&format!("Install {label} now?"), true)? {
            ui.log("  skipped — nothing launched.");
            return Ok(());
        }
        install_one_agent(ctx, ui, &id)?;
        if !container_has_cmd(ctx, cmd_name) {
            bail!(
                "{label} still isn't installed after the attempt — check `egress-log` \
                 for a blocked host, then try Recreate + install again"
            );
        }
    }

    // paseo does NOT wrap the agent launch: `paseo run` headless isn't the
    // intended path. The daemon (started with the pairing QR) supervises agents,
    // and you orchestrate them from the paseo phone/desktop app. So every agent —
    // paseo enabled or not — launches directly in its own tmux window here.
    let flag = resolve_yolo(
        agent.map(|a| a.yolo).unwrap_or(agents::Yolo::None),
        label,
        ui,
    )?;

    let mut cmd = exec_interactive(&ctx.container_name, Some("dev"));
    if id == "claude" {
        // claude launches through run-claude (repo cd + the remote-control 'claude'
        // session). Tell it whether to skip permissions: the flag, or `--safe`.
        cmd = cmd.arg("run-claude").arg(flag.unwrap_or("--safe"));
    } else {
        cmd = cmd.arg(cmd_name);
        if let Some(f) = flag {
            cmd = cmd.arg(f);
        }
    }
    let cmd = cmd.shell_line().to_owned();
    spawn_window(ctx, ui, &format!("agent-{id}"), &cmd)
}

/// Offer to launch with the agent's bypass/auto flag when it has one. Returns
/// the flag to append (`None` = launch with prompts on / no flag). `Always`
/// agents (e.g. pi) need no flag; a note is logged instead.
fn resolve_yolo(yolo: agents::Yolo, label: &str, ui: &mut Ui) -> Result<Option<&'static str>> {
    match yolo {
        agents::Yolo::Bypass(flag) => {
            let on = ui.confirm(
                &format!("Launch {label} with {flag} — skips ALL permission prompts (unattended)?"),
                true,
            )?;
            Ok(on.then_some(flag))
        }
        agents::Yolo::Auto(flag) => {
            let on = ui.confirm(
                &format!(
                    "Launch {label} with {flag} — auto-approves actions (deny rules still apply)?"
                ),
                true,
            )?;
            Ok(on.then_some(flag))
        }
        agents::Yolo::Always => {
            ui.log(format!(
                "  {label} always runs in auto-approve mode — no flag needed."
            ));
            Ok(None)
        }
        agents::Yolo::None => Ok(None),
    }
}

/// The agent's bypass/auto flag, or `None` when it has none (or always
/// auto-approves). The non-interactive counterpart to [`resolve_yolo`], used by
/// the CLI `agent --yolo` launch where the choice comes from a flag, not a prompt.
pub(crate) fn yolo_flag(yolo: agents::Yolo) -> Option<&'static str> {
    match yolo {
        agents::Yolo::Bypass(flag) | agents::Yolo::Auto(flag) => Some(flag),
        agents::Yolo::Always | agents::Yolo::None => None,
    }
}

// ---- egress allowlist -------------------------------------------------------

/// Append hostnames to `WHITELIST_HOSTS`, regenerate the allowlist file, and
/// offer to restart the container to apply it.
pub fn add_allowlist(ctx: &LaunchContext, ui: &mut Ui) -> Result<()> {
    let raw = ui.text("Hostnames to allow (space/comma separated):", false)?;
    let mut config = ctx.config.clone();
    let added = append_whitelist(&mut config, &raw);
    if added.is_empty() {
        ui.log("  nothing new to add.");
        return Ok(());
    }
    save_and_regen_allowlist(
        ctx,
        ui,
        config,
        &format!("added {} host(s): {}", added.len(), added.join(", ")),
    )
}

/// Append the (space/comma/newline/tab-separated) hostnames in `raw` to
/// `config.whitelist_hosts`, skipping blanks and ones already present. Returns
/// the hosts actually newly added. Shared by the menu and CLI allowlist flows.
pub(crate) fn append_whitelist(config: &mut Config, raw: &str) -> Vec<String> {
    let mut added = Vec::new();
    for host in raw
        .split([',', ' ', '\n', '\t'])
        .map(str::trim)
        .filter(|h| !h.is_empty())
    {
        let host = host.to_owned();
        if !config.whitelist_hosts.contains(&host) {
            config.whitelist_hosts.push(host.clone());
            added.push(host);
        }
    }
    added
}

// ---- config toggles (need a recreate to apply) ------------------------------

/// Turn on ntfy.sh push (prompting for the topic) and offer to recreate.
pub fn enable_ntfy(ctx: &LaunchContext, ui: &mut Ui) -> Result<()> {
    let topic = ui.text("ntfy.sh topic (treat like a password):", true)?;
    if topic.trim().is_empty() {
        bail!("a topic is required");
    }
    let mut config = ctx.config.clone();
    config.enable_notify_sh_alerts = true;
    config.ntfy_sh_topic = Some(topic.trim().to_owned());
    save_config(ctx, ui, &config)?;
    offer_recreate(ctx, ui, "ENABLE_NOTIFY_SH_ALERTS=true")
}

/// Install one or more not-yet-selected agents into the running container.
pub fn install_agent(ctx: &LaunchContext, ui: &mut Ui) -> Result<()> {
    require_running(ctx)?;
    let candidates: Vec<String> = agents::AGENTS
        .iter()
        .filter(|a| !a.prebaked && !ctx.config.install_agents.iter().any(|id| id == a.id))
        .map(|a| a.id.to_owned())
        .collect();
    if candidates.is_empty() {
        ui.log("  all supported agents are already selected.");
        return Ok(());
    }
    let picked: Vec<String> = ui
        .multiselect_indexed(
            "Install which agents? (Space toggles, Enter confirms)",
            &candidates,
            &[],
        )?
        .into_iter()
        .filter_map(|i| candidates.get(i).cloned())
        .collect();
    if picked.is_empty() {
        ui.log("  no agents selected — nothing to install.");
        return Ok(());
    }
    let mut config = ctx.config.clone();
    select_agents(&mut config, &picked);
    save_and_regen_allowlist(
        ctx,
        ui,
        config.clone(),
        &format!("selected: {}", picked.join(", ")),
    )?;
    run_install_agents(ctx, ui, &config.install_agents.join(" "))?;
    ui.log(
        "  note: new egress hosts apply after a restart — use Recreate if an install was blocked.",
    );
    Ok(())
}

/// Add each agent id in `ids` to `config.install_agents` (if absent) plus its
/// declared extra egress hosts to `config.whitelist_hosts`. Returns the ids that
/// were newly added. Shared by the menu and CLI install-agent flows.
pub(crate) fn select_agents(config: &mut Config, ids: &[String]) -> Vec<String> {
    let mut added = Vec::new();
    for id in ids {
        if !config.install_agents.contains(id) {
            config.install_agents.push(id.clone());
            added.push(id.clone());
        }
        if let Some(agent) = agents::find(id) {
            for h in agent.host_list() {
                let h = h.to_owned();
                if !config.whitelist_hosts.contains(&h) {
                    config.whitelist_hosts.push(h);
                }
            }
        }
    }
    added
}

/// Install a single agent on demand (used when a selected agent's binary is
/// missing at launch). The agent is already in `.env`; this just (re)runs the
/// in-container installer for it.
fn install_one_agent(ctx: &LaunchContext, ui: &mut Ui, id: &str) -> Result<()> {
    run_install_agents(ctx, ui, id)
}

/// Stream `install-agents` for the given space-separated agent ids.
pub(crate) fn run_install_agents(
    ctx: &LaunchContext,
    f: &mut impl Frontend,
    agent_ids: &str,
) -> Result<()> {
    // Pass INSTALL_AGENTS *into* the container with an `env` prefix — a host-side
    // `.env()` on the podman process is NOT forwarded through `podman exec`, so
    // install-agents would otherwise only see the baked-in list and install
    // nothing. A real install can take a while and is chatty, so run it as a
    // streaming task: progress shows live and the menu stays disabled until done.
    f.run_task(
        "install-agents",
        exec(ctx, Some("dev"))
            .arg("env")
            .arg(format!("INSTALL_AGENTS={agent_ids}"))
            .arg("install-agents"),
    )
}

// ---- copy a host file into the container ------------------------------------

/// Copy a host file/folder into the container's `/home/dev/uploads`.
pub fn copy_file(ctx: &LaunchContext, ui: &mut Ui) -> Result<()> {
    require_running(ctx)?;
    let raw = ui.text("Host path to copy (file or folder):", false)?;
    let src = expand_tilde(raw.trim());
    if !src.exists() {
        bail!("no such path: {}", src.display());
    }
    let dest = format!("{}:/home/dev/uploads/", ctx.container_name);
    exec(ctx, Some("dev"))
        .args(["mkdir", "-p", "/home/dev/uploads"])
        .run()?;
    // A large file/folder can take a while — stream it as a task so the panel
    // shows a spinner rather than freezing on a blocking copy.
    ui.run_task(
        "copying into the container",
        podman().arg("cp").arg(&src).arg(&dest),
    )?;
    exec(ctx, None)
        .args(["chown", "-R", "dev:dev", "/home/dev/uploads"])
        .run()?;
    ui.log(format!("  copied {} -> /home/dev/uploads/", src.display()));
    Ok(())
}

// ---- container lifecycle from the menu --------------------------------------

/// Recreate the container (drop it, keep the volume) to apply frozen `.env`
/// changes, respawning the dev-container window.
pub fn recreate(ctx: &LaunchContext, ui: &mut Ui) -> Result<()> {
    if !ui.confirm(
        "Recreate the container now? (keeps your /home/dev volume)",
        true,
    )? {
        return Ok(());
    }
    remove_container_task(ctx, ui, "recreating the container")?;
    respawn_dev_window(ctx, ui)
}

/// Restart the container in place (re-runs its entrypoint; keeps the volume).
/// `podman restart` starts a stopped container too, so it covers both states.
pub fn restart(ctx: &LaunchContext, f: &mut impl Frontend) -> Result<()> {
    if podman::container_state(&ctx.container_name) == podman::ContainerState::Absent {
        bail!(
            "container {} isn't created yet — use Recreate to build it",
            ctx.container_name
        );
    }
    f.run_task(
        "restarting the container",
        podman().args(["restart", &ctx.container_name]),
    )?;
    f.log("  restarted.");
    Ok(())
}

/// Quit flow: confirm, stop the container, and tell the caller to tear the
/// whole tmux session down (closing every window). `Ok(true)` proceeds with the
/// quit; `Ok(false)` means the user cancelled.
pub fn stop_for_quit(ctx: &LaunchContext, ui: &mut Ui) -> Result<bool> {
    if !ui.confirm(
        "Quit introdus — stop the container and close all its windows?",
        true,
    )? {
        ui.log("  cancelled.");
        return Ok(false);
    }
    if podman::container_running(&ctx.container_name) {
        ui.run_task(
            "stopping the container",
            podman().args(["stop", &ctx.container_name]),
        )?;
        ui.log("  container stopped.");
    } else {
        ui.log("  container already stopped.");
    }
    ui.log("  closing all windows…");
    Ok(true)
}

/// Merged Destroy/Reset: wipe the `/home/dev` volume (the destructive core both
/// operations share), then two follow-up prompts decide which one it was —
/// whether to also delete the local deploy key (destroy), and whether to bring
/// the container back up fresh (reset) or leave it torn down (destroy). Guarded
/// by a yes/no plus the dirty-git scan + typed-'yes' confirmation.
pub fn destroy_or_reset(ctx: &LaunchContext, ui: &mut Ui) -> Result<()> {
    if !podman::container_exists(&ctx.container_name) && !podman::volume_exists(&ctx.volume_name) {
        ui.log("  nothing to do — no container or volume for this project.");
        return Ok(());
    }
    if !ui.confirm(
        "Destroy/Reset this container and permanently wipe its /home/dev volume?",
        false,
    )? {
        ui.log("  aborted.");
        return Ok(());
    }
    // Stronger confirmation: scans for uncommitted work, requires typed 'yes'.
    confirm_wipe(ctx, ui)?;
    remove_container_task(ctx, ui, "wiping the container")?;
    remove_volume_task(ctx, ui, "wiping the volume")?;
    ui.log(format!("  wiped the volume for {}.", ctx.container_name));
    // The destroy extra: optionally delete the local deploy key too.
    offer_remove_deploy_key(ctx, ui)?;
    // Reset vs destroy: bring it back up fresh, or leave it fully torn down.
    if ui.confirm(
        "Bring the container back up now with a fresh volume? (No leaves it torn down)",
        true,
    )? {
        respawn_dev_window(ctx, ui)
    } else {
        // Nothing runs in the dev-container window anymore; close it if present.
        let _ = tmux::kill_window(&session_of(ctx), "dev-container");
        ui.log(format!("  torn down {}.", ctx.container_name));
        Ok(())
    }
}

/// The data-loss guard for reset/destroy, rendered into the pane: warn, show the
/// best-effort dirty-git scan, then require a typed `yes`. Mirrors
/// [`crate::lifecycle::confirm_reset`] but through the panel instead of stdio.
fn confirm_wipe(ctx: &LaunchContext, ui: &mut Ui) -> Result<()> {
    if !podman::volume_exists(&ctx.volume_name) {
        return Ok(());
    }
    ui.log(format!(
        "  !!  wipe will PERMANENTLY DELETE volume {} — the repo, uncommitted",
        ctx.volume_name
    ));
    ui.log("      changes, branches, and installed packages under /home/dev.");
    // Stream the dirty-git scan with a spinner (it takes a few seconds) rather
    // than freezing the panel; the report lands in the output pane as it runs.
    let found = match crate::lifecycle::dirty_scan_cmd(ctx) {
        Some(cmd) => {
            let lines =
                ui.run_task_lines("scanning /home/dev/work for uncommitted git state", cmd)?;
            lines.iter().any(|l| !l.trim().is_empty())
        }
        None => false,
    };
    if found {
        ui.log("  ^^ the git state above would be PERMANENTLY LOST.");
    } else {
        ui.log("      (no uncommitted git state detected — but double-check anyway.)");
    }
    let typed = ui.text(
        "Type 'yes' to permanently wipe it (anything else aborts):",
        false,
    )?;
    if typed.trim() != "yes" {
        bail!("aborted.");
    }
    Ok(())
}

/// Offer to delete the local deploy key (and its `.pub`) after a destroy. The
/// repo-side registration is untouched — that's removed on the git host.
fn offer_remove_deploy_key(ctx: &LaunchContext, ui: &mut Ui) -> Result<()> {
    let key = ctx.config.deploy_key_path.trim();
    if key.is_empty() {
        return Ok(());
    }
    let path = expand_tilde(key);
    if !path.exists() {
        return Ok(());
    }
    ui.log("  note: removes the private key + its .pub here; unregister it on the git host separately.");
    if !ui.confirm(
        &format!("Also delete the local deploy key at {}?", path.display()),
        false,
    )? {
        return Ok(());
    }
    std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    let pubkey = crate::util::pub_sibling(&path);
    if pubkey.exists() {
        let _ = std::fs::remove_file(&pubkey);
    }
    ui.log(format!("  deleted deploy key {}.", path.display()));
    Ok(())
}

// ---- helpers ----------------------------------------------------------------

pub(crate) fn require_running(ctx: &LaunchContext) -> Result<()> {
    if podman::container_running(&ctx.container_name) {
        Ok(())
    } else {
        bail!("container {} is not running", ctx.container_name)
    }
}

/// Force-remove the container (if present) as a spinner-backed task labelled
/// `label`. Routing through `run_task` (which streams on a worker thread) means
/// the panel shows an animated `working: <label>…` footer and stays responsive
/// during the ~10s SIGTERM→SIGKILL teardown, instead of freezing on a blocking
/// `.run()`. No-op if the container is already gone.
pub(crate) fn remove_container_task(ctx: &LaunchContext, ui: &mut Ui, label: &str) -> Result<()> {
    if podman::container_exists(&ctx.container_name) {
        ui.run_task(label, podman().args(["rm", "-f", &ctx.container_name]))?;
    }
    Ok(())
}

/// Remove the project volume (if present) as a spinner-backed task labelled
/// `label`, for the same non-blocking reason as [`remove_container_task`].
fn remove_volume_task(ctx: &LaunchContext, ui: &mut Ui, label: &str) -> Result<()> {
    if podman::volume_exists(&ctx.volume_name) {
        ui.run_task(label, podman().args(["volume", "rm", &ctx.volume_name]))?;
    }
    Ok(())
}

/// Whether `cmd` resolves inside the container for the dev user. Probes with a
/// plain `podman exec … sh -c 'command -v'`, which sees the image's ENV PATH —
/// the exact PATH the agent launch itself runs under (pnpm's global bin dir is
/// on it), so a hit here means the launch will find the binary too.
pub(crate) fn container_has_cmd(ctx: &LaunchContext, cmd: &str) -> bool {
    exec(ctx, Some("dev"))
        .args(["sh", "-c", &format!("command -v {}", shell_quote(cmd))])
        .ok()
}

pub(crate) fn exec(ctx: &LaunchContext, user: Option<&str>) -> introdus_core::process::Cmd {
    podman::exec(&ctx.container_name, user)
}

pub(crate) fn session_of(ctx: &LaunchContext) -> String {
    ctx.config
        .session_name
        .clone()
        .unwrap_or_else(|| session_names::generate(&ctx.config.project_name))
}

/// Open (and focus) a new tmux window running `cmd`.
pub(crate) fn spawn_window(
    ctx: &LaunchContext,
    ui: &mut Ui,
    window: &str,
    cmd: &str,
) -> Result<()> {
    let session = session_of(ctx);
    tmux::new_window(&session, window, cmd, true, &ctx.project_dir)?;
    ui.log(format!(
        "  opened window '{window}' (Ctrl-a then its number to return here)"
    ));
    Ok(())
}

/// Kill and re-open the dev-container window running `introdus up`.
pub(crate) fn respawn_dev_window(ctx: &LaunchContext, ui: &mut Ui) -> Result<()> {
    let session = session_of(ctx);
    let bin = std::env::current_exe().context("locating introdus binary")?;
    let cmd = format!("exec {} up", shell_quote(&bin.to_string_lossy()));
    tmux::kill_window(&session, "dev-container")?;
    tmux::new_window(&session, "dev-container", &cmd, true, &ctx.project_dir)?;
    ui.log("  dev-container window restarted — it will (re)create the container.");
    Ok(())
}

pub(crate) fn save_config(
    ctx: &LaunchContext,
    f: &mut impl Frontend,
    config: &Config,
) -> Result<()> {
    config.save(&env_path(&ctx.project_dir))?;
    f.log("  saved .env");
    Ok(())
}

/// Save the config and (re)write the bind-mounted proxy allowlist file from it.
/// No restart: the allowlist is a file the proxy re-reads on the next start, so
/// callers decide whether a restart/recreate is warranted (a frozen-env change
/// like an nft IP bypass needs a full recreate — see [`offer_recreate`]).
pub(crate) fn save_and_write_allowlist(
    ctx: &LaunchContext,
    f: &mut impl Frontend,
    config: Config,
    summary: &str,
) -> Result<()> {
    save_config(ctx, f, &config)?;
    LaunchContext::resolve(config, ctx.project_dir.clone())?.write_allowlist()?;
    f.log(format!("  {summary}"));
    Ok(())
}

/// [`save_and_write_allowlist`] then offer a restart so the running proxy picks up
/// the new hostnames — for allowlist-only changes (a bind-mounted file re-read on
/// restart).
fn save_and_regen_allowlist(
    ctx: &LaunchContext,
    ui: &mut Ui,
    config: Config,
    summary: &str,
) -> Result<()> {
    save_and_write_allowlist(ctx, ui, config, summary)?;
    if podman::container_running(&ctx.container_name)
        && ui.confirm("Restart the container to apply the new allowlist?", false)?
    {
        ui.run_task(
            "restarting the container",
            podman().args(["restart", &ctx.container_name]),
        )?;
    }
    Ok(())
}

pub(crate) fn offer_recreate(ctx: &LaunchContext, ui: &mut Ui, changed: &str) -> Result<()> {
    ui.log(format!(
        "  {changed} saved — it applies only after a container recreate (env is frozen at create)."
    ));
    if ui.confirm("Recreate the container now?", false)? {
        remove_container_task(ctx, ui, "recreating the container")?;
        return respawn_dev_window(ctx, ui);
    }
    Ok(())
}
