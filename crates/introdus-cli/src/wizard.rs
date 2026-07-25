//! The setup wizard — the interactive replacement for `create-dev-container.sh`.
//! Walks the user through the required `.env` fields, the agent checklist, and
//! deploy-key setup, then writes the project's `.env`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use introdus_core::agents::{self, Agent};
use introdus_core::config::PaseoMode;
use introdus_core::process::Cmd;
use introdus_core::{names, Config};

use crate::ui;
use crate::util::expand_tilde;

/// Run the wizard for a project rooted at `project_dir`, writing `.env` and
/// returning the resulting config.
pub fn run(project_dir: &Path) -> Result<Config> {
    println!("\n=== introdus setup ===");
    println!(
        "Configuring a network-hardened dev container for {}\n",
        project_dir.display()
    );

    let default_name = project_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "my-project".to_owned());
    let project_name = ask_default("Project name", &default_name)?;
    let repo_url = ask_nonempty("Git repo URL (git@github.com:org/repo.git)")?;
    let deploy_key_path = prompt_deploy_key(&project_name, &repo_url)?;
    let webapp_port = ask_port("Webapp port (bound in the container):", 3000)?;

    let install_agents = prompt_agents()?;
    // Paseo is an orchestrator, opted into separately from the agent checklist:
    // it lets you drive the installed agents from a phone/desktop/web/CLI client.
    let (install_paseo, paseo_mode, paseo_origins_all) = prompt_paseo()?;
    let expose_webapp = ui::confirm(
        "Expose the webapp to the internet via a Cloudflare tunnel?",
        false,
    )?;
    let (enable_notify_sh_alerts, ntfy_sh_topic) = prompt_ntfy()?;
    let rc_forward_addr = prompt_forward()?;

    let mut config = Config::new(project_name, repo_url, deploy_key_path, webapp_port);
    apply_agents(&mut config, install_agents);
    apply_paseo(&mut config, install_paseo, paseo_mode, paseo_origins_all);
    config.expose_webapp = expose_webapp;
    config.enable_notify_sh_alerts = enable_notify_sh_alerts;
    config.ntfy_sh_topic = ntfy_sh_topic;
    config.rc_forward_addr = rc_forward_addr;

    let env = crate::context::config_write_path(project_dir);
    config
        .save(&env)
        .with_context(|| format!("writing {}", env.display()))?;
    println!("\n==> wrote {}", env.display());
    Ok(config)
}

/// Set the selected agents and extend the whitelist with their egress hosts.
fn apply_agents(config: &mut Config, selected: Vec<String>) {
    config.install_agents = selected;
    for id in &config.install_agents {
        if let Some(agent) = agents::find(id) {
            for host in agent.host_list() {
                let host = host.to_owned();
                if !config.whitelist_hosts.contains(&host) {
                    config.whitelist_hosts.push(host);
                }
            }
        }
    }
}

/// Ask whether to install paseo and, if so, how it connects: through the
/// official relay (phone/desktop pairing; nothing exposed) or a direct TCP
/// connection over a VPN/zero-trust net (no relay; the daemon port is published
/// on the host and password-protected). The direct-mode port + passphrase are
/// auto-assigned at launch, so the wizard only needs the mode.
fn prompt_paseo() -> Result<(bool, PaseoMode, bool)> {
    let install = ui::confirm(
        "Also install paseo, to drive these agents from your phone/desktop (agent orchestrator)?",
        false,
    )?;
    if !install {
        return Ok((false, PaseoMode::Relay, false));
    }
    let direct = ui::confirm(
        "Use a DIRECT connection over your own VPN/zero-trust network instead of the paseo relay? \
         (No relay contacted; the daemon port is published on the host, protected by a generated \
         password.)",
        false,
    )?;
    if !direct {
        return Ok((true, PaseoMode::Relay, false));
    }
    // Direct mode is reached by a browser client, which the daemon gates by CORS
    // origin. Ask whether to accept any origin or restrict to this machine's
    // client — but be clear it's browser-only hygiene (the password is the gate).
    let all = ui::confirm(
        "Allow ANY browser origin to reach the paseo daemon? (No = restrict to this machine's \
         client for now; add other devices later. Either way it's password-protected — this is \
         browser hygiene, not the access control.)",
        false,
    )?;
    Ok((true, PaseoMode::Direct, all))
}

/// Record the paseo opt-in + connection mode. Relay mode adds paseo's egress host
/// so the daemon can reach the pairing/relay service; direct mode never contacts
/// paseo's hosts (no egress widening) but seeds the browser CORS allowlist
/// (`origins_all` → any origin, else the restricted default).
fn apply_paseo(config: &mut Config, enabled: bool, mode: PaseoMode, origins_all: bool) {
    if enabled {
        config.set_paseo_mode(mode);
        if mode.is_direct() {
            config.set_paseo_origins_all(origins_all);
        }
    } else {
        config.install_paseo = false;
        config.paseo_mode = PaseoMode::Relay;
    }
}

