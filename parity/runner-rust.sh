#!/usr/bin/env bash
# Runs the committed scenarios through a real `nexus serve`, in owner mode with real bearer checks,
# on a throwaway import without recovered documents, because the old baseline has none.
# Usage: runner-rust.sh <nexus binary> <export dir> <out dir>
# Needs sqlite3 and python3. Private data stays in the working dir (tmpfs, mode 700), and the server
# never holds this script's stdio. Port: PARITY_RUST_PORT.
set -euo pipefail
W=${PARITY_RUST_WORK:-${XDG_RUNTIME_DIR:-/tmp}/nexus-rust-parity}
PORT=${PARITY_RUST_PORT:-55431}
FULL=mcp_at_parity_full
TOOLS=mcp_at_parity_tools

up() { (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; }

# Restored in place, so the runner's kept-alive sockets stay valid.
if [ "${1:-}" = reset ]; then
  sqlite3 "$W/data/nexus.db" ".restore '$W/seed/nexus.db'"
  rm -rf "$W/data/blobs"
  cp -a "$W/seed/blobs" "$W/data/blobs"
  exit 0
fi

BIN=$(realpath "$1")
EXPORT=$2
OUT=$(realpath -m "$3")
SCRIPT=$(realpath "$0")
HERE=$(dirname "$SCRIPT")
stop() { [ -f "$W/pid" ] && kill "$(cat "$W/pid")" 2>/dev/null; rm -f "$W/pid"; }
trap stop EXIT
up && { echo "port $PORT is in use" >&2; exit 1; }

rm -rf "$W"
install -d -m 700 "$W"
"$BIN" import --export "$EXPORT" --data-dir "$W/seed" </dev/null >/dev/null
echo parity-passphrase-not-secret | "$BIN" set-passphrase --data-dir "$W/seed" >/dev/null 2>&1
python3 -I - "$W/seed/nexus.db" "$FULL" "$TOOLS" <<'PY'
import hashlib, sqlite3, sys
db = sqlite3.connect(sys.argv[1])
for token, scopes in ((sys.argv[2], '["mcp:tools","mcp:submissions"]'), (sys.argv[3], '["mcp:tools"]')):
    db.execute("INSERT INTO oauth_access_tokens (token_hash,client_id,scopes,sensitive_consent_version,expires_at) "
               "VALUES (?,'parity',?,1,'2999-01-01T00:00:00.000Z')", (hashlib.sha256(token.encode()).hexdigest(), scopes))
db.commit()
db.execute("PRAGMA wal_checkpoint(TRUNCATE)")
db.close()
PY
cp -a "$W/seed" "$W/data"
NEXUS_DATA_DIR=$W/data NEXUS_BIND=127.0.0.1:$PORT NEXUS_PUBLIC_BASE_URL=http://127.0.0.1:$PORT TZ=UTC \
  setsid nohup "$BIN" serve </dev/null >"$W/server.log" 2>&1 &
echo $! >"$W/pid"
for _ in $(seq 1 100); do up && break; sleep 0.1; done
up || { echo "server failed to start" >&2; exit 1; }

rm -rf "$OUT"
rc=0
timeout 400 node "$HERE/runner-mcp/run.mjs" --server-url "http://127.0.0.1:$PORT/api/mcp" --token "$FULL" \
  --token-tools-only "$TOOLS" --reset-cmd "PARITY_RUST_WORK=$W $SCRIPT reset" --out "$OUT" </dev/null >"$W/runner.log" 2>&1 || rc=$?
grep -E 'expect failed|[1-9][0-9]* failed expectations' "$W/runner.log" || true
echo "runner exit $rc; panics in the server log: $(grep -ci panic "$W/server.log" || true)"
exit "$rc"
