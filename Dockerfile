FROM ubuntu:26.04

# Ubuntu minimal ships no locale by default, so CLIs that gate box-drawing /
# emoji on a UTF-8 locale fall back to ASCII. C.UTF-8 is built into glibc,
# no `locales` package needed.
ENV LANG=C.UTF-8 \
    LC_ALL=C.UTF-8

# Workload runs as the non-root user `dev` (home /home/dev). The container
# *starts* as root (the firewall entrypoint needs CAP_NET_ADMIN to install nft),
# then drops to dev. The egress proxy runs as its own uid `rcproxy` so the nft
# filter can grant egress to the proxy alone, by uid.

# Disable apt's _apt sandbox user. The running container drops all caps
# (--cap-drop=ALL --security-opt=no-new-privileges), so apt can't setuid to
# _apt at runtime. apt runs as root both at build time and at runtime (via the
# proxy), so the threat model is unchanged.
RUN echo 'APT::Sandbox::User "root";' > /etc/apt/apt.conf.d/99-no-sandbox \
 && chown -R root:root /var/cache/apt /var/lib/apt

# Note: nftables + tinyproxy are the egress-filter stack; netcat-openbsd
# provides `nc -X connect` for the git-over-SSH ProxyCommand.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      bash-completion \
      ca-certificates \
      curl \
      git \
      openssh-client \
      tmux \
      ripgrep \
      fd-find \
      unzip \
      build-essential \
      sudo \
      iproute2 \
      netcat-openbsd \
      nftables \
      tinyproxy \
 && rm -rf /var/lib/apt/lists/*

# Neovim from the official prebuilt tarball, not apt: the archive's nvim trails
# what current LazyVim requires (and lags further the older the release gets),
# so the tarball keeps this independent of the base image's Ubuntu version.
RUN set -eu; arch="$(uname -m)"; \
    case "$arch" in \
      x86_64)  nvim_arch=x86_64 ;; \
      aarch64) nvim_arch=arm64  ;; \
      *) echo "unsupported arch for neovim: $arch" >&2; exit 1 ;; \
    esac; \
    curl -fsSL "https://github.com/neovim/neovim/releases/download/stable/nvim-linux-${nvim_arch}.tar.gz" \
      | tar -xz -C /opt \
 && ln -s "/opt/nvim-linux-${nvim_arch}/bin/nvim" /usr/local/bin/nvim

# tree-sitter CLI (used by LazyVim's treesitter config at first nvim startup).
RUN set -eu; arch="$(uname -m)"; \
    case "$arch" in \
      x86_64)  ts_arch=x64   ;; \
      aarch64) ts_arch=arm64 ;; \
      *) echo "unsupported arch for tree-sitter: $arch" >&2; exit 1 ;; \
    esac; \
    curl -fsSL "https://github.com/tree-sitter/tree-sitter/releases/latest/download/tree-sitter-linux-${ts_arch}.gz" \
      | gunzip > /usr/local/bin/tree-sitter \
 && chmod +x /usr/local/bin/tree-sitter

# cloudflared: optional public tunnel for the webapp port. Only invoked at
# runtime when EXPOSE_WEBAPP=true (see setup.sh). It speaks a bespoke protocol
# to the Cloudflare edge on 7844, NOT HTTP, so it cannot go through the egress
# proxy — its edge IPs are allowed directly by the nft filter instead.
RUN set -eu; arch="$(uname -m)"; \
    case "$arch" in \
      x86_64)  cf_arch=amd64 ;; \
      aarch64) cf_arch=arm64 ;; \
      *) echo "unsupported arch for cloudflared: $arch" >&2; exit 1 ;; \
    esac; \
    curl -fsSL "https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-${cf_arch}" \
      -o /usr/local/bin/cloudflared \
 && chmod +x /usr/local/bin/cloudflared

# Convenience: `tunnel-url` prints the cached cloudflared quick-tunnel URL.
RUN printf '%s\n' \
    '#!/bin/sh' \
    'if [ -s /home/dev/.logs/tunnel-url.txt ]; then' \
    '  cat /home/dev/.logs/tunnel-url.txt' \
    'else' \
    '  echo "no tunnel URL cached — is EXPOSE_WEBAPP=true and cloudflared running?" >&2' \
    '  exit 1' \
    'fi' \
    > /usr/local/bin/tunnel-url \
 && chmod +x /usr/local/bin/tunnel-url

# Remap tmux prefix C-b -> C-a so it doesn't collide with a host-side tmux.
RUN printf '%s\n' \
    'unbind C-b' \
    'set -g prefix C-a' \
    'bind C-a send-prefix' \
    > /etc/tmux.conf

# ---- users: dev (workload) + rcproxy (egress proxy) ------------------------
# Ubuntu's base image has shipped a default `ubuntu` user at uid 1000 since
# 24.04; remove it so `dev` can take 1000. Nothing references the numeric uid (we
# use the names), but a stable, conventional uid keeps the persistent volume's
# ownership sane. Tolerant of the user being absent, for a base that drops it.
RUN userdel --remove ubuntu 2>/dev/null || true \
 && useradd --create-home --uid 1000 --shell /bin/bash dev \
 && useradd --system   --uid 1001 --shell /usr/sbin/nologin rcproxy

# ---- egress proxy: runtime dirs --------------------------------------------
# The proxy config + firewall entrypoint themselves are COPY'd at the END of
# this file (after the heavy toolchain layers) so iterating on them doesn't
# invalidate the nvim/mise cache. Here we just prep the dirs.
RUN mkdir -p /etc/tinyproxy /var/log/tinyproxy /run/tinyproxy \
 && : > /etc/tinyproxy/egress-allowlist.txt \
 && chown rcproxy:rcproxy /etc/tinyproxy/egress-allowlist.txt /var/log/tinyproxy /run/tinyproxy

# Run tinyproxy from a copied path, NOT the packaged /usr/bin/tinyproxy.
# Ubuntu 24.10+ — which this 26.04 base and any modern host both are — ships
# /etc/apparmor.d/tinyproxy, a Canonical AppArmor profile attached to the binary
# PATH "/usr/bin/tinyproxy".
# AppArmor is enforced host-wide by the kernel on the exec path, so it confines
# the containerized binary too — even with the container itself unconfined and
# even under `apparmor=unconfined` (the profile transitions on exec regardless).
# That profile only permits reading /etc/tinyproxy/tinyproxy.conf, so our custom
# `Filter /etc/tinyproxy/egress-allowlist.txt` gets EACCES and the proxy never
# starts. Executing an identical binary from a path no host profile matches runs
# it unconfined inside the container — which is correct: the container's egress
# guarantee comes from the in-container nft filter + this proxy, never from the
# host's tinyproxy profile. Hardcopy (not symlink) so AppArmor sees a new path.
RUN cp /usr/bin/tinyproxy /usr/local/bin/rc-egress-proxy

# apt through the proxy at runtime (apt ignores HTTP_PROXY; needs its own conf).
# Build-time apt above already ran with direct egress and is unaffected.
RUN printf '%s\n' \
    'Acquire::http::Proxy  "http://127.0.0.1:8888";' \
    'Acquire::https::Proxy "http://127.0.0.1:8888";' \
    > /etc/apt/apt.conf.d/01proxy

# git-over-SSH through the proxy: ssh CONNECTs via the HTTP proxy (nc -X
# connect). accept-new means the first clone records the host key over that
# tunnel — we can't use ssh-keyscan here because it dials :22 directly and the
# firewall would drop it.
RUN install -d -m 700 -o dev -g dev /home/dev/.ssh \
 && printf '%s\n' \
    'Host *' \
    '    ProxyCommand nc -X connect -x 127.0.0.1:8888 %h %p' \
    '    StrictHostKeyChecking accept-new' \
    > /home/dev/.ssh/config \
 && chown dev:dev /home/dev/.ssh/config \
 && chmod 600 /home/dev/.ssh/config

# Claude Code refuses --dangerously-skip-permissions as root unless it believes
# it's in a sandbox. The workload normally runs as dev (no issue), but ad-hoc
# `podman exec` sessions default to root, so keep the opt-in for those.
ENV IS_SANDBOX=1

# ---- dev-user toolchain (mise / node / pnpm / nvim) ------------------------
# Installed under /home/dev so it lands in the project's persistent volume on
# first launch. These steps run with DIRECT build-time egress; the HTTP_PROXY
# env is set further down, AFTER all network build steps, so it only affects
# runtime (a build-time proxy var would point at a proxy that isn't running).
#
# No coding agent is baked in here — every agent (claude included) is installed
# at container setup from $INSTALL_AGENTS via install-agents, so an agent the
# user didn't pick is genuinely absent. That runtime install goes through the
# egress proxy; claude's native binary ships as an npm optionalDependency, so
# the already-whitelisted registry.npmjs.org is all it needs.
# pnpm's global bin directory IS $PNPM_HOME itself (not $PNPM_HOME/bin): that is
# where `pnpm add -g` links agent binaries, and pnpm refuses to install unless
# that exact dir is on PATH. Keep the /bin form too so a pnpm version that uses
# the older layout still resolves.
ENV PNPM_HOME="/home/dev/.local/share/pnpm"
ENV PATH="/home/dev/.local/bin:/home/dev/.local/share/mise/shims:/home/dev/.local/share/pnpm:/home/dev/.local/share/pnpm/bin:${PATH}"

USER dev
WORKDIR /home/dev

RUN curl -fsSL https://mise.jdx.dev/install.sh | sh

# node is pinned to a major (not `lts`) because the runtime proxy env below
# relies on NODE_USE_ENV_PROXY, which only exists on Node >= 24. A floating
# `node@lts` could resolve to an older line and the var would be silently
# ignored — so pin the floor here and assert it, then bump both together.
RUN mkdir -p "$PNPM_HOME" \
 && mise use -g node@24 pnpm@latest \
 && node -e 'const m=+process.versions.node.split(".")[0]; if (m < 24) { console.error(`introdus: need Node >= 24 for NODE_USE_ENV_PROXY, got ${process.version}`); process.exit(1); }'

# Seed LazyVim and pre-install plugins + treesitter parsers so the first
# interactive nvim doesn't race async installers.
RUN git clone --depth 1 https://github.com/LazyVim/starter /home/dev/.config/nvim \
 && rm -rf /home/dev/.config/nvim/.git \
 && nvim --headless "+Lazy! sync" +qa \
 && nvim --headless \
      "+Lazy! load nvim-treesitter" \
      "+TSInstallSync bash c diff html javascript jsdoc json jsonc lua luadoc markdown markdown_inline python query regex toml tsx typescript vim vimdoc xml yaml" \
      +qa

# Interactive shells get mise + pnpm on PATH and bash-completion sourced.
RUN printf '%s\n' \
    '# --- introdus ---' \
    'export PATH="/home/dev/.local/bin:$PATH"' \
    'export PNPM_HOME="/home/dev/.local/share/pnpm"' \
    'export PATH="$PNPM_HOME:$PNPM_HOME/bin:$PATH"' \
    'eval "$(/home/dev/.local/bin/mise activate bash)"' \
    '[ -f /usr/share/bash-completion/bash_completion ] && . /usr/share/bash-completion/bash_completion' \
    '# --- end introdus ---' \
    >> /home/dev/.bashrc

# ---- back to root: runtime proxy env, completions, copies ------------------
USER root

# Runtime egress proxy for proxy-aware tools (mise, pnpm, claude, curl, git over
# https, ...). Set HERE — after every network build step — so the build itself
# never tries to use a proxy that isn't listening yet. NO_PROXY keeps loopback
# and link-local direct; introdus/setup.sh extend it with INTERNAL_ALLOW_CIDRS.
ENV HTTP_PROXY="http://127.0.0.1:8888" \
    HTTPS_PROXY="http://127.0.0.1:8888" \
    http_proxy="http://127.0.0.1:8888" \
    https_proxy="http://127.0.0.1:8888" \
    NO_PROXY="localhost,127.0.0.1,::1" \
    no_proxy="localhost,127.0.0.1,::1"

# Node ignores the *_PROXY vars above unless told to honor them. This makes
# node's BUILT-IN clients (fetch/undici and node:http/https) route through the
# allowlist proxy, so a Node tool that never wired up an explicit proxy agent
# stops hitting the nft default-deny drop. Node >= 24 only (pinned above).
#
# This does NOT widen egress: proxied traffic still has to clear tinyproxy's
# default-deny hostname filter and an allowed ConnectPort. It also does not
# cover raw net.Socket use or libraries that build their own agent/dispatcher.
ENV NODE_USE_ENV_PROXY=1

# Completions for tools installed under dev (root executes dev's binaries).
RUN HOME=/home/dev /home/dev/.local/bin/mise completion bash > /etc/bash_completion.d/mise \
 && HOME=/home/dev /home/dev/.local/bin/mise exec -- pnpm completion bash > /etc/bash_completion.d/pnpm

# Claude settings/hooks (Stop / Notification -> rc-notify). Owned by dev so the
# dev-user claude can read/write them on the persistent volume.
COPY container/claude/ /home/dev/.claude/
RUN chown -R dev:dev /home/dev/.claude \
 && chmod +x /home/dev/.claude/test_notify.sh

# rc-notify: deliver a notification event to the host listener over /run/notify.
COPY container/bin/rc-notify /usr/local/bin/rc-notify
RUN chmod +x /usr/local/bin/rc-notify

# run-claude: cd into the repo, open the 'claude' tmux session, start Claude
# Code with --dangerously-skip-permissions. Self-drops to dev if run as root.
COPY container/bin/run-claude /usr/local/bin/run-claude
RUN chmod +x /usr/local/bin/run-claude

# egress-log: show hostnames the egress proxy blocked (to spot what to allowlist).
COPY container/bin/egress-log /usr/local/bin/egress-log
RUN chmod +x /usr/local/bin/egress-log

# Agent registry + installer. This is the container-side registry; the host
# wizard (introdus) keeps a hand-synced mirror in crates/introdus-core/src/agents.rs.
# install-agents reads this file and
# the $INSTALL_AGENTS env var to install the picked agents at container setup.
# Nothing is baked into the image, so an unpicked agent (claude included) stays
# absent; claude installs via pnpm --allow-build (its native binary is an npm
# optionalDependency), the others via pnpm --ignore-scripts or a vendor script.
COPY container/agents.sh /usr/local/lib/rc-agents.sh
COPY container/bin/install-agents /usr/local/bin/install-agents
RUN chmod +x /usr/local/bin/install-agents

# Egress firewall entrypoint + proxy config — COPY'd LAST so iterating on them
# doesn't invalidate the heavy nvim/mise/claude layers above. Both are also
# bind-mounted by introdus at runtime (so edits apply with no rebuild); the
# baked copies are a fallback for running the image directly.
COPY container/egress/tinyproxy.conf /etc/tinyproxy/tinyproxy.conf
COPY container/egress/firewall-entrypoint.sh /usr/local/bin/firewall-entrypoint.sh
RUN chmod +x /usr/local/bin/firewall-entrypoint.sh

# VS Code Dev Containers reads this label on "Attach to Running Container" and
# runs its server, terminals, and extensions as `dev` instead of the container's
# start user (root). Without it, attaching lands you in root with HOME=/root and
# no view of dev's tmux sessions or /home/dev ownership.
LABEL devcontainer.metadata='[{"remoteUser":"dev"}]'

# Default command. introdus overrides it with the same path explicitly; the
# entrypoint must run as root (the image's default user) to install nft, then
# it drops to dev.
CMD ["/usr/local/bin/firewall-entrypoint.sh"]
