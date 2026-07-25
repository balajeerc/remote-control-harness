//! Building and launching the container: the full `podman run` flag/env/mount
//! set (parity with `launch_dev_container.sh`), plus the `--verify` throwaway
//! self-check and the `--update` in-container refresh.

use std::convert::Infallible;

use anyhow::{bail, Context, Result};
use introdus_core::podman::{self, podman};
use introdus_core::ports::parse_extra_ports;

use crate::context::LaunchContext;

/// Capabilities added back on top of `--cap-drop=ALL` (NET_ADMIN is added
/// separately, only when the egress filter is on).
const CAP_ADD: &[&str] = &[
    "CHOWN",
    "DAC_OVERRIDE",
    "FOWNER",
    "FSETID",
    "SETFCAP",
    "MKNOD",
    "SETUID",
    "SETGID",
];

/// Validate the launch inputs the shell checked at env-parse time: the deploy
/// key exists, a shared-data path (if set) is a directory, extra ports parse.
pub fn validate_inputs(ctx: &LaunchContext) -> Result<()> {
    if !std::path::Path::new(&ctx.config.deploy_key_path).is_file() {
        bail!(
            "DEPLOY_KEY_PATH does not exist: {}",
            ctx.config.deploy_key_path
        );
    }
    if let Some(p) = &ctx.config.shared_data_path {
        if !std::path::Path::new(p).is_dir() {
            bail!("SHARED_DATA_PATH is not a directory: {p}");
        }
    }
    parse_extra_ports(&ctx.config.extra_ports, ctx.config.webapp_port)?;
    Ok(())
}

/// Append a literal argument.
fn lit(a: &mut Vec<String>, s: &str) {
    a.push(s.to_owned());
}

/// Append a `--volume host:dest` pair.
fn vol(a: &mut Vec<String>, host: &str, dest: &str) {
    a.push("--volume".to_owned());
    a.push(format!("{host}:{dest}"));
}

/// Append a `--env KEY=VALUE` pair.
fn env(a: &mut Vec<String>, key: &str, value: String) {
    a.push("--env".to_owned());
    a.push(format!("{key}={value}"));
}

/// Build the argument vector after `podman` for creating the container.
pub fn run_args(ctx: &LaunchContext, disable_network_block: bool) -> Result<Vec<String>> {
    let c = &ctx.config;
    let mut a: Vec<String> = Vec::new();

    lit(&mut a, "run");
    lit(&mut a, "-it");
    lit(&mut a, "--name");
    a.push(ctx.container_name.clone());
    lit(&mut a, "--hostname");
    // The container hostname is what paseo reports as its server name and what
    // the shell prompt shows, so derive it from the project (slugged to a valid
    // DNS label) rather than a fixed literal.
    a.push(introdus_core::names::hostname_slug(&c.project_name));
    lit(&mut a, "--network=pasta");
    a.push(format!("--memory={}", c.mem_limit));
    a.push(format!("--cpus={}", c.cpu_limit));
    a.push(format!("--pids-limit={}", c.pids_limit));
    lit(&mut a, "--cap-drop=ALL");
    for cap in CAP_ADD {
        a.push(format!("--cap-add={cap}"));
    }
    if !disable_network_block {
        lit(&mut a, "--cap-add=NET_ADMIN");
    }
    lit(&mut a, "--security-opt=no-new-privileges");

    push_mounts(ctx, &mut a)?;
    push_env(ctx, disable_network_block, &mut a);
    push_publish(ctx, &mut a)?;

    a.push(ctx.image_name.clone());
    lit(&mut a, "/usr/local/bin/firewall-entrypoint.sh");
    Ok(a)
}

