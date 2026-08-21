//! The control TUI (`introdus menu`) — the persistent two-pane ratatui panel
//! that runs in the `main-control` tmux window. The panel itself (left status +
//! menu, right output pane) lives in [`crate::panel`]; this module owns the menu
//! layout and dispatches selections to the utilities in [`crate::menu_actions`].
//! Host-side, so it can read/write `.env`, drive podman, open root/dev
//! terminals, and spawn agent windows — the things an in-container TUI could
//! never do.

use anyhow::Result;
use introdus_core::podman::{self, ContainerState};

use crate::context::{env_path, LaunchContext};
use crate::menu_actions as act;
use crate::panel::{Selection, Ui};
use crate::ui;
use introdus_core::Config;

/// A menu entry: either a selectable action or an inert section header. A header
/// carries a group glyph and, with the divider drawn before it, segregates the
/// flat list into scannable sections; selecting one just redraws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Header(&'static str, char),
    Item(Action),
}

/// The selectable actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    // Access container
    DevTerminal,
    RootTerminal,
    CopyFile,
    // Agents
    LaunchAgent,
    InstallAgent,
    InstallPaseo,
    UpdatePaseo,
    PaseoQr,
    AddPaseoOrigin,
    // Networking & egress security
    BlockedEgress,
    AddAllowlist,
    ExposeWebapp,
    TunnelUrl,
    EnableNtfy,
    // Container lifecycle
    Restart,
    Recreate,
    Detach,
    DestroyReset,
    QuitStop,
    // Troubleshooting
    Refresh,
    TestNotify,
    NotifyLog,
    RestartNotify,
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Action::DevTerminal => "Open a dev terminal (tmux window)",
            Action::RootTerminal => "Open a root terminal (tmux window)",
            Action::CopyFile => "Copy a host file/folder into the container",
            Action::LaunchAgent => "Launch an installed agent (tmux window)",
            Action::InstallAgent => "Install a coding agent",
            Action::InstallPaseo => "(Re)Install paseo (control agents from your phone)",
            Action::UpdatePaseo => "Update paseo daemon (safe 7-day release delay)",
            Action::PaseoQr => "Show paseo pairing QR code (connect your phone)",
            Action::AddPaseoOrigin => "Add a paseo client origin (direct-mode browser access)",
            Action::BlockedEgress => "List recently blocked egress URLs",
            Action::AddAllowlist => "Add hostnames to the egress allowlist",
            Action::ExposeWebapp => "(Re)Expose app via Cloudflare Tunnel",
            Action::TunnelUrl => "Show tunnel URL",
            Action::EnableNtfy => "Enable ntfy.sh mobile notifications",
            Action::Restart => "Restart the container",
            Action::Recreate => "Recreate the container (apply config changes)",
            Action::Detach => "Detach from this tmux session (keep container running)",
            Action::DestroyReset => "Destroy/Reset the container (wipe the volume)",
            Action::QuitStop => "Quit introdus (stops the container)",
            Action::Refresh => "Refresh container status",
            Action::TestNotify => "Send a test notification to host",
            Action::NotifyLog => "Show the notification log",
            Action::RestartNotify => {
                "Restart the notification service (apply forward/ntfy changes)"
            }
        };
        f.write_str(s)
    }
}

impl Action {
    /// The single-key shortcut that runs this action directly from the menu
    /// (shown in its own column). Kept unique across the whole menu and matched
    /// **case-sensitively**, so a *shifted* key is a distinct, related action:
    /// `T` root vs `t` dev terminal, `P` paseo-QR vs `p` install-paseo, `N`
    /// test-notification vs `n` enable-ntfy. The two irreversible actions
    /// (`DestroyReset`, `QuitStop`) confirm before doing anything.
    fn hotkey(self) -> char {
        match self {
            Action::DevTerminal => 't',
            Action::RootTerminal => 'T',
            Action::CopyFile => 'c',
            Action::LaunchAgent => 'a',
            Action::InstallAgent => 'i',
            Action::InstallPaseo => 'p',
            Action::UpdatePaseo => 'U',
            Action::PaseoQr => 'P',
            Action::AddPaseoOrigin => 'o',
            Action::BlockedEgress => 'b',
            Action::AddAllowlist => 'w',
            Action::ExposeWebapp => 'e',
            Action::TunnelUrl => 'u',
            Action::EnableNtfy => 'n',
            Action::Restart => 's',
            Action::Recreate => 'x',
            Action::Detach => 'h',
            Action::DestroyReset => 'd',
            Action::QuitStop => 'q',
            Action::Refresh => 'f',
            Action::TestNotify => 'N',
            Action::NotifyLog => 'l',
            Action::RestartNotify => 'v',
        }
    }
}

