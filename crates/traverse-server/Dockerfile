# Build from the repository root:
#   docker build -f crates/traverse-server/Dockerfile -t tcode-traverse .
# Releases use --build-arg BINARY_SOURCE=release and the same Linux binaries
# as the downloadable archives, staged in dist/<amd64|arm64>/tcode-traverse.
ARG BINARY_SOURCE=source
FROM rust:1-bookworm AS source
WORKDIR /src
COPY . .
RUN cargo build -p tcode-traverse-server --release --locked \
    && cp target/release/tcode-traverse /tcode-traverse

FROM scratch AS release
ARG TARGETARCH
COPY --chmod=755 dist/${TARGETARCH}/tcode-traverse /tcode-traverse

FROM ${BINARY_SOURCE} AS binary

# Release binaries are built on Ubuntu 24.04 (glibc 2.39).
FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=binary /tcode-traverse /usr/local/bin/tcode-traverse
VOLUME /var/lib/tcode-traverse
EXPOSE 80/tcp 443/tcp 7842/udp
# The CLI's graceful shutdown is driven by tokio::signal::ctrl_c().
STOPSIGNAL SIGINT
ENTRYPOINT ["tcode-traverse"]
CMD ["--config", "/etc/tcode-traverse/traverse.toml"]
