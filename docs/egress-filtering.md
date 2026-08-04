# Egress filtering

> Part of [introdus](../README.md#features). The core guarantee — the workload can only reach hosts you allowlist.

The whole reason introdus exists: a compromised workload inside the container
(a malicious npm postinstall, a prompt-injected agent, a backdoored dependency)
**cannot exfiltrate to arbitrary hosts**. Egress is filtered *inside* the
container, so the host runs no nftables/iptables, needs no `sudo`, and keeps no
per-container firewall state.

## Prerequisites

None — egress filtering is always on (the whole point) and needs nothing on the
host. It runs on **Linux rootless podman** with `--network=pasta`. The only
knobs are which hostnames to allow (`WHITELIST_HOSTS`) and, rarely, direct-IP
exceptions (`INTERNAL_ALLOW_CIDRS`). The one way to turn it off is the debug
[escape hatch](#escape-hatch).

## Usage

### The allowlist

`WHITELIST_HOSTS` in [config](setup-and-configuration.md#configuration-reference)
is a **default-deny** hostname allowlist. Each entry matches the host and its
subdomains — `github.com` matches `api.github.com` but not `notgithub.com`. The
git host, and (when the [webapp tunnel](webapp-tunnel.md) is on)
`api.trycloudflare.com`, are added automatically. Each installed
[agent](coding-agents.md) appends the extra hosts it needs. The default list
covers GitHub, the npm/PyPI registries, the Anthropic API, mise, Ubuntu
archives, and the VS Code marketplace (see [sample.env](../sample.env)).

### Adjusting the allowlist

If a package manager or tool inside the container **hangs on a network call**,
it's almost always a missing host. Two ways to add it:

- From the [control panel](control-panel.md): "Add hostnames to the egress
  allowlist", and "List recently blocked egress URLs" to see what got dropped
  (the container-side `egress-log` command shows the same).
- Edit `WHITELIST_HOSTS` and relaunch.

The proxy filters by **hostname**, not IP, so a CDN-fronted host that rotates its
A records mid-session keeps working — there's no re-resolve loop and no live
filter to mutate. The allowlist is regenerated from `WHITELIST_HOSTS` each time
the entrypoint runs, so a change takes effect on the next launch.

### Internal targets by IP

If a tool must reach a fixed internal target by IP (a private registry, an
internal API) rather than by hostname through the proxy, add its CIDR to
`INTERNAL_ALLOW_CIDRS` — those are allowed directly by the nft filter. Scope it
tightly; it opens direct egress to whatever you list.

### Escape hatch

```bash
introdus --disable-network-block    # sets DISABLE_NETWORK_BLOCK=true
```

runs the workload with **no filter and no proxy** — unfiltered egress. Only for
debugging; never a default.

## How it works

The container's PID 1 is
[`firewall-entrypoint.sh`](../container/egress/firewall-entrypoint.sh), which
starts as **root with `CAP_NET_ADMIN`** and, before handing off to the workload:

1. **Installs an nft default-deny filter**, segregated by uid. `table inet
   egress` hook `output` `policy drop` accepts only: established/related,
   loopback, the **proxy uid** (`rcproxy`, uid 1001), DNS to the container's own
   resolvers, and configured direct-IP exceptions (`INTERNAL_ALLOW_CIDRS`,
   cloudflared edge IPs on 7844, tunnel/relay IPs on 443). The non-root `dev`
   workload (uid 1000) matches none of these, so it has **no direct internet
   egress**.
2. **Starts a loopback-only hostname-allowlist proxy**
   ([tinyproxy](../container/egress/tinyproxy.conf), `127.0.0.1:8888`,
   `FilterDefaultDeny`) as `rcproxy`. Only hostnames matching `WHITELIST_HOSTS`
   are reachable, and `CONNECT` tunnels are limited to ports 443, 563, 22.
3. **Runs an egress self-check** (below).
4. **Drops `CAP_NET_ADMIN`** and `setpriv`-execs the workload
   ([setup.sh](../setup.sh)) as the non-root `dev` user with an empty bounding
   set and `no-new-privileges`.

After the hand-off the workload is non-root, holds no `NET_ADMIN`, and runs under
`no-new-privileges` — so it **cannot modify the filter** or rewrite the proxy
config. Its only way out is the loopback proxy, gated by hostname. Dialing a raw
IP — even a shared-CDN IP that also serves an allowlisted host — is dropped, so
**IP-level bypass isn't available**.

Because the workload has no direct egress, proxy-aware tools are wired to the
proxy: git-over-SSH tunnels through it via an ssh `ProxyCommand`; `apt` uses an
`apt.conf` proxy setting; `HTTP(S)_PROXY` point HTTP-aware tools at it.
Node ignores those vars by default, so `NODE_USE_ENV_PROXY=1` is set too — that
routes node's built-in clients (`fetch`/undici, `node:http`/`https`) through the
proxy without each tool wiring up its own agent (Node >= 24; the image pins the
node major for it). It doesn't cover raw `net.Socket` use or a library that
builds its own agent, and it widens nothing: proxied traffic still has to clear
the hostname filter. `cloudflared`'s edge protocol can't be proxied, so its edge
IPs are allowed directly by IP on 7844.

### Startup self-check

Before handing off, the entrypoint **proves the filter is enforcing** (it is
**fail-closed**) and aborts launch on any failure:

1. A direct dial to a canary IP (`CANARY_BLOCKED_IP`) must **fail** — default-deny
   is enforcing.
2. An allowlisted host must be reachable **through the proxy** — the sanctioned
   path works.
3. A direct dial to that host's resolved IP must **fail** — a known CDN IP can't
   bypass the proxy.

`introdus verify` runs this as a throwaway container.

### Residual risk

**DNS stays open** (the workload needs resolution), so DNS tunnelling is the
residual exfiltration channel. Accepted — a high bar, but real.

The pure allowlist logic (git-host extraction, per-host anchored regex, tunnel
edge IPs) lives in
[egress.rs](../crates/introdus-core/src/egress.rs), kept byte-for-meaning
identical to the bash in `firewall-entrypoint.sh`. See
[05_security.md](../agent_rules/05_security.md) for the full threat model.