fn push_mounts(ctx: &LaunchContext, a: &mut Vec<String>) -> Result<()> {
    vol(a, &ctx.volume_name, "/home/dev");
    vol(a, &ctx.config.deploy_key_path, "/tmp/deploy_key:ro");
    vol(a, &path_str(&ctx.setup_script())?, "/setup.sh:ro");
    vol(
        a,
        &path_str(&ctx.entrypoint())?,
        "/usr/local/bin/firewall-entrypoint.sh:ro",
    );
    vol(
        a,
        &path_str(&ctx.tinyproxy_conf())?,
        "/etc/tinyproxy/tinyproxy.conf:ro",
    );
    vol(
        a,
        &path_str(&ctx.allowlist_file)?,
        "/etc/tinyproxy/egress-allowlist.txt:ro",
    );
    if let Some(shared) = &ctx.config.shared_data_path {
        let canon = std::fs::canonicalize(shared)
            .with_context(|| format!("resolving SHARED_DATA_PATH {shared}"))?;
        vol(a, &path_str(&canon)?, "/home/dev/shared_data:ro");
    }
    // Notification endpoint: bind-mount the host FIFO at /run/notify so the
    // container's rc-notify hook can deliver events to the `introdus notify-host`
    // service. Creating the FIFO is a launch-time side effect owned by the caller
    // (`create_and_exec`); `run_args` stays a pure argv builder that never touches
    // the filesystem, so it's safe to call (and unit-test) concurrently.
    let fifo = crate::notify::fifo_path()?;
    vol(a, &path_str(&fifo)?, "/run/notify");
    Ok(())
}

fn push_env(ctx: &LaunchContext, disable_network_block: bool, a: &mut Vec<String>) {
    let c = &ctx.config;
    env(a, "PROJECT_NAME", c.project_name.clone());
    env(a, "CONTAINER_NAME", ctx.container_name.clone());
    env(a, "REPO_URL", c.repo_url.clone());
    env(a, "WEBAPP_PORT", c.webapp_port.to_string());
    env(
        a,
        "ON_LAUNCH_SCRIPT",
        c.on_launch_script.clone().unwrap_or_default(),
    );
    env(
        a,
        "ON_LAUNCH_ROOT_SCRIPT",
        c.on_launch_root_script.clone().unwrap_or_default(),
    );
    env(
        a,
        "ON_LAUNCH_ROOT_TIMEOUT",
        c.on_launch_root_timeout.to_string(),
    );
    env(a, "CANARY_BLOCKED_IP", c.canary_blocked_ip.clone());
    env(a, "HOST_OS", "linux".to_owned());
    env(
        a,
        "DISABLE_NETWORK_BLOCK",
        disable_network_block.to_string(),
    );
    env(a, "EXPOSE_WEBAPP", c.expose_webapp.to_string());
    env(a, "TUNNEL_EDGE_IPS", ctx.tunnel_edge_ips.join(" "));
    env(a, "TUNNEL_API_IPS", ctx.tunnel_api_ips.join(" "));
    env(a, "PASEO_RELAY_IPS", ctx.paseo_relay_ips.join(" "));
    env(a, "WHITELIST_HOSTS", ctx.container_whitelist.join(" "));
    env(a, "INTERNAL_ALLOW_CIDRS", c.internal_allow_cidrs.join(" "));
    env(
        a,
        "ENABLE_NOTIFY_SH_ALERTS",
        c.enable_notify_sh_alerts.to_string(),
    );
    env(
        a,
        "NTFY_SH_TOPIC",
        c.ntfy_sh_topic.clone().unwrap_or_default(),
    );
    env(a, "INSTALL_AGENTS", c.install_agents.join(" "));
    // paseo is opted into separately from the agent checklist; pass it so a fresh
    // container's setup (install-agents) installs it when enabled — otherwise a
    // wizard opt-in or a recreate would come up without paseo.
    env(a, "INSTALL_PASEO", c.install_paseo.to_string());
    // Direct-mode paseo: the container-side setup binds the daemon to
    // 0.0.0.0:<PASEO_PORT>, disables the relay, and sets this password. Empty in
    // relay mode (setup then keeps the relay defaults).
    env(a, "PASEO_MODE", c.paseo_mode.as_str().to_owned());
    env(
        a,
        "PASEO_PORT",
        c.paseo_port.map(|p| p.to_string()).unwrap_or_default(),
    );
    env(
        a,
        "PASEO_PASSWORD",
        c.paseo_password.clone().unwrap_or_default(),
    );
    // Direct-mode CORS: the browser origins the daemon accepts (space-separated;
    // a sole `*` = any). setup.sh patches these into the daemon's config.json.
    // Empty in relay mode (the relay handshake isn't origin-checked this way).
    env(
        a,
        "PASEO_ALLOWED_ORIGINS",
        c.paseo_allowed_origins.join(" "),
    );
}

