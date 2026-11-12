# Market Search — Onboarding Recipe for Coding Agents

This file is for a coding agent (Claude Code, Codex, or any other) whose
user asks it to set up Market Search. When they do, walk them through
the steps below in order. Pause where this file says PAUSE — those are
the spots that need a human action you can't do yourself. For any other
work in this repository, the steps are reference, and the last two
sections (reporting problems, what not to do) still apply.

Tell the user up front: "There are two parts. Local tools for me (this
Claude Code / Codex session) — that's a binary install plus one setup
command. And, if you want Market Search in claude.ai on the web or your
phone, a link — one command installs a background service on a Mac or
Linux computer that stays on, and it keeps the link up across reboots."

---

## Step 1 — Verify prerequisites

The prebuilt binary needs nothing installed first. `git` and `cargo`
matter only if the user wants to compile from source (Step 2, option b).

| Check | Command | Fix if missing |
|---|---|---|
| git (optional — source build only) | `which git` | macOS: `xcode-select --install`<br/>Linux: `apt install git` or distro equivalent |
| cargo (optional — source build only) | `which cargo` | Skip unless they want to compile. macOS/Linux: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \| sh`<br/>Windows: https://rustup.rs |

Don't stop the setup if `cargo` is missing — just take the prebuilt path below.

## Step 2 — Install the binary

Try these in order. The first one that works is the one to use — only
fall through if a step isn't available.

**(a) Prebuilt binary (no Rust needed).** macOS or Linux:

```bash
curl -fsSL https://eliterminal.com/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://eliterminal.com/install.ps1 | iex
```

Both install to `~/.local/bin/market-search` (Windows:
`%LOCALAPPDATA%\market-search\market-search.exe`) and then run
`market-search setup --yes`, which already does Step 3. Read its output,
then skip to the restart in Step 3. Or grab the binary for your platform
from the latest GitHub release:
https://github.com/efoltyn/market-search/releases/latest

A person on a Mac who would rather not use a terminal can download
https://github.com/efoltyn/market-search/releases/latest/download/MarketSearch.dmg
and click Connect; it does Steps 2 to 4.

**(b) From source, if they already have Rust:**

```bash
cargo install --git https://github.com/efoltyn/market-search market-search
```

This compiles `market-search` into `~/.cargo/bin/`. First build takes a
few minutes while Cargo pulls dependencies; watch for "Installed package
`market-search`" at the end.

Verify any path with: `market-search --version` — it should print a
version string. For the claude.ai / phone link in Step 4, the binary also
needs the `mcp service` command (Step 4 says how to check).

## Step 3 — Connect it to the user's AI apps

This is the part that gives YOU (the agent) tool access. Without it,
Market Search only works in the user's claude.ai web/ChatGPT — not in
this Claude Code/Codex session.

One command finds the AI apps on this machine and adds Market Search to
each: Claude Desktop, Claude Code, Cursor, VS Code, Windsurf (Devin
Desktop), Codex, Gemini CLI, LM Studio and Zed.

Show the user what will change first. This writes nothing:

```bash
market-search setup --dry-run
```

Then do it:

```bash
market-search setup --yes
```

Your shell has no terminal, so `setup` won't ask questions; `--yes`
makes that explicit. To connect only some apps, add
`--client claude-code,cursor`. `market-search setup --list` shows what is
connected. `market-search setup --remove` takes it out again.

What it does, so you can explain it:

- It only adds or changes the entry named `market-search`. Other servers
  and settings stay as they are.
- Before it first changes a file, it saves a copy next to it as
  `<file>.market-search.bak`.
- It never rewrites a file it can't parse, such as a VS Code or Zed file
  with comments. It prints the lines to paste instead.
- If the binary runs from a temporary place (an npx cache, Downloads, a
  disk image), it copies it to `~/.local/bin/market-search` first and
  points the apps there.
- Claude Code is connected with `claude mcp add --scope user`, so it
  works in every project.

Tell the user: "Market Search is connected. **Restart Claude Code** so it
picks up the new server (and quit and reopen Claude Desktop if setup
connected it), then come back here and we'll continue."

PAUSE — wait for the user to confirm restart.

After restart, verify in this session by listing tools — you should see
`finance_timeseries`, `finance_odds`, etc. If you don't, run
`market-search setup --list` and `claude mcp list` to confirm
`market-search` is registered.

### By hand, if setup can't

If setup left an app alone, or you'd rather do it yourself, find the
binary's absolute path with `which market-search`. Use that path, not
the bare name: Claude Desktop and other apps don't see your shell's
PATH.

Claude Code:

```bash
claude mcp add --scope user market-search -- /absolute/path/to/market-search mcp
```

Claude Desktop, Cursor, Gemini CLI, LM Studio and Windsurf take this JSON
in their MCP config file:

```json
{
  "mcpServers": {
    "market-search": {
      "command": "/absolute/path/to/market-search",
      "args": ["mcp"]
    }
  }
}
```

Claude Desktop's file is
`~/Library/Application Support/Claude/claude_desktop_config.json`
(macOS) or `%APPDATA%\Claude\claude_desktop_config.json` (Windows).
Cursor's is `~/.cursor/mcp.json`.

Codex uses TOML in `~/.codex/config.toml`:

```toml
[mcp_servers.market-search]
command = "/absolute/path/to/market-search"
args = ["mcp"]
```

## Step 4 — Use it from claude.ai on the web and the phone

This step is OPTIONAL. Skip it if the user only uses Claude Code, Codex,
or Claude Desktop locally — Step 3 was enough.

claude.ai reaches tools only through a public HTTPS link. One command on a
Mac or Linux computer that stays on installs a background service that
keeps that link up, across reboots, with no terminal left open.

**Version gate.** Run `market-search mcp service --help`. If it says
`unrecognized subcommand`, the binary is too old: reinstall from Step 2.

Ask the user:

> "Do you want Market Search in claude.ai on the web and your phone? I'll
> install a small background service on this computer. It uses a free
> ngrok account so the link stays the same. Keep this computer on and
> plugged in. Or, to just try it, I can make a temporary link that lasts
> while a terminal stays open."

### The background service (recommended)

Run:

```bash
market-search mcp service install
```

It copies the binary somewhere stable if needed, creates a secret token,
installs ngrok if it's missing (Homebrew, or the official download into
`~/.local/bin`), writes a launchd job (macOS) or a `systemd --user` unit
(Linux), starts it, waits for the tunnel, and prints the link.

If ngrok has no authtoken yet, the command opens
https://dashboard.ngrok.com/get-started/your-authtoken in the browser and
exits with code 3, because it can't prompt from your shell. Tell the user:

> "I opened the ngrok page. Sign in with Google, GitHub, or email, copy
> **Your Authtoken**, and paste it here."

PAUSE — wait for them to paste. Then run:

```bash
market-search mcp service install --authtoken <pasted-token>
```

The command ends by printing a link like
`https://<name>.ngrok-free.dev/mcp/<token>`. The token in the link is the
password, so show it only to the user. Tell them:

