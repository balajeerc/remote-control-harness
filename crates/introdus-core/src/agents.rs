//! The selectable coding-agent registry — the Rust mirror of
//! `container/agents.sh` (which remains the source of truth consumed by the
//! in-container `install-agents` script). Kept in lock-step by hand; when you
//! add or change an agent, update both this table and `agents.sh`.

/// How an agent is installed inside the container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallMethod {
    /// `pnpm add -g --ignore-scripts <spec>` — no package lifecycle scripts run.
    Pnpm,
    /// `pnpm add -g --allow-build=<spec> <spec>` — the package's own postinstall
    /// IS allowed to run (claude-code's `install.cjs`, which copies the native
    /// binary shipped as an npm optionalDependency into place). Still pulls only
    /// from the npm registry; the sole relaxation vs. `Pnpm` is running that one
    /// lifecycle script. Flagged in the wizard.
    PnpmBuild,
    /// `curl <spec> | bash` — a vendor installer, NOT contained by
    /// `--ignore-scripts`. Flagged as higher-risk in the wizard.
    Script,
}

/// Whether an agent can bypass its approval/permission prompts, and how — used
/// to offer an unattended-mode launch. Launch-time only: the shell registry in
/// `container/agents.sh` has no counterpart, since nothing on that side ever
/// launches an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Yolo {
    /// A flag that bypasses ALL approval prompts (fully unattended, dangerous).
    Bypass(&'static str),
    /// A flag that auto-approves most actions but still honours deny rules — not
    /// a guaranteed skip-everything.
    Auto(&'static str),
    /// Always auto-approves by design; no flag needed.
    Always,
    /// No bypass/auto option.
    None,
}

/// A single coding agent the harness can install and launch.
#[derive(Debug, Clone, Copy)]
pub struct Agent {
    /// Stable id used in `INSTALL_AGENTS` and on the CLI (e.g. `codex`).
    pub id: &'static str,
    /// Human-facing label shown in the wizard checklist.
    pub label: &'static str,
    /// Install mechanism.
    pub method: InstallMethod,
    /// npm package name (Pnpm) or installer URL (Script).
    pub spec: &'static str,
    /// The command the agent installs, for verification and the run banner.
    pub cmd: &'static str,
    /// Extra egress hosts the agent needs, appended to `WHITELIST_HOSTS` when
    /// selected. Space-split, best-effort runtime/auth hosts.
    pub hosts: &'static str,
    /// Baked into the base image at build time; the in-container installer
    /// skips these and must not clobber their build-time native steps.
    pub prebaked: bool,
    /// How this agent can skip its approval prompts, offered at launch time.
    pub yolo: Yolo,
}

/// The full registry, in display/selection order. Mirrors `AGENT_IDS` and the
/// associative arrays in `container/agents.sh`.
pub const AGENTS: &[Agent] = &[
    Agent {
        id: "claude",
        label: "Claude (Anthropic)",
        // PnpmBuild, not Pnpm: claude-code's postinstall (install.cjs) copies the
        // native binary (an npm optionalDependency, so still fetched only from the
        // registry) over its placeholder. --ignore-scripts would leave a broken
        // stub, so the build script is allowed for this package alone.
        method: InstallMethod::PnpmBuild,
        spec: "@anthropic-ai/claude-code",
        cmd: "claude",
        hosts: "", // native binary ships via the npm registry — no extra host
        prebaked: false,
        yolo: Yolo::Bypass("--dangerously-skip-permissions"),
    },
    Agent {
        id: "codex",
        label: "Codex (OpenAI)",
        method: InstallMethod::Pnpm,
        spec: "@openai/codex",
        cmd: "codex",
        hosts: "api.openai.com auth.openai.com chatgpt.com",
        prebaked: false,
        // `--yolo` is the official alias; the long form is the documented flag.
        yolo: Yolo::Bypass("--dangerously-bypass-approvals-and-sandbox"),
    },
    Agent {
        id: "antigravity",
        label: "Antigravity (Google)",
        method: InstallMethod::Script,
        spec: "https://antigravity.google/cli/install.sh",
        cmd: "agy",
        hosts: "antigravity.google antigravity-cli-auto-updater-974169037036.us-central1.run.app storage.googleapis.com accounts.google.com oauth2.googleapis.com www.googleapis.com cloudcode-pa.googleapis.com iamcredentials.googleapis.com",
        prebaked: false,
        // agy mirrors claude-code's flag name (not Gemini CLI's `--yolo`).
        yolo: Yolo::Bypass("--dangerously-skip-permissions"),
    },
    Agent {
        id: "opencode",
        label: "Opencode (Open source)",
        method: InstallMethod::Pnpm,
        spec: "opencode-ai",
        cmd: "opencode",
        hosts: "opencode.ai models.dev openrouter.ai",
        prebaked: false,
        // `--auto` auto-approves but still honours deny rules — not a full bypass.
        yolo: Yolo::Auto("--auto"),
    },
    Agent {
        id: "pi",
        label: "Pi agent (Open source)",
        method: InstallMethod::Pnpm,
        spec: "@earendil-works/pi-coding-agent",
        cmd: "pi",
        hosts: "console.anthropic.com openrouter.ai",
        prebaked: false,
        yolo: Yolo::Always, // no permission system by design — always auto-approves
    },
    Agent {
        id: "kilocode",
        label: "Kilocode CLI (kilo.sh)",
        method: InstallMethod::Pnpm,
        spec: "@kilocode/cli",
        cmd: "kilo",
        hosts: "kilo.ai api.kilo.ai",
        prebaked: false,
        yolo: Yolo::Auto("--auto"), // autonomous mode; deny rules still apply
    },
];

/// Paseo — the optional agent *orchestrator* (not a coding agent itself). A
/// daemon you pair a phone/desktop/web app with (via the QR flow) and then use
/// to orchestrate the installed agents through the paseo relay: the daemon dials
/// OUT to the relay with end-to-end encryption, so nothing is exposed inbound.
/// The harness does not wrap agent launches in `paseo run` (headless mode isn't
/// the intended path) — it only installs the CLI and wires the relay egress.
/// Opted into separately from the agent checklist (`INSTALL_PASEO`), installed
/// via the same pnpm path. Mirrors the `PASEO_*` constants in
/// `container/agents.sh`.
pub mod paseo {
    /// npm package providing the paseo CLI + daemon.
    pub const SPEC: &str = "@getpaseo/cli";
    /// The command it installs.
    pub const CMD: &str = "paseo";
    /// Proxy-allowlist host for paseo's plain-HTTPS traffic (pairing/registration
    /// under `app.paseo.sh`, which honors `HTTPS_PROXY`). Suffix-matching also
    /// admits `relay.paseo.sh` at the proxy — but the daemon's relay link is a
    /// WebSocket via the `ws` lib, which ignores the proxy and dials the relay
    /// directly, so the proxy entry alone can't carry it. See [`RELAY_HOST`].
    pub const HOST: &str = "paseo.sh";
    /// The relay endpoint the daemon dials OUT to over a WebSocket
    /// (`wss://relay.paseo.sh/ws`). Because `ws` bypasses `HTTPS_PROXY`, this host
    /// is resolved at launch and its IPs are allowed directly on 443 by the nft
    /// filter (same shape as the cloudflared tunnel bypass) — without it, the
    /// workload's default-deny egress blackholes the relay and phone pairing
    /// times out. Anycast/stable enough for a launch-time resolve.
    pub const RELAY_HOST: &str = "relay.paseo.sh";

    /// Adjective + noun wordlists for a friendly, memorable direct-mode passphrase
    /// (e.g. `fast-koala`). Not a high-entropy secret alone — direct mode is meant
    /// for a VPN/zero-trust network and the daemon bcrypt-hashes it — but drawn
    /// from a large enough pool (~90x90 ≈ 8k pairs) not to be guessable off-hand.
    const ADJECTIVES: &[&str] = &[
        "amber", "ancient", "bold", "brave", "brisk", "bronze", "calm", "clever", "cobalt",
        "cosmic", "crimson", "crisp", "daring", "dusky", "eager", "electric", "emerald", "fair",
        "fast", "fearless", "fierce", "frosty", "gentle", "gilded", "golden", "grand", "hidden",
        "humble", "indigo", "iron", "jade", "jolly", "keen", "lively", "lucid", "lunar", "mellow",
        "merry", "misty", "noble", "nimble", "olive", "opal", "placid", "polar", "proud", "quick",
        "quiet", "royal", "rustic", "sable", "sapphire", "scarlet", "serene", "sharp", "silent",
        "silver", "sleek", "smooth", "solar", "spry", "stellar", "sterling", "stoic", "stout",
        "sunny", "swift", "tidy", "timber", "topaz", "tranquil", "trusty", "twilight", "valiant",
        "velvet", "vivid", "warm", "whisper", "wild", "windy", "wise", "witty", "zealous",
    ];
    const NOUNS: &[&str] = &[
        "acorn", "otter", "koala", "falcon", "harbor", "meadow", "cedar", "comet", "ember",
        "canyon", "willow", "badger", "raven", "maple", "brook", "heron", "lynx", "orchid",
        "pebble", "quartz", "summit", "thistle", "walnut", "beacon", "cactus", "dune", "fjord",
        "glacier", "hollow", "island", "jasmine", "kestrel", "lagoon", "marlin", "nectar", "oasis",
        "pine", "reef", "spruce", "tundra", "vale", "wren", "yarrow", "zephyr", "bison", "cove",
        "delta", "elk", "fern", "grove", "hawk", "ibis", "juniper", "lark", "moss", "nomad",
        "onyx", "prairie", "quail", "ridge", "sparrow", "tulip", "vireo", "wolf", "aspen", "birch",
        "cricket", "dew", "eagle", "finch", "gale", "hazel", "iris", "jetty", "kelp", "lotus",
        "mica", "north", "opal", "puffin", "quill", "robin", "sage", "teal", "umber", "violet",
        "wisp", "yew", "zinnia",
    ];

    /// Generate a memorable `adjective-noun` passphrase for the direct-mode daemon
    /// password, seeded from OS randomness (`/dev/urandom`; a time/pid fallback is
    /// used only if that is unreadable, which does not happen on Linux).
    pub fn generate_passphrase() -> String {
        let b = random_bytes();
        let a = ADJECTIVES[pick(&b[0..4], ADJECTIVES.len())];
        let n = NOUNS[pick(&b[4..8], NOUNS.len())];
        format!("{a}-{n}")
    }

    /// The one-line direct-connection URL, in the exact form the paseo client
    /// takes as a paste: `tcp://<host>:<port>?password=<password>`. `host` is
    /// this container host's client-reachable address (see
    /// [`crate::hostnet::detect`]).
    pub fn direct_url(host: &str, port: u16, password: &str) -> String {
        format!("tcp://{host}:{port}?password={password}")
    }

    /// The display lines for a direct-mode connection: the paste-ready
    /// [`direct_url`], the same values broken out for typing into paseo
    /// desktop's "Direct connection" dialog, and the CLI form. A `None` port or
    /// password renders a "relaunch to assign/generate" hint instead of a URL.
    /// Shared by the panel action and the headless subcommand.
    ///
    /// Every line carries the daemon password, so the panel shows this block
    /// only briefly (see the control panel's transient output).
    pub fn direct_connection_help(
        host: &str,
        port: Option<u16>,
        password: Option<&str>,
    ) -> Vec<String> {
        let port_s = port.map_or_else(|| "(relaunch to assign)".to_owned(), |p| p.to_string());
        let pass = password.unwrap_or("(relaunch to generate)");
        let first = match (port, password) {
            (Some(p), Some(pw)) => direct_url(host, p, pw),
            _ => format!("port {port_s}, password {pass}"),
        };
        vec![
            "Paseo DIRECT connection (no relay) — paste into the paseo client:".to_owned(),
            format!("  {first}"),
            format!("  host {host} · port {port_s} · password {pass}"),
            format!("  CLI:  PASEO_HOST={host}:{port_s} PASEO_PASSWORD={pass} paseo ls"),
        ]
    }

    fn pick(b: &[u8], len: usize) -> usize {
        (u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize) % len
    }

    fn random_bytes() -> [u8; 8] {
        use std::io::Read;
        let mut buf = [0u8; 8];
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            if f.read_exact(&mut buf).is_ok() {
                return buf;
            }
        }
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
            ^ u64::from(std::process::id());
        buf.copy_from_slice(&seed.to_le_bytes());
        buf
    }
}

