//! The typed project configuration and its `.env` round-trip.
//!
//! `.env` remains the on-disk source of truth (bash-sourced by the old
//! `launch.sh`, read here via `dotenvy`). [`Config::load`] parses it into a
//! typed struct with the same defaults the shell used; [`Config::render`]
//! writes a canonical, briefly-commented `.env` back. The TUI/wizard is the
//! primary editor now, so a save normalizes the file — the exhaustive guidance
//! lives in `sample.env` and the docs.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};

use crate::env_file::{quote_scalar, read_map, split_list};

/// Default egress allowlist — the hosts the base image, package managers, and
/// Claude need. Mirrors `WHITELIST_HOSTS` in `sample.env`.
pub const DEFAULT_WHITELIST: &[&str] = &[
    "github.com",
    "objects.githubusercontent.com",
    "codeload.github.com",
    "raw.githubusercontent.com",
    "api.github.com",
    "registry.npmjs.org",
    "pypi.org",
    "files.pythonhosted.org",
    "api.anthropic.com",
    "claude.ai",
    "platform.claude.com",
    "statsig.anthropic.com",
    "sentry.io",
    "mise.jdx.dev",
    "archive.ubuntu.com",
    "security.ubuntu.com",
    "update.code.visualstudio.com",
    "vscode.download.prss.microsoft.com",
    "marketplace.visualstudio.com",
];

const DEFAULT_MEM_LIMIT: &str = "8g";
const DEFAULT_CPU_LIMIT: &str = "8";
const DEFAULT_PIDS_LIMIT: u64 = 16384;
const DEFAULT_ROOT_TIMEOUT: u32 = 600;
const DEFAULT_CANARY_IP: &str = "93.184.216.34";

/// How paseo connects when installed: through the official relay (phone/desktop
/// pairing over paseo.sh) or a direct TCP connection on a VPN/zero-trust network
/// (no relay; the daemon port is published on the host and password-protected).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PaseoMode {
    #[default]
    Relay,
    Direct,
}

impl PaseoMode {
    /// The `.env` token.
    pub fn as_str(self) -> &'static str {
        match self {
            PaseoMode::Relay => "relay",
            PaseoMode::Direct => "direct",
        }
    }

    pub fn is_direct(self) -> bool {
        matches!(self, PaseoMode::Direct)
    }

    /// The opposite connection mode — the target of the panel's relay⇄direct switch.
    pub fn other(self) -> PaseoMode {
        match self {
            PaseoMode::Relay => PaseoMode::Direct,
            PaseoMode::Direct => PaseoMode::Relay,
        }
    }

    /// Parse the `.env` token; anything but `direct` (case-insensitive) is relay.
    fn parse(s: &str) -> PaseoMode {
        if s.eq_ignore_ascii_case("direct") {
            PaseoMode::Direct
        } else {
            PaseoMode::Relay
        }
    }
}

/// The base of the obscure port range direct-mode daemons are auto-assigned from.
pub const PASEO_PORT_BASE: u16 = 20190;

/// Default browser origins a direct-mode paseo daemon accepts (CORS
/// `allowedOrigins`): the hosted paseo web app plus a self-hosted client on this
/// machine (`localhost`/`127.0.0.1` on paseo's default client port 6767). Other
/// devices' client origins are added on demand (see `Config::add_paseo_origin`).
/// The CORS check is browser-only — a non-browser client sends any/no `Origin` —
/// so this restricts drive-by web pages, not real access (the password is the gate).
pub const DEFAULT_PASEO_ORIGINS: &[&str] = &[
    "https://app.paseo.sh",
    "http://localhost:6767",
    "http://127.0.0.1:6767",
];

