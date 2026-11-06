// Serves the real old Nexus route handlers on loopback (real OAuth token path, real transport),
// then drives them with parity/runner-mcp/run.mjs.
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import type { AddressInfo } from "node:net";

// lib/db/index.ts lazy-loads its adapter with require(), which cannot load TypeScript under vitest.
// This swaps only that lookup for the same Prisma adapter production uses.
vi.mock("@/lib/db", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/lib/db")>();
  const { PrismaAdapter } = await import("@/lib/db/prisma-adapter");
  const db = new PrismaAdapter();
  return { ...actual, getDb: () => db };
});

const LOOPBACK = new Set(["127.0.0.1", "localhost", "[::1]"]);

it("records old Nexus traces", async () => {
  const { OLD_NEXUS_DIR, DATABASE_URL, PARITY_OUT, PARITY_RESET_CMD, PARITY_ONLY } = process.env;
  if (!OLD_NEXUS_DIR || !DATABASE_URL || !PARITY_OUT || !PARITY_RESET_CMD) {
    throw new Error("use parity/runner-old/run.sh");
  }
  if (!LOOPBACK.has(new URL(DATABASE_URL).hostname)) throw new Error("DATABASE_URL must be loopback");
  if (process.env.GCS_BUCKET || process.env.GOOGLE_APPLICATION_CREDENTIALS) throw new Error("GCS must be unset");

  const realFetch = globalThis.fetch;
  globalThis.fetch = (input, init) => {
    const url = new URL(input instanceof Request ? input.url : String(input));
    if (url.protocol.startsWith("http") && !LOOPBACK.has(url.hostname)) {
      throw new Error(`blocked non-loopback fetch to ${url.hostname}`);
    }
    return realFetch(input, init);
  };

  const route = await import("@/app/api/mcp/route");
  const { NextRequest } = await import(`${OLD_NEXUS_DIR}/node_modules/next/server.js`);
  const server = createServer(async (req, res) => {
    const chunks: Buffer[] = [];
    for await (const chunk of req) chunks.push(chunk as Buffer);
    const headers = Object.entries(req.headers).flatMap(([key, value]) =>
      value === undefined ? [] : [[key, String(value)] as [string, string]]);
    const handler = route[req.method as "GET" | "POST" | "DELETE"];
    const response: Response = await handler(new NextRequest(`http://127.0.0.1${req.url}`, {
      method: req.method,
      headers,
      ...(chunks.length ? { body: Buffer.concat(chunks) } : {}),
    }));
    res.writeHead(response.status, Object.fromEntries(response.headers));
    res.end(Buffer.from(await response.arrayBuffer()));
  });
  await new Promise<void>((done) => server.listen(0, "127.0.0.1", done));
  const { port } = server.address() as AddressInfo;

  const exitCode = await new Promise<number | null>((done, fail) => {
    spawn(process.execPath, [
      new URL("../runner-mcp/run.mjs", import.meta.url).pathname.replace(/^\/@fs/, ""),
      "--server-url", `http://127.0.0.1:${port}/api/mcp`,
      "--token", "mcp_at_parity_full",
      "--token-tools-only", "mcp_at_parity_tools",
      "--reset-cmd", PARITY_RESET_CMD,
      "--out", PARITY_OUT,
      ...(process.env.PARITY_SCENARIOS ? ["--scenarios", process.env.PARITY_SCENARIOS] : []),
      ...(PARITY_ONLY ? ["--only", PARITY_ONLY] : []),
    ], { stdio: "inherit" }).on("exit", done).on("error", fail);
  });
  server.close();
  // Exit 3 means failed expectations, reported by the runner and in each trace. Anything else is a crash.
  expect([0, 3]).toContain(exitCode);
});
