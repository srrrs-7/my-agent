# syntax=docker/dockerfile:1
#
# Container-first development image.
#
# Rationale: every `cargo` invocation (which downloads and *executes* build
# scripts / proc-macros from crates.io) happens inside this container, never on
# the host. A compromised dependency - the Rust equivalent of the npm
# "Shai-Hulud" worm - therefore cannot read host credentials (~/.ssh, ~/.aws,
# keychain) or persist on the host.
#
# Hardening applied here (and in compose.yml):
#   * runs as a non-root user matched to the host UID/GID
#   * no docker socket is ever mounted
#   * all Linux capabilities dropped, no-new-privileges
#   * build caches live in named volumes, not in the bind-mounted source tree

########################################################################
# base - toolchain + system deps
########################################################################
FROM rust:1-bookworm AS base

ARG UID=1000
ARG GID=1000

ENV DEBIAN_FRONTEND=noninteractive \
    CARGO_TARGET_DIR=/target \
    CARGO_INCREMENTAL=1 \
    RUST_BACKTRACE=1

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates \
      git \
      make \
      curl \
      jq \
      pkg-config \
      cmake \
      clang \
      perl \
 && rm -rf /var/lib/apt/lists/*

# `rust-toolchain.toml` pins the channel to `stable`. The base image registers
# its toolchain under a version name, not under `stable`, so without this every
# single `docker compose run` would re-download the channel.
RUN rustup toolchain install stable --profile minimal --component rustfmt --component clippy \
 && rustup default stable

########################################################################
# dev - interactive development / CI stage
########################################################################
FROM base AS dev

ARG UID=1000
ARG GID=1000

# Create a non-root user that mirrors the host UID/GID so that files written
# through the bind mount keep sane ownership on the host.
RUN set -eux; \
    if ! getent group "${GID}" >/dev/null; then groupadd -g "${GID}" agent; fi; \
    if ! getent passwd "${UID}" >/dev/null; then \
      useradd -m -u "${UID}" -g "${GID}" -s /bin/bash agent; \
    fi; \
    mkdir -p /target /workspace /usr/local/cargo/registry /usr/local/cargo/git; \
    chown -R "${UID}:${GID}" /target /workspace /usr/local/cargo

USER ${UID}:${GID}
WORKDIR /workspace

CMD ["bash"]

########################################################################
# builder - compiles a release binary
########################################################################
FROM base AS builder

WORKDIR /build
COPY . .
RUN cargo build --release --locked --bin agent \
 && cp /target/release/agent /usr/local/bin/agent

########################################################################
# runtime - minimal image shipping only the binary
########################################################################
FROM debian:bookworm-slim AS runtime

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd -m -u 10001 -s /usr/sbin/nologin agent \
 && mkdir -p /workspace && chown agent:agent /workspace

COPY --from=builder /usr/local/bin/agent /usr/local/bin/agent

USER agent
WORKDIR /workspace
ENV AGENT_WORKSPACE=/workspace
ENTRYPOINT ["/usr/local/bin/agent"]
