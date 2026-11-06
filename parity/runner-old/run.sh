#!/usr/bin/env bash
# Records old Nexus traces. Usage: run.sh <out-dir> [scenario-name-filter]
# Requires parity/env/pg.sh up. Never contacts anything but loopback.
set -euo pipefail
: "${OLD_NEXUS_DIR:?set OLD_NEXUS_DIR to a checkout of nexus-crm at 235138d with deps installed}"
out=$(realpath -m "$1")
here=$(cd "$(dirname "$0")" && pwd)
work=${PARITY_WORK:-${XDG_RUNTIME_DIR:-/tmp}/nexus-parity}
cd "$OLD_NEXUS_DIR"
exec env -u GCS_BUCKET -u GOOGLE_APPLICATION_CREDENTIALS -u ALLOWED_EMAIL -u BETTER_AUTH_URL -u DB_PROVIDER \
  DATABASE_URL="postgresql://postgres@127.0.0.1:${PARITY_PG_PORT:-55417}/nexus" \
  UPLOAD_DIR="$work/uploads" \
  PARITY_OUT="$out" \
  PARITY_ONLY="${2:-}" \
  PARITY_RESET_CMD="$here/../env/pg.sh reset" \
  npx vitest run --config "$here/vitest.config.ts" --coverage.enabled=false --reporter=dot
