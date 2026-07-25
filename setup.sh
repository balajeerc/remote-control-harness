#!/usr/bin/env bash
# Runs inside the container as the non-root `dev` user, in two phases driven by
# firewall-entrypoint.sh:
#   setup.sh prepare  — clone the repo + apply --pull. The entrypoint then runs
#                       ON_LAUNCH_ROOT_SCRIPT (as root, with the repo present).
#   setup.sh serve    — start the cloudflared tunnel (if enabled), run
#                       ON_LAUNCH_SCRIPT (as dev), print the banner, and idle.
# With no argument it runs both (prepare then serve). One more on-demand mode,
# invoked by the control panel (not the entrypoint):
#   setup.sh restart-tunnel — re-establish a dropped cloudflared quick tunnel
#                       (new URL), reusing the container's baked-in edge IPs.
#
# Before this runs, the firewall entrypoint has already, as root: installed the
# nft egress filter, started the hostname-allowlist proxy, run the egress
# self-check, and staged the deploy key into ~/.ssh. ssh reaches git hosts
# through the egress proxy (see /home/dev/.ssh/config). Safe to re-run —
# /home/dev is a persistent podman volume.
set -euo pipefail

: "${PROJECT_NAME:?}"
: "${REPO_URL:?}"
: "${WEBAPP_PORT:?}"

# HOST_OS is set by introdus and only controls which host-side instructions are
# printed at the end (the container is always Linux).
HOST_OS="${HOST_OS:-linux}"
EXPOSE_WEBAPP="${EXPOSE_WEBAPP:-false}"

HOME="${HOME:-/home/dev}"
WORKDIR="${HOME}/work/${PROJECT_NAME}"

# Container name as created by introdus (carries the per-project suffix). Used
# only to print correct host-side exec/stop commands. Note: exec hints below use
# `--user dev` because the workload, its tmux sessions, and its files all belong
# to dev — a default (root) exec would miss dev's per-uid tmux socket.
CNAME="${CONTAINER_NAME:-introdus-$PROJECT_NAME}"

log() { printf '\n==> %s\n' "$*"; }

# Non-interactive bash does not source .bashrc, so activate mise explicitly.
export PATH="${HOME}/.local/bin:$PATH"
eval "$(${HOME}/.local/bin/mise activate bash)"

# ---- prepare: clone + --pull (repo ends up owned by dev) --------------------
do_prepare() {
    # known_hosts: StrictHostKeyChecking=accept-new (in ~/.ssh/config) records the
    # host key on first clone over the proxy tunnel. ssh-keyscan is NOT usable
    # here — it dials :22 directly and the egress filter drops that.
    mkdir -p "${HOME}/.ssh"
    chmod 700 "${HOME}/.ssh" 2>/dev/null || true
    touch "${HOME}/.ssh/known_hosts"
    chmod 600 "${HOME}/.ssh/known_hosts"

    if [[ -d "$WORKDIR/.git" ]]; then
        log "repo already present at $WORKDIR — skipping clone"
    else
        log "cloning $REPO_URL (git-over-SSH tunneled through the egress proxy)"
        mkdir -p "$(dirname "$WORKDIR")"
        git clone "$REPO_URL" "$WORKDIR"
    fi
    cd "$WORKDIR"

    # One-shot --pull sentinel dropped by introdus. Always consume it (rm first)
    # so a failed pull doesn't keep retrying on every relaunch.
    if [[ -f "${HOME}/.pull-on-next-start" ]]; then
        rm -f "${HOME}/.pull-on-next-start"
        log "fast-forwarding repo (--pull)"
        if git fetch --prune; then
            if ! git pull --ff-only; then
                echo "  warning: not a fast-forward — repo left as-is. resolve manually inside the container."
            fi
        else
            echo "  warning: git fetch failed — repo left as-is"
        fi
    fi

    # Install the coding agents picked in the wizard (idempotent; every agent,
    # claude included, is installed here — nothing is baked into the image).
    # No colon in the default: unset -> claude, explicitly empty -> nothing.
    # Never fatal — the container comes up even if an agent fails to install.
    if [[ -x /usr/local/bin/install-agents ]]; then
        log "installing selected agents: ${INSTALL_AGENTS-claude}"
        INSTALL_AGENTS="${INSTALL_AGENTS-claude}" /usr/local/bin/install-agents \
            || echo "  warning: install-agents reported a problem (continuing)"
    fi
}