fn push_publish(ctx: &LaunchContext, a: &mut Vec<String>) -> Result<()> {
    let c = &ctx.config;
    let port = c.webapp_port;
    a.push("--publish".to_owned());
    a.push(format!("127.0.0.1:{port}:{port}"));
    for (host, container) in parse_extra_ports(&c.extra_ports, port)? {
        a.push("--publish".to_owned());
        a.push(format!("127.0.0.1:{host}:{container}"));
    }
    // Direct-mode paseo: publish the daemon port on ALL host interfaces (0.0.0.0),
    // not just loopback, so paseo desktop on a laptop can reach it over the
    // VPN/tailscale net. The daemon is password-protected; this is the container's
    // one intentional inbound surface. Relay mode publishes nothing (relay is
    // outbound-only).
    if c.paseo_mode.is_direct() {
        if let Some(pport) = c.paseo_port {
            a.push("--publish".to_owned());
            a.push(format!("0.0.0.0:{pport}:{pport}"));
        }
    }
    Ok(())
}

fn path_str(p: &std::path::Path) -> Result<String> {
    p.to_str()
        .map(str::to_owned)
        .with_context(|| format!("path is not valid UTF-8: {}", p.display()))
}

/// Create a fresh container and hand the terminal to it (never returns on
/// success). The caller has already ensured the image, volume, and allowlist.
pub fn create_and_exec(ctx: &LaunchContext, disable_network_block: bool) -> Result<Infallible> {
    println!("==> creating new container {}", ctx.container_name);
    // Create the notification FIFO that run_args bind-mounts at /run/notify. This
    // is the launch-time side effect kept out of the pure run_args builder.
    crate::notify::ensure_fifo(&crate::notify::fifo_path()?)?;
    let argv = run_args(ctx, disable_network_block)?;
    podman().args(argv).exec()
}

/// Start (and attach to) an already-created container.
pub fn start_and_exec(ctx: &LaunchContext) -> Result<Infallible> {
    println!(
        "==> reusing existing container {} (recreate/reset to rebuild it)",
        ctx.container_name
    );
    podman().args(["start", "-ai", &ctx.container_name]).exec()
}

