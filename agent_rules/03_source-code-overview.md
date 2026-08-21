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
