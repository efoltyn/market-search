# Runs the market-search MCP server over stdio (used by Glama and other
# registries that start servers in a container). Downloads the v0.6.0 release
# binary and checks it against the pinned SHA-256 before installing it.
FROM debian:bookworm-slim

ARG VERSION=v0.6.0
ARG SHA256=25b3d59ba90bf30248a9f858c7d1b58fa2392c2b8d042bce96c5acc16f50a56e

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && curl -fsSL -o /usr/local/bin/market-search \
      "https://github.com/efoltyn/market-search/releases/download/${VERSION}/market-search-x86_64-unknown-linux-gnu" \
 && echo "${SHA256}  /usr/local/bin/market-search" | sha256sum -c - \
 && chmod +x /usr/local/bin/market-search \
 && apt-get purge -y curl && apt-get autoremove -y && rm -rf /var/lib/apt/lists/*

RUN useradd --create-home app
USER app

ENTRYPOINT ["market-search", "mcp"]