print_banner() {
    cat <<EOF

============================================================
  Dev container '$CNAME' is up and running.
============================================================

Shell into the container (as the dev user):
  podman exec -it --user dev $CNAME bash
${CLAUDE_BANNER:-}${AGENTS_BANNER:-}
Connect with VSCode (Dev Containers):

${VSCODE_SETUP_INSTRUCTIONS:-}

  Once attached, install the 'Claude Code' extension — it lands on the
  container's persistent volume and survives relaunches.

To stop the container:
  podman stop $CNAME
  or: press Ctrl+C / Ctrl+D in this terminal

============================================================
${TUNNEL_BANNER:-}
${NTFY_BANNER:-}

EOF
}

# ---- cloudflared quick tunnel (shared by `serve` and `restart-tunnel`) -------
# Start (or restart) the cloudflared quick tunnel in tmux session 'tunnel'.
# Pin edge IPs and force HTTP/2 so cloudflared skips SRV-based edge discovery and
# avoids QUIC/UDP. Edge IPs come from TUNNEL_EDGE_IPS, set by introdus and allowed
# (by IP, on 7844) in the nft egress filter — cloudflared's edge protocol can't go
# through the HTTP proxy. Truncates the log and drops the stale URL cache so the
# next wait_tunnel_url reads only this run's URL.
start_tunnel() {
    log "starting cloudflared quick tunnel in tmux session 'tunnel' (-> port $WEBAPP_PORT)"
    tmux kill-session -t tunnel 2>/dev/null || true
    : > "${HOME}/.logs/tunnel.log"
    rm -f "${HOME}/.logs/tunnel-url.txt"
    local edge_args=""
    for ip in ${TUNNEL_EDGE_IPS:-}; do
        edge_args="$edge_args --edge ${ip}:7844"
    done
    tmux new-session -d -s tunnel "cloudflared tunnel --protocol http2 $edge_args --url http://localhost:$WEBAPP_PORT 2>&1; echo '[cloudflared exited]'; exec bash"
    tmux pipe-pane -t tunnel -o "cat >>${HOME}/.logs/tunnel.log"
}

# Poll the tunnel log up to 30s for the assigned quick-tunnel URL; cache it to
# tunnel-url.txt and echo it on stdout (empty if it never appeared).
wait_tunnel_url() {
    local url=""
    for _ in $(seq 1 60); do
        url=$(grep -oE 'https://[a-z0-9-]+\.trycloudflare\.com' "${HOME}/.logs/tunnel.log" 2>/dev/null | grep -v '^https://api\.trycloudflare\.com$' | head -1 || true)
        [[ -n "$url" ]] && break
        sleep 0.5
    done
    [[ -n "$url" ]] && echo "$url" > "${HOME}/.logs/tunnel-url.txt"
    echo "$url"
}

# ---- restart-tunnel: re-establish a dropped quick tunnel (new URL) -----------
# Invoked by the control panel's "(Re)Expose app via Cloudflare Tunnel" when the
# cached URL is no longer routing. Reuses the container's baked-in edge IPs, so it
# only works when the container was created with EXPOSE_WEBAPP=true (holes open).
do_restart_tunnel() {
    mkdir -p "${HOME}/.logs"
    if [[ "$EXPOSE_WEBAPP" != "true" ]]; then
        echo "EXPOSE_WEBAPP is not true for this container — nothing to (re)start." >&2
        exit 3
    fi
    if [[ -z "${TUNNEL_EDGE_IPS:-}" ]]; then
        echo "This container has no tunnel egress holes (created before EXPOSE_WEBAPP)." >&2
        echo "Recreate it to open them." >&2
        exit 4
    fi
    start_tunnel
    log "waiting for the new cloudflared tunnel URL (up to 30s)"
    local url
    url=$(wait_tunnel_url)
    if [[ -z "$url" ]]; then
        echo "  tunnel URL not detected after 30s; check: tmux attach -t tunnel" >&2
        exit 1
    fi
    echo
    echo "  PUBLIC TUNNEL (re)started — new URL:"
    echo "    $url"
    echo
}

