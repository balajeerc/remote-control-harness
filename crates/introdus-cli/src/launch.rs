//! Top-level launch orchestration, tying preflight, image, lifecycle, and run
//! together — the flow the old `launch_dev_container.sh` ran end to end.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use anyhow::{bail, Result};
use introdus_core::{agents, config, paths, podman, ports, Config};

use crate::context::{env_path, LaunchContext};
use crate::lifecycle::Lifecycle;
use crate::{image, lifecycle, preflight, run};

/// A launch marker older than this is treated as stale (a crashed/killed
/// launch) and ignored, so the status never sticks on "starting" forever.
const LAUNCH_MARKER_TTL: Duration = Duration::from_secs(30 * 60);

/// Options shared by the launch-style subcommands.
#[derive(Debug, Clone, Copy, Default)]
pub struct LaunchOpts {
    pub pull: bool,
    pub disable_network_block: bool,
}

/// Load the current project's `.env` from the working directory and resolve its
/// launch context.
pub fn load_context() -> Result<LaunchContext> {
    let dir = std::env::current_dir()?;
    load_context_in(dir)
}

fn load_context_in(dir: PathBuf) -> Result<LaunchContext> {
    let env = env_path(&dir);
    if !env.exists() {
        bail!(
            "no .env in {} — run `introdus` to set the project up (wizard)",
            dir.display()
        );
    }
    let config = Config::load(&env)?;
    LaunchContext::resolve(config, dir)
}

/// Create or start the container for the current project, applying `lifecycle`
/// first. On success this hands the terminal to podman and does not return.
pub fn run_launch(lifecycle: Lifecycle, opts: LaunchOpts) -> Result<()> {
    preflight::check_launch()?;
    provision_paseo_direct()?;
    let mut ctx = load_context()?;
    run::validate_inputs(&ctx)?;

    if lifecycle == Lifecycle::Reset {
        lifecycle::confirm_reset(&ctx)?;
    }
    // Tell the control menu a launch is underway — it shows "starting container…"
    // until the container is running. On success we exec into podman below and
    // the menu clears the marker when it first sees the container running; if the
    // bring-up fails first, clear it here so the status doesn't stick.
    mark_launching(&ctx);
    let result = bring_up(&mut ctx, lifecycle, opts);
    clear_launch_marker(&ctx); // only reached if bring_up failed (else it exec'd)
    result
}

/// Direct-mode paseo needs a stable host port and a daemon passphrase. Assign
/// them once at launch — a free port from [`config::PASEO_PORT_BASE`] and a
/// generated 2-word passphrase — persist them to the project config so they stay
/// consistent across restarts, and do nothing in relay mode or once assigned.
fn provision_paseo_direct() -> Result<()> {
    let dir = std::env::current_dir()?;
    let env = env_path(&dir);
    if !env.exists() {
        return Ok(()); // first run has no config yet — the wizard writes it
    }
    let mut cfg = Config::load(&env)?;
    if !cfg.install_paseo || !cfg.paseo_mode.is_direct() {
        return Ok(());
    }
    let mut changed = false;
    if cfg.paseo_port.is_none() {
        cfg.paseo_port = Some(ports::pick_free_port(config::PASEO_PORT_BASE, &[])?);
        changed = true;
    }
    if cfg.paseo_password.is_none() {
        cfg.paseo_password = Some(agents::paseo::generate_passphrase());
        changed = true;
    }
    if changed {
        cfg.save(&env)?;
    }
    Ok(())
}

/// The actual bring-up. On success the `run::*_and_exec` call replaces this
/// process with podman, so this never returns `Ok`; any earlier failure returns
/// `Err` so the caller can clear the launch marker.
fn bring_up(ctx: &mut LaunchContext, lifecycle: Lifecycle, opts: LaunchOpts) -> Result<()> {
    image::ensure(ctx, false)?;
    lifecycle::apply(ctx, lifecycle)?;
    lifecycle::ensure_volume(ctx)?;
    if opts.pull {
        lifecycle::schedule_pull(ctx)?;
    }
    ctx.write_allowlist()?;

    // Only a create publishes ports — an existing container keeps the mapping it
    // was created with. `lifecycle::apply` has already dropped the container on
    // --recreate/--reset, so our own old publish is not in the way of the pick.
    let exists = podman::container_exists(&ctx.container_name);
    if !exists {
        resolve_webapp_host_port(ctx)?;
    }

    println!(
        "\n==> launching container {} (linux rootless)",
        ctx.container_name
    );
    println!("    repo:   {}", ctx.config.repo_url);
    println!(
        "    webapp: {}",
        ports::publish_desc(ctx.config.webapp_port, ctx.config.webapp_host_port)
    );
    if opts.disable_network_block {
        println!("    WARNING: --disable-network-block — egress filtering OFF");
    }

    if exists {
        run::start_and_exec(ctx)?;
    } else {
        run::create_and_exec(ctx, opts.disable_network_block)?;
    }
    Ok(()) // unreachable: the calls above exec into podman.
}

