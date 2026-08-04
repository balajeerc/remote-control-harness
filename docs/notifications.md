# Task-completion notifications

> Part of [introdus](../README.md#features). "Task done / awaiting input" as a native popup + sound, plus optional phone push.

A coding agent signals "task complete" / "awaiting input" from inside the **dev
container**; the harness turns that into a native desktop popup + sound on your
**dev machine** (and, optionally, a phone push). How the signal reaches you
depends on whether the container host is your laptop or a separate remote box.

```
[dev container] rc-notify --FIFO--> [container host] introdus notify-host
          ├── host IS your laptop  -> render popup + sound here
          └── host is remote       -> forward over the SSH reverse (-R) tunnel
                                        --> [laptop] introdus notify-listen -> popup + sound
```

## Prerequisites

- **Local host:** a desktop notifier + audio player. Linux: `libnotify-bin`
  (`notify-send`) + a PulseAudio/PipeWire player (`paplay`/`pw-play`/`aplay`/`ffplay`);
  macOS: `osascript` + `afplay` (built in).
- **Remote host → laptop:** an SSH client on the laptop with key-based access to
  the host, `autossh` (recommended, for a self-healing tunnel), and a per-user
  `sshd` [forwarding allowance](#host-ssh-forwarding-requirement) on the host.
- **Phone push:** the [ntfy](https://ntfy.sh) app and a private topic.

## Usage

### Local host (host = laptop)

**Nothing to configure.** `introdus` starts `notify-host` automatically as a
detached service for each launched session; it renders the popup and plays
`notification_sound.wav` on the same machine.

### Remote host → laptop (two hops)

A remote/headless host has no desktop, so the event makes a second hop to your
laptop over an SSH **reverse** (`-R`) tunnel. Your laptop dials *out* to the host,
so nothing needs an inbound port (works behind NAT).

**On the host** — answer *yes* to the wizard's *"Forward notifications to a
separate dev machine?"*, or set it by hand in
[config](setup-and-configuration.md#configuration-reference):

```bash
RC_FORWARD_ADDR=127.0.0.1:8765
```

`notify-host` then forwards each event to that loopback port instead of rendering.

> `notify-host` reads `RC_FORWARD_ADDR` **once, when the session starts**. If you
> change it on a running session, use the [control panel](control-panel.md)'s
> "Restart the notification service" — no container recreate needed. "Send a test
> host notification" prints whether it forwards or renders locally.

**On the laptop** — `introdus notify-listen` owns both the reverse tunnel *and*
the listener:

```bash
introdus notify-listen                                  # first run: short wizard
introdus notify-listen --via <ssh-alias> --port 8765    # non-interactive
introdus notify-listen --via <alias> --install-service  # run on every login
```

The wizard saves answers to `~/.config/introdus/notify-listen.env`. Under the
hood it opens `autossh -M 0 -N -o ExitOnForwardFailure=yes -R
8765:127.0.0.1:8765 <alias>` (falling back to plain `ssh`), binds the loopback
port, and renders locally. The port must match `RC_FORWARD_ADDR` on the host.
`--dry-run` prints the plan; `--no-tunnel` runs only the listener.

`--install-service` writes a `systemd --user` unit
(`WantedBy=default.target`, `Restart=on-failure`). It deliberately does **not**
enable linger — the service starts with your graphical session so `notify-send` /
`paplay` inherit its D-Bus and display. On macOS, wrap the foreground command in
a launchd agent.

### Host SSH-forwarding requirement

The laptop reaches the loopback port via the reverse tunnel, so the host's `sshd`
must permit forwarding for your user. Hardened hosts commonly ship
`AllowTcpForwarding no`, which silently blocks it. Grant the minimum for your
user only, in a drop-in such as `/etc/ssh/sshd_config.d/zz-notify-tunnel.conf`.

For notifications **only** (reverse `-R` forward):

```
Match User <your-host-user>
    AllowTcpForwarding remote
    PermitListen 127.0.0.1:8765 localhost:8765
```

If you **also** attach with [VS Code Remote-SSH](remote-host.md) (which needs
*local* `-L` forwarding), use `all` and confine `-L` to loopback so the host
can't become a network pivot:

```
Match User <your-host-user>
    AllowTcpForwarding all
    PermitListen 127.0.0.1:8765 localhost:8765
    PermitOpen 127.0.0.1:* localhost:* [::1]:*
```

then `sudo sshd -t && sudo systemctl reload ssh`. Two gotchas:

- The **`localhost:8765`** entry in `PermitListen` is required — a no-bind `-R`
  forward presents its listen address as `localhost`, not `127.0.0.1`.
- If the host runs file-integrity monitoring (AIDE, etc.), refresh its baseline
  after editing `/etc/ssh`.

### Which container fired it?

Each notification's title carries the container's project name — *"Remote dev:
myproject"* — so you can tell many containers apart. Derived from `PROJECT_NAME`;
override per-container with `RC_LABEL` (takes effect on the next
[`introdus recreate`](persistence-and-lifecycle.md)).

### Phone push (ntfy.sh)

Independently of the desktop path, set `ENABLE_NOTIFY_SH_ALERTS=true` and
`NTFY_SH_TOPIC=<your-private-topic>` in
[config](setup-and-configuration.md#configuration-reference) to also push each
alert to your phone via [ntfy.sh](https://ntfy.sh). Sent from the host over
outbound HTTPS, so it needs no forwarding. **Treat the topic name like a
password** — anyone who knows it can publish and read.

## How it works

- **Dev container:** an agent's `Stop` / `Notification` / `PermissionRequest`
  hooks call [`rc-notify`](../container/bin/rc-notify), which writes
  `event<TAB>project` to the host FIFO mounted at `/run/notify`. It never blocks
  a hook for more than a few seconds and never fails it. The FIFO is
  world-writable (0666) on purpose — rootless podman maps its host owner to
  container-root while `rc-notify` runs as `dev`, so a 0600 FIFO would drop every
  event; it's safe because it sits in `$XDG_RUNTIME_DIR` (0700, owner-only).
- **Paseo terminals:** when that same hook runs inside a terminal the
  [paseo](paseo.md) daemon spawned (it injects `PASEO_TERMINAL_ID`), `rc-notify`
  *also* reports the state to paseo, so the phone gets its own "Terminal needs
  input" / "Terminal finished" push alongside the desktop popup. Everywhere else
  — an ordinary `run-claude` window — it's a no-op and the FIFO stays the only
  path. We report from `rc-notify` rather than enabling paseo's own claude hooks
  because those treat only an `idle_prompt` notification as "needs input" and
  register no `PermissionRequest` hook, so a permission prompt would raise
  nothing; `rc-notify` already fires on the right events and feeds paseo the
  `idle_prompt` sentinel it needs. Agents started through the app (`paseo run`)
  need none of this: paseo tracks those natively, and its SDK call sets
  `settingSources` to include user settings, so `rc-notify` fires there too.
- **Trust boundary:** `introdus notify-host` validates the event against a fixed
  whitelist (`done` / `waiting`) and strips the label to `[A-Za-z0-9._-]`
  (≤40 chars) before it renders under the "Remote dev" brand — a compromised
  container can't spoof arbitrary text or inject control characters. This lives
  in [notify.rs](../crates/introdus-core/src/notify.rs); the services are in
  [notify.rs (cli)](../crates/introdus-cli/src/notify.rs) and
  [notify_listen.rs](../crates/introdus-cli/src/notify_listen.rs).

### Limitations

The desktop path is **best-effort, fire-and-forget**: if the laptop is offline or
the tunnel is mid-reconnect when an event fires, that notification is dropped (no
queue, no retry). The forward never blocks — it fails fast on a refused
connection, capped at 5s otherwise — so a down tunnel never wedges an agent hook.
For a durable record, pair it with the ntfy.sh push; the two are independent and
run together.
