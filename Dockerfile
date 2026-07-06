# Multi-stage build: static-ish Rust binary on a slim Debian runtime.

FROM rust:1.94-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
RUN cargo build --release --locked

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/atlas-team-backend /usr/local/bin/atlas-team-backend
# Unprivileged runtime user.
USER 65532:65532
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/atlas-team-backend"]
