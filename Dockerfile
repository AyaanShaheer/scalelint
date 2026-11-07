# Stage 1: Build the static release binary
FROM rust:1.85-slim-bookworm AS builder

RUN apt-get update && apt-get install -y build-essential pkg-config && rm -rf /var/lib/apt/lists/*

WORKDIR /usr/src/scalelint
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release --locked

# Stage 2: Minimal runtime container for CI/CD runners
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates git && rm -rf /var/lib/apt/lists/*
COPY --from=builder /usr/src/scalelint/target/release/scalelint /usr/local/bin/scalelint

WORKDIR /workspace
ENTRYPOINT ["scalelint"]