//! End-to-end pty tests for the setup wizard, driven through a real pseudo-
//! terminal. The wizard is now a sequence of `ratatui` inline modal prompts
//! (confirm / text / checklist) rather than `inquire`, so we drive it with
//! explicit keystrokes: `\r` is the byte a terminal sends for Enter in raw mode
//! (the same thing tmux emits for the control-menu harness). Each modal renders
//! its prompt text as a contiguous run, so `exp_string` still synchronizes on
//! the prompt wording. Reached via the standalone `introdus init` (no podman).

mod common;

use common::{keygen, Fixture, TIMEOUT_MS};
use rexpect::session::{spawn_command, PtySession};

/// Send a burst of raw bytes and flush — no trailing newline is added, so
/// callers spell out `\r` (Enter) themselves.
fn send(p: &mut PtySession, bytes: &str) {
    p.send(bytes).unwrap();
    p.flush().unwrap();
}

/// Accept a modal's current value (Enter).
fn enter(p: &mut PtySession) {
    send(p, "\r");
}

/// Drive the wizard front half — identity + a freshly-generated deploy key,
/// through the registration confirm — leaving it at the "Webapp port" step.
fn start_with_new_key(p: &mut PtySession) {
    p.exp_string("Project name").unwrap();
    enter(p); // default = dir name "ship-tbc"
    p.exp_string("Git repo URL").unwrap();
    send(p, "git@github.com:o/ship-tbc.git\r");
    p.exp_string("Generate a new per-project deploy key now")
        .unwrap();
    enter(p); // default Yes -> generate
    p.exp_string("Where should the new deploy key be created")
        .unwrap();
    enter(p); // accept the default path
              // Registration step is shown for a freshly generated key.
    p.exp_string("Add this PUBLIC key").unwrap();
    p.exp_string("Press enter once the deploy key is registered")
        .unwrap();
    enter(p);
}

/// Answer the tail of the wizard shared by every branch (webapp port → ntfy),
/// through to the `.env` being written. Every step takes its default (a bare
/// Enter) except the agents checklist, which is now opt-in: nothing is
/// pre-checked, so we tick Claude explicitly with a Space before confirming.
fn finish_wizard(p: &mut PtySession) {
    p.exp_string("Webapp port").unwrap();
    enter(p); // default 3000
    p.exp_string("Coding agents to install").unwrap();
    send(p, " "); // opt-in: Space ticks the first row (Claude)
    enter(p);
    p.exp_string("Also install paseo").unwrap();
    enter(p); // default No
    finish_expose_ntfy(p);
}

/// Expose-webapp (No) + ntfy (No), leaving the wizard at the
/// notification-forward confirm.
fn through_ntfy(p: &mut PtySession) {
    p.exp_string("Expose the webapp").unwrap();
    enter(p); // default No
    p.exp_string("mobile push notifications").unwrap();
    enter(p); // default No
}

/// The final leg shared by every branch: from the "Expose the webapp" confirm
/// through ntfy and the notification-forward confirm to the config being
/// written. Takes each remaining default (No).
fn finish_expose_ntfy(p: &mut PtySession) {
    through_ntfy(p);
    p.exp_string("Forward notifications to a separate dev machine")
        .unwrap();
    enter(p); // default No -> no RC_FORWARD_ADDR
    p.exp_string("wrote").unwrap();
    p.exp_eof().unwrap();
}

/// Spawn `init` and drive identity + key + webapp, stopping with the wizard
/// showing the "Coding agents to install" checklist (nothing ticked yet).
fn init_to_agent_checklist() -> (Fixture, PtySession) {
    let fx = Fixture::new("ship-tbc");
    let mut p = spawn_command(fx.cmd("init"), TIMEOUT_MS).unwrap();
    start_with_new_key(&mut p);
    p.exp_string("Webapp port").unwrap();
    enter(&mut p);
    p.exp_string("Coding agents to install").unwrap();
    (fx, p)
}

/// Tick the first checklist row (Claude) and confirm the agent selection.
fn tick_claude(p: &mut PtySession) {
    send(p, " "); // opt-in: Space ticks the first row (Claude)
    enter(p);
}

#[test]
fn ta74_wizard_generates_a_new_key() {
    let fx = Fixture::new("ship-tbc");
    let mut p = spawn_command(fx.cmd("init"), TIMEOUT_MS).unwrap();

    start_with_new_key(&mut p);
    finish_wizard(&mut p);

    let key = fx.deploy_keys_dir().join("ship-tbc-deploy-key");
    assert!(key.exists(), "generated private key is missing");
    assert!(
        key.with_extension("pub").exists(),
        "generated .pub is missing"
    );
    let env = std::fs::read_to_string(fx.env_file()).unwrap();
    assert!(env.contains("PROJECT_NAME=ship-tbc"), "{env}");
    assert!(
        env.contains(&format!("DEPLOY_KEY_PATH={}", key.display())),
        "{env}"
    );
}

#[test]
fn ta77_wizard_agents_are_opt_in_nothing_preselected() {
    let (fx, mut p) = init_to_agent_checklist();
    // The agents checklist is opt-in: Claude starts UNticked ("[ ]", not "[x]").
    p.exp_string("[ ] Claude").unwrap();
    // Confirm without toggling anything -> no coding agent selected.
    enter(&mut p);
    p.exp_string("Also install paseo").unwrap();
    enter(&mut p); // default No
    finish_expose_ntfy(&mut p);

    let env = std::fs::read_to_string(fx.env_file()).unwrap();
    assert!(
        env.contains("INSTALL_AGENTS=\"\""),
        "agents must be opt-in — nothing selected should write an empty list:\n{env}"
    );
}