/// A project's full configuration, the typed mirror of its `.env`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    // ---- required identity ----
    pub project_name: String,
    pub repo_url: String,
    pub deploy_key_path: String,
    pub webapp_port: u16,

    // ---- agents & egress ----
    pub install_agents: Vec<String>,
    /// Install the paseo orchestrator so agents can be driven from a phone/app
    /// (opt-in, separate from the agent list).
    pub install_paseo: bool,
    /// How paseo connects when installed: via the official relay (default) or a
    /// direct TCP connection on a VPN/zero-trust net (no relay; port published).
    pub paseo_mode: PaseoMode,
    /// The direct-mode daemon port, published on the host. Auto-picked free from
    /// [`PASEO_PORT_BASE`] on the first direct launch, then persisted for
    /// stability. `None` in relay mode or before the first pick.
    pub paseo_port: Option<u16>,
    /// The auto-generated 2-word daemon passphrase for direct mode (bcrypt-hashed
    /// inside the container; kept here so the panel can display it). `None` in
    /// relay mode.
    pub paseo_password: Option<String>,
    /// Browser origins the direct-mode daemon accepts (CORS `allowedOrigins`). A
    /// sole `*` means accept any origin. Empty/ignored in relay mode. Applied to
    /// the daemon's `config.json` by `setup.sh`; the panel can extend it live.
    pub paseo_allowed_origins: Vec<String>,
    pub whitelist_hosts: Vec<String>,
    pub internal_allow_cidrs: Vec<String>,

    // ---- launch hooks ----
    pub on_launch_script: Option<String>,
    pub on_launch_root_script: Option<String>,
    pub on_launch_root_timeout: u32,

    // ---- ports & resources ----
    pub extra_ports: Vec<String>,
    pub mem_limit: String,
    pub cpu_limit: String,
    pub pids_limit: u64,

    // ---- identity / mounts / tmux ----
    pub image_suffix: Option<String>,
    pub shared_data_path: Option<String>,
    pub session_name: Option<String>,

    // ---- exposure & notifications ----
    pub expose_webapp: bool,
    pub enable_notify_sh_alerts: bool,
    pub ntfy_sh_topic: Option<String>,
    pub rc_forward_addr: Option<String>,

    // ---- egress self-check ----
    pub canary_blocked_ip: String,
}

impl Config {
    /// A minimal config with the four required fields set and everything else at
    /// its default — the starting point the wizard fills in.
    pub fn new(
        project_name: String,
        repo_url: String,
        deploy_key_path: String,
        webapp_port: u16,
    ) -> Self {
        Self {
            project_name,
            repo_url,
            deploy_key_path,
            webapp_port,
            install_agents: vec!["claude".to_owned()],
            install_paseo: false,
            paseo_mode: PaseoMode::Relay,
            paseo_port: None,
            paseo_password: None,
            paseo_allowed_origins: Vec::new(),
            whitelist_hosts: DEFAULT_WHITELIST.iter().map(|s| (*s).to_owned()).collect(),
            internal_allow_cidrs: Vec::new(),
            on_launch_script: None,
            on_launch_root_script: None,
            on_launch_root_timeout: DEFAULT_ROOT_TIMEOUT,
            extra_ports: Vec::new(),
            mem_limit: DEFAULT_MEM_LIMIT.to_owned(),
            cpu_limit: DEFAULT_CPU_LIMIT.to_owned(),
            pids_limit: DEFAULT_PIDS_LIMIT,
            image_suffix: None,
            shared_data_path: None,
            session_name: None,
            expose_webapp: false,
            enable_notify_sh_alerts: false,
            ntfy_sh_topic: None,
            rc_forward_addr: None,
            canary_blocked_ip: DEFAULT_CANARY_IP.to_owned(),
        }
    }

    /// Opt into paseo in connection `mode`, normalizing the fields that depend on
    /// it. Relay needs paseo's egress host (its daemon dials out to the relay) and
    /// no published port; direct drops that host (VPN-local TCP, nothing to reach)
    /// and clears the port/password so the next launch re-provisions a fresh pair.
    /// Idempotent — used by the wizard, the panel, and the relay⇄direct switch.
    pub fn set_paseo_mode(&mut self, mode: PaseoMode) {
        self.install_paseo = true;
        self.paseo_mode = mode;
        self.paseo_port = None;
        self.paseo_password = None;
        let host = crate::agents::paseo::HOST.to_owned();
        match mode {
            PaseoMode::Relay => {
                if !self.whitelist_hosts.contains(&host) {
                    self.whitelist_hosts.push(host);
                }
            }
            PaseoMode::Direct => {
                self.whitelist_hosts.retain(|h| h != &host);
                // Ensure a valid CORS allowlist exists for the browser client
                // (a hand-edited `PASEO_MODE=direct` with no origins would else
                // reject every browser). The wizard/panel override via the
                // all-vs-restrict prompt right after.
                if self.paseo_allowed_origins.is_empty() {
                    self.set_paseo_origins_all(false);
                }
            }
        }
    }

