# Build from the repository root:
#   docker build -f crates/traverse-server/Dockerfile -t tcode-traverse .
# TODO: publish this image from .github/workflows/release.yml once the
# release workflow publishes container images at all; today it uploads
# archives only, and the tcode-traverse archives are built there.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build -p tcode-traverse-server --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/tcode-traverse /usr/local/bin/tcode-traverse
VOLUME /var/lib/tcode-traverse
EXPOSE 80/tcp 443/tcp 7842/udp
ENTRYPOINT ["tcode-traverse"]
CMD ["--config", "/etc/tcode-traverse/traverse.toml"]
