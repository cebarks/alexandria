# syntax=docker/dockerfile:1.7

# ---- Build stage ----
# Workspace rust-version is 1.98 (edition 2024 + dep requirements); keep this base
# at or above it or cargo refuses the build. No rust-toolchain.toml in-tree — the
# Dockerfile pins the compiler, dev machines choose their own.
FROM rust:1.98.1-alpine3.22 AS builder

# g++/make for C++ build scripts (tokenizers' esaxx, aws-lc-sys' C sources).
# No openssl-dev: as of the hf-hub 1.0 bump the network stack is rustls +
# aws-lc-sys; openssl-sys is gone from the lockfile (verified via cargo tree).
RUN apk add --no-cache musl-dev g++ make

# Link musl dynamically: proc-macro and cc-based crates misbehave with the
# musl target's default crt-static, and the alpine runtime image provides musl.
ENV RUSTFLAGS="-C target-feature=-crt-static"

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

# Cache the cargo registry and build artifacts across builds; the binary is
# copied out because the target dir lives only inside the cache mount.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked -p alexandria \
    && cp target/release/alexandria /usr/local/bin/alexandria

# ---- Runtime stage ----
FROM alpine:3.22

# libstdc++/libgcc for the statically-built C++ objects' runtime, ca-certificates
# for the Hugging Face model download (trust store read by rustls-native-certs).
# libssl3/libcrypto3 dropped: nothing links OpenSSL anymore.
RUN apk add --no-cache libstdc++ libgcc ca-certificates \
    && adduser -S -u 10001 -h /home/alexandria alexandria

COPY --from=builder /usr/local/bin/alexandria /usr/local/bin/alexandria

ENV ALEXANDRIA_SERVER_TRANSPORT=http \
    ALEXANDRIA_SERVER_HOST=0.0.0.0 \
    ALEXANDRIA_SERVER_PORT=3000 \
    ALEXANDRIA_DATA_DIR=/data/db \
    # hf-hub downloads the embedding model (~80MB) here on first boot;
    # kept under /data so the volume persists it.
    HF_HOME=/data/hf-cache

RUN mkdir -p /data && chown alexandria /data

USER alexandria
VOLUME /data
EXPOSE 3000

ENTRYPOINT ["/usr/local/bin/alexandria"]
