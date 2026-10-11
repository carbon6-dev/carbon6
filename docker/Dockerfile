# syntax=docker/dockerfile:1.7
#
# Builds carbon6 with no local toolchain. Stages:
#   toolchain  Rust + wasm target + Trunk (Tailwind and wasm-bindgen are fetched by Trunk)
#   build      web UI, then release c6h, c6w, c6, c6-eval
#   artifacts  just the binaries (use with --output)
#   test       runs the test suite
#   c6h        runtime image for the head
#   c6w        runtime image for a worker (it works on files in the container)

ARG RUST_VERSION=1.97

FROM rust:${RUST_VERSION}-bookworm AS toolchain
ARG TRUNK_VERSION=0.21.14
RUN rustup target add wasm32-unknown-unknown \
 && curl -fsSL "https://github.com/trunk-rs/trunk/releases/download/v${TRUNK_VERSION}/trunk-$(uname -m)-unknown-linux-gnu.tar.gz" \
    | tar -xz -C /usr/local/bin trunk \
 && trunk --version
WORKDIR /src

FROM toolchain AS build
COPY . .
# Caches survive between builds; the binaries are copied out of the cached target dir.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    --mount=type=cache,target=/root/.cache/trunk \
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
