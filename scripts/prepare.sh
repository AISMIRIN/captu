#!/usr/bin/env bash
# Generate / refresh the .sqlx/ offline query cache.
#
# Runs inside the dev Docker container so the build environment matches exactly.
# Sets DATABASE_URL to a throw-away SQLite file, applies migrations, then runs
# `cargo sqlx prepare` to write query-*.json files for every query! in the crate.
#
# The generated .sqlx/ directory is committed to git so that subsequent builds
# (including CI and the Docker builder) work with SQLX_OFFLINE=true.
#
# Usage:
#   scripts/prepare.sh          # regenerate .sqlx/ cache
#   scripts/prepare.sh --check  # verify .sqlx/ is up to date (fails if stale)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

DEV_IMAGE="captu-dev:latest"
CHECK_FLAG="${1:-}"

# Build the dev image if it is missing.
if ! docker image inspect "$DEV_IMAGE" >/dev/null 2>&1; then
    echo "[prepare.sh] building $DEV_IMAGE from Dockerfile (target: dev) ..."
    docker build -t "$DEV_IMAGE" --target dev -f docker/Dockerfile .
fi

# Allocate a tty only when stdout is one.
TTY_FLAG=()
if [[ -t 1 ]]; then
    TTY_FLAG=(-t)
fi

# Run inside the dev container with a temporary DB for prepare.
exec docker run --rm -i "${TTY_FLAG[@]}" \
    --user "$(id -u):$(id -g)" \
    -e HOME=/app \
    -e CARGO_HOME=/app/.cache/cargo \
    -e DATABASE_URL="sqlite:///tmp/captu-prepare.db?mode=rwc" \
    -e SQLX_OFFLINE=false \
    -v "$REPO_ROOT":/app \
    -w /app \
    "$DEV_IMAGE" \
    bash -c "
        set -euo pipefail

        # Install sqlx-cli if not present or if the cached version is not 0.8.x.
        export PATH=\"/app/.cache/cargo/bin:\$PATH\"
        if ! command -v sqlx >/dev/null 2>&1 || \
           ! sqlx --version 2>&1 | grep -qE '^sqlx-cli 0\\.8'; then
            echo '[prepare.sh] installing sqlx-cli 0.8 ...'
            cargo install sqlx-cli \
                --version '^0.8' \
                --no-default-features \
                --features sqlite,rustls
        fi

        # Create the DB and apply migrations so query! can validate against the schema.
        echo '[prepare.sh] running migrations on prepare DB ...'
        sqlx migrate run --source ./migrations

        # Generate .sqlx/ cache.
        echo '[prepare.sh] running cargo sqlx prepare ...'
        cargo sqlx prepare ${CHECK_FLAG} -- --workspace

        echo '[prepare.sh] done.'
    "
