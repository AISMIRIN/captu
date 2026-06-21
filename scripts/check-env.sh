#!/usr/bin/env bash
# Phase 0: environment check script for captu
# Usage: ./scripts/check-env.sh <path/to/sample.ts>

set -euo pipefail

TS="${1:-}"
if [[ -z "$TS" ]]; then
    echo "Usage: $0 <path/to/sample.ts>"
    exit 1
fi

echo "=== (a) ffmpeg ARIB decoder check ==="
ffmpeg -decoders 2>&1 | grep -i arib || echo "[WARN] No ARIB decoder found"

echo ""
echo "=== (b) Subtitle extraction (ASS) ==="
ffmpeg -y -i "$TS" -map 0:s:0 /tmp/test_captu.ass 2>&1 | tail -5
echo "--- head of /tmp/test_captu.ass ---"
head -50 /tmp/test_captu.ass || echo "[FAIL] ASS file not created"

echo ""
echo "=== (c) EIT / EPG info (ffprobe) ==="
ffprobe -v quiet -show_programs "$TS" 2>&1 | head -50
