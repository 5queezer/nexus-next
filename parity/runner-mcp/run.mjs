#!/usr/bin/env node
// Runs parity scenarios against any Streamable HTTP MCP server and writes one trace per scenario.
// No dependencies: Node 20+ only. See parity/README.md for the scenario and trace formats.
import { execSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdirSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { parseArgs } from "node:util";

const { values: opt } = parseArgs({
  options: {
    "server-url": { type: "string" },
    token: { type: "string" },
    "token-tools-only": { type: "string" },
    "reset-cmd": { type: "string" },
    scenarios: { type: "string", default: new URL("../scenarios", import.meta.url).pathname },
    out: { type: "string" },
    only: { type: "string" },
  },
});
for (const name of ["server-url", "token", "token-tools-only", "reset-cmd", "out"]) {
  if (!opt[name]) throw new Error(`missing --${name}`);
}

const TOKENS = {
  full: opt.token,
  "tools-only": opt["token-tools-only"],
  invalid: "mcp_at_parity_invalid",
  none: null,
};

class HttpFailure extends Error {
  constructor(status) {
    super(`HTTP ${status}`);
    this.status = status;
  }
}

class RpcFailure extends Error {
  constructor(error) {
    super(error.message);
    this.rpc = { code: error.code, message: error.message };
  }
}

async function post(session, body) {
  const headers = { "content-type": "application/json", accept: "application/json, text/event-stream" };
  if (session.token) headers.authorization = `Bearer ${session.token}`;
  if (session.id) headers["mcp-session-id"] = session.id;
  if (session.protocolVersion) headers["mcp-protocol-version"] = session.protocolVersion;
  const response = await fetch(opt["server-url"], { method: "POST", headers, body: JSON.stringify(body) });
  if (!response.ok && response.status !== 202) throw new HttpFailure(response.status);
  const sessionId = response.headers.get("mcp-session-id");
  if (sessionId) session.id = sessionId;
  const text = await response.text();
  if (body.id === undefined) return null;
  const type = response.headers.get("content-type") ?? "";
  const messages = type.includes("text/event-stream")
    ? text.split(/\r?\n/).filter((line) => line.startsWith("data:")).map((line) => JSON.parse(line.slice(5)))
    : [JSON.parse(text)];
  const reply = messages.find((message) => message.id === body.id);
  if (!reply) throw new Error(`no JSON-RPC reply for ${body.method}`);
  if (reply.error) throw new RpcFailure(reply.error);
  return reply.result;
}

let nextRpcId = 1;
const rpc = (session, method, params) => post(session, { jsonrpc: "2.0", id: nextRpcId++, method, params });

const sessions = {};
function connect(auth) {
  sessions[auth] ??= (async () => {
    const session = { token: TOKENS[auth] };
    const init = await rpc(session, "initialize", {
      protocolVersion: "2025-06-18",
      capabilities: {},
      clientInfo: { name: "nexus-parity", version: "1" },
    });
    session.protocolVersion = init.protocolVersion;
    await post(session, { jsonrpc: "2.0", method: "notifications/initialized" });
    return session;
  })();
  return sessions[auth];
}

function digestBase64(value) {
  if (Array.isArray(value)) return value.map(digestBase64);
  if (value === null || typeof value !== "object") return value;
  return Object.fromEntries(Object.entries(value).map(([key, item]) => [
    key,
    key === "contentBase64" && typeof item === "string"
      ? { sha256: createHash("sha256").update(Buffer.from(item, "base64")).digest("hex"), base64Length: item.length }
      : digestBase64(item),
  ]));
}

const stable = (value) => JSON.stringify(value, (_key, item) =>
  item && typeof item === "object" && !Array.isArray(item)
    ? Object.fromEntries(Object.entries(item).sort(([a], [b]) => (a < b ? -1 : 1)))
    : item);

function toTraceResult(result) {
  const text = result.content?.[0]?.text;
  const out = { isError: result.isError === true, contentCount: result.content?.length ?? 0 };
  let parsed;
  try {
    parsed = JSON.parse(text);
    out.json = digestBase64(parsed);
  } catch {
    out.text = text ?? null;
  }
  if (!result.structuredContent) out.structured = "none";
  else if (stable(result.structuredContent) === stable(parsed)) out.structured = "same-as-text";
  else out.structured = digestBase64(result.structuredContent);
  return out;
}

// Path syntax: dot-separated keys and indices. "?key" filters an array to items with a truthy key.
function getPath(value, path) {
  if (path === ".") return value;
  for (const segment of path.split(".")) {
    if (value === null || value === undefined) return undefined;
    value = segment.startsWith("?") ? value.filter((item) => item[segment.slice(1)]) : value[segment];
  }
  return value;
}

function resolve(value, vars) {
  if (Array.isArray(value)) return value.map((item) => resolve(item, vars));
  if (value && typeof value === "object") {
    if ("$repeat" in value) return (value.prefix ?? "") + String(value.$repeat[0]).repeat(value.$repeat[1]);
    return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, resolve(item, vars)]));
  }
  if (typeof value !== "string" || !value.includes("$")) return value;
  const ref = (path) => {
    const [name, ...rest] = path.split(".");
    if (!(name in vars)) throw new Error(`unknown scenario variable: ${name}`);
    return rest.length ? getPath(vars[name], rest.join(".")) : vars[name];
  };
  if (/^\$[\w?.-]+$/.test(value)) return ref(value.slice(1));
  return value.replace(/\$\{([\w?.-]+)\}/g, (_match, path) => String(ref(path)));
}

