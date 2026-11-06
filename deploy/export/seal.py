#!/usr/bin/env python3
"""Turn rows.stream, blobs.stream and nexuscrm.dump into the export `nexus import` reads.

Usage: python3 -I seal.py <export dir> [--recovered <earlier export>/recovered-candidates]

Prints counts only, never row contents. Exits 1 and prints PROBLEMS if anything does not check out.
"""
import argparse, base64, collections, hashlib, json, os, pathlib, re, shutil, subprocess, sys

TABLES = ["Application", "ApplicationEvent", "ApplicationSubmission", "ApplicationToDocument", "Contact",
          "CvPatch", "CvProfile", "Document", "ScannedEmail"]
BLOCKED = {"Account", "Session", "LlmCredential", "EmailIntegration", "UserApiToken", "User", "Verification",
           "McpAccessToken", "McpRefreshToken", "McpAuthCode", "McpOAuthClient", "AgentMcpConnection"}
SECRETS = {"mcp tokens": r"mcp_(at|rt)_", "api tokens": r"\bjt_[A-Za-z0-9]{8,}", "private keys": r"BEGIN [A-Z ]*PRIVATE KEY",
           "provider keys": r"\bsk-[A-Za-z0-9_-]{16,}|ghp_[A-Za-z0-9]{16,}"}
# The commit whose schema these scripts read. Change it only with the importer.
SOURCE_COMMIT = "235138d"

parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
parser.add_argument("export", type=pathlib.Path)
parser.add_argument("--recovered", type=pathlib.Path, help="recovered-candidates directory to carry over")
args = parser.parse_args()
root = args.export
problems = []


def write_json(relative, value):
    (root / relative).write_text(json.dumps(value, indent=1, sort_keys=True))


rows, counts, sequences, current = collections.defaultdict(list), {}, {}, None
for line in (root / "rows.stream").read_text().splitlines():
    if line.startswith("###TABLE "):
        current = line.split()[1]
    elif line.startswith("###COUNT "):
        _, table, count = line.split()
        counts[table] = int(count)
    elif line.startswith("###SEQ "):
        _, table, value = line.split()
        sequences[table] = int(value)
    elif current and line.startswith("{"):
        json.loads(line)
        rows[current].append(line)

if set(counts) != set(TABLES):
    problems.append(f"rows.stream covers {len(counts)} of {len(TABLES)} tables")
(root / "jsonl").mkdir(exist_ok=True)
for table in TABLES:
    (root / "jsonl" / f"{table}.jsonl").write_text("".join(row + "\n" for row in rows[table]))
    print(f"{table}: {len(rows[table])}")
    if len(rows[table]) != counts.get(table):
        problems.append(f"{table}: {len(rows[table])} rows, the database counted {counts.get(table)}")
write_json("row-counts.json", {table: len(rows[table]) for table in TABLES})
write_json("sequences.json", sequences)
floors = {"_comment": "Highest IDs in this export. A higher ID was created during a run and is normalized by compare.py."}
for table in ("Application", "ApplicationEvent", "ApplicationSubmission", "Contact", "CvPatch", "CvProfile", "Document"):
    floors[table] = max((json.loads(row)["id"] for row in rows[table]), default=0)
write_json("id-floors.json", floors)

documents = {document["filename"]: document for document in map(json.loads, rows["Document"])}
(root / "blobs").mkdir(exist_ok=True)
index, where, orphans = {}, collections.Counter(), 0
with (root / "blobs.stream").open() as stream:
    for line in stream:
        blob = json.loads(line)
        if "orphan" in blob:
            orphans += 1
            continue
        where[blob["where"]] += 1
        if not blob["b64"]:
            continue
        document = documents[blob["name"]]
        data = base64.b64decode(blob["b64"])
        digest = hashlib.sha256(data).hexdigest()
        if digest != blob["sha256"]:
            problems.append(f"transfer hash mismatch for document {document['id']}")
        if len(data) != document["size"]:
            problems.append(f"size mismatch for document {document['id']}")
        stored = (document["contentHash"] or "").removeprefix("sha256:")
        if stored and stored != digest:
            problems.append(f"contentHash mismatch for document {document['id']}")
        (root / "blobs" / digest).write_bytes(data)
        index[blob["name"]] = {"sha256": digest, "size": len(data)}
write_json("blob-index.json", index)
missing = [{key: document.get(key) for key in ("id", "filename", "originalName", "size", "mimeType", "uploadedAt")}
           for name, document in documents.items() if name not in index]
write_json("MISSING_DOCUMENTS.json", {"note": "Rows exist, file not found by the blob export.", "documents": missing})
print(f"blobs by location: {dict(where)}; unique files: {len(list((root / 'blobs').iterdir()))}; "
      f"documents with no file: {len(missing)}; bucket objects with no document: {orphans}")

joined = "\n".join(line for table in TABLES for line in rows[table])
for label, pattern in SECRETS.items():
    found = len(re.findall(pattern, joined))
    print(f"credential scan, {label}: {found}")
    if found:
        problems.append(f"credential scan found {label}")

dump = root / "nexuscrm.dump"
if dump.read_bytes()[:5] != b"PGDMP":
    problems.append("nexuscrm.dump has no PGDMP header")
elif shutil.which("pg_restore"):
    listing = subprocess.run(["pg_restore", "-l", str(dump)], capture_output=True, text=True, check=True).stdout
    tables = set(re.findall(r"TABLE DATA public (\S+)", listing))
    print(f"dump tables: {len(tables)}")
    if tables & BLOCKED:
        problems.append(f"blocked tables in the dump: {sorted(tables & BLOCKED)}")
else:
    print("no pg_restore here: the dump was only checked by its header")

if args.recovered:
    shutil.copytree(args.recovered, root / "recovered-candidates", dirs_exist_ok=True)

if problems:
    print("PROBLEMS: " + "; ".join(problems))
    sys.exit(1)
(root / "rows.stream").unlink()
(root / "blobs.stream").unlink()
files = {str(path.relative_to(root)): hashlib.sha256(path.read_bytes()).hexdigest()
         for path in sorted(root.rglob("*")) if path.is_file() and path.name != "MANIFEST.json"}
write_json("MANIFEST.json", {"source_commit": SOURCE_COMMIT, "complete": not missing, "missing_documents": len(missing),
                             "recovered_candidates_verified": False, "files": files})
for path in root.rglob("*"):
    os.chmod(path, 0o700 if path.is_dir() else 0o600)
print(f"manifest entries: {len(files)}")
print("VERIFY OK")
