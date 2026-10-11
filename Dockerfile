# Rust 1.88 is required by the locked Askama dependencies.
ARG RUST_VERSION=1.90.0
ARG NODE_VERSION=24

FROM rust:${RUST_VERSION}-bookworm AS rust-tools
RUN rustup component add rustfmt clippy

FROM rust-tools AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock helm.example.toml ./
COPY src/ src/
COPY assets/ assets/
COPY templates/ templates/
COPY migrations/ migrations/
RUN cargo build --release --locked

FROM node:${NODE_VERSION}-bookworm-slim AS node

FROM debian:bookworm-slim AS runtime
# Add the target project's system tools here, e.g. python3 python3-venv.
# Build tools and the Rust toolchain below let agents work on Helm itself.
ARG PROJECT_PACKAGES="build-essential pkg-config"
ARG CLAUDE_CODE_VERSION=2.1.296
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        git ca-certificates openssh-client gh tini curl sqlite3 ${PROJECT_PACKAGES} \
    && rm -rf /var/lib/apt/lists/*
COPY --from=node /usr/local/ /usr/local/
RUN npm install --global @anthropic-ai/claude-code@${CLAUDE_CODE_VERSION} \
    && npm cache clean --force

# Replace these two copies with the toolchain of your project if it is not Rust.
COPY --from=rust-tools /usr/local/cargo/bin/ /opt/rust/bin/
COPY --from=rust-tools /usr/local/rustup/ /opt/rustup/
ENV HOME=/home/helm \
    CARGO_HOME=/home/helm/.cargo \
    RUSTUP_HOME=/opt/rustup \
    PATH=/opt/rust/bin:${PATH} \
    HELM_BIND=127.0.0.1:7878 \
    HELM_DB=/data/helm.db \
    HELM_CONFIG=/config/helm.toml
RUN groupadd --gid 1000 helm \
    && useradd --uid 1000 --gid helm --create-home --home-dir /home/helm helm \
    && mkdir -p /data /worktrees /config /repo \
        /home/helm/.claude /home/helm/.config/gh /home/helm/.cargo \
    && chown -R helm:helm /data /worktrees /home/helm
COPY --from=build /build/target/release/helm /usr/local/bin/helm
WORKDIR /home/helm
USER helm
# Tini forwards SIGTERM to Helm; Helm terminates each agent's process group.
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["helm"]
