# syntax=docker/dockerfile:1

# Build stage. The cache mounts are what make an incremental rebuild fast: without
# them every code change re-downloads and re-compiles the entire dependency tree.
FROM rust:1.90-slim-bookworm AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY migrations ./migrations

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release --locked --bin rgbpp-indexer \
 && mkdir -p /out && cp target/release/rgbpp-indexer /out/rgbpp-indexer

# Runtime stage. TLS is rustls, so no OpenSSL is needed — only root certificates.
FROM debian:bookworm-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*

RUN useradd --system --create-home --uid 10001 rgbpp
WORKDIR /app

COPY --from=builder /out/rgbpp-indexer /usr/local/bin/rgbpp-indexer
COPY config ./config

USER rgbpp
EXPOSE 8080

ENV RGBPP_CONFIG=/app/config/testnet.toml \
    RUST_LOG=info,sqlx=warn

HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8080/health || exit 1

ENTRYPOINT ["rgbpp-indexer"]
CMD ["run"]
