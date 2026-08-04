# Persistence, lifecycle & updates

> Part of [introdus](../README.md#features). What survives restarts, how to apply config changes, and how to refresh in place.

Each project's container is created once and reused across launches, with a
per-project volume so your work survives. This doc covers what persists, the
`recreate` / `reset` / `destroy` lifecycle, and `introdus update`.

## Prerequisites

None — persistence is the default. `introdus update` needs the container to be
**running** (it routes through the [egress proxy](egress-filtering.md) the
entrypoint installs at startup).

## Lifecycle

```bash
introdus recreate    # drop the container, KEEP the /home/dev volume
introdus reset       # drop the container AND wipe the volume (guarded)
introdus update      # in-container refresh (apt, mise, agents, LazyVim)
introdus rebuild-base # rebuild the shared base image
```

On the [control panel](control-panel.md)'s "Container lifecycle" group these show
as **Restart**, **Recreate**, **Detach**, and a single **Destroy/Reset** entry —
it wipes the volume (guarded), then asks whether to also delete the deploy key
and whether to bring the container back up, so one entry covers both `reset`
(back up, fresh volume) and `destroy` (torn down, key removed) — plus **Quit**
(stops the container).

**When to recreate:** `podman run` flags (capabilities, volumes, env vars,
published ports, `--memory`) are frozen at container-create time — `podman start`
doesn't re-apply them. So changing a run-affecting config
([`EXTRA_PORTS`](host-data-and-ports.md), `MEM_LIMIT`,
[`EXPOSE_WEBAPP`](webapp-tunnel.md), `RC_LABEL`, …) needs `introdus recreate` to
take effect. [Allowlist](egress-filtering.md) and [launch-hook](launch-hooks.md)
changes apply on a plain relaunch.

**Reset is guarded:** before wiping, `introdus reset` scans `/home/dev/work` in a
throwaway container and reports any repo with uncommitted changes, un-pushed
commits, or stashes. If anything turns up it pauses for a `y/N` confirmation —
default **no** — so you won't accidentally destroy in-progress work.

## Updates

`introdus update` refreshes a *running* container without touching the image or
its identity. In order:

- `apt-get update && apt-get -y upgrade` — Ubuntu security patches, etc.
- `mise self-update` + `mise upgrade` — mise and every managed toolchain
  (`node@24`, `pnpm@latest`).
- `install-agents` — installs any newly-selected [agents](coding-agents.md), then
  updates each in place honouring its install method.
- `nvim --headless "+Lazy! sync" +qa` — LazyVim plugin updates.

Run it from the panel, or from a second host terminal while the container is up.
It does **not**: rebuild the base image (Dockerfile edits need `rebuild-base` +
`reset`), update image-level non-apt content (the nvim binary, tree-sitter CLI,
seeded LazyVim config), or recover a broken half-upgrade.

## How it works — what persists

The container has no `--rm`, so its writable overlay persists across restarts:
`apt install` packages, service data dirs (`/var/lib/clickhouse`, `/var/log`),
`/etc` edits, and everything under `/home/dev`. `/home/dev` is *additionally*
backed by a named podman volume `introdus-vol-$PROJECT_NAME`, so the repo working
tree (`/home/dev/work/<project>`), `node_modules`, the pnpm store, mise
toolchains, and Claude auth survive even a full container removal (`recreate`).
The volume is seeded from the image's `/home/dev` on first mount. The rest of the
filesystem lives on the overlay and is lost on `recreate` / `reset`.

**Tradeoff.** Full-filesystem persistence means a compromised session carries into
subsequent launches until you `reset`. This is intentional: the real security
boundary is the [rootless userns + egress filter + scoped deploy
key](container-hardening.md), **not** the container's internal rootfs. The
container is a trusted dev environment, not a sandbox-per-session.

The lifecycle logic is in
[lifecycle.rs](../crates/introdus-cli/src/lifecycle.rs); the update/refresh path
is in [run.rs](../crates/introdus-cli/src/run.rs); the base-image
build/tag/staleness is in [image.rs](../crates/introdus-cli/src/image.rs).