> "Open https://claude.ai/settings/connectors, click **Add custom
> connector**, paste this link, and click **Add**. It works in the Claude
> app on your phone too."

PAUSE — wait for them to confirm it's added.

Say this about it, plainly:

- It runs in the background and comes back after a reboot. On a Mac it
  starts when they log in, and it keeps the Mac awake while it's on power.
- On Linux it runs while they're logged in. To keep it running after they
  log out, they run `sudo loginctl enable-linger $USER` once. Don't run
  sudo for them.
- `market-search mcp service status` checks it,
  `market-search mcp service url` prints the link again, and
  `market-search mcp service uninstall` stops it.

If install stops because another ngrok agent is running on the machine,
ask the user before you re-run with `--force`. A free ngrok account allows
one agent at a time, so `--force` can knock their other tunnel offline.

To preview everything without changing anything, add `--dry-run`.

### A temporary link (quick try)

Run this in a terminal the user keeps open:

```bash
market-search mcp share
```

It needs `cloudflared` (macOS: `brew install cloudflared`; Linux: download
from https://github.com/cloudflare/cloudflared/releases/latest). It prints
a link ending in `/mcp`. Paste it into claude.ai the same way. The link
dies when the terminal closes, and it changes every time.

For ChatGPT: Settings → Apps & Connectors → Create → paste the link → Save.

### Self-host (their own server, no tunnel)

If the user has a machine with a public hostname and wants no third-party
tunnel, point them at SELFHOST.md. The short version: the MCP server is
stateless, so it is one command behind their own TLS/reverse proxy:

```bash
MARKET_SEARCH_MCP_TOKEN=<secret> market-search mcp --http --host 127.0.0.1 --port 8484
```

Clients that send headers use `Authorization: Bearer <secret>`. The
claude.ai connector uses `https://<host>/mcp/<secret>`. Run
`market-search mcp --check` first; it names which optional env vars
(`EIA_API_KEY`, `ELI_SEC_USER_AGENT`, `FRED_API_KEY`, `IBKR_*`) unlock which
tools. The "sovereign" laptop-holds-the-TLS-key gateway in SELFHOST.md is a
design, not built.

## Step 5 — Verify end-to-end

For the background service, run:

```bash
market-search mcp service status
```

It should say the service is running and the tunnel is up. Then:

> "Open a new chat in claude.ai (or the phone app). Turn Market Search on
> from the tools menu. Ask: 'What's SPY trading at right now?'"

Claude should call `finance_timeseries` and return a real quote. If it says
it doesn't have real-time data, the connector isn't working. Check:

1. `market-search mcp service status` shows the tunnel up. (For the
   temporary link: the `share` terminal is still open.)
2. The link in the connector matches `market-search mcp service url`
   exactly, including the `/mcp/<token>` ending.
3. The log that `status` names, for errors.

## Common failure modes

- **`cargo install` fails with linker errors**: User probably needs
  build tools. macOS: Xcode CLT (`xcode-select --install`). Linux: `apt
  install build-essential` or distro equivalent.
- **MCP tools don't appear in Claude Code after Step 3**: They didn't
  restart Claude Code. If they restarted and it's still missing, run
  `market-search setup --list` and `claude mcp list` to confirm
  `market-search` registered; if they wired it via hand-edited JSON, the
  file likely has a syntax error (a stray comma, mismatched braces) —
  read it back and parse to confirm.
- **`mcp service install` exits with code 3**: a person has to act. The
  message says what: the ngrok authtoken (get it as above, then re-run with
  `--authtoken`), or ngrok couldn't be installed (install it from
  https://ngrok.com/download, then re-run).
- **Status shows `ERR_NGROK_108`**: the user's ngrok account has another
  agent online somewhere. They stop that one; the service retries by itself.
- **Status shows `ERR_NGROK_4018`, `105` or `107`**: the ngrok authtoken is
  missing or wrong. Re-run `install --authtoken <token>` with a fresh one.
- **claude.ai rejects the connector**: the link is missing its
  `/mcp/<token>` ending, has extra whitespace, or the binary is older than
  0.3.0. Copy it again from `market-search mcp service url`.
- **`market-search mcp share` says "cloudflared not found"**: install
  cloudflared as above, or use the background service.

## Reporting problems with Market Search

The MCP server has one tool that is not market data: `report_issue`. If a
Market Search tool errors, returns data that looks wrong or stale, or
can't do something the user needed, call it to tell the developer. Say
which tool, the arguments, what you expected and what came back. Leave
out anything private: no portfolio, positions, account details or
credentials. Then tell the user in one line that you sent it.

The note goes to the developer's inbox on tell.dev with the Market Search
version, the OS and the name of the client. A copy lands in the local
activity trail (`finance_log`). Nothing is sent unless you call the tool.

From a shell, the same thing is:

```bash
market-search feedback "what happened" --kind bug --tool finance_odds --args '{"query":"fed"}' --error "the error text"
```

`--dry-run` checks a note without sending it. People can use
https://tell.dev/f/tell_pk_sndQpXc7X_l7y9gOQNEZDocO instead. If the user
does not want this, `MARKET_SEARCH_FEEDBACK=off` in the server's
environment removes the tool and turns off the command.

## What you (the agent) should NOT do

- Do not run `market-search mcp share` yourself in a non-blocking shell — it
  runs forever and you'll wedge the session. `mcp service install` is fine:
  it returns once the link is up.
- Do not edit ngrok's config file directly. Use `--authtoken`, which runs
  `ngrok config add-authtoken` for you.
- Do not paste the connector link anywhere but to the user: not in logs,
  issues, or shared chats. The token in it is the password.
- Do not pass `--force` without asking. It can knock the user's other ngrok
  tunnel offline.
- Do not promise the temporary `share` link keeps working. It dies with its
  terminal.
