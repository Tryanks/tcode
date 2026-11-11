# The Linux x64 / clippy job's machine, for running its Clippy from another
# platform. The packages are the workflow's own list (ci.yml, "Install Linux
# system dependencies") less the test-only xvfb; keep the two the same.
FROM ubuntu:24.04
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl git \
    build-essential clang cmake pkg-config libssl-dev libzstd-dev \
    libasound2-dev libfontconfig-dev libwayland-dev libxkbcommon-dev \
    libxkbcommon-x11-dev libx11-xcb-dev libxcb1-dev libgbm-dev \
    libvulkan-dev libwebkit2gtk-4.1-dev libgtk-3-dev \
 && rm -rf /var/lib/apt/lists/*
ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:$PATH
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable --component clippy
ENV CARGO_TARGET_DIR=/target CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0
WORKDIR /src
