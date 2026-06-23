#!/usr/bin/env bash
# Environment check script for captu
# Usage: ./scripts/check-env.sh <path/to/sample.ts>

set -euo pipefail

TS="${1:-}"
if [[ -z "$TS" ]]; then
    echo "Usage: $0 <path/to/sample.ts>"
    exit 1
fi

echo "=== ffprobe: programs / streams ==="
ffprobe -v quiet -show_programs "$TS" 2>&1 | head -50
