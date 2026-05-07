# Market Search — Onboarding Recipe for Coding Agents

You (Claude Code, Codex, or any other coding agent) are reading this
because someone just asked you to set up Market Search. Walk the user
through the steps below in order. Pause where this file says PAUSE —
those are the spots that need a human action you can't do yourself.

Tell the user up front: "I'll get you a working setup in about 5
minutes. There are two parts — local tools for me (instant) and a public
URL if you want to use Market Search inside claude.ai web or ChatGPT
custom apps (~3 min for permanent, instant for temporary)."

---

## Step 1 — Verify prerequisites

Run these checks. If anything fails, print the install instruction and stop.

| Check | Command | Fix if missing |
|---|---|---|
| Rust toolchain | `which cargo` | macOS: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \| sh`<br/>Linux: same as macOS<br/>Windows: https://rustup.rs |
| git | `which git` | macOS: `xcode-select --install`<br/>Linux: `apt install git` or distro equivalent |

## Step 2 — Install the binary

```bash
cargo install market-search
```

This compiles and installs `market-search` into `~/.cargo/bin/`. It takes 1–3
minutes on first install (Rust is downloading dependencies). Watch for
"Installed package `market-search`" at the end.

Verify: `market-search --version` should print a version string.

## Step 3 — Wire local stdio MCP into the user's coding agent

This is the part that gives YOU (the agent) tool access. Without it,
Market Search only works in the user's claude.ai web/ChatGPT — not in
this Claude Code/Codex session.

### For Claude Code

Read `~/.claude/settings.json`. Find the `mcpServers` block (create it if
it doesn't exist). Add or merge:

```json
{
  "mcpServers": {
    "market-search": {
      "command": "market-search",
      "args": ["mcp"]
    }
  }
}
```

Tell the user: "I added Market Search to your Claude Code config.
**Restart Claude Code** so it picks up the new server, then come back
here and we'll continue."

PAUSE — wait for the user to confirm restart.

After restart, verify in this session by listing tools — you should see
`finance_timeseries`, `finance_odds`, etc. If you don't, the config
didn't take. Check syntax of the JSON file you edited.

### For Codex

Edit Codex's MCP config (path depends on Codex version — search for
`codex.toml` or `~/.config/codex/`). Add an equivalent entry pointing
`command` at `market-search` and `args` at `["mcp"]`.

### For Claude Desktop

Edit `~/Library/Application Support/Claude/claude_desktop_config.json`
(macOS) or `%APPDATA%\Claude\claude_desktop_config.json` (Windows).
Same JSON shape as Claude Code above.

## Step 4 — Get a public URL for claude.ai web or ChatGPT custom apps

This step is OPTIONAL. Skip if the user only uses Claude Code / Codex /
Claude Desktop locally — Step 3 was sufficient.

If they want Market Search in claude.ai web or ChatGPT, ask:

> "How do you want to connect this? Two options:
> 1. **Temporary URL (instant, 5 sec)** — best for trying it out once.
>    URL dies when you close the terminal.
> 2. **Permanent URL (free, ~3 min setup)** — best for everyday use.
>    Requires a one-time email or Google/GitHub signup at ngrok.com.
>
> (A third 'self-host' mode where TLS keys live on the user's laptop
> and a VPS gateway only routes encrypted bytes is described in
> SELFHOST.md, but it's a design spec — not implemented yet. Don't
> offer it as a setup option.)"

### If they pick option 1 (temporary):

Run this in a terminal that the user will keep open:

```bash
market-search mcp share --provider cloudflare
```

This needs the `cloudflared` binary. If missing:
- macOS: `brew install cloudflared`
- Linux: download from https://github.com/cloudflare/cloudflared/releases/latest

(Tunnelmole is also available via `--provider tunnelmole` but dies
silently after a few hours; recommend cloudflared as the default temp.)

The command prints a block with a URL ending in `/mcp`. Copy that URL
and tell the user:

- For claude.ai: Settings → Connectors → Add custom connector → paste URL → Save → toggle on in any chat
- For ChatGPT: Settings → Apps & Connectors → Create → paste URL → Save → toggle on

Tell them: "The URL stays alive while that terminal is open. If you
close it or your machine reboots, you'll need to re-run `market-search mcp share`
and paste the new URL into the connector dialog (it'll be different
each time)."

### If they pick option 2 (permanent ngrok):

First check if ngrok is installed: `which ngrok`. If missing:
- macOS: `brew install ngrok/ngrok/ngrok`
- Linux: download from https://download.ngrok.com (pick your arch)
- Windows: https://download.ngrok.com

Now open ngrok signup in their default browser:
- macOS: `open https://dashboard.ngrok.com/signup`
- Linux: `xdg-open https://dashboard.ngrok.com/signup`
- Windows: `start https://dashboard.ngrok.com/signup`