# ---- paseo daemon (optional agent orchestrator) -----------------------------
# When paseo is opted in (INSTALL_PASEO=true), bring its daemon up as part of
# container boot so `paseo ls` / a client can connect without first opening the
# control panel — and so it comes back after a container stop/start (the
# entrypoint re-execs this on every start). paseo is on the image PATH
# (PNPM_HOME), so a non-login shell finds it.
#
# Two connection modes (PASEO_MODE):
#   relay  (default) — the daemon dials OUT to paseo's relay; nothing is exposed.
#   direct           — the daemon binds 0.0.0.0:PASEO_PORT (published on the host)
#                      with the relay OFF and a bcrypt password, for a plain TCP
#                      connection over a VPN/zero-trust network.
#
# A non-zero `paseo daemon start` is NOT authoritative — its readiness gate can
# report "failed to start" while the worker is actually serving (e.g. a slow
# relay handshake) — so we re-probe the daemon status and warm-retry once. Never
# fatal (set -e safe): the container comes up regardless.
CONFIG_JSON="${HOME}/.paseo/config.json"

_paseo_up() {
    paseo daemon status --json 2>/dev/null | grep -Eq '"localDaemon":[[:space:]]*"running"'
}
_paseo_has_password() {
    [[ -f "$CONFIG_JSON" ]] && grep -q '"password"' "$CONFIG_JSON"
}

# Set the daemon password (direct mode). paseo's `set-password` is an interactive
# TUI prompt with no non-interactive flag, so drive it through a REAL pty via a
# detached tmux session. Idempotent (skips if already set). Returns non-zero if a
# password could not be saved — the caller then refuses to start the daemon.
set_paseo_password() {
    _paseo_has_password && return 0
    [[ -n "${PASEO_PASSWORD:-}" ]] || { echo "  paseo: PASEO_PASSWORD is empty"; return 1; }
    # set-password saves into the daemon home; make sure it's initialized first.
    if [[ ! -f "$CONFIG_JSON" ]]; then
        paseo daemon start >/dev/null 2>&1 || true; sleep 2
        paseo daemon stop  >/dev/null 2>&1 || true
    fi
    tmux kill-session -t paseo-pw 2>/dev/null || true
    tmux new-session -d -s paseo-pw -x 200 -y 50 "paseo daemon set-password; sleep 20"
    sleep 3
    tmux send-keys -t paseo-pw "$PASEO_PASSWORD" Enter; sleep 1
    tmux send-keys -t paseo-pw "$PASEO_PASSWORD" Enter; sleep 3   # confirm step, if any
    tmux kill-session -t paseo-pw 2>/dev/null || true
    _paseo_has_password
}

# Direct mode: ensure a password is set, then patch config.json so the daemon
# binds 0.0.0.0:PASEO_PORT with the relay disabled and the browser CORS allowlist
# applied (a node merge that preserves the password + any other keys). Returns
# non-zero if the password can't be set.
#
# CORS from PASEO_ALLOWED_ORIGINS (space-separated; a sole "*" = accept any
# origin). A wildcard is authoritative (replaces the list); otherwise we UNION the
# desired origins with whatever config.json already has, minus "*" — so a panel
# "add a client origin" (which patches config.json live) survives a plain restart,
# while an all→restrict switch still drops the wildcard.
configure_paseo_direct() {
    set_paseo_password || return 1
    node -e '
      const fs=require("fs"), path=require("path");
      const f=process.env.HOME+"/.paseo/config.json";
      let c={}; try { c=JSON.parse(fs.readFileSync(f,"utf8")); } catch {}
      c.version=c.version||1; c.daemon=c.daemon||{};
      c.daemon.listen=process.argv[1];
      c.daemon.relay=Object.assign({}, c.daemon.relay, {enabled:false});
      const desired=(process.env.PASEO_ALLOWED_ORIGINS||"").split(/\s+/).filter(Boolean);
      if (desired.length) {
        c.daemon.cors=c.daemon.cors||{};
        if (desired.includes("*")) {
          c.daemon.cors.allowedOrigins=["*"];
        } else {
          const existing=(c.daemon.cors.allowedOrigins||[]).filter(o=>o!=="*");
          c.daemon.cors.allowedOrigins=[...new Set([...existing, ...desired])];
        }
      }
      fs.mkdirSync(path.dirname(f), {recursive:true});
      fs.writeFileSync(f, JSON.stringify(c,null,2)+"\n");
    ' "0.0.0.0:${PASEO_PORT:-20190}"
}

