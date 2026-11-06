#!/bin/sh
# Runs on the old host. Streams every document's file as JSON lines, on stdout, using the app
# container's own storage client, so its service-account key stays on the server.
#   ssh <old-host> sh -s -- <app-container> <db-container> <db-name> < deploy/export/blobs.sh > blobs.stream
set -eu
NAMES=$(echo 'select json_agg(filename) from "Document";' | docker exec -i "$2" sh -c 'psql -v ON_ERROR_STOP=1 -U "$POSTGRES_USER" -d "$1" -At' sh "$3")
docker exec -i -e NAMES="$NAMES" "$1" node --input-type=module <<'JS'
import { Storage } from "@google-cloud/storage";
import fs from "node:fs";
import path from "node:path";
import crypto from "node:crypto";
const out = (o) => new Promise((r) => process.stdout.write(JSON.stringify(o) + "\n", r));
(async () => {
  const names = JSON.parse(process.env.NAMES);
  const bucket = new Storage().bucket(process.env.GCS_BUCKET);
  const [objs] = await bucket.getFiles();
  const inBucket = new Set(objs.map((o) => o.name));
  const dir = process.env.UPLOAD_DIR ?? path.join(process.cwd(), "uploads");
  for (const name of names) {
    let buf = null, where = "missing";
    if (inBucket.has(name)) { [buf] = await bucket.file(name).download(); where = "gcs"; }
    else if (fs.existsSync(path.join(dir, name))) { buf = fs.readFileSync(path.join(dir, name)); where = "local"; }
    await out({ name, where, size: buf ? buf.length : null,
      sha256: buf ? crypto.createHash("sha256").update(buf).digest("hex") : null,
      b64: buf ? buf.toString("base64") : null });
  }
  const known = new Set(names);
  for (const o of objs) if (!known.has(o.name)) await out({ orphan: o.name, size: Number(o.metadata.size) });
})().catch((e) => { console.error("blob export failed: " + e.message); process.exit(1); });
JS
