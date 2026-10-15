# Stage the release binary in dist/<amd64|arm64>/tcode-headless, then build
# from the repository root. See docs/docker.md for the release-artifact layout.
FROM node:24-trixie-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bash ca-certificates curl git libssl3t64 libzstd1 openssh-client \
        procps ripgrep tini util-linux xz-utils \
    && rm -rf /var/lib/apt/lists/* \
    && groupmod --new-name tcode node \
    && usermod --login tcode --home /data/home --shell /bin/bash node
COPY crates/headless/container/agents.json crates/headless/container/install-agents.mjs /opt/tcode/
ENV HOME=/data/home \
    TCODE_DATA_DIR=/data/tcode \
    SHELL=/bin/bash \
    NPM_CONFIG_PREFIX=/data/agents/custom \
    NPM_CONFIG_CACHE=/data/agents/npm-cache \
    PATH=/data/agents/bin:/data/bin:/data/home/.local/bin:/data/agents/custom/bin:$PATH

FROM runtime AS bundled-agents
RUN NPM_CONFIG_CACHE=/tmp/npm-cache node /opt/tcode/install-agents.mjs /opt/tcode/agents \
    && rm -rf /tmp/npm-cache

FROM runtime AS slim
ARG TARGETARCH
COPY --chmod=755 dist/${TARGETARCH}/tcode-headless /usr/local/bin/tcode-headless
COPY --chmod=755 crates/headless/container/entrypoint.sh /opt/tcode/entrypoint.sh
COPY crates/headless/container/profile.sh /etc/profile.d/tcode.sh
WORKDIR /projects
VOLUME ["/data"]
EXPOSE 47420/tcp 47420/udp
ENTRYPOINT ["/usr/bin/tini", "--", "/opt/tcode/entrypoint.sh"]
CMD ["serve", "--browser-listen", "0.0.0.0:47420"]

FROM slim AS full
COPY --from=bundled-agents /opt/tcode/agents /opt/tcode/agents