const MENU: &[Row] = &[
    Row::Header("Access container", '$'),
    Row::Item(Action::DevTerminal),
    Row::Item(Action::RootTerminal),
    Row::Item(Action::CopyFile),
    Row::Header("Agents", '✦'),
    Row::Item(Action::LaunchAgent),
    Row::Item(Action::InstallAgent),
    Row::Item(Action::InstallPaseo),
    Row::Item(Action::UpdatePaseo),
    Row::Item(Action::PaseoQr),
    Row::Item(Action::AddPaseoOrigin),
    Row::Header("Networking & egress security", '⇅'),
    Row::Item(Action::BlockedEgress),
    Row::Item(Action::AddAllowlist),
    Row::Item(Action::ExposeWebapp),
    Row::Item(Action::TunnelUrl),
    Row::Item(Action::EnableNtfy),
    Row::Header("Troubleshooting", '?'),
    Row::Item(Action::Refresh),
    Row::Item(Action::TestNotify),
    Row::Item(Action::NotifyLog),
    Row::Item(Action::RestartNotify),
    Row::Header("Container lifecycle", '↻'),
    Row::Item(Action::Restart),
    Row::Item(Action::Recreate),
    Row::Item(Action::Detach),
    Row::Item(Action::DestroyReset),
    Row::Item(Action::QuitStop),
];

/// The visible label for a menu item — mostly the static [`Action`] label, but
/// the paseo "connect" item reads as the direct URL in direct mode (there's no
/// QR to scan; you paste a `tcp://…` URL into the client).
fn item_label(a: Action, ctx: &LaunchContext) -> String {
    match a {
        Action::PaseoQr if ctx.config.paseo_mode.is_direct() => {
            "Show Paseo Direct URL (and password)".to_owned()
        }
        _ => a.to_string(),
    }
}

/// Run the control menu for the current project until the user quits. The
/// [`Ui`] owns the alternate screen for the whole session; each turn re-snapshots
/// the status/menu, then an action's output streams into the right-hand pane.
pub fn run() -> Result<()> {
    let dir = std::env::current_dir()?;
    let env = env_path(&dir);
    // The tmux session to kill once the Ui is torn down (closing every window),
    // or `None` to leave it up. Only "Quit introdus" sets it (it stops the
    // container, then breaks the loop with this value; the enclosing block drops
    // the Ui before we act on it). Detach / Esc do NOT end the loop — the menu
    // process must stay alive in its window so a reattach lands back on it.
    let kill_session: Option<String> = {
        let mut ui = Ui::new()?;
        loop {
            // Reload each iteration so actions that edited .env are reflected, and
            // re-snapshot the container state for the status panel.
            let config = Config::load(&env)?;
            let ctx = LaunchContext::resolve(config, dir.clone())?;
            let status = status_of(&ctx);
            let rows: Vec<ui::Row> = MENU
                .iter()
                .map(|r| match r {
                    Row::Header(h, icon) => ui::Row::Header {
                        icon: *icon,
                        title: (*h).to_owned(),
                    },
                    Row::Item(a) => ui::Row::Item {
                        key: a.hotkey(),
                        label: item_label(*a, &ctx),
                    },
                })
                .collect();
            ui.set_menu(status, rows);

            let action = match ui.run_menu()? {
                Selection::Item(idx) => match MENU[idx] {
                    Row::Item(a) => a,
                    Row::Header(..) => continue,
                },
                // A poll tick: re-snapshot the status (loop top does it) + redraw.
                Selection::Tick => continue,
                // Esc / Ctrl-C: same as the "Detach tmux session" item.
                Selection::Quit => match detach(&ctx) {
                    Detach::Continue => continue,
                    Detach::Exit => break None,
                },
            };
            match action {
                // Detach every client and return to the shell; the session, its
                // windows, and the container keep running, so a later `introdus`
                // reattaches. The menu keeps running (we do NOT end the loop).
                // Run bare (no tmux to detach from) it just exits.
                Action::Detach => match detach(&ctx) {
                    Detach::Continue => continue,
                    Detach::Exit => break None,
                },
                // Refresh just falls through to the next loop, which re-snapshots.
                Action::Refresh => continue,
                // Stop the container, then break out and (below, after the Ui is
                // dropped) kill the whole session — closing every window.
                Action::QuitStop => {
                    ui.begin(&action.to_string());
                    match act::stop_for_quit(&ctx, &mut ui) {
                        Ok(true) => break Some(act::session_of(&ctx)),
                        Ok(false) => ui.drain_input(),
                        Err(e) => {
                            ui.log(format!("  ! {e:#}"));
                            ui.drain_input();
                        }
                    }
                }
                _ => {
                    ui.begin(&action.to_string());
                    if let Err(e) = dispatch(action, &ctx, &mut ui) {
                        ui.log(format!("  ! {e:#}"));
                    }
                    // Discard keys mashed while the (possibly blocking) action ran,
                    // so they don't fire as a cascade of unintended selections.
                    ui.drain_input();
                }
            }
        }
    }; // Ui dropped here: alternate screen exited + terminal restored.

    if let Some(session) = kill_session {
        // Closes every window (this TUI's included); the detached notify service
        // self-exits once the session is gone.
        let _ = introdus_core::tmux::kill_session(&session);
    }
    Ok(())
}