    /// Whether the direct-mode daemon should accept **any** browser origin (the
    /// allowlist is a sole `*`).
    pub fn paseo_allows_all_origins(&self) -> bool {
        self.paseo_allowed_origins.iter().any(|o| o == "*")
    }

    /// Set the direct-mode CORS policy: `all` → `["*"]` (accept any origin);
    /// otherwise the explicit [`DEFAULT_PASEO_ORIGINS`] allowlist.
    pub fn set_paseo_origins_all(&mut self, all: bool) {
        self.paseo_allowed_origins = if all {
            vec!["*".to_owned()]
        } else {
            DEFAULT_PASEO_ORIGINS
                .iter()
                .map(|s| (*s).to_owned())
                .collect()
        };
    }

    /// Add one browser `origin` to the direct-mode allowlist. Returns `true` when
    /// newly added; `false` (no-op) when already accepting all origins, or the
    /// origin is blank or already present.
    pub fn add_paseo_origin(&mut self, origin: &str) -> bool {
        let origin = origin.trim();
        if origin.is_empty() || self.paseo_allows_all_origins() {
            return false;
        }
        let origin = origin.to_owned();
        if self.paseo_allowed_origins.contains(&origin) {
            return false;
        }
        self.paseo_allowed_origins.push(origin);
        true
    }

    /// Parse a `.env` file into a `Config`, applying the shell defaults for any
    /// unset optional field and erroring on a missing required one.
    pub fn load(path: &Path) -> Result<Self> {
        let m = read_map(path)?;
        let cfg = Self {
            project_name: required(&m, "PROJECT_NAME")?,
            repo_url: required(&m, "REPO_URL")?,
            deploy_key_path: required(&m, "DEPLOY_KEY_PATH")?,
            webapp_port: required(&m, "WEBAPP_PORT")?
                .parse()
                .context("WEBAPP_PORT must be a port number")?,
            install_agents: list_or(&m, "INSTALL_AGENTS", &["claude"]),
            install_paseo: flag(&m, "INSTALL_PASEO"),
            paseo_mode: opt(&m, "PASEO_MODE")
                .map(|s| PaseoMode::parse(&s))
                .unwrap_or_default(),
            paseo_port: match opt(&m, "PASEO_PORT") {
                Some(v) => Some(v.parse().context("PASEO_PORT must be a port number")?),
                None => None,
            },
            paseo_password: opt(&m, "PASEO_PASSWORD"),
            paseo_allowed_origins: list_or(&m, "PASEO_ALLOWED_ORIGINS", &[]),
            whitelist_hosts: list_or(&m, "WHITELIST_HOSTS", DEFAULT_WHITELIST),
            internal_allow_cidrs: list_or(&m, "INTERNAL_ALLOW_CIDRS", &[]),
            on_launch_script: opt(&m, "ON_LAUNCH_SCRIPT"),
            on_launch_root_script: opt(&m, "ON_LAUNCH_ROOT_SCRIPT"),
            on_launch_root_timeout: parse_or(&m, "ON_LAUNCH_ROOT_TIMEOUT", DEFAULT_ROOT_TIMEOUT)?,
            extra_ports: list_or(&m, "EXTRA_PORTS", &[]),
            mem_limit: opt(&m, "MEM_LIMIT").unwrap_or_else(|| DEFAULT_MEM_LIMIT.to_owned()),
            cpu_limit: opt(&m, "CPU_LIMIT").unwrap_or_else(|| DEFAULT_CPU_LIMIT.to_owned()),
            pids_limit: parse_or(&m, "PIDS_LIMIT", DEFAULT_PIDS_LIMIT)?,
            image_suffix: opt(&m, "IMAGE_SUFFIX"),
            shared_data_path: opt(&m, "SHARED_DATA_PATH"),
            session_name: opt(&m, "SESSION_NAME"),
            expose_webapp: flag(&m, "EXPOSE_WEBAPP"),
            enable_notify_sh_alerts: flag(&m, "ENABLE_NOTIFY_SH_ALERTS"),
            ntfy_sh_topic: opt(&m, "NTFY_SH_TOPIC"),
            rc_forward_addr: opt(&m, "RC_FORWARD_ADDR"),
            canary_blocked_ip: opt(&m, "CANARY_BLOCKED_IP")
                .unwrap_or_else(|| DEFAULT_CANARY_IP.to_owned()),
        };
        Ok(cfg)
    }