/// Look up an agent by its id.
pub fn find(id: &str) -> Option<&'static Agent> {
    AGENTS.iter().find(|a| a.id == id)
}

/// True if `id` names a known agent.
pub fn is_known(id: &str) -> bool {
    find(id).is_some()
}

impl Agent {
    /// The agent's extra egress hosts, split into individual hostnames.
    pub fn host_list(&self) -> Vec<&'static str> {
        self.hosts.split_whitespace().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ta14_script_agents_use_url_specs() {
        for a in AGENTS {
            if a.method == InstallMethod::Script {
                assert!(
                    a.spec.starts_with("https://"),
                    "{} spec must be a URL",
                    a.id
                );
            }
        }
    }

    #[test]
    fn ta162_paseo_passphrase_is_two_words_from_the_lists() {
        for _ in 0..200 {
            let p = paseo::generate_passphrase();
            let (a, n) = p.split_once('-').expect("passphrase is adjective-noun");
            assert!(!a.is_empty() && !n.is_empty(), "both words present: {p}");
            assert!(
                a.chars().all(|c| c.is_ascii_lowercase()),
                "adjective is lowercase ascii: {p}"
            );
            assert!(
                n.chars().all(|c| c.is_ascii_lowercase()),
                "noun is lowercase ascii: {p}"
            );
        }
    }

