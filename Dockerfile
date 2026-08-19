# Multi-stage Docker build for trim-mcp (Model Context Protocol Server)
FROM rust:1.80-slim-bookworm AS builder

WORKDIR /app
COPY . .

# Build the release binary for trim-mcp
RUN cargo build --release --bin trim-mcp

# Minimal runtime image
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/trim-mcp /usr/local/bin/trim-mcp

# Run as non-root user
RUN useradd -m -u 1000 mcpuser
USER mcpuser

ENTRYPOINT ["trim-mcp"]
