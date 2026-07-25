//! The `introdus` binary — control plane for the network-hardened dev-container
//! harness. Runs on the container host; drives podman + tmux + a full-screen
//! control TUI. See PLAN.md for the milestone roadmap.

mod cli_actions;
mod context;
mod frontend;
mod image;
mod install;
mod launch;
mod lifecycle;
mod menu;
mod menu_actions;
mod menu_paseo;
mod menu_tunnel;
mod notify;
mod notify_listen;
mod panel;
mod panel_draw;
mod preflight;
mod run;
#[cfg(test)]
mod screenshot;
mod send_files;
mod session;
mod ui;
mod util;
mod wizard;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

use launch::LaunchOpts;
use lifecycle::Lifecycle;

/// introdus — launch and drive network-hardened dev containers for AI agents.
#[derive(Debug, Parser)]
#[command(name = "introdus", version = introdus_core::VERSION, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

/// Flags shared by the launch-style subcommands.
#[derive(Debug, Clone, Copy, Args)]
struct LaunchArgs {
    /// On next start, fast-forward the project repo (git fetch + pull --ff-only).
    #[arg(long)]
    pull: bool,
    /// Run with NO egress filtering (all outbound permitted).
    #[arg(long)]
    disable_network_block: bool,
}

impl From<LaunchArgs> for LaunchOpts {
    fn from(a: LaunchArgs) -> Self {
        Self {
            pull: a.pull,
            disable_network_block: a.disable_network_block,
        }
    }
}