fn prompt_agents() -> Result<Vec<String>> {
    let options: Vec<String> = agents::AGENTS.iter().map(choice_label).collect();
    // Every agent is explicit opt-in: nothing is pre-checked, so an agent is
    // installed only if the user ticks it (space toggles). Confirming with none
    // ticked installs no coding agent at all.
    let picked = ui::multiselect_indexed(
        "Coding agents to install (space toggles, enter confirms):",
        &options,
        &[],
    )?;
    let ids: Vec<String> = picked
        .into_iter()
        .filter_map(|i| agents::AGENTS.get(i).map(|a| a.id.to_owned()))
        .collect();
    Ok(ids)
}

/// The checklist label for an agent — its name, flagging vendor installers that
/// run remote code so the user weighs that before ticking the box.
fn choice_label(a: &'static Agent) -> String {
    let method_note = match a.method {
        agents::InstallMethod::Script => "  [vendor installer — runs remote code]",
        agents::InstallMethod::PnpmBuild => "  [runs its own npm postinstall]",
        agents::InstallMethod::Pnpm => "",
    };
    format!("{}{}", a.label, method_note)
}

fn prompt_ntfy() -> Result<(bool, Option<String>)> {
    let enable = ui::confirm("Enable mobile push notifications via ntfy.sh?", false)?;
    if !enable {
        return Ok((false, None));
    }
    let topic = ask_nonempty("ntfy.sh topic (treat like a password)")?;
    Ok((true, Some(topic)))
}

/// Ask whether task notifications should be forwarded to a separate dev machine
/// (a laptop running `introdus notify-listen`) over an SSH reverse tunnel. This
/// is the headless-remote-host case: with no desktop here, `notify-host` forwards
/// to a loopback port the laptop's tunnel carries instead of rendering locally.
/// Answering yes writes `RC_FORWARD_ADDR=127.0.0.1:<port>`; say no if you sit at
/// this host and want its own desktop popups.
fn prompt_forward() -> Result<Option<String>> {
    let enable = ui::confirm(
        "Forward notifications to a separate dev machine over an SSH reverse tunnel? \
         (No if you use this host's own desktop)",
        false,
    )?;
    if !enable {
        return Ok(None);
    }
    // Must match the port the laptop's `introdus notify-listen` binds (its own
    // wizard defaults to the same 8765).
    let port = ask_port(
        "Loopback port your laptop's `introdus notify-listen` uses",
        8765,
    )?;
    Ok(Some(format!("127.0.0.1:{port}")))
}

/// Deploy-key setup: ask up-front whether to generate a fresh key or point at
/// an existing one, branch to resolve the private-key path, then — in BOTH
/// cases — print the public half and wait for the user to register it as a
/// write-access deploy key before the clone step relies on it.
fn prompt_deploy_key(project_name: &str, repo_url: &str) -> Result<String> {
    println!("  (No = point introdus at a deploy key you already have)");
    let generate = ui::confirm("Generate a new per-project deploy key now?", true)?;
    let path = if generate {
        generate_new_deploy_key(project_name)?
    } else {
        prompt_existing_deploy_key(project_name)?
    };
    announce_deploy_key(Path::new(&path), repo_url)?;
    Ok(path)
}

/// Create a fresh ed25519 key at a project-derived default location (which the
/// user can override), refusing to overwrite an existing file. Returns the
/// private-key path; registration is handled by the caller.
fn generate_new_deploy_key(project_name: &str) -> Result<String> {
    let slug = names::image_slug(project_name);
    let filename = format!("{slug}-deploy-key");
    // Group introdus-created keys under their own subdir so they don't clutter
    // ~/.ssh alongside personal keys.
    let default = dirs::home_dir()
        .map(|h| h.join(".ssh/introdus-deploy-keys").join(&filename))
        .unwrap_or_else(|| PathBuf::from(&filename));
    println!("  (a private key is written here; its .pub is printed next to register)");
    loop {
        let raw = ui::text(
            "Where should the new deploy key be created?",
            Some(&default.to_string_lossy()),
            false,
        )?;
        let path = expand_tilde(raw.trim());
        if path.exists() {
            println!(
                "  a file already exists at {} — pick another path to avoid overwriting it.",
                path.display()
            );
            continue;
        }
        create_key_file(&path)?;
        return Ok(path.to_string_lossy().into_owned());
    }
}

/// Point introdus at an already-existing private deploy key. If any keys in the
/// user's ssh dirs resemble this project, offer to reuse one (a plain yes/no,
/// then a picker only when several match); otherwise, or on decline, prompt for
/// a path.
fn prompt_existing_deploy_key(project_name: &str) -> Result<String> {
    if let Some(path) = offer_candidate_keys(&find_candidate_keys(project_name))? {
        return Ok(path);
    }
    prompt_key_path()
}

