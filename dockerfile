# Use Rust 1.70 for building (adjust version if needed)
FROM rust:1.70-slim as builder

# Install build dependencies (if any additional tools are needed, add here)
RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*

# Set working directory
WORKDIR /app

# Copy Cargo.toml and Cargo.lock first for dependency caching
COPY Cargo.toml Cargo.lock ./

# Create dummy src/main.rs to cache dependencies
RUN mkdir -p src && echo "fn main() {}" > src/main.rs

# Build dependencies only
RUN cargo build --release

# Now copy the actual source code
COPY src/ src/
COPY .env ./

# Build the actual binary
RUN cargo build --release

# Runtime stage: Use a smaller Debian image
FROM debian:bookworm-slim

# Install runtime dependencies (SSL certs for reqwest, etc.)
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

# Create a non-root user for security
RUN useradd -r -s /bin/false appuser

# Set working directory
WORKDIR /app

# Copy the built binary (replace "trial_bot" with your package name from Cargo.toml)
COPY --from=builder /app/target/release/trial_bot /usr/local/bin/trial_bot

# Copy necessary files (JSON configs, data dir, .env)
COPY --from=builder /app/src/ src/
COPY --from=builder /app/data/ data/
COPY --from=builder /app/.env .env

# Ensure directories are writable by the user
RUN chown -R appuser:appuser /app

# Switch to non-root user
USER appuser

# Expose no ports (bot uses Telegram API)
# CMD to run the bot
CMD ["trial_bot"]