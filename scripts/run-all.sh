#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

mkdir -p target
cargo run --manifest-path wisp/Cargo.toml --release -p wisp-benchmark -- \
  --track all \
  --json target/wisp-benchmark.json \
  --md target/wisp-benchmark.md \
  --victory-table WISP_RESULTS.md

echo "Wisp benchmark report written to target/wisp-benchmark.json, target/wisp-benchmark.md, and WISP_RESULTS.md"
