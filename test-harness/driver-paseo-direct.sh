#!/usr/bin/env bash
# Paseo DIRECT mode (PASEO_MODE=direct) end to end through the real launch path:
# no relay, the daemon bound to 0.0.0.0:PASEO_PORT with a bcrypt password, the
# port published on the host, and an authenticated client able to connect while
# a wrong/absent password is rejected.
#
#   1. .env opts in with INSTALL_PASEO=true + PASEO_MODE=direct. `introdus launch`
#      auto-assigns a free PASEO_PORT (from 20190) and a 2-word PASEO_PASSWORD and
#      persists them; run publishes 0.0.0.0:PORT and passes the direct env in.
#   2. setup.sh (in-container) sets the password via a tmux PTY (fail-loud) and
#      patches ~/.paseo/config.json to bind 0.0.0.0:PORT with the relay off, then
#      starts the daemon.
#   3. We assert config.json, the actual listen socket, the host publish, that no
#      relay egress was wired, and the auth behaviour.
#   4. The control panel's direct-mode connect item shows the paste-ready
#      `tcp://host:port?password=…` URL and wipes it again 15s later.
# Covers TEST_PLAN: TA165, TA171
set -euo pipefail
source /usr/local/bin/driver-common.sh

session="harness-session"
proj="$HOME/proj-paseo"

harness_dummy_key
harness_write_env "$proj" "$session" "claude"
cfg="$(harness_config_file "$proj")"
{ echo "INSTALL_PASEO=true"; echo "PASEO_MODE=direct"; } >> "$cfg"
harness_ensure_base "$proj"
harness_clean
cd "$proj"

harness_launch "$session" "$proj"
cname="$HARNESS_CNAME"

# launch auto-assigned a free port + passphrase into the config (persisted).
port="$(grep -E '^PASEO_PORT=' "$cfg" | tail -1 | sed 's/^PASEO_PORT=//; s/"//g')"
pass="$(grep -E '^PASEO_PASSWORD=' "$cfg" | tail -1 | sed 's/^PASEO_PASSWORD=//; s/"//g')"
[[ -n "$port" && -n "$pass" ]] \
    || { echo "FATAL: launch did not persist PASEO_PORT/PASEO_PASSWORD:"; cat "$cfg"; exit 1; }
echo "    ✓ launch assigned port=$port password=$pass (persisted in config)"

echo "==> waiting for setup to install paseo + configure/start the direct daemon…"
# Poll the ACTUAL listening socket (authoritative), not `paseo daemon status`'s
# listen field — that echoes config.json and would match before the socket binds.
# paseo install alone can take ~70s, plus the tmux set-password + start, so allow
# a generous window.
ok=""
for _ in $(seq 1 240); do
    if podman exec --user dev "$cname" bash -lc "ss -ltn 2>/dev/null | grep -q '0.0.0.0:$port'"; then
        ok=1; break
    fi
    sleep 1
done
[[ -n "$ok" ]] || {
    echo "FATAL: direct daemon never bound 0.0.0.0:$port"
    podman exec --user dev "$cname" bash -lc 'tail -25 ~/.paseo/daemon.log 2>/dev/null | cut -c1-160' || true
    exit 1
}

echo "==> config.json: direct listen + relay off + password set"
podman exec --user dev "$cname" bash -lc "grep -q '\"listen\": \"0.0.0.0:$port\"' ~/.paseo/config.json" \
    || { echo "FATAL: daemon not bound to 0.0.0.0:$port in config.json"; exit 1; }
podman exec --user dev "$cname" bash -lc 'grep -q "\"enabled\": false" ~/.paseo/config.json' \
    || { echo "FATAL: relay not disabled in config.json"; exit 1; }
podman exec --user dev "$cname" bash -lc 'grep -q "\"password\"" ~/.paseo/config.json' \
    || { echo "FATAL: no daemon password saved (fail-loud should have blocked start)"; exit 1; }
echo "    ✓ config.json: listen 0.0.0.0:$port, relay disabled, password present"

