#!/bin/bash
# Offline production-path acceptance. These tests use the real hub, node, mTLS,
# sandbox, and Git layers with deterministic runners, so they spend no model tokens.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

cargo test -p tagteam-node --test execution_spine
cargo test -p tagteam-node --test exec_sandbox
cargo test -p tagteam-node --test egress_sandbox
cargo test -p tagteam-node --test two_node_recovery
pnpm --dir "$ROOT/runner" run typecheck
pnpm --dir "$ROOT/runner" test
