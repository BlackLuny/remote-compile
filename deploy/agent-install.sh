#!/usr/bin/env bash
#
# Install or upgrade rc-agent on the current machine (no root required).
#
#   curl -fsSL https://github.com/BlackLuny/remote-compile/releases/latest/download/agent-install.sh | sh
#
# Pin version / arch / install dir:
#   RC_RELEASE=v0.1.1 RC_INSTALL_DIR=~/.local/bin ./deploy/agent-install.sh
#
# Then point your coding agent MCP config at the binary and run:
#   rc-agent configure --server http://<control-plane>:7701 --token <agent-token>

set -euo pipefail

RC_GITHUB_REPO="${RC_GITHUB_REPO:-BlackLuny/remote-compile}"
RC_RELEASE="${RC_RELEASE:-latest}"
RC_BINARY_URL="${RC_BINARY_URL:-}"
RC_INSTALL_DIR="${RC_INSTALL_DIR:-${HOME}/.local/bin}"
INSTALL_PATH="${RC_INSTALL_DIR}/rc-agent"

die() { echo "error: $*" >&2; exit 1; }
note() { echo "==> $*"; }

host_arch() {
  case "$(uname -m)" in
    x86_64|amd64) echo x86_64 ;;
    aarch64|arm64) echo aarch64 ;;
    *) die "unsupported architecture: $(uname -m)" ;;
  esac
}

host_os() {
  case "$(uname -s)" in
    Linux) echo linux ;;
    Darwin) echo darwin ;;
    *) die "unsupported OS: $(uname -s)" ;;
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

command -v curl >/dev/null || die "curl is required"
mkdir -p "$RC_INSTALL_DIR"

arch="$(host_arch)"
os="$(host_os)"
name="rc-agent-${os}-${arch}"
tmp="$(mktemp)"

if [ -n "$RC_BINARY_URL" ]; then
  url="$RC_BINARY_URL"
elif [ -x "./rc-agent" ]; then
  note "installing ./rc-agent → $INSTALL_PATH"
  install -m 0755 ./rc-agent "$INSTALL_PATH"
  note "installed $($INSTALL_PATH --version 2>/dev/null || true)"
  note "ensure $RC_INSTALL_DIR is on PATH"
  exit 0
else
  url="$(release_asset_url "$name")"
fi

note "downloading $url"
curl_get "$url" "$tmp" || die "download failed: $url"
chmod +x "$tmp"
"$tmp" --version >/dev/null 2>&1 || die "downloaded file is not a runnable rc-agent for this host"

sums="$(mktemp)"
if curl_get "$(release_asset_url SHA256SUMS)" "$sums" 2>/dev/null; then
  expect="$(awk -v f="$name" '$2 == f { print $1; exit }' "$sums" || true)"
  if [ -n "$expect" ]; then
    got="$(sha256sum "$tmp" 2>/dev/null | awk '{print $1}' || shasum -a 256 "$tmp" | awk '{print $1}')"
    [ "$got" = "$expect" ] || die "SHA256 mismatch for $name"
    note "checksum ok"
  fi
fi
rm -f "$sums"

install -m 0755 "$tmp" "$INSTALL_PATH"
rm -f "$tmp"
note "installed $($INSTALL_PATH --version 2>/dev/null || echo rc-agent) → $INSTALL_PATH"
case ":$PATH:" in
  *":$RC_INSTALL_DIR:"*) ;;
  *) note "add to PATH: export PATH=\"$RC_INSTALL_DIR:\$PATH\"" ;;
esac
note "configure once: rc-agent configure --server http://<ctrl>:7701 --token <agent-token>"
