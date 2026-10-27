#!/bin/bash
# Opt-in live proof. This spends model tokens and requires a signed-in Claude Code/Agent SDK host.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [[ "${TAGTEAM_LIVE_SDK:-}" != "1" ]]; then
  printf 'Refusing to spend model tokens. Re-run with TAGTEAM_LIVE_SDK=1.\n' >&2
  exit 2
fi

pnpm -C runner typecheck
TAGTEAM_LIVE_SDK=1 pnpm -C runner exec vitest run test/live-sdk.test.ts
