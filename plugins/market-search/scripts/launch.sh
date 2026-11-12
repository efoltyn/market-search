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

VERSION="v0.6.1"
REPO="efoltyn/market-search"

for c in "${MARKET_SEARCH_BIN:-}" "$(command -v market-search 2>/dev/null || true)" "$HOME/.local/bin/market-search" "$HOME/.cargo/bin/market-search"; do
  if [ -n "$c" ] && [ -x "$c" ]; then exec "$c" "$@"; fi
done

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64)          T=aarch64-apple-darwin;      SUM=01bf2d4a3345de66192715655da8cda65d7103d99cba366bacd897e8cd8cf6ad ;;
  Darwin-x86_64)         T=x86_64-apple-darwin;       SUM=c5ef1d719ad26eaac25d8c548eb0b546d870e1c5f44e7242a153f7243f82289d ;;
  Linux-x86_64|Linux-amd64) T=x86_64-unknown-linux-gnu; SUM=fb4d89fc4fe6cf1d2534e7613e1b4550317fdf8501017b1995e83e3409caca2f ;;
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
