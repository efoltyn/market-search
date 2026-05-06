# Market Search — Self-Host Architecture

**Status: design phase.** The implementation (`eli-gateway` crate +
sovereign mode in the `market-search` binary) is on the roadmap below. For now,
this document describes the architecture so you can review the threat
model and contribute to the build.

If you just want to use Market Search today and don't care about
sovereignty, use `market-search mcp share --provider ngrok` (permanent, free) or
`--provider tunnelmole` (temporary, instant). See the README.

---

## Why self-host

Even with ngrok or tunnelmole, your MCP traffic flows through their
edge. They terminate TLS, which means they could in principle observe
the contents of your queries and tool responses.

For most users that's fine — Market Search pulls public market data, so
the privacy story is "hopeful disclosure of finance queries" not
"sensitive personal data leaking." Acceptable trade-off for a free
service.

For sensitive use (firm research, compliance-bound work, anything
involving non-public watchlists or proprietary signals), the only
acceptable posture is: **TLS terminates on YOUR laptop, the relay sees
only encrypted bytes.** That's what self-host gives you.

---

## Architecture

```
                  Claude / ChatGPT / any HTTPS MCP client
                           │
                           │  https://device.mcp.yourdomain.com/c-<secret>/mcp
                           ▼
        ┌──────────────────────────────────────────────┐
        │  eli-gateway on YOUR VPS                     │
        │                                              │
        │  - TCP :443 listener                         │
        │  - Reads TLS ClientHello                     │
        │  - Routes by SNI to the right tunnel         │
        │  - Forwards raw encrypted bytes              │
        │  - Does NOT terminate TLS                    │
        │  - Does NOT parse HTTP                       │
        │  - Does NOT hold per-device TLS keys         │
        └──────────────────────────────────────────────┘
                           │
                           │  QUIC tunnel (long-lived outbound from laptop)
                           ▼
        ┌──────────────────────────────────────────────┐
        │  eli on YOUR LAPTOP                          │
        │                                              │
        │  - TLS terminates here (rustls)              │
        │  - TLS private key generated locally,        │
        │    never leaves this machine                 │
        │  - rustls-acme issues cert via TLS-ALPN-01   │
        │    (challenge traverses the SNI passthrough) │
        │  - Validates /c-<secret>/mcp path            │
        │  - Runs the MCP server (existing MCP server)    │
        └──────────────────────────────────────────────┘
```

Cost: ~$5/mo for a VPS (e.g. Hetzner CAX11) + ~$10/yr for a domain.
Software is free (AGPL).

---

## Threat model

### What the gateway CAN see
- The SNI hostname of incoming TLS connections (e.g. `device.mcp.yourdomain.com`)
- TCP timing and byte counts
- Source IP of the client

### What the gateway CANNOT see
- TLS-encrypted MCP request bodies (JSON-RPC method names, tool inputs)
- Tool responses (current quotes, options chains, time series data, etc.)
- The path component of the request (`/c-<secret>/mcp`) — that's inside
  the encrypted TLS session, only the laptop sees it
- The TLS private key for the device hostname — generated and stored on
  the laptop, never transmitted

### Active certificate replacement attack
A compromised gateway operator could in principle try to obtain a fresh
certificate for `device.mcp.yourdomain.com` and start MITMing future
connections.

**Basic sovereign mitigation**: detection. Certificate Transparency logs
publish every issued cert publicly; a `cert-watcher` script can alert
you within seconds.

**Compliance-locked sovereign mitigation**: prevention via CAA account
binding. Add a CAA record:

```
device.mcp.yourdomain.com.  CAA  0 issue "letsencrypt.org;accounturi=https://acme-v02.api.letsencrypt.org/acme/acct/<your-laptop-acct-id>;validationmethods=tls-alpn-01"
device.mcp.yourdomain.com.  CAA  0 issuewild ";"
```

This restricts cert issuance to your laptop-held ACME account using
TLS-ALPN-01 specifically. Even if the VPS is fully compromised, the
attacker cannot issue a replacement cert without also having the
laptop-side ACME account key.

