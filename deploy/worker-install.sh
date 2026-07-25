#!/usr/bin/env bash
#
# One-click rc-worker install (§8.1).
#
#   curl -fsSL https://<your-host>/worker-install.sh | \
#     sudo RC_SERVER=http://ctrl:7701 RC_ENROLLMENT_TOKEN=<token> sh
#
# Uninstall:
#   sudo /usr/local/bin/rc-worker uninstall --yes && sudo rm -f /usr/local/bin/rc-worker
#
# A worker machine must be dedicated to compilation: holding the Docker socket
# is equivalent to root on this host (§7.1).

set -euo pipefail

RC_SERVER="${RC_SERVER:-}"
RC_ENROLLMENT_TOKEN="${RC_ENROLLMENT_TOKEN:-}"
RC_BINARY_URL="${RC_BINARY_URL:-}"
RC_DATA_DIR="${RC_DATA_DIR:-/var/lib/rc-worker}"
RC_USER="${RC_USER:-rc-worker}"
RC_MAX_PARALLEL="${RC_MAX_PARALLEL:-}"
# Shared sccache backend, e.g. redis://127.0.0.1:6379 from
# deploy/docker-registry.yml. Unset means a machine-local cache.
RC_SCCACHE_REDIS="${RC_SCCACHE_REDIS:-}"
INSTALL_PATH="/usr/local/bin/rc-worker"

die() { echo "error: $*" >&2; exit 1; }
note() { echo "==> $*"; }

[ "$(id -u)" = "0" ] || die "run as root (the systemd unit and docker group need it)"
[ -n "$RC_SERVER" ] || die "set RC_SERVER, e.g. http://control-plane:7701"
[ -n "$RC_ENROLLMENT_TOKEN" ] || die "set RC_ENROLLMENT_TOKEN (generate one in the console: Workers → 生成 enrollment token)"

command -v docker >/dev/null || die "docker is required"
docker info >/dev/null 2>&1 || die "the docker daemon is not reachable"
command -v git >/dev/null || die "git is required for the L1 baseline layer (§4.1)"

# ---------------------------------------------------------------- binary
if [ -n "$RC_BINARY_URL" ]; then
  note "downloading rc-worker from $RC_BINARY_URL"
  curl -fsSL "$RC_BINARY_URL" -o "$INSTALL_PATH.new"
  chmod +x "$INSTALL_PATH.new"
  mv "$INSTALL_PATH.new" "$INSTALL_PATH"
elif [ -x "./rc-worker" ]; then
  note "installing ./rc-worker"
  install -m 0755 ./rc-worker "$INSTALL_PATH"
elif [ -x "$INSTALL_PATH" ]; then
  note "reusing the rc-worker already at $INSTALL_PATH"
else
  die "no binary: set RC_BINARY_URL or run this from a directory containing ./rc-worker"
fi
"$INSTALL_PATH" --version >/dev/null || die "$INSTALL_PATH is not runnable on this host"

# ------------------------------------------------------------------ user
if ! id "$RC_USER" >/dev/null 2>&1; then
  note "creating system user $RC_USER"
  useradd --system --home-dir "$RC_DATA_DIR" --shell /usr/sbin/nologin "$RC_USER"
fi
# Needed to talk to the daemon; this is also why the machine must be dedicated.
usermod -aG docker "$RC_USER" 2>/dev/null || true
mkdir -p "$RC_DATA_DIR"
chown -R "$RC_USER":"$RC_USER" "$RC_DATA_DIR"

# --------------------------------------------------------------- sccache
# Dependencies ride sccache; local crates ride the per-worktree target volume
# (§7.2). The daemon lives on the host so the remote-cache credentials never
# enter an untrusted build container.
#
# rc-worker only mounts the socket into build containers; it does not run the
# daemon, so nothing listens on that socket unless something starts it. Give it
# a unit, or the wrapper is stripped from every build and the cache is silently
# dead.
if ! command -v sccache >/dev/null; then
  note "sccache not found — builds will run without the shared compilation cache"
  note "  install it with: cargo install sccache   (or your distro package)"
else
  note "writing /etc/systemd/system/rc-sccache.service"
  mkdir -p "$RC_DATA_DIR/sccache"
  chown -R "$RC_USER":"$RC_USER" "$RC_DATA_DIR/sccache"
  cat > /etc/systemd/system/rc-sccache.service <<EOF
[Unit]
Description=sccache server for rc-worker
After=network-online.target docker.service

[Service]
Type=simple
User=$RC_USER
Group=$RC_USER
# Build containers drop every capability (§7.1), so their root cannot bypass
# file permissions the way root normally does. The socket has to be reachable
# outright or every compile dies on "sccache: Permission denied".
UMask=0000
Environment=SCCACHE_SERVER_UDS=$RC_DATA_DIR/sccache/sccache.sock
${RC_SCCACHE_REDIS:+Environment=SCCACHE_REDIS_ENDPOINT=$RC_SCCACHE_REDIS}
Environment=SCCACHE_START_SERVER=1
Environment=SCCACHE_NO_DAEMON=1
Environment=SCCACHE_IDLE_TIMEOUT=0
ExecStart=$(command -v sccache)
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF
  systemctl daemon-reload
  systemctl enable --now rc-sccache
  [ -n "${RC_SCCACHE_REDIS:-}" ] || note "  no RC_SCCACHE_REDIS set — the cache is local to this machine"
fi

# ------------------------------------------------------------------ enroll
if [ ! -f "$RC_DATA_DIR/worker.json" ]; then
  note "enrolling with $RC_SERVER"
  ENROLL_ARGS="--server $RC_SERVER --token $RC_ENROLLMENT_TOKEN"
  [ -n "$RC_MAX_PARALLEL" ] && ENROLL_ARGS="$ENROLL_ARGS --max-parallel $RC_MAX_PARALLEL"
  # shellcheck disable=SC2086
  runuser -u "$RC_USER" -- "$INSTALL_PATH" --data-dir "$RC_DATA_DIR" enroll $ENROLL_ARGS
else
  note "already enrolled ($RC_DATA_DIR/worker.json exists); skipping"
fi

# ----------------------------------------------------------------- systemd
note "writing /etc/systemd/system/rc-worker.service"
cat > /etc/systemd/system/rc-worker.service <<EOF
[Unit]
Description=remote-compile build worker
After=docker.service network-online.target
Requires=docker.service

[Service]
Type=simple
User=$RC_USER
Group=$RC_USER
SupplementaryGroups=docker
Environment=RUST_LOG=rc_worker=info
Environment=SCCACHE_SERVER_UDS=$RC_DATA_DIR/sccache/sccache.sock
ExecStart=$INSTALL_PATH --data-dir $RC_DATA_DIR run
Restart=always
RestartSec=5
# Builds are memory hungry by nature; let the container limits do the capping
# rather than the unit, but never let a runaway worker take the box down.
OOMPolicy=continue
TimeoutStopSec=600
KillSignal=SIGINT

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable --now rc-worker
note "done — follow it with: journalctl -u rc-worker -f"
note "to take this worker out of rotation gracefully, use Drain in the console (§8.1)"