/// Settle the host side of the webapp publish before a create, and persist it.
///
/// Rootless podman is only reachable from the host through a published port, so
/// two projects that share a `WEBAPP_PORT` would otherwise collide at create time
/// with a bare "address already in use". `WEBAPP_PORT` stays the preference — it
/// is used whenever it is actually free, so the common case keeps its familiar
/// `localhost:<port>` and any `ssh -L` set up against it — and only a genuine
/// conflict moves the host side to a free port just above it. The pick is
/// persisted so it stays put across restarts while the conflict lasts, and is
/// dropped again once the preferred port frees up.
fn resolve_webapp_host_port(ctx: &mut LaunchContext) -> Result<()> {
    let cfg = &ctx.config;
    let preferred = cfg.webapp_port;
    let resolved = if ports::port_is_free(preferred) {
        None // the natural 1:1 mapping is available
    } else if cfg.webapp_host_port.is_some_and(ports::port_is_free) {
        cfg.webapp_host_port // keep the persisted fallback while the conflict lasts
    } else {
        // Don't steal a port this same container is about to publish.
        let mut avoid: Vec<u16> = ports::parse_extra_ports(&cfg.extra_ports, preferred)?
            .into_iter()
            .map(|(host, _)| host)
            .collect();
        avoid.extend(cfg.paseo_port);
        Some(ports::pick_free_port(preferred.saturating_add(1), &avoid)?)
    };

    if resolved == cfg.webapp_host_port {
        return Ok(());
    }
    if let Some(host_port) = resolved {
        println!(
            "\n==> host port {preferred} is taken — publishing the webapp on {host_port} instead"
        );
        println!("    (inside the container the app still listens on {preferred})");
    }
    ctx.config.webapp_host_port = resolved;
    ctx.config.save(&env_path(&ctx.project_dir))?;
    Ok(())
}

/// Write the launch-in-progress marker (best-effort — a failure here just means
/// the menu shows "not created"/"stopped" during bring-up, never a hard error).
pub(crate) fn mark_launching(ctx: &LaunchContext) {
    if let Ok(p) = paths::launch_marker(&ctx.container_name) {
        let _ = std::fs::write(&p, b"");
    }
}

/// Remove the launch-in-progress marker (best-effort).
pub(crate) fn clear_launch_marker(ctx: &LaunchContext) {
    if let Ok(p) = paths::launch_marker(&ctx.container_name) {
        let _ = std::fs::remove_file(p);
    }
}

/// True when a fresh launch marker exists — i.e. a launch is underway and the
/// container isn't running yet. A stale marker (past the TTL) reads as false.
pub(crate) fn is_launching(ctx: &LaunchContext) -> bool {
    let Ok(p) = paths::launch_marker(&ctx.container_name) else {
        return false;
    };
    match std::fs::metadata(&p).and_then(|m| m.modified()) {
        Ok(modified) => marker_fresh(modified, SystemTime::now()),
        Err(_) => false,
    }
}

/// A marker is "fresh" if it was written within the TTL. A modified time in the
/// future (clock skew right after writing) also counts as fresh.
fn marker_fresh(modified: SystemTime, now: SystemTime) -> bool {
    match now.duration_since(modified) {
        Ok(age) => age < LAUNCH_MARKER_TTL,
        Err(_) => true,
    }
}

/// `introdus verify`.
pub fn run_verify() -> Result<()> {
    preflight::check_launch()?;
    let ctx = load_context()?;
    image::ensure(&ctx, false)?;
    run::verify(&ctx)
}

/// `introdus update`.
pub fn run_update() -> Result<()> {
    preflight::check_launch()?;
    run::update(&load_context()?)
}

/// `introdus rebuild-base`.
pub fn run_rebuild_base() -> Result<()> {
    preflight::check_launch()?;
    image::rebuild(&load_context()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ta20_launch_marker_freshness() {
        let now = SystemTime::now();
        // Just written -> fresh.
        assert!(marker_fresh(now, now));
        // Within the TTL -> fresh.
        assert!(marker_fresh(now - Duration::from_secs(60), now));
        // Past the TTL -> stale (a crashed launch shouldn't stick on "starting").
        assert!(!marker_fresh(
            now - (LAUNCH_MARKER_TTL + Duration::from_secs(1)),
            now
        ));
        // A future mtime (clock skew right after writing) still counts as fresh.
        assert!(marker_fresh(now + Duration::from_secs(5), now));
    }
}