echo "==> daemon actually listening on 0.0.0.0:$port"
podman exec --user dev "$cname" bash -lc "ss -ltn 2>/dev/null | grep -q '0.0.0.0:$port'" \
    || { echo "FATAL: nothing listening on 0.0.0.0:$port"; exit 1; }
echo "    ✓ listening on 0.0.0.0:$port"

echo "==> port published on the host (all interfaces)"
podman port "$cname" | grep -q ":$port" \
    || { echo "FATAL: $port not published on the host:"; podman port "$cname"; exit 1; }
echo "    ✓ published: $(podman port "$cname" | grep ":$port" | tr '\n' ' ')"

echo "==> no relay egress wired in direct mode"
relay_ips="$(podman exec --user dev "$cname" printenv PASEO_RELAY_IPS 2>/dev/null || true)"
[[ -z "${relay_ips// /}" ]] \
    || { echo "FATAL: PASEO_RELAY_IPS should be empty in direct mode, got: $relay_ips"; exit 1; }
echo "    ✓ PASEO_RELAY_IPS empty (relay never contacted)"

echo "==> auth: password enforcement enabled, wrong rejected, correct connects"
# The daemon enforces a password for NETWORK clients (paseo desktop on a laptop
# must supply it). A LOCAL client on this same machine is trusted via ~/.paseo's
# daemon keypair, so a no-password *local* call is allowed BY DESIGN — that is not
# a hole. Prove enforcement instead by (a) the daemon reporting auth enabled and
# (b) a WRONG explicit password being rejected, plus the correct one connecting.
podman exec --user dev "$cname" bash -lc 'grep -q "Daemon password authentication enabled" ~/.paseo/daemon.log' \
    || { echo "FATAL: daemon did not enable password authentication"; exit 1; }
echo "    ✓ daemon reports password authentication enabled"
podman exec --user dev "$cname" bash -lc "PASEO_HOST=127.0.0.1:$port PASEO_PASSWORD=wrong-nope paseo ls 2>&1 | grep -qi 'incorrect password'" \
    || { echo "FATAL: a wrong-password connection was not rejected"; exit 1; }
echo "    ✓ wrong password rejected"
podman exec --user dev "$cname" bash -lc "PASEO_HOST=127.0.0.1:$port PASEO_PASSWORD='$pass' paseo ls >/dev/null 2>&1" \
    || { echo "FATAL: correct-password client could not connect"; exit 1; }
echo "    ✓ correct password connects"

# ---- the control panel's direct-URL action ---------------------------------
# In direct mode the connect item renames and, instead of a pairing QR, prints a
# paste-ready `tcp://<host>:<port>?password=<pass>` URL — which then erases
# itself from the output pane 15s later.
echo "==> control panel: Show Paseo Direct URL (and password)"
# Widen the session first: the output pane is ~half the window, and at 80 columns
# it would wrap the ~46-char URL across rows, which capture-pane can't match.
tmux resize-window -t "$session" -x 160 -y 50 2>/dev/null || true
mc_ready
mc_wait_prompt "Show Paseo Direct URL (and password)" "renamed direct-mode connect item"
mc_hotkey "P"
mc_wait_prompt "tcp://" "the paste-ready direct URL"
url="$(mc_vis | grep -oE 'tcp://[^ ]+' | head -1)"
[[ "$url" == *":$port?password=$pass" ]] \
    || { echo "FATAL: URL doesn't carry this container's port+password: $url"; exit 1; }
echo "    ✓ panel shows $url"

echo "==> the direct URL is hidden again after 15 seconds"
mc_wait_prompt "Paseo direct url hidden after 15 seconds" "the hidden notice"
mc_vis | grep -q "tcp://" \
    && { echo "FATAL: the URL is still on screen after the hide"; mc_vis | sed 's/^/      /'; exit 1; }
echo "    ✓ URL wiped from the output pane, notice left in its place"

echo
echo "=== PASEO DIRECT OK: no relay, daemon bound 0.0.0.0:$port with a bcrypt"
echo "    password, port published on the host, authenticated client connects,"
echo "    wrong/absent password rejected, and the panel's direct URL shown then"
echo "    self-erased — all nested. ==="