    #[test]
    fn ta165_direct_connection_help_shows_port_and_password() {
        let host = "100.117.172.96";
        let lines = paseo::direct_connection_help(host, Some(20190), Some("fast-koala")).join("\n");
        assert!(lines.contains("20190"), "shows the port: {lines}");
        assert!(lines.contains("fast-koala"), "shows the password: {lines}");
        assert!(lines.contains("PASEO_HOST"), "shows the CLI form: {lines}");
        // Missing values render a relaunch hint, not an empty/garbage line.
        let hint = paseo::direct_connection_help("10.0.0.2", None, None).join("\n");
        assert!(hint.contains("relaunch"), "None renders a hint: {hint}");
        assert!(
            !hint.contains("tcp://"),
            "no URL without a port/password: {hint}"
        );
    }

    #[test]
    fn ta169_direct_url_is_the_paste_ready_client_form() {
        // The exact shape the paseo client accepts as a paste — the help block
        // must lead with it.
        let host = "100.117.172.96";
        let url = "tcp://100.117.172.96:6767?password=my-password";
        assert_eq!(paseo::direct_url(host, 6767, "my-password"), url);
        let lines = paseo::direct_connection_help(host, Some(6767), Some("my-password"));
        assert_eq!(
            lines[1].trim(),
            url,
            "URL is the first thing after the heading: {lines:?}"
        );
    }

