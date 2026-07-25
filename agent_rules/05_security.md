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
