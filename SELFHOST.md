# Market Search — Sovereign Self-Host

Sovereign architecture for organizations that cannot accept third-party tunnel
providers (ngrok, Cloudflare, tunnelmole) in the TLS trust chain. Designed for
hedge funds, family offices, RIAs, treasury desks, and any firm running
compliance-bound research on Market Search.

The defining property: **third-party tunnel providers cannot decrypt MCP
traffic** because TLS terminates on the user's laptop, not at any provider's
edge. The gateway VPS sees only encrypted bytes plus SNI hostname / source
IP / byte counts.

This document is the architecture and engagement scope. **Eli Terminal
deploys it in your environment as a paid implementation.** Everything stays
inside your perimeter — your VPS, your domain, your DNS, your laptops, your
TLS keys. Eli Terminal's role is implementing the architecture correctly so
its security properties are actually realized in your specific stack.

For scoping: **efoltyn@eliterminal.com**

---

## Tunnel modes

| Mode | Command | Who can decrypt MCP traffic? | Provisioning |
|---|---|---|---|
| Local stdio MCP | `market-search mcp` | No public network path — nobody but you | Self-serve, OSS binary |
| Cloudflare quick tunnel | `market-search mcp share --provider cloudflare` | Cloudflare terminates public TLS | Self-serve, OSS binary |
| Tunnelmole | `market-search mcp share --provider tunnelmole` | Tunnelmole terminates public TLS | Self-serve, OSS binary |
| Ngrok with reserved subdomain | `market-search mcp share --provider ngrok --domain ...` | Ngrok terminates public TLS | Self-serve, OSS binary |
| **Sovereign self-host** | (custom deployment in your environment) | **TLS terminates on your laptop — gateway sees encrypted bytes only** | **Per-engagement implementation by Eli Terminal** |

The first four modes are appropriate for individual users running public-data
research. They ship in the open-source binary on crates.io as
`cargo install market-search`.

The fifth mode — sovereign self-host — is the architecture this document
describes. It requires gateway code, domain configuration, certificate
lifecycle setup, and per-environment hardening that Eli Terminal handles per
engagement.

---

## Why self-host

Even with ngrok or Cloudflare, your MCP traffic flows through their edge.
They terminate TLS, which means they could in principle observe the contents
of your queries and tool responses, comply with subpoenas against that data,
or be compromised in a way that exposes it.

For individual users running public-data research, that's an acceptable
trade-off. Market Search pulls public market data; the privacy story is
"hopeful disclosure of finance queries," not "sensitive personal data
leaking."

For firms running proprietary research, internal watchlists, or any work
where the security team will not sign off on a third-party tunnel provider
in the TLS trust chain, sovereign self-host is the architecture that
satisfies both the technical and compliance requirements.

---

## Architecture

```
                  Claude / ChatGPT / any HTTPS MCP client
                           │
                           │  https://device.mcp.yourdomain.com/c-<secret>/mcp
                           ▼
        ┌──────────────────────────────────────────────┐
        │  Gateway on YOUR VPS                         │
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
        │  Market Search on YOUR LAPTOP                │
        │                                              │
        │  - TLS terminates here (rustls)              │
        │  - TLS private key generated locally,        │
        │    never leaves this machine                 │
        │  - rustls-acme issues cert via TLS-ALPN-01   │
        │    (challenge traverses the SNI passthrough) │
        │  - Validates /c-<secret>/mcp path            │
        │  - Runs the MCP server                       │
        └──────────────────────────────────────────────┘
```

Operational cost (yours): ~$5/mo for a VPS (e.g. Hetzner CAX11) + ~$10/yr
for a domain. Software is AGPL-3.0.

Implementation cost (Eli Terminal): scoped per engagement. Email
**efoltyn@eliterminal.com** for a quote.

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

**Basic mitigation**: detection. Certificate Transparency logs publish every
issued cert publicly; a `cert-watcher` script can alert you within seconds.

**Compliance-locked mitigation**: prevention via CAA account binding. Add a
CAA record:

```
device.mcp.yourdomain.com.  CAA  0 issue "letsencrypt.org;accounturi=https://acme-v02.api.letsencrypt.org/acme/acct/<your-laptop-acct-id>;validationmethods=tls-alpn-01"
device.mcp.yourdomain.com.  CAA  0 issuewild ";"
```