ensure_paseo_daemon() {
    if ! command -v paseo >/dev/null 2>&1; then
        echo "  paseo enabled but its CLI isn't installed (install may have been blocked) — skipping daemon start"
        return 0
    fi
    if [[ "${PASEO_MODE:-relay}" == "direct" ]]; then
        if ! configure_paseo_direct; then
            echo "  ERROR: could not set the paseo daemon password — refusing to start an"
            echo "         unauthenticated daemon on 0.0.0.0. Check ~/.paseo and PASEO_PASSWORD."
            return 0
        fi
        local want="0.0.0.0:${PASEO_PORT:-20190}"
        if _paseo_up && paseo daemon status --json 2>/dev/null | grep -q "$want"; then
            echo "  paseo daemon already running on $want (direct mode)"
            return 0
        fi
        # A daemon from a prior boot may be on the wrong address — restart it so it
        # re-reads the direct listen/relay config.
        paseo daemon stop >/dev/null 2>&1 || true
    elif _paseo_up; then
        echo "  paseo daemon already running"
        return 0
    fi
    paseo daemon start || true
    _paseo_up && { echo "  paseo daemon started"; return 0; }
    paseo daemon start || true   # warm retry — a non-zero start is not fatal
    if _paseo_up; then
        echo "  paseo daemon started (after retry)"
    else
        echo "  paseo daemon still not up after two attempts — see ~/.paseo/daemon.log"
    fi
}

