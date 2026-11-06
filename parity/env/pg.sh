#!/usr/bin/env bash
# Throwaway Postgres 17 + uploads dir holding a restored copy of the production export.
# Usage: pg.sh up | reset | counts | down
# Data lives on tmpfs and is rebuilt from the dump on every `up`. Only 127.0.0.1 is bound.
set -euo pipefail

: "${OLD_NEXUS_DIR:?set OLD_NEXUS_DIR to a checkout of nexus-crm at 235138d with deps installed}"
EXPORT_DIR=${NEXUS_EXPORT_DIR:-$HOME/.local/share/nexus-export/20261106T001410Z}
WORK=${PARITY_WORK:-${XDG_RUNTIME_DIR:-/tmp}/nexus-parity}
PORT=${PARITY_PG_PORT:-55417}
CTR=nexus-parity-pg

psql_c() { podman exec -i "$CTR" psql -q -v ON_ERROR_STOP=1 -U postgres -d nexus "$@"; }

owner_id() { python3 -c "import json,sys; print(json.loads(open(sys.argv[1]).readline())['userId'])" "$EXPORT_DIR/jsonl/Application.jsonl"; }

# The User table is not exported. Real event hashes cover the actor, so the owner row reuses the
# actor stored on real MCP events; otherwise replays of real idempotency keys could not match.
owner_email() {
  python3 - "$EXPORT_DIR/jsonl/ApplicationEvent.jsonl" <<'PY'
import collections, json, sys
actors = collections.Counter(event["actor"] for event in map(json.loads, open(sys.argv[1])) if event["source"] == "mcp" and event["actor"])
print(actors.most_common(1)[0][0])
PY
}

load_data() {
  psql_c <<SQL
DO \$\$ DECLARE t text; BEGIN
  SELECT string_agg(format('%I', tablename), ', ') INTO t FROM pg_tables
   WHERE schemaname = 'public' AND tablename <> '_prisma_migrations';
  EXECUTE 'TRUNCATE ' || t || ' RESTART IDENTITY CASCADE';
END \$\$;
INSERT INTO "User" (id, email, name, "emailVerified", "isAdmin", "updatedAt")
  VALUES ('$(owner_id)', '$(owner_email)', 'Parity Owner', true, false, '2026-01-01T00:00:00Z');
INSERT INTO "McpAccessToken" (id, "tokenHash", "clientId", "userId", scopes, "sensitiveConsentVersion", "expiresAt")
  SELECT 'parity-' || name, encode(sha256(('mcp_at_parity_' || name)::bytea), 'hex'), 'parity', '$(owner_id)', scopes, consent, '2099-01-01T00:00:00Z'
  FROM (VALUES ('full', ARRAY['mcp:tools','mcp:submissions'], 1), ('tools', ARRAY['mcp:tools'], 0)) AS v(name, scopes, consent);
SQL
  podman exec -i "$CTR" pg_restore -U postgres -d nexus --data-only --disable-triggers --no-owner --exit-on-error <"$EXPORT_DIR/nexuscrm.dump"
  rm -rf "$WORK/uploads"
  mkdir -p "$WORK/uploads"
  python3 - "$EXPORT_DIR" "$WORK/uploads" <<'PY'
import json, os, shutil, sys
export, uploads = sys.argv[1:]
for filename, blob in json.load(open(os.path.join(export, "blob-index.json"))).items():
    shutil.copyfile(os.path.join(export, "blobs", blob["sha256"]), os.path.join(uploads, filename))
PY
}

counts() {
  psql_c -At <<'SQL'
SELECT 'Application', count(*) FROM "Application" UNION ALL
SELECT 'ApplicationEvent', count(*) FROM "ApplicationEvent" UNION ALL
SELECT 'Contact', count(*) FROM "Contact" UNION ALL
SELECT 'Document', count(*) FROM "Document" UNION ALL
SELECT '_ApplicationToDocument', count(*) FROM "_ApplicationToDocument" UNION ALL
SELECT 'ApplicationSubmission', count(*) FROM "ApplicationSubmission";
SQL
}

case "${1:-}" in
  up)
    install -d -m 700 "$WORK"
    podman rm -f "$CTR" >/dev/null 2>&1 || true
    podman run -d --pull=never --name "$CTR" -p "127.0.0.1:$PORT:5432" \
      --tmpfs /var/lib/postgresql/data:rw,size=1g \
      -e POSTGRES_HOST_AUTH_METHOD=trust -e POSTGRES_DB=nexus \
      docker.io/library/postgres:17 >/dev/null
    until podman exec "$CTR" pg_isready -q -U postgres -d nexus -h 127.0.0.1; do sleep 0.3; done
    for dir in "$OLD_NEXUS_DIR"/prisma/migrations/*/; do
      psql_c <"$dir/migration.sql"
    done
    load_data
    counts
    ;;
  reset) load_data ;;
  counts) counts ;;
  down) podman rm -f "$CTR" >/dev/null; rm -rf "$WORK" ;;
  *) echo "usage: $0 up|reset|counts|down" >&2; exit 2 ;;
esac