/// Whether the menu loop should keep running after a detach request.
enum Detach {
    /// Stay in the loop — the menu process lives on in its (now-unattached) window.
    Continue,
    /// End the loop and exit the process.
    Exit,
}

/// Handle a "Detach tmux session" / Esc request. Inside tmux (the normal case:
/// the menu is the `main-control` window's command), detach every client from
/// this session — each returns to the shell that ran `introdus` — while the
/// session, its windows, and the container keep running, so a later `introdus`
/// reattaches to the same session; the menu process stays alive, so we
/// [`Detach::Continue`]. Run bare (no `$TMUX`, e.g. a direct `introdus menu`),
/// there is nothing to detach from, so Esc simply [`Detach::Exit`]s. Detach
/// errors are swallowed: detaching when no client is attached (e.g. under the
/// test harness) is a harmless no-op.
fn detach(ctx: &LaunchContext) -> Detach {
    if std::env::var_os("TMUX").is_none() {
        return Detach::Exit;
    }
    let _ = introdus_core::tmux::detach_client(&act::session_of(ctx));
    Detach::Continue
}

fn dispatch(action: Action, ctx: &LaunchContext, ui: &mut Ui) -> Result<()> {
    match action {
        Action::TunnelUrl => act::tunnel_url(ctx, ui),
        Action::ExposeWebapp => crate::menu_tunnel::reexpose_webapp(ctx, ui),
        Action::EnableNtfy => act::enable_ntfy(ctx, ui),
        Action::CopyFile => act::copy_file(ctx, ui),
        Action::InstallAgent => act::install_agent(ctx, ui),
        Action::InstallPaseo => crate::menu_paseo::install_paseo(ctx, ui),
        Action::UpdatePaseo => crate::menu_paseo::update_paseo(ctx, ui),
        Action::PaseoQr => crate::menu_paseo::paseo_qr(ctx, ui),
        Action::AddPaseoOrigin => crate::menu_paseo::add_origin(ctx, ui),
        Action::LaunchAgent => act::launch_agent(ctx, ui),
        Action::BlockedEgress => act::blocked_egress(ctx, ui),
        Action::AddAllowlist => act::add_allowlist(ctx, ui),
        Action::RootTerminal => act::open_terminal(ctx, ui, None),
        Action::DevTerminal => act::open_terminal(ctx, ui, Some("dev")),
        Action::TestNotify => act::test_notify(ctx, ui),
        Action::NotifyLog => act::notify_log(ctx, ui),
        Action::RestartNotify => act::restart_notify(ctx, ui),
        Action::Restart => act::restart(ctx, ui),
        Action::Recreate => act::recreate(ctx, ui),
        Action::DestroyReset => act::destroy_or_reset(ctx, ui),
        // Handled directly in `run()` (detach / refresh / end the loop), never
        // dispatched.
        Action::Refresh | Action::Detach | Action::QuitStop => Ok(()),
    }
}

/// Snapshot the live status shown in the panel's header.
fn status_of(ctx: &LaunchContext) -> ui::Status {
    let launching = crate::launch::is_launching(ctx);
    let state = match podman::container_state(&ctx.container_name) {
        ContainerState::Running => {
            // The container is up — the launch (if any) is done, so drop the
            // marker; a later Stop must read as "stopped", not "starting".
            crate::launch::clear_launch_marker(ctx);
            "running"
        }
        // A launch is underway but the container isn't running yet (still being
        // created, or existing-but-not-started): report it as starting.
        ContainerState::Stopped | ContainerState::Absent if launching => "starting container…",
        ContainerState::Stopped => "stopped",
        ContainerState::Absent => "not created",
    };
    ui::Status {
        project: ctx.config.project_name.clone(),
        container: ctx.container_name.clone(),
        state,
        webapp_port: ctx.config.webapp_port,
        webapp_host_port: ctx.config.webapp_host_port,
        agents: ctx.config.install_agents.join(", "),
    }
}
