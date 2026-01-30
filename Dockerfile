# Build stage - use rust:latest for edition2024 support
FROM rust:latest as builder

WORKDIR /build

# Copy manifest files
COPY Cargo.toml Cargo.lock* ./

# Create a dummy main.rs to build dependencies
RUN mkdir -p src && \
    echo "fn main() {}" > src/main.rs

# Build dependencies (this layer will be cached if Cargo.toml doesn't change)
RUN cargo build --release && \
    rm src/main.rs

# Copy source code
COPY src ./src

# Copy static files
COPY static ./static

# Build the actual application
RUN touch src/main.rs && \
    cargo build --release && \
    strip target/release/ptp-fast-event-consumer

# Runtime stage - use Fedora for newer GLIBC (2.39+)
FROM registry.fedoraproject.org/fedora-minimal:latest

# Install CA certificates and curl for HTTPS and healthcheck
RUN microdnf update -y && \
    microdnf install -y ca-certificates curl && \
    microdnf clean all && \
    rm -rf /var/cache/yum

# Create app user
RUN useradd -r -u 1001 -g root -m -d /app -s /sbin/nologin appuser

WORKDIR /app

# Copy the binary from builder
COPY --from=builder /build/target/release/ptp-fast-event-consumer /app/

# Copy static files from builder
COPY --from=builder /build/static /app/static

# Set ownership
RUN chown -R appuser:root /app && \
    chmod -R 755 /app

# Switch to non-root user
USER appuser

# Expose port
EXPOSE 8080

# Health check
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:8080/api/status || exit 1

# Run the application
CMD ["./ptp-fast-event-consumer"]
