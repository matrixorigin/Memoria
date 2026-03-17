# Multi-stage build for Memoria Rust API
FROM rust:1.82-slim AS builder

RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY memoria_rs/ .

ENV SQLX_OFFLINE=true
RUN cargo build --release --bin memoria-api --bin memoria-mcp-rs --bin memoria-rs

# Runtime
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/* && \
    useradd -m -u 1000 appuser

COPY --from=builder /build/target/release/memoria-api /usr/local/bin/
COPY --from=builder /build/target/release/memoria-mcp-rs /usr/local/bin/
COPY --from=builder /build/target/release/memoria-rs /usr/local/bin/

USER appuser
EXPOSE 8100

CMD ["memoria-api"]
