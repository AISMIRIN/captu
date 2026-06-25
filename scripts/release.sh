#!/usr/bin/env bash
# Bump version, create a git tag, and push — all in one step.
#
# Uses cargo-release inside the dev Docker container (same pattern as prepare.sh)
# for version editing / commit / tag creation, then pushes from the host where
# git credentials live.
#
# Usage:
#   scripts/release.sh patch     # 0.1.0 → 0.1.1
#   scripts/release.sh minor     # 0.1.0 → 0.2.0
#   scripts/release.sh major     # 0.1.0 → 1.0.0
#   scripts/release.sh 1.2.3     # explicit version

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

DEV_IMAGE="captu-dev:latest"
LEVEL="${1:-}"

# -- Validate argument --------------------------------------------------------
if [[ -z "$LEVEL" ]]; then
    echo "Usage: $0 patch|minor|major|X.Y.Z" >&2
    exit 1
fi

# -- Pre-flight checks (host side) --------------------------------------------

# Must be on main.
CURRENT_BRANCH="$(git rev-parse --abbrev-ref HEAD)"
if [[ "$CURRENT_BRANCH" != "main" ]]; then
    echo "error: releases must be cut from main (currently on '$CURRENT_BRANCH')" >&2
    exit 1
fi

# Working tree must be clean.
if ! git diff --quiet || ! git diff --cached --quiet; then
    echo "error: working tree has uncommitted changes — commit or stash first" >&2
    exit 1
fi

# Fetch and verify we are up to date with origin/main.
echo "[release.sh] fetching origin ..."
git fetch origin main
LOCAL="$(git rev-parse HEAD)"
REMOTE="$(git rev-parse origin/main)"
if [[ "$LOCAL" != "$REMOTE" ]]; then
    echo "error: local main is not up to date with origin/main — pull first" >&2
    exit 1
fi

# -- Build dev image if missing -----------------------------------------------
if ! docker image inspect "$DEV_IMAGE" >/dev/null 2>&1; then
    echo "[release.sh] building $DEV_IMAGE from Dockerfile (target: dev) ..."
    docker build -t "$DEV_IMAGE" --target dev -f docker/Dockerfile .
fi

# Allocate a tty only when stdout is one, so this also works in CI/non-interactive.
TTY_FLAG=()
if [[ -t 1 ]]; then
    TTY_FLAG=(-t)
fi

# -- Run cargo-release inside container (bump + commit + tag) -----------------
echo "[release.sh] running cargo-release '$LEVEL' inside container ..."
docker run --rm -i "${TTY_FLAG[@]}" \
    --user "$(id -u):$(id -g)" \
    -e HOME=/app \
    -e CARGO_HOME=/app/.cache/cargo \
    -e SQLX_OFFLINE=true \
    -v "$REPO_ROOT":/app \
    -v /etc/passwd:/etc/passwd:ro \
    -v /etc/group:/etc/group:ro \
    -w /app \
    "$DEV_IMAGE" \
    bash -c "
        set -euo pipefail
        export PATH=\"/app/.cache/cargo/bin:\$PATH\"

        # Install cargo-release if not present.
        if ! command -v cargo-release >/dev/null 2>&1; then
            echo '[release.sh] installing cargo-release ...'
            cargo install cargo-release
        fi

        # Bump version, commit, and tag. Push is disabled in Cargo.toml config.
        cargo release '$LEVEL' --execute --no-confirm
    "

# -- Push commit + tag from host (where git credentials live) -----------------
echo "[release.sh] pushing commit and tag to origin/main ..."
git push --follow-tags origin main

echo "[release.sh] done. CI will now create the GitHub release automatically."
