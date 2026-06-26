#!/usr/bin/env bash
# Measure test coverage using cargo-llvm-cov (nightly toolchain).
#
# #[coverage(off)] on externally-dependent functions (ffmpeg, FFI, bootstrap)
# keeps the gate focused on testable logic only.
#
# Usage:
#   scripts/cov.sh              # HTML report  → target/llvm-cov/html/index.html
#   scripts/cov.sh summary      # Text summary to stdout
#   scripts/cov.sh fail         # Same as CI: exit non-zero if below threshold
#
# Threshold: set --fail-under-lines to the value agreed with CI (ci.yml coverage job).
# Run `scripts/cov.sh summary` after initial attribute placement to measure the
# baseline, then set both this script and ci.yml to (baseline - a few points).
# Target: raise toward 90% as coverage improves.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# Excluded paths: vendor C source and auto-generated bindings.
IGNORE='crates/aribcaption-sys/(vendor|src/bindings)'

# Coverage threshold (line coverage %) — keep in sync with .github/workflows/ci.yml.
# Baseline measured 2026-06-26: 47.96% with initial coverage(off) marks.
# Raised 2026-06-26: 84.32% after route/parser/ingest-op integration tests.
# Set 4 points below measured value to absorb minor regressions.
# Target: raise toward 90% as coverage improves.
THRESHOLD=80

case "${1:-html}" in
  summary)
    scripts/dev.sh +nightly llvm-cov --workspace \
      --ignore-filename-regex "$IGNORE" \
      --summary-only
    ;;
  fail)
    scripts/dev.sh +nightly llvm-cov --workspace \
      --ignore-filename-regex "$IGNORE" \
      --fail-under-lines "$THRESHOLD" \
      --summary-only
    ;;
  html|*)
    scripts/dev.sh +nightly llvm-cov --workspace \
      --ignore-filename-regex "$IGNORE" \
      --html
    echo ""
    echo "Report: target/llvm-cov/html/index.html"
    ;;
esac