# ---- serve: tunnel + ON_LAUNCH_SCRIPT (dev) + banner + idle -----------------
do_serve() {
    cd "$WORKDIR" 2>/dev/null || cd "$HOME"
    mkdir -p "${HOME}/.logs"

    if [[ "$EXPOSE_WEBAPP" == "true" ]]; then
        start_tunnel
        echo "  attach: podman exec -it --user dev $CNAME tmux attach -t tunnel"
        echo "  tail:   podman exec -it --user dev $CNAME tail -f ~/.logs/tunnel.log"
    fi

    if [[ "${INSTALL_PASEO:-false}" == "true" ]]; then
        log "ensuring the paseo daemon is up (INSTALL_PASEO=true)"
        ensure_paseo_daemon
    fi

    # VSCode connection instructions are OS- and locality-aware.
    if [[ "$HOST_OS" == "macos" ]]; then
        VSCODE_SETUP_INSTRUCTIONS="  Local-only setup (containers live in the podman machine VM):

    1. Find your podman socket path:
         podman machine inspect | python3 -c \"
import sys, json
d = json.load(sys.stdin)
print(d[0]['ConnectionInfo']['PodmanSocket']['Path'])
\"
       (typically ~/.local/share/containers/podman/machine/.../podman.sock)

    2. In VSCode settings.json:
         \"dev.containers.dockerPath\": \"podman\"

    3. Command Palette -> 'Dev Containers: Attach to Running Container...'
       -> pick '$CNAME'."
    else
        VSCODE_SETUP_INSTRUCTIONS="  If THIS host is your local machine:

    1. In VSCode settings.json:
         \"dev.containers.dockerPath\": \"podman\"

    2. Command Palette -> 'Dev Containers: Attach to Running Container...'
       -> pick '$CNAME'.

  If THIS host is a remote (Hetzner/Oracle/etc.) that you SSH into:

    1. Install the Remote-SSH extension on your laptop and connect to
       this host (F1 -> 'Remote-SSH: Connect to Host...').
    2. In the resulting remote VSCode window, install Dev Containers
       (it installs into the remote, not your laptop).
    3. F1 -> 'Dev Containers: Attach to Running Container...' ->
       pick '$CNAME'.
    See docs/remote-host.md for the full walkthrough."
    fi

    TUNNEL_BANNER=""
    if [[ "$EXPOSE_WEBAPP" == "true" ]]; then
        log "waiting for cloudflared tunnel URL (up to 30s)"
        TUNNEL_URL=$(wait_tunnel_url)
        if [[ -n "$TUNNEL_URL" ]]; then
            TUNNEL_BANNER=$(cat <<TBEOF

============================================================
  PUBLIC TUNNEL ACTIVE — webapp exposed to the internet
============================================================

  $TUNNEL_URL

  Anyone with this URL can reach your webapp; the URL is the
  only access control. Stable until the next introdus launch.
  Cached at ~/.logs/tunnel-url.txt inside the container.

  attach: podman exec -it --user dev $CNAME tmux attach -t tunnel
  log:    podman exec -it --user dev $CNAME tail -f ~/.logs/tunnel.log

============================================================
TBEOF
)
        else
            TUNNEL_BANNER=$(cat <<TBEOF

============================================================
  PUBLIC TUNNEL — URL not detected after 30s
============================================================

  Check the tunnel session:
    podman exec -it --user dev $CNAME tmux attach -t tunnel
    podman exec -it --user dev $CNAME tail -f ~/.logs/tunnel.log

============================================================
TBEOF
)
        fi
    fi

    # Claude gets its own section (with the remote-control pairing note) — but
    # only when it was actually selected, since it's now opt-out-able.
    CLAUDE_BANNER=""
    case " ${INSTALL_AGENTS-claude} " in
        *" claude "*)
            CLAUDE_BANNER=$(cat <<CBEOF

Start Claude Code (remote control is on by default — pair from
claude.ai/code or the mobile app to drive it from your phone):
  podman exec -it --user dev $CNAME run-claude
  (cds into the repo, opens the 'claude' tmux session, and runs
   claude --dangerously-skip-permissions; re-running re-attaches.
   Ctrl-a d detaches without killing it.)
CBEOF
)
            ;;
    esac

    # List the other agents the user picked (claude has its own section above)
    # and the bare command each one installs, so they know how to launch them.
    AGENTS_BANNER=""
    if [[ -f /usr/local/lib/rc-agents.sh ]]; then
        # shellcheck source=/dev/null
        source /usr/local/lib/rc-agents.sh
        _lines=""
        for _id in ${INSTALL_AGENTS-}; do
            [[ "$_id" == "claude" ]] && continue
            [[ -n "${AGENT_LABEL[$_id]:-}" ]] || continue
            _lines+=$(printf '\n    %-24s run: %s' "${AGENT_LABEL[$_id]}" "${AGENT_CMD[$_id]:-$_id}")
        done
        if [[ -n "$_lines" ]]; then
            AGENTS_BANNER=$(cat <<ABEOF

Other agents installed in this container (run inside a shell —
  podman exec -it --user dev $CNAME bash):$_lines
ABEOF
)
        fi
    fi

    NTFY_BANNER=""
    if [[ "${ENABLE_NOTIFY_SH_ALERTS:-false}" == "true" && -n "${NTFY_SH_TOPIC:-}" ]]; then
        NTFY_BANNER=$(cat <<NBEOF

============================================================
  MOBILE NOTIFICATIONS via ntfy.sh
============================================================

  You are also subscribed to mobile notifications via ntfy.sh.
  Subscribe to the following topic name to access it via the
  ntfy.sh app: $NTFY_SH_TOPIC

============================================================
NBEOF
)
    fi

    print_banner

    trap 'echo; echo "shutting down..."; exit 0' INT TERM

    # Optional per-project launch hook (runs as dev). Multi-line is fine — it's
    # executed as a script. If it blocks (e.g. a foreground dev server), the
    # container stays alive on it; if it returns, we fall through to the cat
    # blocker below.
    if [[ -n "${ON_LAUNCH_SCRIPT:-}" ]]; then
        log "running ON_LAUNCH_SCRIPT (as dev)"
        bash -c "$ON_LAUNCH_SCRIPT" || echo "  warning: ON_LAUNCH_SCRIPT exited with status $?"
        print_banner
    fi

    cat >/dev/null || true
    echo
    echo "stdin closed, shutting down..."
}

case "${1:-all}" in
    prepare)        do_prepare ;;
    serve)          do_serve ;;
    restart-tunnel) do_restart_tunnel ;;
    *)              do_prepare; do_serve ;;
esac