function checkExpect(expect, result, vars) {
  if (!expect) return [];
  const failures = [];
  const check = (name, actual, wanted) => {
    if (stable(actual ?? null) !== stable(wanted)) failures.push(name);
  };
  if ("isError" in expect) check("isError", result.isError, expect.isError);
  if ("errorCode" in expect) check("errorCode", result.json?.error?.code, expect.errorCode);
  if ("text" in expect) check("text", result.text, expect.text);
  if ("httpStatus" in expect) check("httpStatus", result.httpStatus, expect.httpStatus);
  for (const [path, wanted] of Object.entries(expect.paths ?? {})) {
    const actual = getPath(result.json, path);
    if (wanted === "$absent") {
      if (actual !== undefined) failures.push(path);
    } else {
      check(path, actual, resolve(wanted, vars));
    }
  }
  return failures;
}

async function invoke(step, args) {
  const auth = step.auth ?? "full";
  try {
    const session = await connect(auth);
    if (step.tool === "$tools/list") {
      const { tools } = await rpc(session, "tools/list", {});
      return { isError: false, contentCount: 1, json: { tools }, structured: "none" };
    }
    return toTraceResult(await rpc(session, "tools/call", { name: step.tool, arguments: args }));
  } catch (error) {
    if (error instanceof HttpFailure) {
      delete sessions[auth];
      return { httpStatus: error.status };
    }
    if (error instanceof RpcFailure) return { rpcError: error.rpc };
    throw error;
  }
}

async function runCall(step, vars, trace, group) {
  const args = resolve(step.args ?? {}, vars);
  const result = await invoke(step, args);
  const entry = { tool: step.tool, auth: step.auth ?? "full", args: digestBase64(args), ...result };
  if (group !== undefined) entry.group = group;
  if (step.compare) entry.compare = step.compare;
  if (step.allowDiff) entry.allowDiff = step.allowDiff;
  for (const [name, path] of Object.entries(step.save ?? {})) vars[name] = getPath(result.json, path);
  const failures = checkExpect(step.expect, result, vars);
  if (failures.length) entry.expectFailures = failures;
  trace.calls.push(entry);
}

// Holds whatever the other side did: exactly these errors, in any order, and every other call succeeded.
function checkErrorCodes(step, calls) {
  if (!step.expectErrorCodes) return;
  const failed = calls.filter((call) => call.isError !== false).map((call) => call.json?.error?.code ?? null);
  if (stable(failed.sort()) !== stable([...step.expectErrorCodes].sort())) {
    calls[0].expectFailures = [...(calls[0].expectFailures ?? []), "expectErrorCodes"];
  }
}

async function runScenario(scenario, file) {
  if (scenario.resetDb) execSync(opt["reset-cmd"], { stdio: "inherit" });
  const trace = { format: 1, scenario: scenario.name, startedAt: new Date().toISOString(), calls: [] };
  const vars = {};
  let group = 0;
  for (const step of scenario.calls) {
    if (step.parallel) {
      group += 1;
      const settled = { calls: [] };
      await Promise.all(step.parallel.map((inner) => runCall(inner, vars, settled, group)));
      checkErrorCodes(step, settled.calls);
      trace.calls.push(...settled.calls);
    } else if (step.forEach) {
      const items = resolve(step.forEach, vars);
      if (!Array.isArray(items)) throw new Error(`${scenario.name}: forEach ${step.forEach} is not a list`);
      for (const item of items) {
        await runCall(step, { ...vars, [step.bind]: item }, trace);
      }
    } else {
      await runCall(step, vars, trace);
    }
  }
  trace.finishedAt = new Date().toISOString();
  writeFileSync(join(opt.out, file), JSON.stringify(trace, null, 1), { mode: 0o600 });
  return trace;
}

mkdirSync(opt.out, { recursive: true, mode: 0o700 });
let failedExpectations = 0;
for (const file of readdirSync(opt.scenarios).filter((name) => name.endsWith(".json")).sort()) {
  const scenario = JSON.parse(readFileSync(join(opt.scenarios, file), "utf8"));
  if (opt.only && !scenario.name.includes(opt.only)) continue;
  const trace = await runScenario(scenario, file);
  const failed = trace.calls
    .map((call, index) => ({ call, index }))
    .filter(({ call }) => call.expectFailures);
  failedExpectations += failed.length;
  console.log(`${scenario.name}: ${trace.calls.length} calls, ${failed.length} failed expectations`);
  for (const { call, index } of failed) {
    console.log(`  call ${index} ${call.tool}: expect failed on ${call.expectFailures.join(", ")}`);
  }
}
process.exitCode = failedExpectations ? 3 : 0;