    #[test]
    fn ta130_paseo_relay_host_is_under_the_proxy_allowlist_host() {
        // The relay bypass resolves RELAY_HOST by IP, but the same host must also
        // be admitted at the proxy (suffix of HOST) so paseo's non-WS traffic to
        // it isn't separately blocked.
        assert_eq!(paseo::RELAY_HOST, "relay.paseo.sh");
        assert!(
            paseo::RELAY_HOST.ends_with(paseo::HOST),
            "relay host {} must be covered by the proxy allowlist host {}",
            paseo::RELAY_HOST,
            paseo::HOST
        );
    }

    #[test]
    fn ta14_yolo_flags_match_the_known_cli_flags() {
        // Guards the exact (typo-prone, dangerous) flag strings per agent.
        let want = |id| find(id).map(|a| a.yolo);
        assert_eq!(
            want("claude"),
            Some(Yolo::Bypass("--dangerously-skip-permissions"))
        );
        assert_eq!(
            want("codex"),
            Some(Yolo::Bypass("--dangerously-bypass-approvals-and-sandbox"))
        );
        assert_eq!(
            want("antigravity"),
            Some(Yolo::Bypass("--dangerously-skip-permissions"))
        );
        assert_eq!(want("opencode"), Some(Yolo::Auto("--auto")));
        assert_eq!(want("pi"), Some(Yolo::Always));
        assert_eq!(want("kilocode"), Some(Yolo::Auto("--auto")));
        // Every Bypass/Auto flag is a real `--flag`.
        for a in AGENTS {
            if let Yolo::Bypass(f) | Yolo::Auto(f) = a.yolo {
                assert!(f.starts_with("--"), "{} yolo flag must be a --flag", a.id);
            }
        }
    }
}
