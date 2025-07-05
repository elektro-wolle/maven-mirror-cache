# Multi-stage Dockerfile for Maven Mirror Cache

# Build stage
FROM rust:1.88.0-slim AS builder

# Install dependencies
RUN apt-get update && \
    apt-get install -y \
        curl \
        libpq-dev \
        libssl-dev \
        pkg-config && \
    rm -rf /var/lib/apt/lists/*

# Create app directory
WORKDIR /app

# Copy manifests
COPY Cargo.toml Cargo.lock ./

# Create a dummy main.rs to build dependencies
RUN mkdir src && echo "fn main() {}" > src/main.rs

# Build dependencies (this will be cached)
COPY additional-certs.pem /app/cert.pem
RUN cat cert.pem >> /etc/ssl/certs/ca-certificates.crt
RUN cargo build --release
RUN rm src/main.rs

# Copy source code
COPY src ./src
RUN touch src/main.rs

# Build the application
RUN cargo build --release

# Runtime stage
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && \
    apt-get install -y \
        ca-certificates \
        libpq5 \
        libssl3 && \
    rm -rf /var/lib/apt/lists/* && \
    groupadd -r appuser && useradd -r -g appuser appuser

# Create app directory
WORKDIR /app

# Copy binary from builder stage
COPY --from=builder /app/target/release/maven-mirror-cache /app/maven-mirror-cache

# Create cache directory
RUN mkdir -p cache && chown appuser:appuser cache .

# Switch to app user
USER appuser

# Expose port
EXPOSE 8080

# Run the application
CMD ["/app/maven-mirror-cache"]