This restricts cert issuance to your laptop-held ACME account using
TLS-ALPN-01 specifically. Even if the VPS is fully compromised, the
attacker cannot issue a replacement cert without also having the
laptop-side ACME account key.

This is the kill shot for the active-MITM threat. Adds ~5 minutes of
one-time DNS configuration per device.

---

## Engagement phases

A typical Eli Terminal in-house implementation runs across these stages.
Total elapsed time is usually 4-6 weeks depending on your environment's
DNS provider, change-management process, and security review cycle.

### Phase 0 — Architecture review + frp prototype (~1 week)

- Architecture review against your security policies
- Threat-model walkthrough with your security team
- Hetzner / your-cloud VPS provisioning
- Wildcard DNS `*.mcp.yourdomain.com` → VPS, configured in your DNS provider
- frp on the VPS doing HTTPS subdomain routing (used as scaffolding)
- Local Rust HTTPS server using `rustls-acme` against Let's Encrypt
  staging, then production
- Acceptance test: a designated laptop connects from outside your network to
  `https://d-test.mcp.yourdomain.com/c-test/mcp`, MCP request roundtrips,
  no TLS key on VPS, real Let's Encrypt production cert in the browser

### Phase 1 — Custom gateway deployment (~2 weeks)

Replace the frp scaffolding with the proper sovereign gateway:

- TCP :443 listener with TLS ClientHello parser
- SNI extraction → routing table → raw byte forwarding via QUIC
- Long-lived QUIC server using `quinn` for laptop tunnels
- Device enrollment via one-time tokens, Ed25519 public key storage
- Active session map, signed-nonce reauth on reconnect
- Confirmed: gateway never terminates TLS, never parses HTTP, never holds
  per-device TLS keys

### Phase 2 — Service install + state management (~1 week)

- Laptop-side service install (launchd on macOS, systemd `--user` on Linux,
  Windows Service on Windows)
- VPS-side gateway service install with auto-restart
- State file (`~/.eli/tunnel/state.json`) backup/restore tooling for
  laptop replacement / device migration
- ARI-aware ACME renewal (cert lifecycle automated)
- End-to-end diagnostic command for your IT support

### Phase 3 — Documentation for your security team (~1 week)

- Threat-model writeup with assumptions explicit and tied to your environment
- CAA-locked tier walkthrough with the exact DNS records for your domain
- Compliance one-pager your security team can hand to legal
- Operational runbook for your IT / DevOps team
- Optional: demo video showing the architecture for internal training

---

## State file (laptop)

Each laptop running sovereign mode persists its identity in a state file
under the user's profile (`~/.eli/tunnel/state.json` or platform equivalent):

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
keychain entries) survive. Backup/restore tooling is included in Phase 2 to
make device migration a one-command operation — critical for laptop
replacement cycles or staff turnover.

---

## Engaging Eli Terminal

If your firm needs sovereign self-host deployed in your environment, the
fastest path is:

**Email: efoltyn@eliterminal.com**

Subject: "Market Search self-host"

Helpful information for the first reply:
- Cloud or on-prem deployment? Which provider? (AWS / GCP / Azure / Hetzner / your own DC)
- Which domain would the gateway live under? Is it managed in Cloudflare / Route53 / something else?
- How many users (laptops) would be in scope at rollout?
- Compliance framework you're working under (SOC 2, ISO 27001, sector-specific, etc.)
- Timeline pressure (do you need this in 4 weeks, 4 months, or "exploring")?

Engagements are scoped per-environment. AGPL-3.0 covers the open-source
binary; in-house deployment work is contracted separately.

---

## Open-source contribution

The first four tunnel modes (ngrok, cloudflare, tunnelmole, local stdio)
ship in the open-source `market-search` crate on crates.io. The sovereign
self-host architecture described above is currently implemented as
per-engagement deployments by Eli Terminal; an OSS CLI version
(`--provider self-host` self-serve) is on the contribution roadmap.

Phase 0 is well-scoped weekend work for contributors comfortable with frp,
ACME, and Hetzner. PRs welcome on github.com/efoltyn/market-search.

---

## "Self-host" vs "sovereign"

You'll see "sovereign" in the architecture and code identifiers — that's
the security-property name (the user is sovereign over their data path).
"Self-host" is the user-facing language because it's plainer. Both refer
to the same thing.
