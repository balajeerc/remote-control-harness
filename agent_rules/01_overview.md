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