Tell them:

> "I opened ngrok signup. Sign up with Google, GitHub, or email (the
> OAuth options are fastest — no email verification step). Once you're
> in the dashboard, go to **Your Authtoken** in the left sidebar and
> copy the token. Paste it back to me."

PAUSE — wait for them to paste.

When they paste the token (it'll look like
`2abc...XYZ_ABCdef123ghi456jkl789mno`), reserve their static subdomain.

Ngrok auto-assigns each free account ONE static subdomain on
`*.ngrok-free.dev`. Find it on the dashboard's Domains page (or skip —
the next command will show it). Ask them what they want it named, or
just use the auto-assigned one.

Run:

```bash
market-search mcp share --provider ngrok --authtoken <pasted-token>
```

(Without `--domain`, ngrok uses a random URL. To pin to your reserved
subdomain, re-run with `--domain mysub.ngrok-free.dev`.)

The command prints the same paste-ready block. The URL persists across
restarts as long as you keep your ngrok account and stay under the
free-tier limits (1 GB/mo data, 20K requests/mo — typical Market Search
use is far below this).

Have them paste the URL into claude.ai or ChatGPT as described above.

### Self-host (NOT a setup option):

If the user asks about it, explain: "Self-host is a design spec, not
runnable code yet. SELFHOST.md describes a future architecture where
your laptop holds the TLS private keys and a VPS gateway only routes
encrypted bytes (so even a compromised gateway can't decrypt your MCP
traffic). It requires gateway code that doesn't exist in this build.
For sensitive work today, use `market-search mcp` over stdio locally —
no public URL means no public attack surface."

If they want to contribute, point them at SELFHOST.md and the GitHub
repo issues.

## Step 5 — Verify end-to-end

Whichever option they picked, test it:

> "Open a new chat in claude.ai (or ChatGPT). Toggle Market Search on.
> Ask: 'What's SPY trading at right now?'"

Claude/ChatGPT should call the `finance_timeseries` tool and return a
real quote with the current price. If you see "I don't have access to
real-time data" or similar, the connector isn't working — check:

1. The `market-search mcp share` terminal is still running and showing connection
   logs when claude.ai hits it
2. The URL in the connector dialog matches exactly what `market-search mcp share`
   printed (including `/mcp` at the end)
3. For ngrok: `cat ~/.config/ngrok/ngrok.yml` shows your authtoken set

## Common failure modes

- **`cargo install` fails with linker errors**: User probably needs
  build tools. macOS: Xcode CLT (`xcode-select --install`). Linux: `apt
  install build-essential` or distro equivalent.
- **MCP tools don't appear in Claude Code after editing settings.json**:
  They didn't restart Claude Code, OR the JSON file has a syntax error
  (a stray comma, mismatched braces). Read the file back and parse it
  to confirm.
- **`market-search mcp share --provider tunnelmole` errors with "npx not found"**:
  Node.js isn't installed. Just use the default `--provider cloudflare` instead
  (no Node dependency, more reliable).
- **`market-search mcp share --provider cloudflare` errors with "cloudflared not
  found"**: Install cloudflared as in option 1 above.
- **Public URL works in `curl` but not in claude.ai**: Most often the
  user pasted the URL without `/mcp` at the end, or pasted with extra
  whitespace. Re-copy from the terminal block and paste again.

## What you (the agent) should NOT do

- Do not run `market-search mcp share` yourself in a non-blocking shell — it runs
  forever and you'll wedge the session. Tell the USER to run it in a
  terminal they keep open.
- Do not edit `~/.config/ngrok/ngrok.yml` directly. Use `ngrok config
  add-authtoken` (which `market-search mcp share --provider ngrok --authtoken
  <token>` does for you).
- Do not promise that the temporary URL will keep working — it won't.
  Be explicit about persistence in every option.
