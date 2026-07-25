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
# The shared compilation cache (§7.2) is off, and this script deliberately does
# not set up a daemon for it.
#
# As designed, the sccache *server* runs here on the host and build containers
# reach it over a mounted unix socket, so that remote-cache credentials never
# enter untrusted build code. But the server is the half that invokes the
# compiler, and the path it gets handed lives inside the container image — this
# host does not have that toolchain, so every compile dies on a dropped
# connection. Running a daemon would only make rc-worker try to use it.
#
# Local crates still ride the per-worktree target volume, which is where most of
# the win is. rc-worker leaves the wrapper off unless RC_ENABLE_SCCACHE=1.
note "shared sccache is off by design; per-worktree target volumes still cache local crates"

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
