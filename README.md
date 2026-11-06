# Nexus

Job-application CRM rebuilt as a single Rust binary: an MCP tool server for agents, with a web dashboard for humans.

Status: the M1 code is in place: storage, importer, the 35-tool MCP server, OAuth and owner login, read-only dashboard, calendar feed and encrypted backups. Not done: Claude.ai through a real ingress, and the cutover itself, so nothing is deployed yet. See [`docs/CUTOVER.md`](docs/CUTOVER.md).

- [`docs/SPEC.md`](docs/SPEC.md): architecture, data model, migration, milestones, open items
- [`docs/CUTOVER.md`](docs/CUTOVER.md): runbook from today's state to cutover, with what has never been run
- [`contract/mcp-tools.snapshot.json`](contract/mcp-tools.snapshot.json): frozen MCP tool contract (35 tools, from `nexus-crm` at `235138d`, the deployed commit)

## Build and test

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release -p nexus
```

The target is `aarch64-unknown-linux-musl`. SQLite is bundled and there is no OpenSSL, so `cross build --release -p nexus --target aarch64-unknown-linux-musl` (or `cargo zigbuild`) needs no libraries from the board.

## Import an export

```sh
nexus import --export <export dir> --data-dir <data dir> [--approve-recovered <file>] [--force]
```

- Run it with the server stopped. It writes `nexus.db` and `blobs/<sha256>` in the data directory, and prints counts only.
- Every file it uses is checked against `MANIFEST.json`. Row counts, foreign keys, blob sizes and stored content hashes are verified as well. Any mismatch fails the run and leaves the previous database in place.
- Re-running gives the same rows: the database is built in a temp file and renamed into place. An existing database is replaced only if it is empty or unchanged since its own import; anything else needs `--force`, which keeps the old database's blobs. `--force` never replaces a database another process has open; a WAL left by a crash is replayed, so it doesn't block the import.
- `blob-index.json` decides which file belongs to a document. A filename absent from it imports with `blob_state = 'missing'`, and the summary counts any document `MISSING_DOCUMENTS.json` contradicts. A recovered candidate is used only if its document ID is listed in the approval file (one per line, `#` for comments). Without that file, none are. An approved ID whose candidate is ambiguous fails the run.

Keep exports and data directories out of this repository: they hold private data, and test fixtures must stay synthetic.