/// Two-step reuse flow for project-matching keys: confirm intent first, then —
/// only if several matched — pick which. `Ok(None)` means "none / not these",
/// so the caller falls through to a manual path prompt.
fn offer_candidate_keys(matches: &[PathBuf]) -> Result<Option<String>> {
    let (first, rest) = match matches.split_first() {
        Some(pair) => pair,
        None => return Ok(None),
    };
    let question = if rest.is_empty() {
        format!("Reuse the existing key at {}?", first.display())
    } else {
        format!(
            "Reuse one of the {} existing keys matching this project?",
            matches.len()
        )
    };
    let reuse = ui::confirm(&question, true)?;
    if !reuse {
        return Ok(None);
    }
    if rest.is_empty() {
        return Ok(Some(first.to_string_lossy().into_owned()));
    }
    let options: Vec<String> = matches
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let choice = ui::select("Which key?", options)?;
    Ok(Some(choice))
}

/// Prompt for the path to a private deploy key, re-asking until it points at a
/// real file.
fn prompt_key_path() -> Result<String> {
    loop {
        let raw = ask_nonempty("Path to your existing deploy key (the private key file)")?;
        let path = expand_tilde(&raw);
        if path.is_file() {
            return Ok(path.to_string_lossy().into_owned());
        }
        println!(
            "  no file at {} — enter the path to your existing private key.",
            path.display()
        );
    }
}

/// Scan the user's ssh dirs for private keys whose filename resembles the
/// project, best-match-first. A file is treated as a private key when it has a
/// sibling `.pub` — which skips `config`, `known_hosts`, and public keys.
fn find_candidate_keys(project_name: &str) -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let tokens = key_match_tokens(project_name);
    if tokens.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(usize, PathBuf)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for dir in [home.join(".ssh"), home.join(".ssh/introdus-deploy-keys")] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !is_private_key(&path) {
                continue;
            }
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            let score = tokens.iter().filter(|t| name.contains(*t)).count();
            if score > 0 && seen.insert(path.clone()) {
                scored.push((score, path));
            }
        }
    }
    // Highest score first, then alphabetical for a stable, predictable order.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, p)| p).collect()
}

/// The lowercase tokens (each ≥3 chars) a key filename must contain to be
/// considered a match — derived from the project's image slug.
fn key_match_tokens(project_name: &str) -> Vec<String> {
    let slug = names::image_slug(project_name);
    let mut tokens: Vec<String> = slug
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| t.len() >= 3)
        .map(str::to_owned)
        .collect();
    if slug.len() >= 3 {
        tokens.push(slug);
    }
    tokens.sort();
    tokens.dedup();
    tokens
}

/// True when `path` is a private key file: a regular file with no `.pub`
/// extension that has a sibling `<name>.pub`.
fn is_private_key(path: &Path) -> bool {
    if !path.is_file() || path.extension().is_some_and(|e| e == "pub") {
        return false;
    }
    crate::util::pub_sibling(path).is_file()
}

/// `ssh-keygen` a fresh passphrase-less ed25519 key at `path`.
fn create_key_file(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        // Lock down our own key directory (holds private keys). Guarded by name
        // so a user-chosen custom path's parent is never re-permissioned.
        if parent
            .file_name()
            .is_some_and(|n| n == "introdus-deploy-keys")
        {
            restrict_dir(parent);
        }
    }
    // Capture (and discard) ssh-keygen's stdout — the "key pair generated",
    // fingerprint, and randomart art are noise here; stderr stays visible for
    // real errors. Mirrors the old wizard's `ssh-keygen … >/dev/null`.
    Cmd::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-C", "introdus-deploy-key", "-f"])
        .arg(path)
        .stdout()
        .context("ssh-keygen failed")?;
    Ok(())
}

/// Print the public half of the deploy key and wait for the user to register it
/// with the git host. Run for both freshly-generated and reused keys, so the
/// clone step never proceeds against an unregistered key.
fn announce_deploy_key(path: &Path, repo_url: &str) -> Result<()> {
    let pubkey = read_public_key(path)?;
    println!("\n  Deploy key: {}", path.display());
    println!("  Add this PUBLIC key to {repo_url} as a deploy key WITH WRITE ACCESS:\n");
    println!("    {}", pubkey.trim());
    ui::confirm(
        "Press enter once the deploy key is registered with the repo",
        true,
    )?;
    Ok(())
}

