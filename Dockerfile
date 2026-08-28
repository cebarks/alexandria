# syntax=docker/dockerfile:1.7

# ---- Build stage ----
# Workspace MSRV is 1.88, but locked deps (fastnum 0.7.5) require rustc 1.94.
FROM rust:1.94-alpine3.22 AS builder

# g++/make for C++ build scripts (tokenizers' esaxx), openssl-dev for openssl-sys.
RUN apk add --no-cache musl-dev g++ make pkgconfig openssl-dev

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

# libstdc++/libgcc for the statically-built C++ objects' runtime, libssl for
# openssl-sys, ca-certificates for the Hugging Face model download.
RUN apk add --no-cache libssl3 libcrypto3 libstdc++ libgcc ca-certificates \
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
