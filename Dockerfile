# Stage 1: Build WardSONDB
FROM rust:latest AS wardsondb-builder
WORKDIR /build
RUN apt-get update && apt-get install -y git && rm -rf /var/lib/apt/lists/*
RUN git clone https://github.com/ward-software-defined-systems/wardsondb.git .
RUN cargo build --release

# Stage 2: Build embraOS Phase 0
FROM rust:latest AS embra-builder
WORKDIR /build
COPY Cargo.toml Cargo.lock* ./
COPY src/ src/
RUN cargo build --release

# Stage 3: Runtime
FROM debian:trixie-slim
RUN apt-get update && apt-get install -y ca-certificates iproute2 procps git && rm -rf /var/lib/apt/lists/*

COPY --from=wardsondb-builder /build/target/release/wardsondb /usr/local/bin/
COPY --from=embra-builder /build/target/release/embrad /usr/local/bin/

# Data directory for WardSONDB and workspace
RUN mkdir -p /embra/data /embra/config /embra/workspace/repos

# Default environment
ENV EMBRA_DATA_DIR=/embra/data
ENV WARDSONDB_DATA_DIR=/embra/data/wardsondb
ENV EMBRA_VERSION=0.1.0-phase0

VOLUME ["/embra/data"]

ENTRYPOINT ["embrad"]
