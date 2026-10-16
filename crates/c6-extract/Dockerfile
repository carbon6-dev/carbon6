# c6-extract: carbon6's parsers (mail, PDF, Word, HTML, EXIF, thumbnails,
# faces) built from source, with ffmpeg for video and libheif for HEIC.
FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked
FROM debian:bookworm-slim
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends ffmpeg libheif-examples \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/c6-extract /usr/local/bin/c6-extract