/// `introdus verify`: run the firewall self-check in a throwaway container.
pub fn verify(ctx: &LaunchContext) -> Result<()> {
    println!("==> verify: running egress filter + proxy self-check in a throwaway container");
    ctx.write_allowlist()?;
    podman()
        .args(["run", "--rm", "--cap-drop=ALL"])
        .args([
            "--cap-add=CHOWN",
            "--cap-add=DAC_OVERRIDE",
            "--cap-add=FOWNER",
        ])
        .args([
            "--cap-add=SETUID",
            "--cap-add=SETGID",
            "--cap-add=NET_ADMIN",
        ])
        .args(["--security-opt=no-new-privileges", "--network=pasta"])
        .args(["--env", "VERIFY_ONLY=true"])
        .args([
            "--env",
            &format!("WHITELIST_HOSTS={}", ctx.container_whitelist.join(" ")),
        ])
        .args([
            "--env",
            &format!(
                "INTERNAL_ALLOW_CIDRS={}",
                ctx.config.internal_allow_cidrs.join(" ")
            ),
        ])
        .args([
            "--env",
            &format!("TUNNEL_EDGE_IPS={}", ctx.tunnel_edge_ips.join(" ")),
        ])
        .args([
            "--env",
            &format!("TUNNEL_API_IPS={}", ctx.tunnel_api_ips.join(" ")),
        ])
        .args([
            "--env",
            &format!("PASEO_RELAY_IPS={}", ctx.paseo_relay_ips.join(" ")),
        ])
        .args([
            "--env",
            &format!("CANARY_BLOCKED_IP={}", ctx.config.canary_blocked_ip),
        ])
        .args(["--env", &format!("REPO_URL={}", ctx.config.repo_url)])
        .arg("--volume")
        .arg(format!(
            "{}:/usr/local/bin/firewall-entrypoint.sh:ro",
            path_str(&ctx.entrypoint())?
        ))
        .arg("--volume")
        .arg(format!(
            "{}:/etc/tinyproxy/tinyproxy.conf:ro",
            path_str(&ctx.tinyproxy_conf())?
        ))
        .arg("--volume")
        .arg(format!(
            "{}:/etc/tinyproxy/egress-allowlist.txt:ro",
            path_str(&ctx.allowlist_file)?
        ))
        .arg(&ctx.image_name)
        .arg("/usr/local/bin/firewall-entrypoint.sh")
        .run()?;
    println!("==> verify passed");
    Ok(())
}

/// `introdus update`: in-container refresh (apt, mise, agents, LazyVim) against
/// a running container. Requires the container to be up (it routes through the
/// egress filter the entrypoint installed).
pub fn update(ctx: &LaunchContext) -> Result<()> {
    if !podman::container_running(&ctx.container_name) {
        bail!(
            "container {} is not running. launch it first.",
            ctx.container_name
        );
    }
    println!("==> update: apt upgrade (as root, via the proxy)");
    podman::exec(&ctx.container_name, None)
        .args(["bash", "-c", APT_UPGRADE])
        .run()?;
    println!("==> update: mise / agents / lazyvim (as dev)");
    podman::exec(&ctx.container_name, Some("dev"))
        .env("INSTALL_AGENTS", ctx.config.install_agents.join(" "))
        .args(["bash", "-c", DEV_UPDATE])
        .run()?;
    println!("==> update: done");
    Ok(())
}

const APT_UPGRADE: &str = "set -e; export DEBIAN_FRONTEND=noninteractive; \
     apt-get update && apt-get -y upgrade";

const DEV_UPDATE: &str = r#"set -e
export HOME=/home/dev
export PATH="/home/dev/.local/bin:/home/dev/.local/share/mise/shims:/home/dev/.local/share/pnpm/bin:$PATH"
eval "$(/home/dev/.local/bin/mise activate bash)"
mise self-update -y || true
mise upgrade
# Install any newly-selected agents (idempotent — skips those already present).
[ -x /usr/local/bin/install-agents ] && /usr/local/bin/install-agents || true
# Update the installed agents in place, honouring each one's install method.
# claude is no longer special-cased — it's a normal pnpm-build agent now.
if [ -f /usr/local/lib/rc-agents.sh ]; then
  . /usr/local/lib/rc-agents.sh
  for _id in ${INSTALL_AGENTS-claude}; do
    case "${AGENT_METHOD[$_id]:-}" in
      pnpm)       pnpm update -g --ignore-scripts "${AGENT_SPEC[$_id]}" || true ;;
      pnpm-build) pnpm update -g --allow-build="${AGENT_SPEC[$_id]}" "${AGENT_SPEC[$_id]}" || true ;;
    esac
  done
fi
nvim --headless "+Lazy! sync" +qa"#;

#[cfg(test)]
mod tests {
    use super::*;
    use introdus_core::Config;

    fn ctx() -> LaunchContext {
        let mut cfg = Config::new(
            "web".to_owned(),
            "git@github.com:o/r.git".to_owned(),
            "/dev/null".to_owned(), // exists as a file for validate_inputs
            3000,
        );
        cfg.image_suffix = Some("ab12".to_owned());
        cfg.extra_ports = vec!["8123".to_owned()];
        LaunchContext::resolve(cfg, std::env::temp_dir()).unwrap()
    }