    /// Render a canonical, briefly-commented `.env`. `load(render(cfg)) == cfg`.
    pub fn render(&self) -> String {
        let mut o = String::new();
        let _ = writeln!(
            o,
            "# introdus project config. Generated/edited by `introdus`; hand-editable."
        );
        let _ = writeln!(o, "# Full field docs live in sample.env and the docs/.\n");

        section(&mut o, "Required identity");
        scalar(&mut o, "PROJECT_NAME", &self.project_name);
        scalar(&mut o, "REPO_URL", &self.repo_url);
        scalar(&mut o, "DEPLOY_KEY_PATH", &self.deploy_key_path);
        scalar(&mut o, "WEBAPP_PORT", &self.webapp_port.to_string());

        section(
            &mut o,
            "Coding agents (space-separated ids; see container/agents.sh)",
        );
        inline_list(&mut o, "INSTALL_AGENTS", &self.install_agents);
        scalar(&mut o, "INSTALL_PASEO", bool_str(self.install_paseo));
        scalar(&mut o, "PASEO_MODE", self.paseo_mode.as_str());
        let paseo_port_s = self.paseo_port.map(|p| p.to_string());
        opt_scalar(&mut o, "PASEO_PORT", paseo_port_s.as_deref());
        opt_scalar(&mut o, "PASEO_PASSWORD", self.paseo_password.as_deref());
        inline_list(&mut o, "PASEO_ALLOWED_ORIGINS", &self.paseo_allowed_origins);

        section(&mut o, "Egress: proxy hostname allowlist (default-deny)");
        multiline_list(&mut o, "WHITELIST_HOSTS", &self.whitelist_hosts);
        inline_list(&mut o, "INTERNAL_ALLOW_CIDRS", &self.internal_allow_cidrs);
        scalar(&mut o, "CANARY_BLOCKED_IP", &self.canary_blocked_ip);

        section(&mut o, "Launch hooks");
        opt_multiline(
            &mut o,
            "ON_LAUNCH_ROOT_SCRIPT",
            self.on_launch_root_script.as_deref(),
        );
        scalar(
            &mut o,
            "ON_LAUNCH_ROOT_TIMEOUT",
            &self.on_launch_root_timeout.to_string(),
        );
        opt_multiline(&mut o, "ON_LAUNCH_SCRIPT", self.on_launch_script.as_deref());

        section(&mut o, "Ports & resources");
        multiline_list(&mut o, "EXTRA_PORTS", &self.extra_ports);
        scalar(&mut o, "MEM_LIMIT", &self.mem_limit);
        scalar(&mut o, "CPU_LIMIT", &self.cpu_limit);
        scalar(&mut o, "PIDS_LIMIT", &self.pids_limit.to_string());

        section(&mut o, "Identity / mounts / tmux session");
        opt_scalar(&mut o, "IMAGE_SUFFIX", self.image_suffix.as_deref());
        opt_scalar(&mut o, "SHARED_DATA_PATH", self.shared_data_path.as_deref());
        opt_scalar(&mut o, "SESSION_NAME", self.session_name.as_deref());

        section(&mut o, "Exposure & notifications");
        scalar(&mut o, "EXPOSE_WEBAPP", bool_str(self.expose_webapp));
        scalar(
            &mut o,
            "ENABLE_NOTIFY_SH_ALERTS",
            bool_str(self.enable_notify_sh_alerts),
        );
        opt_scalar(&mut o, "NTFY_SH_TOPIC", self.ntfy_sh_topic.as_deref());
        opt_scalar(&mut o, "RC_FORWARD_ADDR", self.rc_forward_addr.as_deref());
        o
    }

    /// Write the rendered config to `path`, creating its parent directory (the
    /// canonical location is `<project>/.introdus/config.env`, so the `.introdus`
    /// dir may not exist yet on a first save).
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating config dir {}", parent.display()))?;
        }
        std::fs::write(path, self.render())
            .with_context(|| format!("writing config to {}", path.display()))
    }
}

// ---- parse helpers ----------------------------------------------------------

