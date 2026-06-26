# Market Search — Onboarding Recipe for Coding Agents

You (Claude Code, Codex, or any other coding agent) are reading this
because someone just asked you to set up Market Search. Walk the user
through the steps below in order. Pause where this file says PAUSE —
those are the spots that need a human action you can't do yourself.

Tell the user up front: "There are two parts. Local tools for me (this
Claude Code / Codex session) — that's a binary install plus one config
line. And, if you want Market Search inside claude.ai web or ChatGPT, a
public URL — that part needs the v0.3.0 binary and a terminal that stays
open, because the link only lives while the process runs."

---

## Step 1 — Verify prerequisites

Only one thing is required: git. The prebuilt binary and the `npx` path
need no Rust toolchain — `cargo` matters only if the user wants to
compile from source (Step 2, option b).

| Check | Command | Fix if missing |
|---|---|---|
| git (required) | `which git` | macOS: `xcode-select --install`<br/>Linux: `apt install git` or distro equivalent |
| cargo (optional — source build only) | `which cargo` | Skip unless they want to compile. macOS/Linux: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \| sh`<br/>Windows: https://rustup.rs |

Don't stop the setup if `cargo` is missing — just take the prebuilt path below.

## Step 2 — Install the binary

Try these in order. The first one that works is the one to use — only
fall through if a step isn't available.

**(a) Prebuilt binary (no Rust needed):**

```bash
curl -fsSL https://eliterminal.com/install.sh | sh
```

Or grab the binary for your platform straight from the latest GitHub
release: https://github.com/efoltyn/market-search/releases/latest —
download, `chmod +x`, and move it onto your `PATH`.

**(b) From source, if they already have Rust:**

```bash
cargo install market-search
```

This compiles `market-search` into `~/.cargo/bin/`. First build takes a
few minutes while Cargo pulls dependencies; watch for "Installed package
`market-search`" at the end.

**(c) Via npx (once published to npm):**

```bash
npx -y market-search mcp
```

Verify any path with: `market-search --version` — it should print a
version string. For the claude.ai / phone URL in Step 4, that version
must be **0.3.0 or newer** (see the note there).

## Step 3 — Wire local stdio MCP into the user's coding agent

This is the part that gives YOU (the agent) tool access. Without it,
Market Search only works in the user's claude.ai web/ChatGPT — not in
this Claude Code/Codex session.

### For Claude Code

One line (current 2026 syntax) — `market-search` is on `PATH` from Step 2:

```bash
claude mcp add market-search -- market-search mcp
```

Add `--scope user` to make it available in every project, not just this
directory:

```bash
claude mcp add --scope user market-search -- market-search mcp
```

If you'd rather edit config by hand, the equivalent entry in
`~/.claude/settings.json` (or any `.mcp.json`) is:

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

Tell the user: "Market Search is in your Claude Code config now.
**Restart Claude Code** so it picks up the new server, then come back
here and we'll continue."

PAUSE — wait for the user to confirm restart.

After restart, verify in this session by listing tools — you should see
`finance_timeseries`, `finance_odds`, etc. If you don't, the config
didn't take: run `claude mcp list` to confirm `market-search` is
registered, and if you edited JSON by hand, re-read it and check for a
syntax error (stray comma, mismatched braces).

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

**Version gate — check this first.** The claude.ai connector handshake
(OAuth and the `Mcp-Session-Id` header) was only fixed in **0.3.0**. An
older binary — including one from an earlier `cargo install` — gets
rejected by claude.ai. Run `market-search --version`; if it's below
0.3.0, reinstall from Step 2 before going further.

**Persistence reality — say this to the user before they pick.** There
is no background keep-alive service yet. The public link is alive only
while the `market-search mcp share` process is running. Close the
terminal, log out, or reboot, and the link goes dead. For an always-on
phone connector, run it on a machine that stays on (and keep the process
up — e.g. `nohup market-search mcp share … &`). What ngrok adds is a
permanent *address* you can reserve; it does not keep the *process*
alive for you.

If they want Market Search in claude.ai web or ChatGPT, ask:

> "How do you want to connect this? Two options, and note neither runs
> on its own — the link lives only while the share process is running:
> 1. **Temporary URL (instant)** — good for trying it once. A fresh
>    random URL each time; gone when the terminal closes.
> 2. **Reserved address (free ngrok account)** — same URL every run, so
>    you don't re-paste into the connector. Needs a one-time
>    Google/GitHub/email signup at ngrok.com. Still only live while the
>    process runs.
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

### If they pick option 2 (reserved ngrok address):

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
market-search mcp share --provider ngrok --authtoken <pasted-token> --domain mysub.ngrok-free.dev
```

Use the reserved subdomain from their dashboard's Domains page in place
of `mysub`. Without `--domain`, ngrok hands out a random URL that
changes each run — defeating the point of signing up.

The command prints the same paste-ready block. The *address* stays the
same across restarts as long as they keep the ngrok account and stay
under the free-tier limits (1 GB/mo data, 20K requests/mo — typical
Market Search use is well below this). But the connector only reaches a
live process: when this command isn't running, the URL is dead. Tell
them plainly: "Same URL every time, so you paste it into the connector
once. It only answers while this `share` command is running, so keep it
up on an always-on box (or `nohup … &`) if you want it on your phone
later."

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
- **MCP tools don't appear in Claude Code after Step 3**: They didn't
  restart Claude Code. If they restarted and it's still missing, run
  `claude mcp list` to confirm `market-search` registered; if they wired
  it via hand-edited JSON, the file likely has a syntax error (a stray
  comma, mismatched braces) — read it back and parse to confirm.
- **claude.ai rejects the connector / OAuth fails**: The binary is older
  than 0.3.0. `market-search --version`, then reinstall from Step 2. A
  binary from an earlier `cargo install` is the usual culprit.
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
- Do not imply the phone/claude.ai connector survives a reboot or a
  closed terminal. There is no background keep-alive service yet — even
  the reserved ngrok address only answers while the `share` process is
  up. Don't sell turnkey phone persistence.