    #[test]
    fn ta42_run_args_have_the_hardening_flags() {
        let a = run_args(&ctx(), false).unwrap();
        assert!(a.contains(&"--cap-drop=ALL".to_owned()));
        assert!(a.contains(&"--cap-add=NET_ADMIN".to_owned()));
        assert!(a.contains(&"--security-opt=no-new-privileges".to_owned()));
        assert!(a.contains(&"--network=pasta".to_owned()));
        // ends with the entrypoint after the image
        let img = a
            .iter()
            .position(|s| s == "introdus-web-ab12:latest")
            .unwrap();
        assert_eq!(a[img + 1], "/usr/local/bin/firewall-entrypoint.sh");
    }

    #[test]
    fn ta43_disable_network_block_drops_net_admin() {
        let a = run_args(&ctx(), true).unwrap();
        assert!(!a.contains(&"--cap-add=NET_ADMIN".to_owned()));
        assert!(a.iter().any(|s| s == "DISABLE_NETWORK_BLOCK=true"));
    }

    #[test]
    fn ta44_publishes_webapp_and_extra_ports() {
        let a = run_args(&ctx(), false).unwrap();
        assert!(a.contains(&"127.0.0.1:3000:3000".to_owned()));
        assert!(a.contains(&"127.0.0.1:8123:8123".to_owned()));
        // Relay mode (the default) publishes NO paseo port and passes empty
        // direct-mode env.
        assert!(!a.iter().any(|s| s.contains(":20190:")));
        assert!(a.iter().any(|s| s == "PASEO_MODE=relay"));
    }

    #[test]
    fn ta164_direct_mode_publishes_paseo_port_on_all_interfaces() {
        let mut cfg = Config::new(
            "web".to_owned(),
            "git@github.com:o/r.git".to_owned(),
            "/dev/null".to_owned(),
            3000,
        );
        cfg.image_suffix = Some("ab12".to_owned());
        cfg.install_paseo = true;
        cfg.paseo_mode = introdus_core::config::PaseoMode::Direct;
        cfg.paseo_port = Some(20190);
        cfg.paseo_password = Some("fast-koala".to_owned());
        cfg.paseo_allowed_origins = vec![
            "https://app.paseo.sh".to_owned(),
            "http://127.0.0.1:6767".to_owned(),
        ];
        let c = LaunchContext::resolve(cfg, std::env::temp_dir()).unwrap();
        let a = run_args(&c, false).unwrap();
        // Published on 0.0.0.0 (all interfaces) so a laptop can reach it over VPN.
        assert!(a.contains(&"0.0.0.0:20190:20190".to_owned()));
        // The direct-mode env the container-side setup reads.
        assert!(a.iter().any(|s| s == "PASEO_MODE=direct"));
        assert!(a.iter().any(|s| s == "PASEO_PORT=20190"));
        assert!(a.iter().any(|s| s == "PASEO_PASSWORD=fast-koala"));
        // CORS allowlist passed space-separated for setup.sh to apply.
        assert!(a
            .iter()
            .any(|s| s == "PASEO_ALLOWED_ORIGINS=https://app.paseo.sh http://127.0.0.1:6767"));
    }

    // run_args must be a PURE argv builder: it bind-mounts the notify FIFO at
    // /run/notify but performs no filesystem side effect (creating the FIFO
    // belongs to create_and_exec). That purity is what lets the ta4x tests run
    // concurrently without racing on the host-shared FIFO path — the old
    // regression. Asserted on the returned argv only, so the test itself never
    // touches the shared path.
    #[test]
    fn ta45_run_args_bind_mounts_notify_fifo() {
        let a = run_args(&ctx(), false).unwrap();
        assert!(
            a.iter().any(|s| s.ends_with(":/run/notify")),
            "run_args should bind-mount the notify FIFO at /run/notify"
        );
    }
}
