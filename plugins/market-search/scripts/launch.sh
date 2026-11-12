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

VERSION="v0.6.0"
REPO="efoltyn/market-search"

for c in "${MARKET_SEARCH_BIN:-}" "$(command -v market-search 2>/dev/null || true)" "$HOME/.local/bin/market-search" "$HOME/.cargo/bin/market-search"; do
  if [ -n "$c" ] && [ -x "$c" ]; then exec "$c" "$@"; fi
done

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64)          T=aarch64-apple-darwin;      SUM=236e68627402ad989af821dbf00c5801940fc43558643cb39912512242a356ea ;;
  Darwin-x86_64)         T=x86_64-apple-darwin;       SUM=80d470b7d108a8ddc3f24aa050e79975df6a80ff9de2484eaefb4e59bdb0daa5 ;;
  Linux-x86_64|Linux-amd64) T=x86_64-unknown-linux-gnu; SUM=25b3d59ba90bf30248a9f858c7d1b58fa2392c2b8d042bce96c5acc16f50a56e ;;
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
