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

VERSION="v0.6.2"
REPO="efoltyn/market-search"

for c in "${MARKET_SEARCH_BIN:-}" "$(command -v market-search 2>/dev/null || true)" "$HOME/.local/bin/market-search" "$HOME/.cargo/bin/market-search"; do
  if [ -n "$c" ] && [ -x "$c" ]; then exec "$c" "$@"; fi
done

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64)          T=aarch64-apple-darwin;      SUM=7673d2ebcbacff4ddcb18929c158e07c40df6d3a5162edebf3b90929f88972ac ;;
  Darwin-x86_64)         T=x86_64-apple-darwin;       SUM=2fc3ccfee988a844d15d5d93fccfab038d75ac2ae5703c81272a30880dbd1a77 ;;
  Linux-x86_64|Linux-amd64) T=x86_64-unknown-linux-gnu; SUM=353fbd51d88abb55f3843e90c9cfb3bd598be786548eff65ff75cef1ed450833 ;;
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