impl From<NotifyListenArgs> for notify_listen::Options {
    fn from(a: NotifyListenArgs) -> Self {
        Self {
            via: a.via,
            port: a.port,
            install_service: a.install_service,
            no_tunnel: a.no_tunnel,
            dry_run: a.dry_run,
        }
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Ensure the tmux session, control TUI, and container are up (default).
    Launch(LaunchArgs),
    /// Run the setup wizard standalone: (re)write this project's .env without
    /// launching a container. No podman required.
    Init,
    /// (internal) Run/start the podman container, streaming its logs.
    Up(LaunchArgs),
    /// (internal) Render the control TUI in the main-control pane.
    Menu,
    /// Run the egress smoke test without starting the workload.
    Verify,
    /// Remove and recreate the container (keeps the /home/dev volume).
    Recreate(LaunchArgs),
    /// Remove the container and wipe the /home/dev volume.
    Reset(LaunchArgs),
    /// In-container refresh: apt, mise, agents, LazyVim.
    Update,
    /// Rebuild the shared base image.
    RebuildBase,
    /// Host notification service: render / forward / ntfy push.
    NotifyHost,
    /// Dev-machine listener + ssh reverse tunnel for forwarded notifications.
    NotifyListen(NotifyListenArgs),
    /// Dual-pane file browser to send files/folders into a running introdus
    /// container — on this machine or an ssh-reachable host.
    SendFiles,
    /// Put the binary on PATH and set up host services.
    Install,

    // ---- control-panel utilities, also available headlessly ----------------
    /// Show the container's cached Cloudflare quick-tunnel URL.
    TunnelUrl,
    /// List hostnames the egress proxy recently blocked.
    BlockedEgress,
    /// Add hostnames to the egress allowlist (`WHITELIST_HOSTS`).
    Allow {
        /// Hostnames to allow (repeat for several).
        #[arg(required = true, value_name = "HOST")]
        hosts: Vec<String>,
        /// Restart the container so the running proxy picks them up.
        #[arg(long)]
        restart: bool,
    },
    /// (Re)expose the in-container app via a Cloudflare quick tunnel.
    ExposeWebapp {
        /// Recreate the container now to apply it (needed the first time).
        #[arg(long)]
        recreate: bool,
    },
    /// Enable ntfy.sh mobile push for a topic.
    Ntfy {
        /// The ntfy.sh topic to publish to (treat it like a password).
        #[arg(long)]
        topic: String,
        /// Recreate the container now to apply it.
        #[arg(long)]
        recreate: bool,
    },
    /// Install one or more coding agents into the running container.
    InstallAgent {
        /// Agent ids (e.g. `claude codex opencode`).
        #[arg(required = true, value_name = "AGENT")]
        agents: Vec<String>,
        /// Restart the container to apply the new egress hosts.
        #[arg(long)]
        restart: bool,
    },
    /// Launch an installed agent in the foreground.
    Agent {
        /// The installed agent id to launch (e.g. `claude`).
        #[arg(value_name = "AGENT")]
        id: String,
        /// Launch with the agent's skip-permissions / auto-approve flag.
        #[arg(long)]
        yolo: bool,
    },
    /// Install paseo (drive agents from your phone).
    InstallPaseo {
        /// Recreate the container now to wire paseo's relay access.
        #[arg(long)]
        recreate: bool,
    },
    /// Print the paseo pairing URL for the running daemon.
    PaseoUrl,
    /// Allow a browser origin to reach the direct-mode paseo daemon (CORS). The
    /// origin is the URL a browser loads the paseo UI from, e.g.
    /// http://<host-ip>:6767 (the host's IP + client port, not the phone's).
    PaseoAllowOrigin {
        /// The browser origin to allow (scheme://host:port).
        origin: String,
    },
    /// Open an interactive shell in the container (the `dev` user).
    DevShell,
    /// Open an interactive root shell in the container.
    RootShell,
    /// Send a test notification to the host.
    TestNotify,
    /// Show the tail of the notification service log.
    NotifyLog,
    /// Restart the notification service (apply forward/ntfy changes).
    RestartNotify,
    /// Restart the container in place (keeps the volume).
    Restart,
    /// Stop the container (requires `--yes`).
    Stop {
        /// Confirm stopping the container.
        #[arg(long)]
        yes: bool,
    },
}

/// Flags for `introdus notify-listen` (the dev-machine side). With no `--via`
/// and no `--port` (and nothing saved from a prior run), an interactive wizard
/// collects them.
#[derive(Debug, Clone, Args)]
struct NotifyListenArgs {
    /// SSH alias/host to open the reverse tunnel to (the container host, as named
    /// in your `~/.ssh/config`). Omit to be prompted / use the saved value.
    #[arg(long)]
    via: Option<String>,
    /// Loopback port used on both ends of the tunnel and by the listener
    /// (must match `RC_FORWARD_ADDR` on the host). Defaults to 8765.
    #[arg(long)]
    port: Option<u16>,
    /// Install and enable a `systemd --user` unit that runs this on each login,
    /// instead of running in the foreground now.
    #[arg(long)]
    install_service: bool,
    /// Only run the listener; don't manage the ssh reverse tunnel (bring your own).
    #[arg(long)]
    no_tunnel: bool,
    /// Resolve settings (running the wizard if needed) and print the plan without
    /// binding the port, opening the tunnel, or touching systemd.
    #[arg(long)]
    dry_run: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Launch(LaunchArgs {
        pull: false,
        disable_network_block: false,
    })) {
        // Launch drives the tmux session model; Up is the container run that
        // executes inside the session's dev-container window.
        Command::Launch(a) => session::launch(a.into()),
        Command::Init => run_init(),
        Command::Up(a) => launch::run_launch(Lifecycle::Keep, a.into()),
        Command::Recreate(a) => launch::run_launch(Lifecycle::Recreate, a.into()),
        Command::Reset(a) => launch::run_launch(Lifecycle::Reset, a.into()),
        Command::Verify => launch::run_verify(),
        Command::Update => launch::run_update(),
        Command::RebuildBase => launch::run_rebuild_base(),
        Command::Menu => menu::run(),
        Command::NotifyHost => notify::run_host(),
        Command::NotifyListen(a) => notify_listen::run(a.into()),
        Command::SendFiles => send_files::run(),
        Command::Install => install::run(),
        Command::TunnelUrl => cli_actions::tunnel_url(),
        Command::BlockedEgress => cli_actions::blocked_egress(),
        Command::Allow { hosts, restart } => cli_actions::allow(&hosts, restart),
        Command::ExposeWebapp { recreate } => cli_actions::expose_webapp(recreate),
        Command::Ntfy { topic, recreate } => cli_actions::ntfy(&topic, recreate),
        Command::InstallAgent { agents, restart } => cli_actions::install_agent(&agents, restart),
        Command::Agent { id, yolo } => cli_actions::agent(&id, yolo),
        Command::InstallPaseo { recreate } => cli_actions::install_paseo(recreate),
        Command::PaseoUrl => cli_actions::paseo_url(),
        Command::PaseoAllowOrigin { origin } => cli_actions::paseo_allow_origin(&origin),
        Command::DevShell => cli_actions::shell(false),
        Command::RootShell => cli_actions::shell(true),
        Command::TestNotify => cli_actions::test_notify(),
        Command::NotifyLog => cli_actions::notify_log(),
        Command::RestartNotify => cli_actions::restart_notify(),
        Command::Restart => cli_actions::restart(),
        Command::Stop { yes } => cli_actions::stop(yes),
    }
}

/// Run the setup wizard for the current directory, standalone. If a config
/// already exists, confirm before overwriting it.
fn run_init() -> Result<()> {
    let dir = std::env::current_dir()?;
    // Offer to relocate a legacy `./.env` before we decide what to reconfigure.
    context::migrate_legacy_config(&dir)?;
    let env = context::env_path(&dir);
    if env.exists()
        && !ui::confirm(
            &format!("{} exists — reconfigure it?", env.display()),
            false,
        )?
    {
        println!("  left config unchanged.");
        return Ok(());
    }
    wizard::run(&dir)?;
    Ok(())
}