fn opt(m: &HashMap<String, String>, key: &str) -> Option<String> {
    m.get(key)
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

fn required(m: &HashMap<String, String>, key: &str) -> Result<String> {
    opt(m, key).with_context(|| format!("{key} is required but missing/empty in .env"))
}

fn flag(m: &HashMap<String, String>, key: &str) -> bool {
    opt(m, key).as_deref() == Some("true")
}

fn list_or(m: &HashMap<String, String>, key: &str, default: &[&str]) -> Vec<String> {
    match m.get(key) {
        Some(v) => split_list(v),
        None => default.iter().map(|s| (*s).to_owned()).collect(),
    }
}

fn parse_or<T: std::str::FromStr>(m: &HashMap<String, String>, key: &str, default: T) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    match opt(m, key) {
        None => Ok(default),
        Some(v) => v
            .parse()
            .map_err(|e| anyhow::anyhow!("{key} is invalid: {e}")),
    }
}

// ---- render helpers ---------------------------------------------------------

fn bool_str(b: bool) -> &'static str {
    if b {
        "true"
    } else {
        "false"
    }
}

fn section(o: &mut String, title: &str) {
    let _ = writeln!(o, "\n# --- {title} ---");
}

fn scalar(o: &mut String, key: &str, value: &str) {
    let _ = writeln!(o, "{key}={}", quote_scalar(value));
}

fn opt_scalar(o: &mut String, key: &str, value: Option<&str>) {
    if let Some(v) = value {
        scalar(o, key, v);
    }
}

fn inline_list(o: &mut String, key: &str, items: &[String]) {
    let _ = writeln!(o, "{key}={}", quote_scalar(&items.join(" ")));
}

fn multiline_list(o: &mut String, key: &str, items: &[String]) {
    if items.is_empty() {
        let _ = writeln!(o, "{key}=\"\"");
        return;
    }
    let _ = writeln!(o, "{key}=\"");
    for item in items {
        let _ = writeln!(o, "{item}");
    }
    let _ = writeln!(o, "\"");
}

