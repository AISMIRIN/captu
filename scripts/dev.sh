#!/usr/bin/env bash
# Run any cargo command inside the dev container as the host user.
# This keeps build artifacts (target/, data/, Cargo.lock) owned by the host
# user instead of root, so they stay editable/removable outside the container.
#
# Usage:
#   scripts/dev.sh build
#   scripts/dev.sh test
#   scripts/dev.sh run --bin extract -- /mnt/nas/video/sample.ts

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

DEV_IMAGE="captu-dev:latest"
NAS_HOST="${CAPTU_NAS_HOST:-./ts}"  # override: CAPTU_NAS_HOST=/mnt/your/recordings scripts/dev.sh ...
NAS_CONTAINER="/mnt/nas/video"

# Build the dev image if it is missing.
if ! docker image inspect "$DEV_IMAGE" >/dev/null 2>&1; then
    echo "[dev.sh] building $DEV_IMAGE from Dockerfile (target: dev) ..."
    docker build -t "$DEV_IMAGE" --target dev -f docker/Dockerfile .
fi

# Allocate a tty only when stdout is one, so this also works in CI/non-interactive.
TTY_FLAG=()
if [[ -t 1 ]]; then
    TTY_FLAG=(-t)
fi

# Mount the NAS read-only only if it exists (needed for extract/ingest).
NAS_MOUNT=()
if [[ -d "$NAS_HOST" ]]; then
    NAS_MOUNT=(-v "$NAS_HOST":"$NAS_CONTAINER":ro)
fi

# --user maps the container process to the host user so generated files are
# owned by us. CARGO_HOME/HOME are redirected into the bind mount because the
# image's default /usr/local/cargo is root-owned and unwritable as non-root.
# Dev paths are separate from prod so both can coexist without interfering.
exec docker run --rm -i "${TTY_FLAG[@]}" \
    --user "$(id -u):$(id -g)" \
    -e HOME=/app \
    -e CARGO_HOME=/app/.cache/cargo \
    -e CAPTU_DB_PATH=/app/data/captu-dev.db \
    -e CAPTU_CACHE_DIR=/app/cache/dev \
    -e SQLX_OFFLINE=true \
    -v "$REPO_ROOT":/app \
    "${NAS_MOUNT[@]}" \
    -w /app \
    "$DEV_IMAGE" \
    cargo "$@"
