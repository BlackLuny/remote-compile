#!/usr/bin/env bash
#
# One-click rc-worker install / upgrade (§8.1).
#
# First install (needs a single-use enrollment token from the console):
#   curl -fsSL https://github.com/BlackLuny/remote-compile/releases/latest/download/worker-install.sh \
#     | sudo RC_SERVER=http://ctrl:7701 RC_ENROLLMENT_TOKEN=<token> sh
#
# Upgrade to latest GitHub Release (already enrolled — no token needed):
#   curl -fsSL https://github.com/BlackLuny/remote-compile/releases/latest/download/worker-install.sh \
#     | sudo sh
#
# Pin version / fork / direct URL:
#   sudo RC_RELEASE=v0.1.1 ./deploy/worker-install.sh
#   sudo RC_BINARY_URL=https://.../rc-worker-linux-aarch64 ./deploy/worker-install.sh
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
RC_GITHUB_REPO="${RC_GITHUB_REPO:-BlackLuny/remote-compile}"
RC_RELEASE="${RC_RELEASE:-latest}"
RC_DATA_DIR="${RC_DATA_DIR:-/var/lib/rc-worker}"
RC_USER="${RC_USER:-rc-worker}"
RC_MAX_PARALLEL="${RC_MAX_PARALLEL:-}"
INSTALL_PATH="/usr/local/bin/rc-worker"

die() { echo "error: $*" >&2; exit 1; }
note() { echo "==> $*"; }

[ "$(id -u)" = "0" ] || die "run as root (the systemd unit and docker group need it)"

host_arch() {
  case "$(uname -m)" in
    x86_64|amd64) echo x86_64 ;;
    aarch64|arm64) echo aarch64 ;;
    *) die "unsupported architecture: $(uname -m) (need x86_64 or aarch64)" ;;
  esac
}

host_os() {
  case "$(uname -s)" in
    Linux) echo linux ;;
    *) die "rc-worker requires Linux + Docker (got $(uname -s))" ;;
  esac
}

release_asset_url() {
  local name="$1"
  local tag="$RC_RELEASE"
  if [ "$tag" = "latest" ]; then
    echo "https://github.com/${RC_GITHUB_REPO}/releases/latest/download/${name}"
  else
    case "$tag" in v*) ;; *) tag="v${tag}" ;; esac
    echo "https://github.com/${RC_GITHUB_REPO}/releases/download/${tag}/${name}"
  fi
}

curl_get() {
  local url="$1" out="$2"
  local args=(-fsSL --retry 3 --retry-delay 2 -o "$out")
  if [ -n "${GITHUB_TOKEN:-${GH_TOKEN:-}}" ]; then
    args+=(-H "Authorization: Bearer ${GITHUB_TOKEN:-$GH_TOKEN}")
  fi
  curl "${args[@]}" "$url"
}

command -v docker >/dev/null || die "docker is required"
docker info >/dev/null 2>&1 || die "the docker daemon is not reachable"
command -v git >/dev/null || die "git is required for the L1 baseline layer (§4.1)"
command -v curl >/dev/null || die "curl is required to download release binaries"

ENROLLED=0
if [ -f "$RC_DATA_DIR/worker.json" ]; then
  ENROLLED=1
fi

# First install needs server + token; upgrade of an enrolled worker needs neither.
if [ "$ENROLLED" -eq 0 ]; then
  [ -n "$RC_SERVER" ] || die "set RC_SERVER, e.g. http://control-plane:7701"
  [ -n "$RC_ENROLLMENT_TOKEN" ] || die "set RC_ENROLLMENT_TOKEN (console: Workers → 生成 enrollment token)"
fi

# ---------------------------------------------------------------- binary
install_binary() {
  local arch os name url tmp sums expect got
  arch="$(host_arch)"
  os="$(host_os)"
  name="rc-worker-${os}-${arch}"
  tmp="$(mktemp)"

  if [ -n "$RC_BINARY_URL" ]; then
    url="$RC_BINARY_URL"
    note "downloading rc-worker from $url"
    curl_get "$url" "$tmp" || die "download failed: $url"
  elif [ -x "./rc-worker" ]; then
    note "installing ./rc-worker"
    install -m 0755 ./rc-worker "$INSTALL_PATH"
    return
  else
    url="$(release_asset_url "$name")"
    note "downloading $url (RC_RELEASE=$RC_RELEASE)"
    curl_get "$url" "$tmp" || die "download failed: $url
hint: set RC_BINARY_URL, place ./rc-worker next to this script, or check RC_RELEASE/RC_GITHUB_REPO"
  fi

  chmod +x "$tmp"
  "$tmp" --version >/dev/null 2>&1 || die "$tmp is not runnable on this host (wrong arch?)"

  sums="$(mktemp)"
  if [ -z "$RC_BINARY_URL" ] && curl_get "$(release_asset_url SHA256SUMS)" "$sums" 2>/dev/null; then
    expect="$(awk -v f="$name" '$2 == f { print $1; exit }' "$sums" || true)"
    if [ -n "$expect" ]; then
      got="$(sha256sum "$tmp" | awk '{print $1}')"
      [ "$got" = "$expect" ] || die "SHA256 mismatch for $name (got $got want $expect)"
      note "checksum ok ($name)"
    fi
  fi
  rm -f "$sums"

  install -m 0755 "$tmp" "$INSTALL_PATH"
  rm -f "$tmp"
}

install_binary
note "installed $($INSTALL_PATH --version 2>/dev/null || echo rc-worker) → $INSTALL_PATH"

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
if [ "$ENROLLED" -eq 0 ]; then
  note "enrolling with $RC_SERVER"
  ENROLL_ARGS="--server $RC_SERVER --token $RC_ENROLLMENT_TOKEN"
  [ -n "$RC_MAX_PARALLEL" ] && ENROLL_ARGS="$ENROLL_ARGS --max-parallel $RC_MAX_PARALLEL"
  # shellcheck disable=SC2086
  runuser -u "$RC_USER" -- "$INSTALL_PATH" --data-dir "$RC_DATA_DIR" enroll $ENROLL_ARGS
else
  note "already enrolled ($RC_DATA_DIR/worker.json exists); keeping identity, upgrading binary only"
fi

# ----------------------------------------------------------------- systemd
UNIT=/etc/systemd/system/rc-worker.service
# Keep a hand-tuned unit on upgrade (FORCE_UNIT=1 to rewrite).
if [ -f "$UNIT" ] && [ "${FORCE_UNIT:-0}" != "1" ]; then
  note "keeping existing $UNIT (FORCE_UNIT=1 to regenerate)"
else
  note "writing $UNIT"
  cat > "$UNIT" <<EOF
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
fi

systemctl daemon-reload
systemctl enable --now rc-worker
# Restart so an upgrade actually loads the new binary (enable --now is a no-op
# when the unit was already active).
systemctl restart rc-worker
note "done — $($INSTALL_PATH --version 2>/dev/null || true)"
note "follow it with: journalctl -u rc-worker -f"
note "to take this worker out of rotation gracefully, use Drain in the console (§8.1)"
