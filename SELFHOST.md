# Self-hosting market-search

The MCP server is stateless, so self-hosting it means running one binary.
There's no session store, no database server, and no shared mutable state, so
you don't need sticky routing either.

- **No sessions.** Every `tools/call` runs as a fresh subprocess of the same
  binary. Nothing carries over from one request to the next. Any replica can
  answer any request.
- **Speaks both MCP eras.** Clients on the stateless 2026-07-28 spec send
  their protocol version on every request and skip the handshake; the server
  answers `server/discover`, validates the mirrored `Mcp-Method`/`Mcp-Name`
  headers, and never mints a session id for them. Older clients that open with
  `initialize` (claude.ai's connector today) still get the legacy path, where
  the `Mcp-Session-Id` header just echoes back whatever the client sent,
  because some of them need one to be present.
- **No external services.** Everything the tools use is a public HTTP API
  (Yahoo, Kalshi, Polymarket, FRED, SEC, Treasury, the central banks, ...).
  The odds search index is an embedded SQLite file (compiled in, nothing to
  install), and it builds itself on first use.
- **Disk is cache only.** The binary writes rebuildable caches and an optional
  activity log under your user profile (see [Disk](#disk)). You can delete all
  of it at any time.

Clone to working server:

```bash
git clone https://github.com/efoltyn/market-search.git && cd market-search
cargo build --release --bin market-search
./target/release/market-search mcp --check
```

---

## Quickstart

### 1. Build

You need a Rust toolchain from [rustup.rs](https://rustup.rs). On Debian/Ubuntu,
also install `build-essential pkg-config libssl-dev`. macOS needs only the
Xcode command line tools.

```bash
cargo build --release --bin market-search      # binary: target/release/market-search
```

The release profile uses full LTO with one codegen unit, so the first build
is slow: 28 minutes on an Apple Silicon laptop from a clean clone. A debug
build (`cargo build --bin market-search`, binary in `target/debug/`) finishes
in about 10 minutes and serves the same tools.

No Rust? Use the prebuilt binaries on the
[releases page](https://github.com/efoltyn/market-search/releases/latest) instead.

### 2. See what your machine can serve

```bash
market-search mcp --check
```

With no configuration at all, you get:

```
market-search 0.3.0 mcp --check
  stateless: no session store; every tool call runs as an isolated subprocess
  off   finance_eia: EIA_API_KEY not set (free key: https://www.eia.gov/opendata/register.php)
  off   finance_filings, finance_insider: ELI_SEC_USER_AGENT not set (SEC EDGAR requires a contact, e.g. "Jane Doe jane@example.com"; no signup)
  basic FRED_API_KEY not set: FRED data still works keyless; finance_search uses its built-in FRED catalog
  opt   IBKR not configured: IBKR:* tickers unavailable; all other tools use free sources
  disk  activity trail + response archive at ~/.../eli/logs/activity.jsonl (grows unbounded; ELI_AUDIT_LOG=0 disables)
```

Every tool not listed there works as is. The server prints the same report
to stderr when it starts.

### 3a. Local use (stdio): Claude Code, Claude Desktop, Cursor, Codex

```bash
claude mcp add market-search -- market-search mcp
```

Any other client takes the same two fields: `command: market-search`,
`args: ["mcp"]`. Stdio has no network surface, so it needs no auth.

### 3b. Serve over HTTP for other machines

```bash
export MARKET_SEARCH_MCP_TOKEN="$(openssl rand -hex 32)"
market-search mcp --http --host 127.0.0.1 --port 8484
```

Then put TLS in front and point clients at `https://<your-host>/mcp`. A
minimal [Caddy](https://caddyserver.com) config:

```
mcp.example.com {
    reverse_proxy 127.0.0.1:8484
}
```

A client that can send headers connects with
`Authorization: Bearer $MARKET_SEARCH_MCP_TOKEN`. For Claude Code:

```bash
claude mcp add --transport http market-search https://mcp.example.com/mcp \
  --header "Authorization: Bearer <token>"
```

A client that can't send headers, like the claude.ai connector, puts the
token in the path instead: `https://mcp.example.com/mcp/<token>`.

Endpoints: `POST /mcp` (JSON-RPC; needs the bearer token when a token is
set), `POST /mcp/<token>` (the same, with the token in the path; only when a
token is set), `GET /` (health check, always open, reports nothing about your
config). `GET /mcp` and `GET /mcp/<token>` answer 405, as the MCP spec asks.

### 3c. Keep a claude.ai link up in the background (macOS, Linux)

```bash
market-search mcp service install
```

This sets up a machine that stays on as a claude.ai connector, for the web
and the phone app:

- It copies the binary to `~/.local/bin/market-search` when it runs from a
  temporary or protected place (Downloads, Desktop, Documents, a build
  folder, an npx cache, an app bundle). macOS won't start a background job
  from those folders.
- It creates a random token in `mcp-token` in the config directory. Only you
  can read the file.
- It finds ngrok, or installs it (Homebrew, else the official download into
  `~/.local/bin`), and saves your ngrok authtoken. A free ngrok account comes
  with one domain that stays the same, so your link does too.
- It writes the service file and loads it. On macOS that's
  `~/Library/LaunchAgents/com.eliterminal.market-search.plist`, which starts
  at login and restarts if it stops; the log is
  `~/Library/Logs/market-search.log`. On Linux it's a `systemd --user` unit,
  `~/.config/systemd/user/market-search.service`, logging to
  `~/.local/state/market-search/service.log`.
- It waits for the tunnel and prints `https://<your-domain>/mcp/<token>`.

Paste that link into claude.ai: Settings → Connectors → Add custom connector
→ paste → Add ([claude.ai/settings/connectors](https://claude.ai/settings/connectors)).

The service runs `market-search mcp share --service`. It serves on
127.0.0.1:8485 only, behind the token. It restarts ngrok when ngrok exits.
It also asks ngrok's local API once a minute whether the tunnel still
exists, and restarts ngrok when it doesn't; that catches an agent that lost
its session but kept running, without spending the free plan's request
quota on the public URL. On a Mac it keeps the computer awake while it's on
power (`caffeinate -s`).

| Command | What it does |
|---|---|
| `mcp service install` | Install or update the service, then print the link. `--dry-run` shows the service file and every command, and changes nothing. `--new-token` replaces the token, so the old link stops working. `--provider cloudflare` uses a quick tunnel instead, with a new link on every restart. |
| `mcp service status` | Shows the service, the local server, the tunnel, and the link. |
| `mcp service url` | Prints the link. |
| `mcp service uninstall` | Stops and removes the service. The token and the binary stay, so a reinstall brings the same link back. |

For apps and scripts, `--json` (on `install`, `status`, `url`, `uninstall`)
prints one JSON object on stdout and nothing else, and `--yes` never
prompts. When a person has to act, `install` exits 3 and the JSON says what
it needs: `"needs": "ngrok_authtoken"` or `"ngrok_install"`.

Things to know:

- A free ngrok account allows one agent online at a time. If another ngrok
  agent already runs on the machine, `install` stops and says so. `--force`
  overrides that.
- On Linux the service runs while you're logged in. To keep it running after
  you log out, run `sudo loginctl enable-linger $USER` once.
- On a Mac it starts when you log in. For a Mac that may reboot on its own,
  turn on automatic login.
- Background services start with an empty environment, so `install` copies
  your shell's `EIA_API_KEY`, `ELI_SEC_USER_AGENT`, `FRED_API_KEY`, `ELI_*`
  and `IBKR_*` settings into the service file (mode 0600 when it holds a
  key). Run `install` again after you change them, or keep them in
  `~/.config/eli/inv.toml`.

To try it without installing anything, `market-search mcp share` prints a
temporary link that works while that command runs.

---

## Environment variables

Every variable is optional.

| Variable | What it unlocks | Without it |
|---|---|---|
| `EIA_API_KEY` | `finance_eia` (US crude, gasoline, distillate, natgas storage). Free key: [eia.gov/opendata/register.php](https://www.eia.gov/opendata/register.php) | `finance_eia` returns an error naming this variable |
| `ELI_SEC_USER_AGENT` | `finance_filings`, `finance_insider`. SEC EDGAR has no key or signup, but it rejects requests without a contact User-Agent, e.g. `"Jane Doe jane@example.com"` | Both tools return an error naming this variable |
| `FRED_API_KEY` | Live search across FRED's full series catalog in `finance_search`, and the FRED release calendar in `finance_schedule` | FRED **data** still works (`finance_timeseries --tickers DGS10,UNRATE`). Search falls back to a built-in catalog of common series. |
| `IBKR_HOST`, `IBKR_PORT`, `IBKR_CLIENT_ID`, `IBKR_ACCOUNT`, `IBKR_MARKET_DATA_TYPE`, `IBKR_TIMEOUT_SECS` | `IBKR:*` tickers through your own running IB Gateway or TWS (premium, needs an IBKR account) | `IBKR:*` tickers fail. Auto-routed tickers use Yahoo. Nothing requires IBKR. |
| `MARKET_SEARCH_MCP_TOKEN` | Token auth in HTTP mode: `POST /mcp/<token>`, or `POST /mcp` with `Authorization: Bearer <token>`. Read from the environment (never a flag) so it doesn't show up in `ps`. The background service uses its own token file instead (`mcp-token` in the config directory). | HTTP mode is open to anyone who can reach the port. The server prints a warning when bound to a non-loopback address without a token. |
| `ELI_AUDIT_LOG=0` | Turns off the local activity trail | On by default (see [Disk](#disk)) |
| `ELI_AUDIT_ARCHIVE=0` | Keeps activity records but skips archiving response payloads | Payloads are archived gzipped |
| `ELI_INV_PATH` | Alternate path for the credentials file | `~/.config/eli/inv.toml` |

Instead of environment variables, you can put credentials in
`~/.config/eli/inv.toml`. Environment variables win when both are set.

```toml
[eia]
api_key = "..."

[fred]
api_key = "..."

[ibkr]
host = "127.0.0.1"
port = 4001
client_id = 7
```

To store the SEC User-Agent persistently instead:
`market-search config --set sec_user_agent --value "Jane Doe jane@example.com"`.

Kalshi and Polymarket credentials (`KALSHI_*`, `POLYMARKET_*`) are **not**
needed by any MCP tool. Only the CLI's paper-trading command uses them.

---

## What works with zero keys

We tested this on 2026-09-13 with an empty `HOME`, no credentials file, and a
stripped environment. Each tool was called through `market-search mcp` over
stdio:

| Status | Tools |
|---|---|
| **Works, no keys** (21) | `finance_timeseries` (Yahoo, FRED, Kalshi, Polymarket), `finance_odds`, `finance_rate_path`, `finance_options`, `finance_fundamentals`, `finance_movers`, `finance_curve`, `finance_search`, `finance_schedule`, `finance_auctions`, `finance_cot`, `finance_nyfed`, `finance_volsurface`, `finance_stress`, `finance_fiscal`, `finance_ecb`, `finance_bis`, `finance_boj`, `finance_boe`, `finance_short`, `finance_log` |
| **Needs a contact string, not a key** (2) | `finance_filings`, `finance_insider`: set `ELI_SEC_USER_AGENT` |
| **Needs a free key** (1) | `finance_eia`: set `EIA_API_KEY` |
| **Degraded without a key** | `finance_search`: FRED lookup uses the built-in catalog without `FRED_API_KEY` |
| **Optional premium** | `IBKR:*` tickers: need IB Gateway/TWS. Without it: `provider error: no listener on 127.0.0.1:7497`. |

On first run, `finance_odds` builds its SQLite index while it answers the
query (about 2.5s for the first search, under 1s after that). No sync step
is needed.

Both transports (stdio and streamable HTTP) were exercised: `initialize`,
`tools/list` (24 tools), `tools/call`, token rejection (401), and token
acceptance.

---

## Disk

Everything lives under your user profile. It's all rebuildable and safe to
delete.

| What | macOS | Linux |
|---|---|---|
| Odds search index (`odds/markets.db`), timeseries cache, SEC downloads | `~/Library/Caches/eli/`, `~/Library/Caches/dev.eli.eli/` | `~/.cache/eli/` |
| Activity trail + archived responses (`logs/`) | `~/Library/Application Support/eli/` | `~/.local/share/eli/` |
| Config (`config.toml`), audit chain head | `~/Library/Application Support/eli/`, `.../dev.eli.eli/` | `~/.config/eli/` |
| Large stdio responses (`eli_<tool>_<ts>.json`) | `/tmp` | `/tmp` |

Paths come from `HOME` (and the XDG variables on Linux). A container just
needs a writable `HOME`. The whole footprint after a full tool sweep was
about 4 MB.

**The activity trail grows without bound.** It's on by default and appends
one hash-chained record per tool call. It also archives every response,
gzipped and deduplicated by content. That's useful for reconstructing what
data a piece of research saw. On a long-running shared server, turn it off
(`ELI_AUDIT_LOG=0`), turn off just the payload archive
(`ELI_AUDIT_ARCHIVE=0`), or rotate the directory yourself.

Over HTTP, responses are returned inline in full, and the scratch-file
helper tool isn't reachable at all.

---

## Hosting for other people

It works, but here's what that means:

- **One IP, shared rate limits.** Every user's requests go out from your
  server's IP to Yahoo, SEC, Kalshi, and the rest. SEC enforces 10
  requests/second per IP. Yahoo throttles heavy traffic from one address
  without warning. A handful of analysts is fine. A public service for
  thousands isn't what this is built for.
- **One token, one trust boundary.** `MARKET_SEARCH_MCP_TOKEN` is a single
  shared secret. There are no per-user accounts, scopes, or quotas. For those,
  put an authenticating proxy in front (oauth2-proxy, Cloudflare Access,
  Tailscale).
- **Replicas are trivial.** Because nothing is stateful, you can run N copies
  behind any load balancer, round-robin, no sticky sessions. Each replica
  keeps its own local cache.
- **AGPL-3.0.** If you run a modified version as a network service for
  others, section 13 requires offering those users the source of your
  modified version.

---

## Honest limits

These are still hard or not built:

- **One token in the URL, no real OAuth.** The claude.ai connector has no
  header field, so its token goes in the URL path (`/mcp/<token>`). The OAuth
  endpoints are stubs that let the connector's discovery step pass; when a
  token is set they grant nothing. Anyone who has the full URL can use the
  tools, so treat it like a password, and run `mcp service install
  --new-token` if it leaks.
- **Background service on macOS and Linux only.** `mcp service install`
  covers launchd and `systemd --user`. On Windows, wrap
  `market-search mcp --http` in your own tooling (nssm, Task Scheduler, a
  container restart policy).
- **Tested on macOS only.** CI builds Linux and Windows binaries, but the
  zero-key tool sweep above ran on macOS. The Linux build dependencies listed
  in the quickstart are the standard OpenSSL requirements, not verified in a
  clean container. On Windows, large stdio responses go to the OS temp
  directory.
- **The tunnel provider sees your traffic.** `market-search mcp share` (a
  quick, temporary URL) and `mcp service install` (a URL that survives
  restarts) both go through a third-party tunnel (ngrok, Cloudflare,
  tunnelmole), and the provider terminates TLS.
- **Sovereign mode is design only.** The architecture below, where TLS
  terminates on a laptop behind a VPS gateway that can't decrypt traffic,
  doesn't exist in this build.

---

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `eia api key missing; set EIA_API_KEY ...` | Set `EIA_API_KEY` (free) in the environment the **server** runs in, not your shell. MCP clients launch stdio servers with their own env: pass it in the client config's `env` block. |
| `SEC EDGAR requires a User-Agent with contact email` | Set `ELI_SEC_USER_AGENT="Your Name you@example.com"`, or `market-search config --set sec_user_agent --value "..."`. SEC returns 403 for generic or empty User-Agents. |
| `provider error: no listener on 127.0.0.1:7497` | An `IBKR:*` ticker was requested with no IB Gateway/TWS running. Use a Yahoo ticker (`CL=F` instead of `IBKR:FUT:CL:NYMEX`) or start the gateway. |
| HTTP `401 missing or invalid bearer token` | The client isn't sending `Authorization: Bearer <MARKET_SEARCH_MCP_TOKEN>`. Check for stray whitespace or newlines in the token. A client without headers uses `/mcp/<token>` instead. |
| HTTP `401 invalid token in the URL path` | The token in `/mcp/<token>` is wrong or old. `market-search mcp service url` prints the current URL. |
| claude.ai says "not a valid MCP server" | The URL is missing `/mcp` (or `/mcp/<token>` when a token is set), or the binary is older than 0.3.0. |
| `mcp service status`: tunnel down, `ERR_NGROK_108` | Your ngrok account already has an agent online somewhere. Stop that one; the service retries on its own. |
| `mcp service status`: `ERR_NGROK_4018`, `105` or `107` | ngrok's authtoken is missing or wrong. Run `market-search mcp service install --authtoken <token>`. |
| `mcp service install`: port 8485 in use | Pick another with `--port`. |
| `GET /mcp` returns 405 | Correct. `/mcp` is POST-only per the MCP streamable-HTTP spec. Health is `GET /`. |
| `bind 127.0.0.1:8484` fails | Port in use. Pick `--port`. |
| Odds search is empty or stale | Delete the cache dir from [Disk](#disk). It rebuilds on the next query. |
| Build fails on Linux with an OpenSSL / `pkg-config` error | `apt install build-essential pkg-config libssl-dev` (or your distro's equivalents). |
| Tools missing in your client | Restart the client after adding the server, then `claude mcp list` to confirm registration. |

---

## Sovereign mode (design, not built)

Some organizations can't accept a third-party tunnel provider (ngrok,
Cloudflare, tunnelmole) in the TLS trust chain. For them, the target
architecture keeps the TLS private key on the analyst's machine. A VPS
gateway routes connections by SNI hostname and forwards encrypted bytes, and
it never terminates TLS.

```
     MCP client ── https://device.mcp.yourdomain.com/c-<secret>/mcp
                           │
        Gateway on your VPS: reads ClientHello SNI, forwards raw bytes,
        never terminates TLS, never parses HTTP, holds no device keys
                           │  QUIC tunnel (outbound from laptop)
        market-search on the laptop: TLS terminates here (rustls),
        rustls-acme issues the cert via TLS-ALPN-01 through the passthrough
```

The gateway **can** see the SNI hostname, source IP, timing, and byte counts.
It **cannot** see request or response bodies, the capability path, or the
device's TLS key. A CAA record that binds issuance to the laptop's ACME
account and `tls-alpn-01` stops a compromised gateway from obtaining a
replacement certificate:

```
device.mcp.yourdomain.com.  CAA  0 issue "letsencrypt.org;accounturi=https://acme-v02.api.letsencrypt.org/acme/acct/<laptop-acct-id>;validationmethods=tls-alpn-01"
device.mcp.yourdomain.com.  CAA  0 issuewild ";"
```

Building it needs a gateway (TCP :443 ClientHello parser, SNI routing, a QUIC
server for device tunnels, device enrollment), laptop-side ACME and a service
install, plus certificate lifecycle tooling. None of that is in this
repository. The stateless HTTP server above is the part that already exists.
Sovereign mode would wrap it rather than replace it.

For compliance-bound firms that want it deployed in their own environment,
Eli Terminal scopes that as an engagement: **efoltyn@eliterminal.com**
(subject "Market Search self-host"). Contributions toward an open-source
implementation are welcome at
[github.com/efoltyn/market-search](https://github.com/efoltyn/market-search).