#[test]
fn ta127_wizard_paseo_opt_in_records_flag_and_relay_host() {
    let (fx, mut p) = init_to_agent_checklist();
    tick_claude(&mut p);
    // Opt INTO paseo (default is No) — a single 'y' submits the confirm.
    p.exp_string("Also install paseo").unwrap();
    send(&mut p, "y");
    // Then the connection-mode prompt — default No keeps the relay.
    p.exp_string("DIRECT connection").unwrap();
    enter(&mut p); // default No = relay
    finish_expose_ntfy(&mut p);

    let env = std::fs::read_to_string(fx.env_file()).unwrap();
    assert!(
        env.contains("INSTALL_PASEO=\"true\"") || env.contains("INSTALL_PASEO=true"),
        "paseo opt-in must set INSTALL_PASEO:\n{env}"
    );
    assert!(
        env.contains("paseo.sh"),
        "relay-mode paseo opt-in must allowlist the relay host paseo.sh:\n{env}"
    );
}

#[test]
fn ta161_wizard_paseo_direct_mode_records_mode_and_skips_relay_host() {
    let (fx, mut p) = init_to_agent_checklist();
    tick_claude(&mut p);
    p.exp_string("Also install paseo").unwrap();
    send(&mut p, "y");
    // Choose the DIRECT (VPN/zero-trust) connection mode.
    p.exp_string("DIRECT connection").unwrap();
    send(&mut p, "y");
    // CORS: decline "allow ANY origin" → restrict to this machine's client.
    p.exp_string("Allow ANY browser origin").unwrap();
    send(&mut p, "n");
    finish_expose_ntfy(&mut p);

    let env = std::fs::read_to_string(fx.env_file()).unwrap();
    assert!(
        env.contains("PASEO_MODE=\"direct\"") || env.contains("PASEO_MODE=direct"),
        "direct opt-in must record PASEO_MODE=direct:\n{env}"
    );
    // No relay EGRESS host. Strip the CORS origin `https://app.paseo.sh` first so
    // its substring doesn't masquerade as the bare relay host `paseo.sh`.
    let egress = env.replace("app.paseo.sh", "");
    assert!(
        !egress.contains("paseo.sh"),
        "direct mode must NOT allowlist the relay host paseo.sh:\n{env}"
    );
    assert!(
        env.contains("PASEO_ALLOWED_ORIGINS") && env.contains("127.0.0.1:6767"),
        "restricted direct mode must seed the default CORS origin allowlist:\n{env}"
    );
}

#[test]
fn ta138_wizard_forward_opt_in_sets_rc_forward_addr() {
    let (fx, mut p) = init_to_agent_checklist();
    tick_claude(&mut p);
    p.exp_string("Also install paseo").unwrap();
    enter(&mut p); // No
    through_ntfy(&mut p);
    // Opt INTO forwarding to a separate dev machine, accept the default port.
    p.exp_string("Forward notifications to a separate dev machine")
        .unwrap();
    send(&mut p, "y");
    p.exp_string("Loopback port your laptop").unwrap();
    enter(&mut p); // default 8765
    p.exp_string("wrote").unwrap();
    p.exp_eof().unwrap();

    let env = std::fs::read_to_string(fx.env_file()).unwrap();
    assert!(
        env.contains("RC_FORWARD_ADDR=127.0.0.1:8765"),
        "forward opt-in must set RC_FORWARD_ADDR:\n{env}"
    );
}

#[test]
fn ta136_init_migrates_legacy_env_into_introdus_dir() {
    let fx = Fixture::new("ship-tbc");
    // A project still on the pre-`.introdus/` layout.
    fx.write_env_at(&fx.legacy_env_file(), "ship-tbc");

    let mut p = spawn_command(fx.cmd("init"), TIMEOUT_MS).unwrap();
    // First: the migration offer (default Yes).
    p.exp_string("Move this project's config").unwrap();
    send(&mut p, "y");
    // Then: since a config now exists, `init` asks whether to reconfigure it.
    p.exp_string("reconfigure it").unwrap();
    send(&mut p, "n"); // leave it as migrated
    p.exp_string("left config unchanged").unwrap();
    p.exp_eof().unwrap();

    assert!(
        fx.env_file().exists(),
        "config should have moved to .introdus/config.env"
    );
    assert!(
        !fx.legacy_env_file().exists(),
        "legacy .env should be gone after migration"
    );
}

#[test]
fn ta75_wizard_reuses_a_matching_key_and_still_shows_registration() {
    let fx = Fixture::new("ship-tbc");
    // A pre-existing key whose name matches the project — the reuse flow should
    // find it and offer it via a plain yes/no.
    let key = fx.deploy_keys_dir().join("ship-tbc-deploy-key");
    keygen(&key);

    let mut p = spawn_command(fx.cmd("init"), TIMEOUT_MS).unwrap();
    p.exp_string("Project name").unwrap();
    enter(&mut p);
    p.exp_string("Git repo URL").unwrap();
    send(&mut p, "git@github.com:o/ship-tbc.git\r");
    p.exp_string("Generate a new per-project deploy key now")
        .unwrap();
    send(&mut p, "n"); // a single 'n' submits the confirm -> reuse-existing branch
    p.exp_string("Reuse the existing key at").unwrap();
    enter(&mut p); // default Yes -> reuse it
                   // Registration is shown for a REUSED key too (the whole point of the fix).
    p.exp_string("Add this PUBLIC key").unwrap();
    p.exp_string("Press enter once the deploy key is registered")
        .unwrap();
    enter(&mut p);
    finish_wizard(&mut p);

    let env = std::fs::read_to_string(fx.env_file()).unwrap();
    assert!(
        env.contains(&format!("DEPLOY_KEY_PATH={}", key.display())),
        "reused key not recorded:\n{env}"
    );
}
