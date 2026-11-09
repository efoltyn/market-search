#!/bin/sh
# Starts the market-search MCP server for the Claude / Codex plugin.
#
# 1. If a market-search binary is already installed (PATH or ~/.local/bin), use it.
# 2. Otherwise download the pinned release binary for this OS/arch from GitHub,
#    verify it against the SHA-256 pinned below, cache it, and run it.
#
# Nothing else is fetched or executed. Data requests go from your machine
# straight to the public sources (Kalshi, Polymarket, SEC, FRED, ...).
set -eu

VERSION="v0.5.0"
REPO="efoltyn/market-search"

for c in "${MARKET_SEARCH_BIN:-}" "$(command -v market-search 2>/dev/null || true)" "$HOME/.local/bin/market-search" "$HOME/.cargo/bin/market-search"; do
  if [ -n "$c" ] && [ -x "$c" ]; then exec "$c" "$@"; fi
done

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64)          T=aarch64-apple-darwin;      SUM=8d2336d34f1b3531a7bfd11db1a65442ab976f074f661dd06bb6f8cbb4b4ff64 ;;
  Darwin-x86_64)         T=x86_64-apple-darwin;       SUM=9fd9904daa3f61c14f88989658ea8d0b03276a2b8279397a75d79373ff3d1434 ;;
  Linux-x86_64|Linux-amd64) T=x86_64-unknown-linux-gnu; SUM=98b7ac55324f6989372d15917c3c8ee399c079c2bb673d9bb87436bbb5f7780d ;;
  *) echo "market-search: no prebuilt binary for $(uname -s)-$(uname -m). Install with: cargo install market-search" >&2; exit 1 ;;
esac

DIR="${XDG_CACHE_HOME:-$HOME/.cache}/market-search/$VERSION"
BIN="$DIR/market-search"
if [ ! -x "$BIN" ]; then
  mkdir -p "$DIR"
  tmp="$DIR/.download.$$"
  trap 'rm -f "$tmp"' EXIT INT TERM
  URL="https://github.com/$REPO/releases/download/$VERSION/market-search-$T"
  echo "market-search: first run, downloading $VERSION for $T" >&2
  if command -v curl >/dev/null 2>&1; then curl -fsSL "$URL" -o "$tmp"; else wget -qO "$tmp" "$URL"; fi
  if command -v sha256sum >/dev/null 2>&1; then got=$(sha256sum "$tmp" | awk '{print $1}'); else got=$(shasum -a 256 "$tmp" | awk '{print $1}'); fi
  if [ "$got" != "$SUM" ]; then echo "market-search: checksum mismatch for $URL, refusing to run it" >&2; exit 1; fi
  chmod +x "$tmp" && mv "$tmp" "$BIN"
  trap - EXIT INT TERM
fi
exec "$BIN" "$@"
