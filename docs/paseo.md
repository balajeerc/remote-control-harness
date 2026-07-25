# Paseo orchestrator

> Part of [introdus](../README.md#features). Drive your installed agents from a phone, desktop, web, or CLI client.

[paseo](https://paseo.sh) is an agent **orchestrator** (not a coding agent
itself). It runs your installed [agents](coding-agents.md) under a local daemon
and lets you drive them from a phone/desktop/web/CLI client through the paseo
relay.

## Prerequisites

- One or more [coding agents](coding-agents.md) installed — paseo natively
  supports claude, codex, opencode, and pi.
- The paseo relay host `paseo.sh` is added to the
  [allowlist](egress-filtering.md) automatically when paseo is enabled.

## Connection modes

paseo can connect in one of two ways, chosen when you install it:

- **Relay** (default) — the daemon dials **out** to the `paseo.sh` relay with
  end-to-end encryption, so nothing is exposed inbound. You pair a phone/desktop
  by scanning a QR code. Adds `paseo.sh` to the [allowlist](egress-filtering.md).
- **Direct** — a relay-free TCP connection for when your dev machine and the
  container host share a VPN/zero-trust network (tailscale, WireGuard, …). The
  daemon binds a host-published port (auto-assigned from `20190`, persisted to
  config) protected by a generated two-word password. **No** paseo relay/app host
  is added to egress — direct mode never contacts paseo's servers. See the
  [security model](../agent_rules/05_security.md#3-egress-hardening--default-deny--hostname-allowlist)
  for the inbound-surface caveats (scope the port to your VPN).

## Usage

Enable it in the [setup wizard](setup-and-configuration.md) (which asks
relay-vs-direct), set `INSTALL_PASEO="true"` (+ optional `PASEO_MODE="direct"`)
in [config](setup-and-configuration.md#configuration-reference), or install it
into a running container from the [control panel](control-panel.md) →
**"(Re)Install paseo"**:

- On a **fresh install**, the panel asks which connection mode you want, then
  offers the container recreate that wires it.
- When paseo is **already installed**, the same item offers to **switch to the
  other mode** (relay⇄direct) and recreate to apply it — a recreate keeps your
  `/home/dev` volume (repo, `~/.paseo`, toolchains). This is the supported way to
  move an existing relay container onto direct access and back.

On a **fresh direct install** you also choose the daemon's browser **CORS
policy**: allow any origin, or restrict to this machine's client (see
[Browser clients & CORS](#browser-clients--cors) below).

With paseo on:

- The panel's "Launch an installed agent" offers a **"via paseo"** mode for the
  natively-supported providers.
- The panel gains a **connect** item: **"Show paseo pairing QR code"** in relay
  mode (scan it from the app), or **"Show Paseo port & password"** in direct mode
  (the host `port` + `password` to enter in paseo desktop's Direct Connection).
- In direct mode it also gains **"Add a paseo client origin"** — see below.

Installed via `pnpm add -g @getpaseo/cli`. The headless equivalents are
`introdus install-paseo`, `introdus paseo-url` (prints the pairing URL, or the
port + password in direct mode), and `introdus paseo-allow-origin <url>`.

## Browser clients & CORS

A direct-mode daemon gates **browser** WebSocket connections by their `Origin`
(the URL the paseo web UI is loaded from), via `PASEO_ALLOWED_ORIGINS`. This is a
browser-only check — the `paseo` CLI and native apps send no browser `Origin`, so
it never restricts them. **The password + your VPN are the real access control;
CORS just stops a drive-by web page.**

- **Allow all** → `PASEO_ALLOWED_ORIGINS="*"`; any origin is accepted.
- **Restrict** (default) → the daemon accepts `https://app.paseo.sh` plus a
  self-hosted client on this machine (`http://localhost:6767`,
  `http://127.0.0.1:6767` — paseo's default client port).

If you run the paseo web client on **another device** (e.g. a phone browser
loading the client over tailscale at `http://<host-ip>:6767`), that page's origin
isn't `127.0.0.1` — it's **the host's IP + the client port** (not the phone's
IP). Add it with the panel's **"Add a paseo client origin"** (or
`introdus paseo-allow-origin http://<host-ip>:6767`). It applies **live** —
patches the running daemon and restarts it, no container recreate — and persists
so a later recreate re-applies it. (Native paseo mobile/desktop apps aren't
browsers, so they don't need this.)

## How it works

In relay mode the daemon dials **out** to the relay with end-to-end encryption,
so nothing is exposed inbound — the same no-inbound-port posture as
[Claude remote control](claude-remote-control.md). In direct mode the daemon
binds `0.0.0.0:PASEO_PORT` (published on the host) and enforces the generated
password for network clients (a local client is trusted via the `~/.paseo`
keypair). Either way the daemon supervises agents and you orchestrate them from
the paseo client; `paseo run` headless isn't the intended path, so agents still
launch directly in their own tmux windows.

The host-side constants (relay host, install spec, direct-mode passphrase
generator) are in [agents.rs](../crates/introdus-core/src/agents.rs); the mode
normalization is `Config::set_paseo_mode` in
[config.rs](../crates/introdus-core/src/config.rs); the panel actions ((re)install
+ mode switch, connect, daemon-ensure snippet) are in
[menu_paseo.rs](../crates/introdus-cli/src/menu_paseo.rs).