fn opt_multiline(o: &mut String, key: &str, value: Option<&str>) {
    if let Some(v) = value {
        // Multi-line script: double-quote, escaping only `"`, `\`, backtick so
        // `$VAR` in the hook is preserved literally for bash to expand later.
        let mut esc = String::with_capacity(v.len());
        for c in v.chars() {
            if matches!(c, '"' | '\\' | '`') {
                esc.push('\\');
            }
            esc.push(c);
        }
        let _ = writeln!(o, "{key}=\"{esc}\"");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A distinctly-named temp path under the OS temp dir (no external crates).
    fn temp_env_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("introdus-cfg-{}-{tag}.env", std::process::id()))
    }

    fn sample() -> Config {
        let mut c = Config::new(
            "myproj".to_owned(),
            "git@github.com:org/repo.git".to_owned(),
            "/home/you/.ssh/deploy".to_owned(),
            3000,
        );
        c.install_agents = vec!["claude".to_owned(), "codex".to_owned()];
        c.install_paseo = true;
        c.paseo_mode = PaseoMode::Direct;
        c.paseo_port = Some(20191);
        c.paseo_password = Some("fast-koala".to_owned());
        c.paseo_allowed_origins = vec![
            "https://app.paseo.sh".to_owned(),
            "http://127.0.0.1:6767".to_owned(),
        ];
        c.internal_allow_cidrs = vec!["10.2.5.131".to_owned()];
        c.extra_ports = vec!["8123".to_owned(), "16379:6379".to_owned()];
        c.on_launch_script = Some("pnpm install\npnpm dev --host 0.0.0.0".to_owned());
        c.on_launch_root_script = Some("clickhouse start".to_owned());
        c.image_suffix = Some("ab12".to_owned());
        c.shared_data_path = Some("/data/in".to_owned());
        c.session_name = Some("introdus-fast-roving-car".to_owned());
        c.expose_webapp = true;
        c.enable_notify_sh_alerts = true;
        c.ntfy_sh_topic = Some("secret-topic-7c4a".to_owned());
        c.mem_limit = "12g".to_owned();
        c
    }

    #[test]
    fn ta06_round_trip_preserves_config() {
        let cfg = sample();
        let path = temp_env_path("roundtrip");
        cfg.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(cfg, loaded, "render/load round-trip must be lossless");
    }

    #[test]
    fn ta07_defaults_applied_for_minimal_env() {
        let path = temp_env_path("minimal");
        std::fs::write(
            &path,
            "PROJECT_NAME=web\nREPO_URL=git@github.com:o/r.git\nDEPLOY_KEY_PATH=/k\nWEBAPP_PORT=5173\n",
        )
        .unwrap();
        let c = Config::load(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(c.install_agents, vec!["claude".to_owned()]);
        assert!(!c.install_paseo);
        // paseo defaults: relay mode, no port/password until a direct launch.
        assert_eq!(c.paseo_mode, PaseoMode::Relay);
        assert!(c.paseo_port.is_none());
        assert!(c.paseo_password.is_none());
        assert!(c.paseo_allowed_origins.is_empty());
        assert_eq!(c.whitelist_hosts.len(), DEFAULT_WHITELIST.len());
        assert_eq!(c.mem_limit, "8g");
        assert_eq!(c.pids_limit, 16384);
        assert_eq!(c.on_launch_root_timeout, 600);
        assert_eq!(c.canary_blocked_ip, "93.184.216.34");
        assert!(!c.expose_webapp);
        assert!(c.session_name.is_none());
    }

    #[test]
    fn ta166_set_paseo_mode_normalizes_and_toggles() {
        assert_eq!(PaseoMode::Relay.other(), PaseoMode::Direct);
        assert_eq!(PaseoMode::Direct.other(), PaseoMode::Relay);
        let host = crate::agents::paseo::HOST.to_owned();

        // Fresh direct opt-in: relay host absent, port/password left for launch,
        // and a default CORS allowlist seeded (so a browser client isn't rejected).
        let mut c = Config::new("p".into(), "r".into(), "k".into(), 3000);
        c.set_paseo_mode(PaseoMode::Direct);
        assert!(c.install_paseo);
        assert_eq!(c.paseo_mode, PaseoMode::Direct);
        assert!(!c.whitelist_hosts.contains(&host));
        assert_eq!(c.paseo_allowed_origins.len(), DEFAULT_PASEO_ORIGINS.len());
        assert!(c
            .paseo_allowed_origins
            .iter()
            .any(|o| o == "http://127.0.0.1:6767"));
        assert!(!c.paseo_allows_all_origins());

        // Switch direct -> relay: relay host added, provisioned port/password cleared.
        c.paseo_port = Some(20191);
        c.paseo_password = Some("fast-koala".to_owned());
        c.set_paseo_mode(PaseoMode::Relay);
        assert_eq!(c.paseo_mode, PaseoMode::Relay);
        assert!(c.whitelist_hosts.contains(&host));
        assert!(c.paseo_port.is_none() && c.paseo_password.is_none());

        // Switch back relay -> direct: relay host dropped again (no duplicate churn).
        c.set_paseo_mode(PaseoMode::Direct);
        assert!(!c.whitelist_hosts.contains(&host));
    }

    #[test]
    fn ta167_paseo_origin_policy_and_add() {
        let mut c = Config::new("p".into(), "r".into(), "k".into(), 3000);

        // Restrict → explicit default list; add appends a new origin once.
        c.set_paseo_origins_all(false);
        assert!(!c.paseo_allows_all_origins());
        assert!(c.add_paseo_origin("  http://10.0.0.5:6767 "));
        assert!(c
            .paseo_allowed_origins
            .contains(&"http://10.0.0.5:6767".to_owned()));
        assert!(
            !c.add_paseo_origin("http://10.0.0.5:6767"),
            "duplicate is a no-op"
        );
        assert!(!c.add_paseo_origin("   "), "blank is a no-op");

        // Allow-all → sole `*`; adding is then a no-op (already open).
        c.set_paseo_origins_all(true);
        assert_eq!(c.paseo_allowed_origins, vec!["*".to_owned()]);
        assert!(c.paseo_allows_all_origins());
        assert!(!c.add_paseo_origin("http://example:1234"));
        assert_eq!(c.paseo_allowed_origins, vec!["*".to_owned()]);
    }

    #[test]
    fn ta08_missing_required_field_errors() {
        let path = temp_env_path("bad");
        std::fs::write(&path, "PROJECT_NAME=web\n").unwrap();
        let err = Config::load(&path).unwrap_err();
        std::fs::remove_file(&path).ok();
        assert!(err.to_string().contains("REPO_URL"));
    }
}
