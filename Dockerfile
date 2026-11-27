# Container image for @bitefoundry/sharepoint (stdio MCP server, CLI, and TUI).
#
# Installs the verified Linux release binary (GLIBC <= 2.28) directly from
# GitHub Releases instead of compiling Rust from source, so builds complete in
# seconds and match the published release artifact.
FROM debian:bookworm-slim

ARG SHAREPOINT_VERSION=0.4.8
ARG TARGETARCH

LABEL org.opencontainers.image.title="sharepoint" \
      org.opencontainers.image.description="Single Rust binary pairing a stdio MCP server, CLI, local HTTP API, and Ratatui TUI for Microsoft SharePoint and OneDrive over an embedded SQLite WAL + FTS5 cache." \
      org.opencontainers.image.source="https://github.com/BiteFoundry/sharepoint" \
      org.opencontainers.image.documentation="https://github.com/BiteFoundry/sharepoint#readme" \
      org.opencontainers.image.version="${SHAREPOINT_VERSION}" \
      org.opencontainers.image.licenses="MIT" \
      io.modelcontextprotocol.server.name="io.github.BiteFoundry/sharepoint"

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*

RUN set -eu; \
    arch="${TARGETARCH:-$(dpkg --print-architecture)}"; \
    case "$arch" in \
      amd64) target="x86_64-unknown-linux-gnu" ;; \
      arm64) target="aarch64-unknown-linux-gnu" ;; \
      *) echo "Unsupported architecture: $arch" >&2; exit 1 ;; \
    esac; \
    base="https://github.com/BiteFoundry/sharepoint/releases/download/v${SHAREPOINT_VERSION}"; \
    archive="sharepoint-${SHAREPOINT_VERSION}-${target}.tar.gz"; \
    cd /tmp; \
    curl -fsSLO "${base}/${archive}"; \
    curl -fsSLO "${base}/SHA256SUMS.txt"; \
    grep -F "  ${archive}" SHA256SUMS.txt > expected.sha256; \
    test "$(wc -l < expected.sha256)" -eq 1; \
    sha256sum -c expected.sha256; \
    tar -xzf "${archive}" sharepoint; \
    install -m 0755 sharepoint /usr/local/bin/sharepoint; \
    rm -f "/tmp/${archive}" /tmp/SHA256SUMS.txt /tmp/expected.sha256 /tmp/sharepoint; \
    sharepoint --version

ENV SHAREPOINT_CONFIG_DIR=/data/config \
    SHAREPOINT_DATA_DIR=/data/state

RUN mkdir -p /data/config /data/state \
 && chmod 0777 /data /data/config /data/state

WORKDIR /data

ENTRYPOINT ["sharepoint"]
CMD ["mcp"]