This is the kill shot for the active-MITM threat. Implementing it adds
~5 minutes of one-time DNS configuration per device.

---

## Roadmap

### Phase 0 — frp prototype (~1 weekend)

Goal: prove the byte path works end-to-end with a real Let's Encrypt
certificate.

- Hetzner CAX11 in Falkenstein, ~$5/mo
- Wildcard DNS `*.mcp.yourdomain.com` → VPS
- frp on the VPS doing HTTPS subdomain routing
- Local Rust HTTPS server using `rustls-acme` against Let's Encrypt
  staging, then production
- Acceptance test: phone connects to
  `https://d-test.mcp.yourdomain.com/c-test/mcp`, MCP request roundtrips,
  no TLS key on VPS, real Let's Encrypt production cert in browser

### Phase 1 — Replace frp with `eli-gateway` (~2 weekends)

Goal: own the gateway code so we control the security properties.

- New crate: `eli-gateway`
- TCP :443 listener with TLS ClientHello parser
- SNI extraction → routing table → raw byte forwarding via QUIC
- Long-lived QUIC server using `quinn` for laptop tunnels
- Device enrollment via one-time tokens, Ed25519 public key storage
- Active session map, signed-nonce reauth on reconnect
- Never terminates TLS, never parses HTTP, never holds device-hostname
  TLS keys

### Phase 2 — Permanence polish (~1 week)

Goal: make it feel like infrastructure, not a script.

- `market-search mcp share --provider self-host` boots the laptop side
- `eli-gateway init` / `enroll` / `status` for VPS-side admin
- Service install (launchd / systemd --user)
- State backup/restore (`~/.eli/tunnel/state.json` is the continuity
  guarantee — if the user loses it, the URL is gone forever)
- ARI-aware ACME renewal
- `eli doctor mcp` end-to-end diagnostic

### Phase 3 — Documentation & institutional hardening (~1 week)

Goal: make it auditable by a security team.

- Threat-model writeup with explicit assumptions
- CAA-locked tier walkthrough with exact DNS records
- Compliance one-pager firms can hand to legal
- Demo video showing the SanDisk physical-security analogy

---

## State file (laptop)

`~/.eli/tunnel/state.json` (when implemented):

```json
{
  "version": 1,
  "mode": "sovereign",
  "gateway_domain": "mcp.yourdomain.com",
  "device_id": "d-7m4k2p9q8v6x",
  "device_public_key": "...",
  "device_private_key_ref": "os-keychain:eli-tunnel-device",
  "capability_secret_ref": "os-keychain:eli-tunnel-capability",
  "tls_private_key_path": "~/.eli/tunnel/tls.key",
  "cert_chain_path": "~/.eli/tunnel/fullchain.pem",
  "acme_account_key_path": "~/.eli/tunnel/acme-account.key",
  "acme_account_uri": "https://acme-v02.api.letsencrypt.org/acme/acct/...",
  "created_at": "...",
  "last_successful_renewal": "..."
}
```

The permanent URL is stable as long as this file (and the corresponding
keychain entries) survive. `market-search mcp tunnel backup` / `restore` will
ship in Phase 2 to make this a one-command operation.

---

## What you can do today

1. **Read the architecture above** — does it match your threat model?
   File issues against this repo with concerns.
2. **If you're already running a sovereign-shaped tunnel** (your own VPS
   + cloudflared named tunnel + Cloudflare-issued cert), you have most
   of the security properties already. The thing you don't have is
   "TLS terminates on laptop" — Cloudflare does it for you, so they
   could decrypt if compelled. Self-host fixes that.
3. **Contribute** — Phase 0 is well-scoped and a single weekend's work
   for someone comfortable with frp + ACME + Hetzner. PRs welcome.

---

## Why "self-host" not "sovereign"

You'll see "sovereign" in the Phase 1+ commits and architecture talk —
that's the security-property name (the user is sovereign over their
data path). User-facing language is "self-host" because it's plainer.
Both refer to the same thing.
