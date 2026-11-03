# syntax=docker/dockerfile:1
#
# Multi-stage build for the `ekg-backend` binary.
#
# The build context is the repository root, not this directory: the backend pulls
# in the whole cargo workspace (`crates/*` and `apps/backend`), so every `COPY` path
# below is relative to the root. `just podman-build` runs:
#
#   podman build -f apps/backend/Dockerfile -t localhost/ekg-backend:local .
#
# Stage 1 compiles in `--release` on the stable toolchain pinned in
# rust-toolchain.toml (currently 1.98), with cargo-chef so the dependency compile
# is a cached layer that only busts when Cargo.{toml,lock} change. Stage 2 is a
# slim Debian runtime with the binary, CA certificates for outbound HTTPS, and
# curl for the compose healthcheck.

# --- chef: the cargo-chef base shared by planner and builder ---
FROM docker.io/library/rust:1.98-slim-bookworm AS chef
WORKDIR /build
# --locked builds cargo-chef from its own pinned lockfile.
RUN cargo install cargo-chef --locked

# --- planner: capture the dependency graph as a recipe ---
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# --- builder: cook deps (cached), then compile the app ---
FROM chef AS builder
COPY --from=planner /build/recipe.json recipe.json
# Compile only the dependency graph. This layer is reused until the recipe (i.e.
# Cargo.toml / Cargo.lock) changes, so day-to-day source edits skip the dep build.
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release -p ekg-backend

# --- runtime: slim Debian with the binary and CA certs ---
FROM docker.io/library/debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/ekg-backend /usr/local/bin/ekg-backend
# 8090 is the public /api (webhook and human oversight); 9080 is the Restate SDK endpoint
# the runtime dials back into (registered as http://ekg-backend:9080 on the compose net).
EXPOSE 8090 9080
ENTRYPOINT ["/usr/local/bin/ekg-backend"]