/// The public key for a private key `path`: read the sibling `.pub` if present,
/// else derive it from the private key with `ssh-keygen -y`.
fn read_public_key(path: &Path) -> Result<String> {
    let pubfile = crate::util::pub_sibling(path);
    if pubfile.is_file() {
        return std::fs::read_to_string(&pubfile)
            .with_context(|| format!("reading {}", pubfile.display()));
    }
    Cmd::new("ssh-keygen")
        .args(["-y", "-f"])
        .arg(path)
        .stdout()
        .context("deriving public key with `ssh-keygen -y`")
}

/// Best-effort `chmod 700` on our key directory (private keys live there).
#[cfg(unix)]
fn restrict_dir(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn restrict_dir(_dir: &Path) {}

/// Prompt until the user enters something non-blank; returns the trimmed value.
/// Shared with the `notify-listen` wizard.
pub(crate) fn ask_nonempty(prompt: &str) -> Result<String> {
    loop {
        let value = ui::text(prompt, None, false)?;
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_owned());
        }
        println!("  (required)");
    }
}

fn ask_default(prompt: &str, default: &str) -> Result<String> {
    let value = ui::text(prompt, Some(default), false)?;
    Ok(value.trim().to_owned())
}

/// Prompt for a `u16` port, pre-filled with `default`, re-asking on anything
/// that doesn't parse. Shared with the `notify-listen` wizard.
pub(crate) fn ask_port(prompt: &str, default: u16) -> Result<u16> {
    let default = default.to_string();
    loop {
        let value = ui::text(prompt, Some(&default), false)?;
        match value.trim().parse::<u16>() {
            Ok(port) => return Ok(port),
            Err(_) => println!("  (enter a whole number between 0 and 65535)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ta72_apply_agents_extends_whitelist() {
        let mut c = Config::new(
            "p".to_owned(),
            "git@github.com:o/r.git".to_owned(),
            "/k".to_owned(),
            3000,
        );
        let before = c.whitelist_hosts.len();
        apply_agents(&mut c, vec!["claude".to_owned(), "codex".to_owned()]);
        assert_eq!(c.install_agents, vec!["claude", "codex"]);
        // codex's hosts (api.openai.com, ...) were appended.
        assert!(c.whitelist_hosts.contains(&"api.openai.com".to_owned()));
        assert!(c.whitelist_hosts.len() > before);
    }

    #[test]
    fn ta126_apply_paseo_sets_flag_and_host() {
        let mut c = Config::new(
            "p".to_owned(),
            "git@github.com:o/r.git".to_owned(),
            "/k".to_owned(),
            3000,
        );
        apply_paseo(&mut c, false, PaseoMode::Relay, false);
        assert!(!c.install_paseo);
        assert!(!c.whitelist_hosts.contains(&"paseo.sh".to_owned()));

        apply_paseo(&mut c, true, PaseoMode::Relay, false);
        assert!(c.install_paseo);
        assert_eq!(c.paseo_mode, PaseoMode::Relay);
        assert!(c.whitelist_hosts.contains(&"paseo.sh".to_owned()));
        // Idempotent: enabling again doesn't duplicate the host.
        let count = c
            .whitelist_hosts
            .iter()
            .filter(|h| *h == "paseo.sh")
            .count();
        apply_paseo(&mut c, true, PaseoMode::Relay, false);
        assert_eq!(
            c.whitelist_hosts
                .iter()
                .filter(|h| *h == "paseo.sh")
                .count(),
            count
        );
    }

    #[test]
    fn ta160_apply_paseo_direct_mode_skips_relay_host() {
        let mut c = Config::new(
            "p".to_owned(),
            "git@github.com:o/r.git".to_owned(),
            "/k".to_owned(),
            3000,
        );
        // Direct mode records the mode but never widens egress with paseo.sh —
        // it's a VPN-local TCP connection that never contacts paseo's relay.
        // Restrict (origins_all=false) seeds the explicit CORS default; the
        // wildcard case sets a sole `*`.
        apply_paseo(&mut c, true, PaseoMode::Direct, false);
        assert!(c.install_paseo);
        assert_eq!(c.paseo_mode, PaseoMode::Direct);
        assert!(!c.whitelist_hosts.contains(&"paseo.sh".to_owned()));
        assert!(!c.paseo_allows_all_origins());
        assert!(!c.paseo_allowed_origins.is_empty());
        apply_paseo(&mut c, true, PaseoMode::Direct, true);
        assert!(c.paseo_allows_all_origins());
    }

    #[test]
    fn ta74_key_match_tokens_splits_slug() {
        let tokens = key_match_tokens("Ship TBC");
        // "ship tbc" -> slug "ship-tbc": tokens "ship", "tbc", plus the slug.
        assert!(tokens.contains(&"ship".to_owned()));
        assert!(tokens.contains(&"tbc".to_owned()));
        assert!(tokens.contains(&"ship-tbc".to_owned()));
        // Too-short to yield any ≥3-char token or slug.
        assert!(key_match_tokens("x").is_empty());
    }
}
