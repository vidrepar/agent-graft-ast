#!/bin/sh
# Install agent-graft-ast from the latest GitHub release.
# Usage: curl -fsSL https://raw.githubusercontent.com/vidrepar/agent-graft-ast/main/install.sh | sh
# Env: AGENT_GRAFT_INSTALL_DIR (default: $HOME/.local/bin)
set -eu

REPO="vidrepar/agent-graft-ast"
BIN="agent-graft-ast"
INSTALL_DIR="${AGENT_GRAFT_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

case "$(uname -s)" in
  Linux) os="unknown-linux-gnu" ;;
  Darwin) os="apple-darwin" ;;
  *) die "unsupported OS: $(uname -s)" ;;
esac

case "$(uname -m)" in
  x86_64 | amd64) arch="x86_64" ;;
  aarch64 | arm64) arch="aarch64" ;;
  *) die "unsupported architecture: $(uname -m)" ;;
esac

TARGET="$arch-$os"
ASSET="$BIN-$TARGET.tar.gz"
URL="https://github.com/$REPO/releases/latest/download/$ASSET"

TMP="$(mktemp -d)"
trap 'rm -rf "${TMP:?}"' EXIT INT TERM

fetch() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL -o "$2" "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "$2" "$1"
  else
    die "need curl or wget"
  fi
}

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    die "need sha256sum or shasum to verify the download"
  fi
}

from_source() {
  say "No prebuilt release for $TARGET; building from source."
  if command -v cargo >/dev/null 2>&1; then
    cargo install --git "https://github.com/$REPO" --locked
    say "Installed $BIN with cargo (into ~/.cargo/bin)."
    exit 0
  fi
  die "cargo not found. Install Rust from https://rustup.rs, then re-run this script."
}

say "Downloading $ASSET"
if ! fetch "$URL" "$TMP/$ASSET" 2>/dev/null || ! fetch "$URL.sha256" "$TMP/$ASSET.sha256" 2>/dev/null; then
  from_source
fi

expected="$(awk '{print $1}' "$TMP/$ASSET.sha256")"
actual="$(sha256 "$TMP/$ASSET")"
[ -n "$expected" ] && [ "$expected" = "$actual" ] || die "checksum mismatch for $ASSET (expected $expected, got $actual)"

tar -xzf "$TMP/$ASSET" -C "$TMP"
mkdir -p "$INSTALL_DIR"
if command -v install >/dev/null 2>&1; then
  install -m 755 "$TMP/$BIN" "$INSTALL_DIR/$BIN"
else
  cp "$TMP/$BIN" "$INSTALL_DIR/$BIN" && chmod 755 "$INSTALL_DIR/$BIN"
fi

say "Installed $BIN to $INSTALL_DIR/$BIN"
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) say "Note: $INSTALL_DIR is not on your PATH. Add this to your shell profile:"
     say "  export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
esac
