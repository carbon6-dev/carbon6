# syntax=docker/dockerfile:1.7
#
# Builds carbon6 with no local toolchain. Stages:
#   toolchain  Rust + wasm target + Trunk (Tailwind and wasm-bindgen are fetched by Trunk)
#   build      web UI, then release c6h, c6w, c6, c6-eval
#   artifacts  just the binaries (use with --output)
#   test       runs the test suite
#   check      runs `make check`: formatting, clippy (also for the web UI's wasm) and the test lint
#   fmt        the toolchain with rustfmt, to format the source (see `make docker-fmt`)
#   c6h        runtime image for the head
#   c6w        runtime image for a worker (it works on files in the container)
#   debs       .deb packages of c6h and c6w with systemd units, and c6,
#              built on ubuntu:${UBUNTU} (use with --output)
#   deb-host   Ubuntu with systemd as PID 1, to try the packages in

ARG RUST_VERSION=1.97
ARG TRUNK_VERSION=0.21.14
ARG UBUNTU=22.04

FROM rust:${RUST_VERSION}-bookworm AS toolchain
ARG TRUNK_VERSION
RUN rustup target add wasm32-unknown-unknown \
 && curl -fsSL "https://github.com/trunk-rs/trunk/releases/download/v${TRUNK_VERSION}/trunk-$(uname -m)-unknown-linux-gnu.tar.gz" \
    | tar -xz -C /usr/local/bin trunk \
 && trunk --version
WORKDIR /src

FROM toolchain AS build
ARG TARGETARCH
COPY . .
# Caches survive between builds; the binaries are copied out of the cached target dir.
# Build outputs and Trunk's downloaded tools are per architecture.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,id=c6-target-${TARGETARCH},target=/src/target \
    --mount=type=cache,id=c6-trunk-${TARGETARCH},target=/root/.cache/trunk \
    cd crates/c6-ui && trunk build --release && cd /src \
 && cargo build --release --locked -p c6h -p c6w -p c6 -p c6-eval \
 && mkdir -p /out && cp target/release/c6h target/release/c6w target/release/c6 target/release/c6-eval /out/

FROM scratch AS artifacts
COPY --from=build /out/ /

FROM toolchain AS test
RUN apt-get update && apt-get install -y --no-install-recommends python3 && rm -rf /var/lib/apt/lists/*
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    git config --global user.email test@c6 && git config --global user.name test \
 && cargo test --locked

FROM toolchain AS check
RUN rustup component add rustfmt clippy
COPY . .
# The build context has no .git; the test lint reads the files through git.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    git init -q && git add -A && make check

FROM toolchain AS fmt
RUN rustup component add rustfmt

FROM debian:bookworm-slim AS c6h
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /out/c6h /usr/local/bin/c6h
COPY docker/c6h.toml /etc/c6h/c6h.toml
VOLUME /data
EXPOSE 7700 7701
ENTRYPOINT ["c6h", "--config", "/etc/c6h/c6h.toml"]

FROM debian:bookworm-slim AS c6w
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates git bash procps python3 && rm -rf /var/lib/apt/lists/*
COPY --from=build /out/c6w /usr/local/bin/c6w
VOLUME /state
ENTRYPOINT ["c6w", "--state-dir", "/state"]
CMD ["run"]

# Ubuntu packages. Built on the oldest release they target, so the binaries
# need no newer glibc than it has (and install on every later release).
FROM ubuntu:${UBUNTU} AS deb-toolchain
ARG RUST_VERSION
ARG TRUNK_VERSION
ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo PATH=/usr/local/cargo/bin:$PATH
RUN apt-get update && apt-get install -y --no-install-recommends build-essential ca-certificates curl \
 && rm -rf /var/lib/apt/lists/* \
 && curl -fsSL https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal \
      --default-toolchain "${RUST_VERSION}" --target wasm32-unknown-unknown \
 && curl -fsSL "https://github.com/trunk-rs/trunk/releases/download/v${TRUNK_VERSION}/trunk-$(uname -m)-unknown-linux-gnu.tar.gz" \
    | tar -xz -C /usr/local/bin trunk \
 && trunk --version
WORKDIR /src

FROM deb-toolchain AS deb-build
ARG UBUNTU
ARG TARGETARCH
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,id=c6-deb-target-${UBUNTU}-${TARGETARCH},target=/src/target \
    --mount=type=cache,id=c6-trunk-${TARGETARCH},target=/root/.cache/trunk \
    cd crates/c6-ui && trunk build --release && cd /src \
 && cargo build --release --locked -p c6h -p c6w -p c6 \
 && mkdir -p /out && cp target/release/c6h target/release/c6w target/release/c6 /out/
ARG DEB_VERSION=0.1.0
ARG MAINTAINER
RUN MAINTAINER="${MAINTAINER}" packaging/deb/build.sh /out "${DEB_VERSION}" /debs

FROM scratch AS debs
COPY --from=deb-build /debs/ /

FROM ubuntu:${UBUNTU} AS deb-host
RUN apt-get update && apt-get install -y --no-install-recommends systemd systemd-sysv dbus sudo curl ca-certificates \
 && rm -rf /var/lib/apt/lists/*
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
