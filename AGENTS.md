<!--
  GENERATED FILE — DO NOT EDIT.

  Concatenated from agent_rules/*.md by scripts/gen-agent-rules.sh, for the
  coding agents (Codex, Pi, opencode) that auto-load a single root AGENTS.md
  and cannot glob a rules directory. Edit the sources in agent_rules/ and rerun
  scripts/gen-agent-rules.sh.
-->

# Agent instructions

## Where these rules live (single source of truth)

`agent_rules/` is the **only** place to edit project rules. Every agent is fed
these same files, but each expects them somewhere different, so we mirror
`agent_rules/` out to each without duplicating content:

- **Claude Code** reads `.claude/rules/*.md` — these are **symlinks** into
  `../../agent_rules/*.md` (one symlink per rule file). Editing a source file is
  picked up automatically; no regeneration needed.
- **Codex, Pi, and opencode** each auto-load a single root **`AGENTS.md`** and
  cannot glob a rules directory. `AGENTS.md` is therefore a **generated file** —
  `agent_rules/*.md` concatenated by
  [`scripts/gen-agent-rules.sh`](../scripts/gen-agent-rules.sh).

**Never edit `.claude/rules/*.md` or `AGENTS.md` directly.** Edit the file under
`agent_rules/`, then run `./scripts/gen-agent-rules.sh` to refresh `AGENTS.md`
(the symlinks need nothing). Adding a brand-new rule file also needs a matching
symlink: `ln -s ../../agent_rules/NN_name.md .claude/rules/NN_name.md`.
`./scripts/lint.sh --quick` fails if `AGENTS.md` has drifted from the sources.

## General

- Performance is important, but it's not worth optimizing for at the cost of readability and maintainability.
  - Especially true for borrow checker struggles in Rust. If the code is more readable with some clones, it's definitely worth it.
- Just before marking a task complete, consider if the changes just made need an update to the documentation in `agent_rules/03_source-code-overview.md`
- Always run `./scripts/lint.sh` --full before marking a task complete
  - If any errors are reported, fix them and run the command again until it reports no errors.
- You can run `./scripts/lint.sh --quick` often since it completes fast and gives you quick feedback on linter rules conformance

# Project overview

`introdus` runs your dev environment — including AI coding agents
like Claude Code — inside a **network-hardened, rootless podman container**, so
a supply-chain compromise of your tooling is confined to the container's blast
radius instead of reaching your real machine.

The core guarantee: the workload runs as a non-root `dev` user with **no direct
internet egress**. Its only way out is a loopback hostname-allowlist proxy,
backed by a default-deny nft filter it cannot touch (it is non-root, has no
`CAP_NET_ADMIN`, and runs `no-new-privileges`). A startup self-check aborts
launch if the filter isn't actually enforcing. See [05_security.md](05_security.md).

## Three tiers (any of which can collapse onto one machine)

1. **Dev machine** — the laptop you sit at. Attach VS Code over SSH, drive the
   agent, receive native task-completion notifications.
2. **Container host** — a remote KVM/VPS or the same laptop. Runs rootless
   podman and the notification service.
3. **Dev container** — your repo clone + the agent, with egress filtering
   enforced *inside* the container.

Run it all on one laptop, or push the container host onto a beefy remote box and
keep your laptop thin — the same control path and "task done / awaiting input"
notifications work either way (tunnelled back over an SSH reverse tunnel when
the host is remote).

## introdus — the single-binary control plane

The control plane is **`introdus`**, one self-contained Rust binary that runs on
the container host. It replaced the former pile of host shell scripts
(`launch_dev_container.sh`, `create-dev-container.sh`, `host_install.sh`, the
`host_listener.py` notifier). The **container-side security core stays bash** —
`introdus` embeds `firewall-entrypoint.sh`, `tinyproxy.conf`, `setup.sh`, and
`container/bin/*` and bind-mounts them at launch, so the egress guarantee rests
on the same audited shell + nft + tinyproxy as before.

```bash
cargo build --release              # -> target/release/introdus (one binary)
./target/release/introdus install  # copy onto ~/.local/bin (PATH)
mkdir ~/myproject && cd ~/myproject
introdus                           # first run: setup wizard writes .env, then launches
```

`introdus` puts each container inside one **tmux session** with a persistent
two-pane **control TUI** (`main-control` window) beside the container logs
(`dev-container` window); the notification service runs detached (no window),
with its output in a per-session log the menu shows on demand. From the control menu
you drive lifecycle and utilities that only make sense on the host: show the
tunnel URL, install/launch agents, edit the egress allowlist, open root/dev
terminals, copy files in, toggle the webapp tunnel / ntfy, recreate/reset —
persisting to `.env` where it matters.

Subcommands: `introdus [launch]`, `up`, `menu`, `verify`, `recreate`, `reset`,
`update`, `rebuild-base`, `notify-host`, `notify-listen`, `send-files`,
`install`. Every control-panel utility also has a headless subcommand for
scripting (the panel's prompts become flags): `tunnel-url`, `blocked-egress`,
`allow`, `expose-webapp`, `ntfy`, `install-agent`, `agent`, `install-paseo`,
`update-paseo`, `paseo-url`, `dev-shell`, `root-shell`, `test-notify`, `notify-log`,
`restart-notify`, `restart`, `stop`.

`introdus send-files` is the dev-machine file-transfer tool: it lists the
introdus containers running on this laptop plus the remote hosts in your
`~/.ssh/config`, and — once you pick a container (local or remote) — opens a
two-pane file browser (laptop on the left, the container's filesystem on the
right) to send a file/folder into a chosen container directory (via `podman cp`
locally, a tar-stream over ssh for a remote host). Each pane can be re-sorted
(`o` cycles name / modified / created), fuzzy-filtered on the current folder
(`/`), and toggled to show/hide dotfiles (`.`, hidden by default).

## Highlights

- Rootless podman, no host firewall changes, no sudo — egress filtering runs
  entirely inside each container.
- Per-project persistent volume (repo, `node_modules`, toolchains survive
  restarts).
- Pick which agents to install (Claude, Codex, Antigravity, Opencode, Pi,
  Kilocode) from the wizard checklist; npm agents install with
  `pnpm add -g --ignore-scripts` to minimize supply-chain exposure. Nothing is
  baked into the base image.
- Optional paseo orchestrator for driving agents from a phone.
- Attach VS Code ("Attach to Running Container"), directly or via Remote-SSH.
- Optional read-only host-dir mount (`SHARED_DATA_PATH`), per-project launch
  hook (`ON_LAUNCH_SCRIPT`), extra published ports (`EXTRA_PORTS`), public
  webapp tunnel via Cloudflare (`EXPOSE_WEBAPP`), and ntfy.sh phone push.
- Task-completion notifications: container → host FIFO → (remote) SSH reverse
  tunnel → laptop popup + sound, tagged per project.

See the user-facing [README.md](../README.md) and [docs/](../docs/) for the full
prose; [PLAN.md](../PLAN.md) for the rewrite's design and milestone status.

# Technical / architecture overview

## The shape

`introdus` is a Rust control plane that runs **on the container host** — where it
has full access to `.env`, `podman`, deploy keys, and `podman exec -u root`. It
drives everything through a persistent **tmux session** with a full-screen
**control TUI**. The **dev container itself runs no Rust**: its security core is
audited bash that `introdus` embeds and bind-mounts at launch.

One binary, three roles mirroring the three tiers:

- **Container host:** `introdus` (default = launch + control TUI),
  `introdus notify-host` (desktop render / forward / ntfy push).
- **Dev machine (laptop):** `introdus notify-listen` (+ ssh reverse tunnel).
- **Dev container:** no Rust — embedded bash entrypoint/setup + `rc-notify`.

## Cargo workspace

Two crates (`resolver = "2"`, edition 2021, `rust-version` 1.80):

- **`introdus-core`** — pure library: typed `.env` config, host paths, podman
  object naming, the embedded container-side bash assets, the agent registry,
  egress-allowlist logic, the notification trust boundary, and thin
  `podman`/`tmux`/process wrappers. Deps: `anyhow`, `dotenvy`, `dirs`.
- **`introdus-cli`** — the `introdus` binary: clap CLI, launch orchestration,
  the ratatui TUI (wizard + control panel), tmux session model, and the
  notification services. Deps: `clap`, `anyhow`, `dirs`, `ratatui` (crossterm
  backend re-exported by ratatui — no separate crossterm dep). Dev-dep:
  `rexpect` for pty integration tests.

## Launch flow

`introdus launch` (the default subcommand) runs, end to end:

1. **preflight** — Linux rootless podman only; check `podman` + `pasta` (+ tmux
   for the session model). Egress lives in the container, so the host needs
   nothing else.
2. **config** — load/parse `.introdus/config.env` into a typed `Config` (or run
   the **wizard** on first launch, writing it).
3. **context** — resolve a `LaunchContext`: podman object names, a per-container
   assets dir (materialized bash core + build context), the generated proxy
   allowlist, and resolved cloudflared/paseo tunnel IPs.
4. **image** — build the shared `introdus-base:latest` when stale (staleness =
   the introdus binary is newer than the image, since assets are re-materialized
   each launch), tag a cheap per-project alias.
5. **lifecycle** — legacy cleanup; `--recreate` drops the container but keeps
   the volume; `--reset` also wipes the volume (guarded by a dirty-git scan +
   typed confirmation).
6. **run** — the full `podman run` flag/env/mount set, then hand off to the
   container. `--verify` runs a throwaway egress self-check; `--update` does an
   in-container refresh.

Everything then lives inside **one tmux session per container**
(`introdus-<adjective>-<adjective>-<noun>`, derived deterministically from the
project name and persisted as `SESSION_NAME`): two windows — `main-control` (the
control TUI) and `dev-container` (podman logs) — plus the `notify-host` service
running **detached, with no tmux window** (its output goes to a per-session log
the menu shows on demand), and on-demand `root-bash` / `dev-bash` / per-agent
windows.

## Inside the container (PID 1 = `firewall-entrypoint.sh`, root + CAP_NET_ADMIN)

1. Stage the deploy key into `dev`'s `~/.ssh` (while still root).
2. Install an nft **default-deny** egress filter, segregated by uid — only the
   proxy user (`rcproxy`) may reach the internet; DNS, loopback, and configured
   internal CIDRs are permitted.
3. Start the loopback hostname-allowlist proxy (tinyproxy) as `rcproxy`.
4. **Egress self-check**: canary IP direct-dial must fail, an allowlisted host
   must be reachable *through the proxy*, and a direct dial to that host's IP
   must fail (no IP bypass). Any failure aborts.
5. Drop all privilege and `exec` `/setup.sh` as non-root `dev` (which clones the
   repo, runs the optional launch hook, and starts the workload).

See [05_security.md](05_security.md) for the full threat model.

## Config and persistence

A per-project `.introdus/config.env` file is the on-disk source of truth (typed
`Config` ⇄ env-format via `dotenvy`), kept hand-editable; the wizard/TUI is the
primary editor and normalizes the file on save. It reads the canonical
`.introdus/config.env`, falling back to a legacy top-level `./.env` (from before
the move under `.introdus/`) and offering a one-time migration on the interactive
entry points. The dev-machine `notify-listen` settings live separately at
`$XDG_CONFIG_HOME/introdus/notify-listen.env`. Host-side generated artifacts (the
bind-mounted proxy allowlist, the materialized bash core) live under
`$XDG_STATE_HOME/introdus`. Per-project data (repo, toolchains, `node_modules`)
persists in a podman volume across restarts.

## What stays bash, and why

The security-critical container core (`firewall-entrypoint.sh`, `tinyproxy.conf`,
`setup.sh`, `container/bin/*`, `container/agents.sh`) is **not** rewritten in
Rust — it is embedded via `include_str!`, materialized, and bind-mounted at
launch. This keeps the egress guarantee resting on the same battle-tested shell +
nft + tinyproxy, and lets edits to those files apply on a plain relaunch without
an image rebuild. The agent registry exists in two hand-synced copies:
`crates/introdus-core/src/agents.rs` (host-side, for the wizard/launch) and
`container/agents.sh` (in-container installer) — change both together.

# Source-code overview

A concise map of the tree. Keep this current: when you add/move/rename a module
or change what one owns, update the matching line (per
[00_agent_instructions.md](00_agent_instructions.md)).

## `crates/introdus-core/` — pure library (no I/O orchestration)

| File          | Owns |
| ------------- | ---- |
| `lib.rs`      | Crate root; `pub mod` list, `VERSION`, `BIN_NAME`. |
| `config.rs`   | Typed project `Config` ⇄ `.env` round-trip (`load`/`render`/`save`), default whitelist; `WEBAPP_HOST_PORT` (launch-managed host side of the webapp publish when `WEBAPP_PORT` was taken); `PaseoMode` (relay/direct, `other()` toggle) + `PASEO_PORT`/`PASEO_PASSWORD`/`PASEO_PORT_BASE`; `set_paseo_mode` (opt-in + per-mode whitelist/port normalization + seeds the CORS default, shared by wizard/panel/switch); direct-mode browser CORS allowlist `PASEO_ALLOWED_ORIGINS` + `DEFAULT_PASEO_ORIGINS` and `set_paseo_origins_all`/`add_paseo_origin`/`paseo_allows_all_origins`. |
| `env_file.rs` | Low-level `.env` I/O: `dotenvy` read, list-splitting, value quoting. |
| `egress.rs`   | Pure allowlist logic: git-host extraction, per-host anchored regex, container whitelist assembly, tunnel edge IPs/hosts. |
| `agents.rs`   | The coding-agent registry (`AGENTS`) + install method / yolo-flag metadata; `paseo` orchestrator constants + direct-mode passphrase generator (`generate_passphrase`) and the direct-connection display lines (`direct_url` — the paste-ready `tcp://host:port?password=…` — + `direct_connection_help`). Hand-mirrors `container/agents.sh`. |
| `hostnet.rs`  | Host IPv4 address discovery for the direct-mode paseo URL: parse `ip -o -4 addr show` / `hostname -I`, rank tailscale (interface name or `100.64.0.0/10`) over LAN over podman/bridge, `detect()` the best client-reachable address. |
| `assets.rs`   | The embedded container-side bash core (`include_str!`) and `materialize` into the per-container assets/build-context dir. |
| `names.rs`    | Podman object naming (base image, per-project image tag, container, volume); deterministic suffix fallback; project→hostname slug for the container `--hostname`. |
| `paths.rs`    | Host state dir (`$XDG_STATE_HOME/introdus`) + generated-artifact paths (allowlist, notify log, launch marker, assets dir); config dir (`$XDG_CONFIG_HOME/introdus`) + the `notify-listen` config path. |
| `ports.rs`    | Parse/validate `EXTRA_PORTS` entries; the host-port availability primitives — `port_is_free` (bind test), `pick_free_port` (picker built on it, for the direct-mode paseo daemon port and the webapp host-port fallback), `port_owner` + `PS_PORTS_FORMAT` (which container publishes a busy host port, for the conflict message), and `publish_desc` (how a webapp remap reads, shared by the launch line and the panel header). |
| `session.rs`  | Whimsical deterministic tmux session-name generation. |
| `notify.rs`   | The notification trust boundary: wire-format parse, event whitelist, label sanitization. |
| `podman.rs`   | Thin `podman` command constructors + existence/state probes. |
| `remote.rs`   | `Location` (`Local`/`Remote(ssh-alias)`): build a `podman` argv/`Cmd` here or ssh-wrapped (one shell-quoted command); non-interactive ssh opts. Used by `send-files`. |
| `containers.rs` | Pure parsing for `send-files`: `podman ps` → introdus-only `Container`s; `find -printf`/`ls` → `DirEntry`s (dir vs file + mtime/btime); `SortMode` + `sort_entries` (name/modified/created, dirs-first) + `fuzzy_match` for the filter. |
| `sshconfig.rs` | Container-capable `Host` aliases from `~/.ssh/config` (patterns/negations + git-forge remotes — `User git` / a forge `HostName` — dropped) for the `send-files` remote-host list. |
| `tmux.rs`     | Thin `tmux` helpers (sessions, windows, attach); per-session project-dir tagging (`@introdus_project_dir`) + lookup for attach-or-create. |
| `process.rs`  | `Cmd` — the logged wrapper over `std::process::Command` all external tools go through; stdout capture guard for the TUI output pane; shared `sh_quote`. |

## `crates/introdus-cli/` — the `introdus` binary

| File             | Owns |
| ---------------- | ---- |
| `main.rs`        | clap CLI: `Command` enum → subcommand dispatch. |
| `wizard.rs`      | First-run setup wizard (inline ratatui modals) → writes `.env`. |
| `preflight.rs`   | Host checks: rootless podman + pasta (+ tmux for the session). |
| `context.rs`     | `LaunchContext` — everything the launch path derives from a `Config` (names, assets dir, allowlist, tunnel IPs); per-project config path resolution (`.introdus/config.env`, legacy `./.env` fallback + one-time migration offer). |
| `launch.rs`      | Top-level launch orchestration (preflight → image → lifecycle → run); `verify`/`update`/`rebuild-base`; `resolve_webapp_host_port` — settles the host side of the webapp publish just before a create (prefers `WEBAPP_PORT`, falls back to a free port above it when taken, persists as `WEBAPP_HOST_PORT`, clears it once the preference frees up). |
| `image.rs`       | Base-image build/tag/prune; binary-newer-than-image staleness. |
| `lifecycle.rs`   | Container/volume lifecycle: cleanup, `--recreate`, `--reset` (dirty-git guard + typed confirm). |
| `run.rs`         | The full `podman run` flag/env/mount set; `--verify` self-check; `--update` in-container refresh. |
| `session.rs`     | The tmux session model — puts each container in one session with its windows; spawns/respawns the detached `notify-host` service. |
| `menu.rs`        | The control TUI (`introdus menu`): menu layout (group icons + per-item hotkeys), dispatch to `menu_actions`. |
| `menu_actions.rs`| Implementations of each control-menu utility (tunnel URL, (re)expose webapp — host-side probe of the cached quick-tunnel URL + in-place cloudflared restart, agents, allowlist, terminals, copy-in, ntfy, test/restart the notify service, recreate/reset/stop/destroy). The reusable, decision-free cores (`save_and_write_allowlist`, `append_whitelist`, `select_agents`, `run_install_agents`, the no-prompt actions) plus the shared `pub(crate)` task helpers (`remove_container_task`, `respawn_dev_window`, `spawn_window`) are generic over [`Frontend`] so `cli_actions` / `menu_paseo` can reuse them. |
| `menu_paseo.rs`  | The paseo control-panel actions, split out of `menu_actions`: **(Re)Install paseo** (`install_paseo`) — first install prompts relay-vs-direct then (direct) all-vs-restrict origins, an existing install offers a relay⇄direct **mode switch**, both persisting via [`Config::set_paseo_mode`]/`set_paseo_origins_all` + a recreate; the safe daemon package update (`update_paseo`/`run_update_paseo`) — pnpm 11+, isolated root-owned config, strict seven-day `minimumReleaseAge`, lifecycle + pnpmfile scripts/cached side effects blocked, trust/registry/source/integrity guards, then daemon restart; the "connect" action (`paseo_qr`: pairing QR in relay mode; in direct mode the paste-ready `tcp://host:port?password=…` URL — host from [`hostnet::detect`], shown via the panel's self-erasing transient block); and **Add a paseo client origin** (`add_origin`/`allow_origin`: appends a browser origin to the direct-mode CORS allowlist and applies it live — patches the running daemon's `config.json` + restarts the daemon, no recreate). Exposes the `pub(crate)` `paseo_opt_in` / `run_install_paseo` / `run_update_paseo` / `allow_origin` / `PASEO_ENSURE_DAEMON` cores `cli_actions` reuses. |
| `menu_tunnel.rs` | The panel's "(Re)Expose webapp" action + the `pub(crate)` `refresh_running_tunnel` / `container_has_tunnel_holes` cores (host-side quick-tunnel probe + in-place cloudflared restart) reused by `cli_actions`. |
| `cli_actions.rs` | Headless one-shot subcommands mirroring the panel utilities (`tunnel-url`, `blocked-egress`, `allow`, `expose-webapp`, `ntfy`, `install-agent`, `agent`, `install-paseo`, `update-paseo`, `paseo-url`, `paseo-allow-origin`, `dev-shell`/`root-shell`, `test-notify`, `notify-log`, `restart-notify`, `restart`, `stop`). Reuses the `menu_actions`/`menu_tunnel`/`menu_paseo` cores via a `StdioFrontend`; interactive prompts become CLI flags/args (`--restart`/`--recreate`/`--yolo`/`--yes`). |
| `frontend.rs`    | The `Frontend` trait — the output surface (`log` + `run_task`) shared by the interactive panel `Ui` and the headless `StdioFrontend`, so the action cores drive either. |
| `panel.rs`       | The persistent two-pane control panel: the `Ui` that owns the alternate screen, the input loop (hotkeys + `/` filter), task streaming, popup prompts, and `log_transient` — an output block that erases itself (replaced by a message) once its TTL is up, retired on the next redraw. |
| `panel_draw.rs`  | Pure rendering for the panel: `MenuView`/`Popup` types + the side-effect-free `draw_frame` and its status/menu/output/footer/prompt helpers. |
| `ui.rs`          | Shared ratatui primitives: status/row types (headers carry a group icon, items a hotkey), key reading, prompt state machines, the wizard's standalone inline modals. |
| `notify.rs`      | Host notification service: `notify-host` (FIFO/socket → ntfy/forward/desktop) and the laptop-side listen loop (`bind_listener` + `serve_listener`). |
| `notify_listen.rs`| The dev-machine `notify-listen` orchestration: flag/env/saved-config/wizard resolution, ssh reverse-tunnel supervision (autossh-or-ssh), the `systemd --user` unit install (no-linger, `default.target`), idempotency, `--dry-run`. |
| `install.rs`     | `introdus install` — put the binary on `PATH`. |
| `send_files/`    | `introdus send-files`: standalone alternate-screen app (`mod.rs` host/container pickers + spinner), the dual-pane file browser (`browser.rs`, laptop FS ⇆ container FS, per-pane sort `o` + fuzzy filter `/` + hidden-file toggle `.`, persisted `ListState` scroll), and the tar-stream/`podman cp` transfer (`transfer.rs`). Local or ssh-remote via `core::remote::Location`. |
| `util.rs`        | Small shared helpers (tilde expansion, shell quoting). |
| `screenshot.rs`  | Test-only (`#[cfg(test)]`): render a real frame into a `TestBackend` buffer and serialize it to a colour SVG. Driven by the `#[ignore]`d `shot_*` generators in `panel`/`ui`/`send_files::browser` that write the docs' `docs/img/*.svg`. |
| `tests/`         | pty integration tests (`wizard_pty.rs`, `menu_pty.rs`, `send_files_pty.rs`) + `common/`. See [06_testing.md](06_testing.md). |

## Embedded container-side bash (`container/`, `setup.sh`, `Dockerfile`)

Not compiled — embedded by `assets.rs` and bind-mounted at launch.

- `container/egress/firewall-entrypoint.sh` — PID 1: nft default-deny + tinyproxy
  + egress self-check, then drops privilege to `dev`.
- `container/egress/tinyproxy.conf` — the hostname-allowlist proxy config.
- `setup.sh` — post-firewall: clone repo, run launch hooks, auto-start the paseo
  daemon when `INSTALL_PASEO=true` (re-ensured on every container start; in
  `PASEO_MODE=direct` it sets a bcrypt password via a tmux PTY — fail-loud — and
  patches `~/.paseo/config.json` to bind `0.0.0.0:PASEO_PORT` with the relay off
  and the browser CORS `allowedOrigins` set from `PASEO_ALLOWED_ORIGINS` (a sole
  `*` = any; else union-with-existing minus `*`, so a live-added origin survives a
  restart while an all→restrict switch drops the wildcard)), start the workload;
  `restart-tunnel` mode re-establishes a dropped cloudflared quick tunnel.
- `container/agents.sh` — in-container agent-install registry (mirror of
  `agents.rs`).
- `container/bin/*` — `run-claude`, `install-agents`, `rc-notify` (container→host
  event sender; inside a paseo-spawned terminal it also mirrors the event to the
  paseo daemon as terminal activity — `waiting` → needs-input via the
  `idle_prompt` sentinel, `done` → idle), `egress-log`.
- `Dockerfile` — the base image; `COPY`s the `container/` tree.

## Tooling / meta

- `scripts/lint.sh`, `scripts/install-pre-commit.sh` — see [04_linting.md](04_linting.md).
- `scripts/gen-agent-rules.sh` — regenerate the root `AGENTS.md` (Codex/Pi/opencode)
  from `agent_rules/*.md`; `--check` mode gates drift in `lint.sh --quick`. See
  [00_agent_instructions.md](00_agent_instructions.md) for the rules-distribution setup.
- `test-harness/` — heavy nested-podman E2E suite — see [06_testing.md](06_testing.md).
- `deny.toml`, `clippy.toml`, `rustfmt.toml`, `Cargo.toml` `[workspace.lints]` —
  lint config.
- `PLAN.md` — rewrite design + milestone status. `TEST_PLAN.md` — test matrix.

# Linting

What each linter checks, where its config lives, how to run it standalone, and
how to install it. Single entry point is [`scripts/lint.sh`](../scripts/lint.sh);
this doc explains what's inside the box. The workspace is **Rust-only** — there
is no Kotlin/Android tooling.

## Modes (each is a strict superset of the previous)

| Mode         | Runs                                                                | Typical use                |
| ------------ | ------------------------------------------------------------------- | -------------------------- |
| `--quick`    | rustfmt + clippy                                                    | manual dev (~10 s warm)    |
| `--full`     | adds tokei budget, cargo-machete, cargo-deny, cargo-audit, jscpd    | CI / on demand             |
| `--security` | adds semgrep (Rust rule pack)                                       | default pre-commit         |

Default is `--quick`.

**No silent skips.** If a tool the mode would run isn't installed, the script
records a failure (accumulating across the whole run so you see every install you
owe in one pass) with the install hint and exits non-zero.

## Pre-commit hook

[`scripts/install-pre-commit.sh`](../scripts/install-pre-commit.sh) installs a
hook that runs **`cargo test --workspace` then `scripts/lint.sh <mode>`**:

```bash
./scripts/install-pre-commit.sh             # default: --security
./scripts/install-pre-commit.sh --full      # everything except semgrep
./scripts/install-pre-commit.sh --quick     # fmt + clippy only
```

Idempotent, but refuses to clobber a hook it didn't write. Bypass a single
commit with `git commit --no-verify`.

## The linters

### rustfmt

What: enforces a single canonical Rust source format.

Config: [`rustfmt.toml`](../rustfmt.toml) at repo root (stable options only;
`max_width = 100`, 4-space, Unix newlines).

```bash
cargo fmt --all -- --check     # what lint.sh runs
cargo fmt --all                # apply formatting
```

Bundled with rustup; nothing to install separately.

### clippy

What: Rust's official linter. Lint level is `-D warnings` (every warning is an
error). We additionally opt into the nursery `cognitive_complexity` lint via
[`Cargo.toml`](../Cargo.toml) `[workspace.lints.clippy]` (it is *not* covered by
`-D warnings`), with the threshold dropped to **15** in
[`clippy.toml`](../clippy.toml). Per-site overrides go in source with
`#[allow(clippy::lint_name)] // why: …`.

```bash
cargo clippy --workspace --all-targets --no-deps -- -D warnings
```

Bundled with rustup.

### tokei (file-size budget)

What: per-Rust-file budget of **≤ 600 lines of code** — a soft signal that a
module wants splitting.

Config: budget is a literal in [`scripts/lint.sh`](../scripts/lint.sh) (the
Python one-liner after the `tokei` call). Tokei itself takes no config.

```bash
tokei -t Rust crates/
```

Install: `cargo install tokei`.

### cargo-machete

What: finds Cargo dependencies declared in a `Cargo.toml` but not actually used.

Config: optional `[package.metadata.cargo-machete] ignored = [...]` in a crate's
`Cargo.toml` for known false positives.

```bash
cargo machete
cargo machete --with-metadata   # check against Cargo.lock instead of the grep heuristic
```

Install: `cargo install cargo-machete`.

### cargo-deny

What: dependency / license / advisory policy. Refuses unknown registries, unknown
git sources, disallowed licenses, and yanked crates.

Config: [`deny.toml`](../deny.toml) at repo root (`licenses`, `bans`, `sources`,
`advisories`). Advisory ignores mirror `.cargo/audit.toml`.

```bash
cargo deny --no-default-features check                # everything (what lint.sh runs)
cargo deny --no-default-features check advisories     # only the RustSec db
cargo deny --no-default-features check licenses|bans|sources
```

Install: `cargo install cargo-deny --locked`.

### cargo-audit

What: scans `Cargo.lock` against the RustSec advisory database.

Config: [`.cargo/audit.toml`](../.cargo/audit.toml) — per-CVE waivers under
`[advisories].ignore`, each justified in a comment above it (kept in sync with
`deny.toml`'s advisory ignores).

```bash
cargo audit
```

Install: `cargo install cargo-audit --locked`.

### jscpd (copy-paste duplication)

What: spots copy-pasted code blocks across the Rust sources.

Config: passed on the command line in [`scripts/lint.sh`](../scripts/lint.sh).
The gate is `--exitCode 1` (fail the moment any clone is detected), **not**
`--threshold N` — a percentage threshold lets real duplication slip past at low
percentages. Drive duplication out via refactoring, never by softening this gate.

Standalone (from the repo root):

```bash
./tools/node_modules/.bin/jscpd \
  --pattern "crates/**/*.rs" \
  --ignore "**/target/**,**/node_modules/**" \
  --min-lines 8 --min-tokens 50 \
  --exitCode 1 --reporters consoleFull .
```

Install (local, via pnpm — keeps the global npm namespace clean):

```bash
cd tools && pnpm install
```

`tools/` holds [`package.json`](../tools/package.json) and `pnpm-lock.yaml`
(committed); `tools/node_modules/` is gitignored.

### semgrep

What: SAST / pattern-based static analysis. We pin to the Rust rule pack.

Config: command-line `--config p/rust`. Per-call suppressions go on the source
line above the match as `// nosemgrep: rule.id` plus a why-comment.

```bash
semgrep --error --config p/rust --exclude target --exclude node_modules .
```

Install: `pipx install semgrep`.

## Suppression patterns by linter

Silence a genuine false positive at the one site, with a comment documenting
*why*:

| Linter        | Suppression                                                                        |
| ------------- | ---------------------------------------------------------------------------------- |
| clippy        | `#[allow(clippy::lint_name)]` on the item; add a `// why:` line.                    |
| rustfmt       | `#[rustfmt::skip]` on an item, or `// rustfmt-skip` on a single line.               |
| cargo-machete | `[package.metadata.cargo-machete] ignored = ["..."]` in the crate's `Cargo.toml`.  |
| cargo-deny    | edit `deny.toml`'s `[licenses]` / `[bans]` / `[advisories]` sections explicitly.   |
| cargo-audit   | `.cargo/audit.toml` with documented `[advisories].ignore` entries.                 |
| semgrep       | `// nosemgrep: rule.id` on the line immediately above the match.                    |

## Adding a new linter

1. Add the tool to one of the modes in `scripts/lint.sh` — guard it with
   `require_cmd` so a missing install is reported as a failure, not skipped.
2. If it has a config file, put it at repo root with the rest
   ([`rustfmt.toml`](../rustfmt.toml), [`clippy.toml`](../clippy.toml),
   [`deny.toml`](../deny.toml)).
3. Document it in this file: what / config / standalone / install.

# Security model

The whole reason the project exists: run untrusted-ish dev tooling (AI coding
agents + their dependency trees) so that a compromise is **confined to the
container**, never your real machine, and cannot exfiltrate to arbitrary hosts.

Threat model: a **compromised workload** inside the container (malicious npm
postinstall, a prompt-injected agent, a backdoored dependency). Goals: it cannot
(a) reach the host, (b) reach non-allowlisted network destinations, or (c)
disable the controls that enforce (a) and (b). The container host and your
laptop are **trusted**; `introdus` runs there with full privilege by design.

## 1. Host isolation — rootless podman

Containers run under **rootless podman** (the only supported config): no host
nftables changes, no systemd-host requirement, no sudo. Root-in-container maps to
your unprivileged host uid, so a container breakout lands as a non-privileged
host user. The egress filter is installed and enforced **inside** each container,
not on the host — nothing to tear down on the host, and the host firewall is
never touched.

## 2. In-container privilege drop

PID 1 is `container/egress/firewall-entrypoint.sh`, which starts as **root with
`CAP_NET_ADMIN`** and, in order:

1. Stages the host-mounted deploy key into `dev`'s `~/.ssh` (readable only by
   `dev`), while still root.
2. Installs the nft egress filter (below) and starts the proxy.
3. Runs the egress self-check.
4. **Drops all privilege** and `exec`s the workload as the non-root **`dev`**
   user via `setpriv --reuid`, with the container's **`no-new-privileges`** flag
   set.

After the hand-off the workload can never regain `CAP_NET_ADMIN` (reuid clears
the cap sets; `no-new-privileges` blocks re-acquisition), so it **cannot touch
nft**. It is non-root, so it cannot rewrite the proxy config or allowlist.

## 3. Egress hardening — default-deny + hostname allowlist

Two layers, both inside the container:

- **nft default-deny, segregated by uid.** `table inet egress` hook `output`
  `policy drop`. Accepts only: established/related, loopback, the **proxy uid**
  (`rcproxy`), DNS to the container's own resolvers, and configured direct-IP
  exceptions (`INTERNAL_ALLOW_CIDRS`, cloudflared edge IPs on 7844, tunnel API +
  paseo relay IPs on 443). Everything else — including all direct egress from
  `dev` — is dropped and counted.
- **Loopback hostname-allowlist proxy (tinyproxy).** The workload's only way out.
  Permits only hostnames matching `WHITELIST_HOSTS` (subdomain matches count; the
  git host and, when the webapp tunnel is on, `api.trycloudflare.com` are
  auto-added). Patterns are anchored, case-insensitive extended regex generated
  from the host list (see `egress.rs` / `firewall-entrypoint.sh`, kept
  byte-for-meaning identical).

Because direct egress is dropped **regardless of destination**, knowing a
whitelisted host's CDN IP and dialing it directly is not a bypass. `apt` and
HTTP(S) tools are proxy-configured; git-over-SSH tunnels through the proxy via an
ssh `ProxyCommand`; cloudflared/paseo, which can't be proxied, are allowed by IP
on their fixed ports only.

### Startup self-check (fail-closed)

Before handing off, the entrypoint proves the filter is actually enforcing, and
**aborts launch on any failure**: (a) a direct dial to a canary IP must fail,
(b) an allowlisted host must be reachable *through the proxy*, (c) a direct dial
to that host's resolved IP must fail (no IP bypass). `introdus verify` runs this
as a throwaway container.

### Residual risks (documented, not fixed)

- **DNS stays open** (the workload needs resolution), so **DNS tunnelling** is
  the residual exfiltration channel. Accepted.
- `INTERNAL_ALLOW_CIDRS` opens direct-IP egress to whatever you list — scope it
  tightly.
- `DISABLE_NETWORK_BLOCK=true` is an explicit escape hatch that runs the workload
  with **no firewall and no proxy**. Only for debugging; never a default.
- **paseo direct mode** (`PASEO_MODE=direct`) is the one intentional *inbound*
  surface: the daemon binds `0.0.0.0:PASEO_PORT` and introdus publishes it on the
  host's **all-interfaces** address so a laptop can reach it over a VPN/tailscale
  net. It is protected only by a generated bcrypt passphrase (`set-password`,
  driven via a PTY; the daemon otherwise runs unauthenticated, so setup **fails
  loud** and refuses to start the daemon if the password can't be set). Scope host
  reachability of that port to your VPN — do not run direct mode on a host whose
  `PASEO_PORT` is reachable from the public internet. The default (relay mode)
  exposes nothing inbound (the daemon dials out to the relay). Direct mode also
  does *not* widen egress — it never contacts paseo's relay/app hosts. The
  daemon's browser CORS allowlist (`PASEO_ALLOWED_ORIGINS`, applied to
  `config.json` by setup.sh) is **not** part of this boundary: CORS is
  browser-enforced only, so a native app / CLI / attacker sends any or no
  `Origin` and is unaffected. It restricts drive-by *web pages*, nothing more —
  the passphrase + VPN scoping remain the actual access control.

## 4. Supply-chain posture — agent installs

Nothing is baked into the base image; you pick agents in the wizard. Install
methods (`crates/introdus-core/src/agents.rs`, mirrored in `container/agents.sh`):

- **`Pnpm`** — `pnpm add -g --ignore-scripts <spec>`: no package lifecycle
  scripts run. The default for npm agents.
- **`PnpmBuild`** — `pnpm add -g --allow-build=<spec>`: the package's own
  postinstall *is* allowed (only claude-code, whose `install.cjs` places its
  native binary shipped as an npm optionalDependency). Still registry-only;
  flagged in the wizard.
- **`Script`** — `curl <spec> | bash`, a vendor installer **not** contained by
  `--ignore-scripts` (e.g. Antigravity). Flagged as higher-risk in the wizard.

Each agent declares the extra egress hosts it needs, appended to the allowlist
only when selected — no agent widens egress unless you install it.

The dedicated Paseo update (`introdus update-paseo` / the panel's `U` action)
uses pnpm's stricter supply-chain controls and refuses pnpm older than v11. It
blocks lifecycle scripts, cached script side effects, and `.pnpmfile` hooks
(under an isolated root-owned config directory); enforces a strict seven-day
`minimumReleaseAge` (including failing when publish-time metadata is absent);
rejects publisher-trust downgrades and exotic transitive sources; pins the
`@getpaseo` scope to the TLS npmjs registry; and keeps store-integrity checks on.
The running daemon is restarted only after the package update succeeds.

## 5. Deploy-key handling

A per-project deploy key lives on the **host**, mounted read-only, and is copied
into `dev`'s `~/.ssh` at startup (mode 600, dev-owned). Scope it to the single
repo. `introdus reset` / harness `destroy` delete the local key on teardown.

## 6. Notification trust boundary

The only attacker-influenceable text that crosses into a host-side desktop
notification is the **label** a container sends. `crates/introdus-core/src/notify.rs`
is the trust boundary: the wire format is `event` or `event<TAB>label`; the event
must match a fixed whitelist, and the label is stripped to a safe charset and
length-capped (`LABEL_MAX`) before it renders under the "Remote dev" brand.
Read input is bounded (`READ_LIMIT`). This mirrors the sanitization that lived in
the old `host_listener.py` / `host_notify.sh`.

## 7. Static analysis / dependency gates

Security-relevant checks live in the lint suite (see [04_linting.md](04_linting.md)):
**cargo-deny** (license/source/advisory policy, yanked crates), **cargo-audit**
(RustSec advisories), and **semgrep** (`p/rust` SAST) in the `--security` mode
the pre-commit hook uses by default. Advisory ignores must be justified inline in
both `deny.toml` and `.cargo/audit.toml`; prefer upgrades over ignores.

# Testing

Three tiers, fastest first. **Every feature gets coverage at the tier that can
actually prove it** — pure logic as a unit test, interactive UI as a pty test,
real container/tmux/firewall behaviour as a nested-harness driver. Don't ship a
feature with only unit tests if its real behaviour lives in the container.

## 1. Unit tests — `cargo test --workspace`

Fast, hermetic, in-module `#[cfg(test)]` blocks across nearly every file in both
crates (pure logic: config round-trips, egress regex, naming, port parsing,
notify sanitization, process wrapper, agent registry, etc.). This is the
`cargo test` suite the pre-commit hook runs on every commit.

```bash
cargo test --workspace          # everything below the pty/harness tiers
cargo test ta06                 # run the case(s) whose fn name carries an ID
```

Test cases are catalogued in [TEST_PLAN.md](../TEST_PLAN.md) with a **stable
`TAnn` ID**. The backing test function is **named with that ID** (`ta06_…`,
`ta25_*`), so the ID is the link between plan and code — `rg ta06` / `cargo test
ta06` finds it. New cases get the next free number; IDs are never renumbered.

## 2. pty integration tests — `crates/introdus-cli/tests/`

The interactive ratatui UI (wizard + control panel) is driven through a **real
pseudo-terminal** with the `rexpect` dev-dependency, feeding explicit keystrokes
(`\r` is Enter in raw mode) and synchronizing on rendered prompt text. Still run
by plain `cargo test` — no podman needed.

- `wizard_pty.rs` — drives `introdus init`'s inline modal sequence (confirm /
  text / checklist) end to end.
- `menu_pty.rs` — starts the two-pane control panel against a project whose
  container was never created (regression guard: no `no such container` leak),
  asserts Esc quits cleanly.
- `common/mod.rs` — shared fixture (temp project dir, `.env`, deploy-key gen).

The full-screen panel's on-screen *layout* is a cursor-addressed frame not
scraped byte-for-byte here; that's the harness's job (below).

## 3. Full-experience E2E — `test-harness/`

The heavy, **opt-in** tier — not part of `cargo test`. Drives the *real*
experience (`introdus launch` → tmux → rootless podman dev container → egress
firewall → clone → live control TUI) inside a **rootless podman-in-podman**
container and **asserts** on it (any bad assertion → `exit 1`). Requires a
rootless-podman host with `/dev/fuse` and `/dev/net/tun`.

```bash
test-harness/harness.sh            # all (default): the full end-to-end sweep
test-harness/harness.sh verify     # egress firewall self-check only (fast-ish)
test-harness/harness.sh launch     # container up + clone through the proxy
test-harness/harness.sh reattach   # repeat launch from a dir reattaches to one session
test-harness/harness.sh menu       # drive the live control TUI over tmux
test-harness/harness.sh egress     # workload default-deny enforcement
test-harness/harness.sh lifecycle  # recreate persistence + destroy teardown
test-harness/harness.sh install    # binary onto PATH
test-harness/harness.sh agents     # claude opt-out absent + opt-in menu install
test-harness/harness.sh agent-launch / agent-missing / quit-stop / detach / paseo / paseo-direct
test-harness/harness.sh send-files # send a host file into a container via the dual-pane TUI
test-harness/harness.sh cli        # headless subcommands drive a real container
```

Each target is a scripted `driver-*.sh` that drives the real UI over tmux and
asserts. First run builds the nested Ubuntu base image (a few minutes), cached
in the `introdus-harness-storage` volume; force a clean rebuild with
`podman volume rm introdus-harness-storage`. See
[test-harness/README.md](../test-harness/README.md) for how the nesting,
`--privileged` outer flags, and HTTPS clone-mocking work.

In [TEST_PLAN.md](../TEST_PLAN.md), rows proven here are marked **harness
`<target>`** in the Automated column (vs `✅` for `cargo test`, `⚠️` helper-only,
`❌` none).

## Pre-commit gate

`scripts/install-pre-commit.sh` installs a hook that runs `cargo test
--workspace` **then** `scripts/lint.sh` (default `--security`). It refuses to
clobber a hook it didn't write; bypass a single commit with `--no-verify`. See
[04_linting.md](04_linting.md).
