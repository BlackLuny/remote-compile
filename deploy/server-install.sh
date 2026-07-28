#!/usr/bin/env bash
#
# Install or upgrade rc-server (control plane) on Linux.
#
# First install:
#   curl -fsSL https://github.com/BlackLuny/remote-compile/releases/latest/download/server-install.sh \
#     | sudo sh
#
# Upgrade to latest GitHub Release (keeps /var/lib/rc-server data):
#   curl -fsSL https://github.com/BlackLuny/remote-compile/releases/latest/download/server-install.sh \
#     | sudo sh
#
# Pin a version / arch / fork:
#   sudo RC_RELEASE=v0.1.1 RC_GITHUB_REPO=BlackLuny/remote-compile ./deploy/server-install.sh
#
# Or install a local binary:
#   sudo ./deploy/server-install.sh          # uses ./rc-server if present
#   sudo RC_BINARY_URL=https://.../rc-server-linux-x86_64 ./deploy/server-install.sh

set -euo pipefail

RC_GITHUB_REPO="${RC_GITHUB_REPO:-BlackLuny/remote-compile}"
RC_RELEASE="${RC_RELEASE:-latest}"
RC_BINARY_URL="${RC_BINARY_URL:-}"
RC_DATA_DIR="${RC_DATA_DIR:-/var/lib/rc-server}"
RC_USER="${RC_USER:-rc-server}"
RC_HTTP_ADDR="${RC_HTTP_ADDR:-127.0.0.1:7700}"
RC_GRPC_ADDR="${RC_GRPC_ADDR:-0.0.0.0:7701}"
INSTALL_PATH="/usr/local/bin/rc-server"
SKIP_SYSTEMD="${SKIP_SYSTEMD:-0}"

die() { echo "error: $*" >&2; exit 1; }
note() { echo "==> $*"; }

[ "$(id -u)" = "0" ] || die "run as root (systemd unit and data dir need it)"

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
    *) die "rc-server install script targets Linux (got $(uname -s))" ;;
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

download_binary() {
  local arch os name url tmp sums
  arch="$(host_arch)"
  os="$(host_os)"
  name="rc-server-${os}-${arch}"
  tmp="$(mktemp)"
  if [ -n "$RC_BINARY_URL" ]; then
    url="$RC_BINARY_URL"
  elif [ -x "./rc-server" ]; then
    note "installing ./rc-server"
    install -m 0755 ./rc-server "$INSTALL_PATH"
    return
  else
    url="$(release_asset_url "$name")"
  fi
  note "downloading $url"
  curl_get "$url" "$tmp" || die "download failed: $url
hint: set RC_BINARY_URL, or place ./rc-server next to this script, or check RC_RELEASE/RC_GITHUB_REPO"
  chmod +x "$tmp"
  if ! "$tmp" --version >/dev/null 2>&1; then
    echo "---- binary probe ----" >&2
    file "$tmp" 2>/dev/null || true
    "$tmp" --version 2>&1 || true
    ldd "$tmp" 2>&1 | head -20 || true
    die "downloaded file is not a runnable rc-server for this host (glibc too old? rebuild on Ubuntu 22.04)"
  fi
  # Best-effort checksum when publishing a full release.
  sums="$(mktemp)"
  if curl_get "$(release_asset_url SHA256SUMS)" "$sums" 2>/dev/null; then
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

# ---------------------------------------------------------------- binary
download_binary
note "installed $($INSTALL_PATH --version 2>/dev/null || echo rc-server) → $INSTALL_PATH"

# ------------------------------------------------------------------ user
if ! id "$RC_USER" >/dev/null 2>&1; then
  note "creating system user $RC_USER"
  useradd --system --home-dir "$RC_DATA_DIR" --shell /usr/sbin/nologin "$RC_USER"
fi
mkdir -p "$RC_DATA_DIR"
chown -R "$RC_USER":"$RC_USER" "$RC_DATA_DIR"

# ----------------------------------------------------------------- systemd
if [ "$SKIP_SYSTEMD" = "1" ]; then
  note "SKIP_SYSTEMD=1 — binary only"
  exit 0
fi

UNIT=/etc/systemd/system/rc-server.service
# Upgrades keep an existing unit so reverse-proxy bind addresses (loopback,
# custom ports, extra Environment=) are not clobbered. Set FORCE_UNIT=1 to
# rewrite from the template below.
if [ -f "$UNIT" ] && [ "${FORCE_UNIT:-0}" != "1" ]; then
  note "keeping existing $UNIT (FORCE_UNIT=1 to regenerate)"
else
  note "writing $UNIT"
  cat > "$UNIT" <<EOF
[Unit]
Description=remote-compile control plane
After=network-online.target

[Service]
Type=simple
User=$RC_USER
Group=$RC_USER
Environment=RUST_LOG=rc_server=info
ExecStart=$INSTALL_PATH --data-dir $RC_DATA_DIR serve \\
  --http-addr $RC_HTTP_ADDR \\
  --grpc-addr $RC_GRPC_ADDR
Restart=always
RestartSec=5
StateDirectory=rc-server
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
NoNewPrivileges=true
ReadWritePaths=$RC_DATA_DIR

[Install]
WantedBy=multi-user.target
EOF
fi

systemctl daemon-reload
systemctl enable rc-server
systemctl restart rc-server
note "rc-server is running ($($INSTALL_PATH --version 2>/dev/null || true))"
if [ ! -f "$RC_DATA_DIR/rc-server.sqlite" ]; then
  note "first install: create an admin user, then agents/workers tokens:"
  echo "  sudo -u $RC_USER $INSTALL_PATH --data-dir $RC_DATA_DIR admin --username admin --password '<password>'"
  echo "  sudo -u $RC_USER $INSTALL_PATH --data-dir $RC_DATA_DIR enroll-token"
  echo "  sudo -u $RC_USER $INSTALL_PATH --data-dir $RC_DATA_DIR agent-token"
fi
note "logs: journalctl -u rc-server -f"
