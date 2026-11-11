const assert = require("node:assert/strict");
const fs = require("node:fs");
const test = require("node:test");
const vm = require("node:vm");

const source = fs.readFileSync(require.resolve("./voice.js"), "utf8");
const flush = () => new Promise((resolve) => setImmediate(resolve));

class FakeElement {
  constructor() {
    this.children = [];
    this.classList = { add() {}, remove() {} };
    this.dataset = {};
    this.hidden = false;
    this.listeners = new Map();
    this.textContent = "";
    this.scrollHeight = 0;
    this.scrollTop = 0;
    this.clientHeight = 0;
  }

  addEventListener(type, listener) {
    const listeners = this.listeners.get(type) || [];
    listeners.push(listener);
    this.listeners.set(type, listeners);
  }

  append(...children) {
    this.children.push(...children);
  }

  replaceChildren() { this.children = []; }

  async emit(type, event = {}) {
    for (const listener of this.listeners.get(type) || []) await listener(event);
  }

  get firstElementChild() {
    return this.children[0];
  }

  setAttribute() {}
  scrollIntoView() {}
  closest() { return null; }
  remove() {}
}

class FakeChannel extends FakeElement {
  constructor() {
    super();
    this.readyState = "open";
    this.sent = [];
  }

  send(message) {
    this.sent.push(JSON.parse(message));
  }

  close() {
    this.readyState = "closed";
  }
}

function harness(options = {}) {
  let now = 1;
  const toolResponse = options.toolResponse;
  const lookup = options.lookup;
  const sessionResponse = options.sessionResponse;
  const ids = [
    "voice",
    "voice-status",
    "voice-connect",
    "voice-stop",
    "voice-live",
    "voice-hold",
    "voice-handsfree",
    "voice-remaining",
    "voice-transcript",
    "voice-tools",
    "voice-phase", "voice-mic-state", "voice-hold-label", "voice-interrupt",
    "voice-transcript-empty", "voice-tools-empty", "voice-announcement",
  ];
  const elements = Object.fromEntries(ids.map((id) => [id, new FakeElement()]));
  elements.voice.dataset.csrf = "csrf";
  const documentListeners = {};
  const document = {
    hidden: false,
    createElement: () => new FakeElement(),
    getElementById: (id) => elements[id] || null,
    addEventListener(type, listener) { documentListeners[type] = listener; },
  };
  const channel = new FakeChannel();
  class Peer {
    constructor() {
      this.connectionState = "connected";
      this.localDescription = { sdp: "offer" };
    }
    addEventListener() {}
    addTrack() {}
    close() {}
    createDataChannel() { return channel; }
    async createOffer() { return { sdp: "offer", type: "offer" }; }
    async setLocalDescription() {}
    async setRemoteDescription() {}
  }
  const track = { enabled: false, stopped: false, stop() { this.stopped = true; this.enabled = false; } };
  const stream = { getAudioTracks: () => [track], getTracks: () => [track] };
  const toolCalls = [];
  const lookups = [];
  const fetch = async (url, options = {}) => {
    if (url === "/api/voice/session") {
      if (sessionResponse) return sessionResponse;
      return {
        ok: true,
        json: async () => ({
          callsUrl: "https://calls.test",
          clientSecret: "short-lived",
          idleSeconds: 300,
          lifetimeSeconds: 900,
          voiceSessionId: "voice-1",
        }),
      };
    }
    if (url === "https://calls.test") return { ok: true, text: async () => "answer" };
    if (url === "/api/voice/tool") {
      const request = JSON.parse(options.body);
      // The page's own read for a card's title, kept apart from the calls the model made.
      if (request.name === "get_application") {
        lookups.push(request);
        return lookup || { status: 200, json: async () => ({ isError: false, output: JSON.stringify({
          id: request.arguments.id, company: "Example Robotics", role: "AI Agent Engineer (Python)",
          contacts: [{ id: "7", name: "Person B" }],
        }) }) };
      }
      toolCalls.push(request);
      if (toolResponse) return toolResponse;
      return { json: async () => ({ isError: false, output: '{"updated":true}' }) };
    }
    throw new Error(`unexpected fetch: ${url}`);
  };
  const window = { addEventListener() {}, location: { origin: "https://nexus.test" }, opener: null };
  const updates = [];
  class BroadcastChannel {
    close() {}
    postMessage(message) { updates.push(message.tool); }
  }
  window.BroadcastChannel = BroadcastChannel;
  class Audio {
    set srcObject(value) { this.value = value; }
  }
  vm.runInNewContext(source, {
    AbortController,
    AbortSignal,
    Audio,
    BroadcastChannel,
    Element: FakeElement,
    JSON,
    Map,
    Promise,
    RTCPeerConnection: Peer,
    Set,
    clearInterval() {},
    clearTimeout() {},
    document,
    fetch,
    navigator: { mediaDevices: { getUserMedia: options.getUserMedia || (async () => stream) } },
    performance: { now: () => now },
    setInterval: () => 1,
    setTimeout: () => 1,
    window,
  });
  return { channel, elements, toolCalls, lookups, updates, document, documentListeners, track, advance: (ms) => { now += ms; } };
}

async function connect(h) {
  await h.elements["voice-connect"].emit("click");
  await h.channel.emit("open");
}

function response(status, calls) {
  return {
    data: JSON.stringify({
      type: "response.done",
      response: { output: calls, status },
    }),
  };
}

// A write that needs no tap, so these tests are about the relay alone.
const call = (id) => ({
  arguments: '{"applicationId":"1","note":"changed"}',
  call_id: id,
  name: "append_application_note",
  type: "function_call",
});

test("cancelled calls do not run and duplicate call ids run once", async () => {
  const h = harness();
  await connect(h);
  await h.channel.emit("message", response("cancelled", [call("same")]));
  await flush();
  assert.equal(h.toolCalls.length, 0);

  const completed = response("completed", [call("same")]);
  await h.channel.emit("message", completed);
  await flush();
  await flush();
  await h.channel.emit("message", completed);
  await flush();
  await flush();

  assert.equal(h.toolCalls.length, 1);
  assert.equal(h.channel.sent.filter((event) => event.type === "response.create").length, 1);
});

test("Stop prevents mutations queued behind an active call from starting", async () => {
  const h = harness();
  await connect(h);
  await h.channel.emit("message", response("completed", [call("first"), call("second")]));
  await h.elements["voice-stop"].emit("click");
  await flush();
  await flush();
  assert.deepEqual(h.toolCalls.map((request) => request.arguments.applicationId), ["1"]);
});

const event = (h, type, extra = {}) => h.channel.emit("message", { data: JSON.stringify({ type, ...extra }) });

test("tap to talk exposes actual mic state, then thinking and speaking", async () => {
  const h = harness();
  await connect(h);
  assert.equal(h.elements.voice.dataset.phase, "ready");
  assert.equal(h.track.enabled, false);
  await h.elements["voice-hold"].emit("click");
  assert.equal(h.elements.voice.dataset.phase, "listening");
  assert.equal(h.elements["voice-mic-state"].textContent, "Microphone on");
  h.advance(500);
  await h.elements["voice-hold"].emit("click");
  assert.equal(h.track.enabled, false);
  assert.equal(h.elements.voice.dataset.phase, "thinking");
  await event(h, "response.created");
  await event(h, "output_audio_buffer.started");
  assert.equal(h.elements.voice.dataset.phase, "speaking");
  await event(h, "response.done", { response: { status: "completed", output: [] } });
  assert.equal(h.elements.voice.dataset.phase, "speaking");
  await event(h, "output_audio_buffer.stopped");
  assert.equal(h.elements.voice.dataset.phase, "ready");
});

test("end preserves readable history and reconnect clears it", async () => {
  const h = harness();
  await connect(h);
  await event(h, "response.output_text.done", { item_id: "reply", text: "Saved." });
  const transcript = h.elements["voice-transcript"];
  assert.equal(transcript.children[0].children[1].textContent, "Saved.");
  await h.elements["voice-stop"].emit("click");
  assert.equal(h.elements.voice.dataset.phase, "ended");
  assert.equal(h.track.stopped, true);
  assert.equal(transcript.children.length, 1);
  await connect(h);
  assert.equal(transcript.children.length, 0);
});

test("permission failure is actionable and does not connect", async () => {
  const h = harness({ getUserMedia: async () => { throw { name: "NotAllowedError" }; } });
  await h.elements["voice-connect"].emit("click");
  assert.equal(h.elements.voice.dataset.phase, "error");
  assert.match(h.elements["voice-status"].textContent, /Allow it/);
  assert.equal(h.elements["voice-mic-state"].textContent, "Microphone off");
});

test("ending during permission prompt stops a late microphone stream", async () => {
  let resolve;
  const h = harness({ getUserMedia: () => new Promise(r => { resolve = r; }) });
  const opening = h.elements["voice-connect"].emit("click");
  assert.equal(h.elements["voice-stop"].hidden, false);
  await h.elements["voice-stop"].emit("click");
  resolve({ getTracks: () => [h.track] });
  await opening;
  assert.equal(h.track.stopped, true);
  assert.equal(h.elements.voice.dataset.sessionState, "stopped");
});

test("interrupt keeps connection and suppresses late completed calls", async () => {
  const h = harness();
  await connect(h);
  await event(h, "response.created");
  await event(h, "output_audio_buffer.started");
  await h.elements["voice-interrupt"].emit("click");
  await h.channel.emit("message", response("completed", [call("late")]));
  await flush();
  assert.equal(h.toolCalls.length, 0);
  assert.equal(h.elements.voice.dataset.sessionState, "live");
  assert.equal(h.channel.sent.some(e => e.type === "response.cancel"), true);
  assert.equal(h.channel.sent.some(e => e.type === "output_audio_buffer.clear"), true);
});

test("hands-free is opt-in and hidden page never claims microphone on", async () => {
  const h = harness();
  await connect(h);
  const toggle = h.elements["voice-handsfree"];
  toggle.checked = true;
  await toggle.emit("change");
  assert.equal(h.track.enabled, true);
  assert.equal(h.elements.voice.dataset.phase, "listening");
  h.document.hidden = true;
  h.documentListeners.visibilitychange();
  assert.equal(h.track.enabled, false);
  assert.equal(h.elements.voice.dataset.phase, "paused");
  assert.equal(h.elements["voice-mic-state"].textContent, "Microphone off");
  h.document.hidden = false;
  h.documentListeners.visibilitychange();
  assert.equal(h.track.enabled, true);
  toggle.checked = false;
  await toggle.emit("change");
  assert.equal(h.track.enabled, false);
});

test("Space holds to talk without moving focus or triggering the tap handler", async () => {
  const h = harness();
  await connect(h);
  const key = { code: "Space", target: h.elements["voice-hold"], preventDefault() {} };
  h.documentListeners.keydown(key);
  assert.equal(h.track.enabled, true);
  h.advance(500);
  h.documentListeners.keyup(key);
  assert.equal(h.track.enabled, false);
  assert.equal(h.channel.sent.filter(e => e.type === "input_audio_buffer.commit").length, 1);
});

test("streaming text does not pull a reader away from older messages", async () => {
  const h = harness();
  await connect(h);
  const transcript = h.elements["voice-transcript"];
  transcript.scrollHeight = 1000;
  transcript.clientHeight = 200;
  transcript.scrollTop = 100;
  await event(h, "response.output_text.delta", { item_id: "reply", delta: "Hello" });
  assert.equal(transcript.scrollTop, 100);
  assert.equal(h.elements["voice-announcement"].textContent, "");
  await event(h, "response.output_text.done", { item_id: "reply", text: "Hello." });
  assert.equal(h.elements["voice-announcement"].textContent, "Assistant: Hello.");
});


test("an interrupted old response stays suppressed after a new response starts", async () => {
  const h = harness();
  await connect(h);
  await event(h, "response.created", { response: { id: "old" } });
  await h.elements["voice-interrupt"].emit("click");
  await h.elements["voice-hold"].emit("click");
  h.advance(500);
  await h.elements["voice-hold"].emit("click");
  await event(h, "response.created", { response: { id: "new" } });
  await event(h, "response.done", { response: { id: "old", status: "completed", output: [call("old-write")] } });
  await flush();
  assert.equal(h.toolCalls.length, 0);
  assert.equal(h.elements.voice.dataset.phase, "thinking");
});

test("failed model response shows recovery instead of silently returning ready", async () => {
  const h = harness();
  await connect(h);
  await event(h, "response.created");
  await event(h, "response.done", { response: { status: "failed", output: [] } });
  assert.equal(h.elements.voice.dataset.phase, "error");
  assert.match(h.elements["voice-status"].textContent, /Try asking again/);
});


test("interrupt before response.created suppresses the delayed old response", async () => {
  const h = harness();
  await connect(h);
  await h.elements["voice-hold"].emit("click");
  h.advance(500);
  await h.elements["voice-hold"].emit("click");
  await h.elements["voice-interrupt"].emit("click");
  await event(h, "response.created", { response: { id: "delayed" } });
  await event(h, "response.done", { response: { id: "delayed", status: "completed", output: [call("late-write")] } });
  await flush();
  assert.equal(h.toolCalls.length, 0);
  assert.equal(h.elements.voice.dataset.sessionState, "live");
});


for (const [name, toolResponse, expected] of [
  ["empty success body", { ok: true, status: 200, json: async () => ({}) }, "unknown"],
  ["proxy error", { ok: false, status: 502, json: async () => { throw new Error("HTML"); } }, "unknown"],
  ["structured upstream failure", { ok: false, status: 500, json: async () => ({ error: { code: "internal" } }) }, "unknown"],
  ["preview", { ok: true, status: 200, json: async () => ({ isError: false, output: '{"dryRun":true}' }) }, "preview"],
]) {
  test(`write feedback is honest for ${name}`, async () => {
    const h = harness({ toolResponse });
    await connect(h);
    await h.channel.emit("message", response("completed", [call("write")]));
    await flush(); await flush();
    const state = h.elements["voice-tools"].children[0].children[2];
    assert.equal(state.dataset.state, expected);
    assert.notEqual(state.textContent, "Saved");
  });
}


test("interrupt delivers an active action result but skips queued writes and continuation", async () => {
  let finish;
  const h = harness({ toolResponse: {
    ok: true, status: 200,
    json: () => new Promise(resolve => { finish = resolve; }),
  } });
  await connect(h);
  await event(h, "response.created", { response: { id: "action-response" } });
  await event(h, "response.done", { response: {
    id: "action-response", status: "completed", output: [call("first"), call("second")],
  } });
  await flush();
  assert.equal(h.toolCalls.length, 1);
  await h.elements["voice-interrupt"].emit("click");
  finish({ isError: false, output: '{"updated":true}' });
  await flush(); await flush();
  assert.equal(h.toolCalls.length, 1);
  assert.equal(h.channel.sent.filter(e => e.type === "conversation.item.create").length, 1);
  assert.equal(h.channel.sent.filter(e => e.type === "response.create").length, 0);
  assert.equal(h.elements["voice-tools"].children[0].children[2].textContent, "Saved");
});


test("an unreadable unauthorized response still ends the session", async () => {
  const h = harness({ toolResponse: { ok: false, status: 401, json: async () => { throw new Error("HTML"); } } });
  await connect(h);
  await h.channel.emit("message", response("completed", [call("unauthorized")]));
  await flush(); await flush();
  assert.equal(h.elements.voice.dataset.sessionState, "stopped");
  assert.equal(h.track.stopped, true);
});

test("a key OpenAI refuses leads to the key's page instead of asking to try again", async () => {
  const error = { code: "voice_key_refused", message: "OpenAI does not accept the key set on this server." };
  const h = harness({ sessionResponse: { ok: false, status: 502, json: async () => ({ error }) } });
  await h.elements["voice-connect"].emit("click");
  await flush();
  const status = h.elements["voice-status"];
  assert.equal(status.textContent, error.message);
  assert.equal(status.children.at(-1).href, "/openai-key");
});

const write = (id, name, args) => ({ arguments: JSON.stringify(args), call_id: id, name, type: "function_call" });
const cards = (h) => h.elements["voice-transcript"].children.filter((row) => row.className === "action");
const texts = (card) => card.children.map((child) => child.textContent);
const tap = async (card, text) => {
  const buttons = card.children.find((child) => child.className === "action-buttons");
  await buttons.children.find((button) => button.textContent === text).emit("click");
  await flush(); await flush();
};
const outputs = (h) => h.channel.sent
  .filter((sent) => sent.type === "conversation.item.create")
  .map((sent) => JSON.parse(sent.item.output));

test("a write waits for Allow, and its card is built from the arguments with the link in full", async () => {
  const h = harness();
  await connect(h);
  const url = "https://jobs.example.test/apply?next=https%3A%2F%2Fevil.example%2Flogin&ref=" + "a".repeat(200);
  await h.channel.emit("message", response("completed", [
    write("link", "update_application", { id: "14", company: "Trusted Bank", nextAction: "Sign in to confirm", jobUrl: url, expectedUpdatedAt: "t" }),
  ]));
  await flush(); await flush();
  assert.equal(h.toolCalls.length, 0);
  assert.equal(h.channel.sent.some((sent) => sent.type === "conversation.item.create"), false);
  assert.equal(h.elements.voice.dataset.phase, "confirm");
  const [card] = cards(h);
  // The name is the server's, whatever company the arguments claim.
  assert.deepEqual(texts(card).slice(0, 4), [
    "Change application #14: Example Robotics, AI Agent Engineer (Python)",
    "company: Trusted Bank", "nextAction: Sign in to confirm", `jobUrl: ${url}`,
  ]);
  assert.deepEqual(h.lookups, [{ name: "get_application", arguments: { id: "14" }, confirmed: false }]);
  assert.equal(texts(card).some((text) => text.includes("expectedUpdatedAt")), false);
  assert.equal(h.elements["voice-announcement"].textContent, "Allow or decline: Change application #14: Example Robotics, AI Agent Engineer (Python)");

  await tap(card, "Allow");
  assert.deepEqual(h.toolCalls.map((request) => [request.name, request.confirmed]), [["update_application", true]]);
  assert.deepEqual(outputs(h), [{ updated: true }]);
  assert.equal(card.children.find((child) => child.className === "tag").textContent, "Saved");
  assert.equal(h.channel.sent.filter((sent) => sent.type === "response.create").length, 1);
});

test("Don't answers the model that the owner declined, and nothing is fetched", async () => {
  const h = harness();
  await connect(h);
  await h.channel.emit("message", response("completed", [
    write("mail", "update_contact", { contactId: "7", applicationId: "14", name: "Your Bank", email: "attacker@evil.example" }),
  ]));
  await flush(); await flush();
  const [card] = cards(h);
  assert.deepEqual(texts(card).slice(0, 3), [
    "Change contact #7 (Person B) in application #14: Example Robotics, AI Agent Engineer (Python)",
    "name: Your Bank", "email: attacker@evil.example",
  ]);
  await tap(card, "Don’t");
  assert.equal(h.toolCalls.length, 0);
  assert.equal(outputs(h)[0].error.code, "voice_owner_declined");
  assert.equal(card.children.find((child) => child.className === "tag").textContent, "Declined");
  assert.equal(h.channel.sent.filter((sent) => sent.type === "response.create").length, 1);
  assert.equal(h.elements.voice.dataset.phase, "thinking");
});

test("a read runs at once without a card, and a note or CV patch runs at once with one", async () => {
  const h = harness();
  await connect(h);
  await h.channel.emit("message", response("completed", [
    write("read", "list_applications", {}),
    write("patch", "patch_cv", { expectedRevision: 3, summary: "Shorter", edits: [{ oldText: "old words", newText: "new words" }] }),
  ]));
  await flush(); await flush(); await flush();
  assert.deepEqual(h.toolCalls.map((request) => [request.name, request.confirmed]), [
    ["list_applications", false], ["patch_cv", false],
  ]);
  assert.equal(h.lookups.length, 0);
  const [card, more] = cards(h);
  assert.equal(more, undefined);
  assert.deepEqual(texts(card), ["Edit the CV draft: 1 edit", "summary: Shorter", "replace: old words", "with: new words", "Saved"]);
});

const ARCHIVE_14 = "Archive application #14: Example Robotics, AI Agent Engineer (Python)";

for (const [name, lookup] of [
  ["is not found", { status: 200, json: async () => ({ isError: true, output: "Application not found" }) }],
  ["fails", { ok: false, status: 502, json: async () => { throw new Error("HTML"); } }],
  ["answers for another record", { status: 200, json: async () => ({ isError: false, output: '{"id":"15","company":"Other","role":"Other"}' }) }],
]) {
  test(`a card says so when its application ${name}, and still waits`, async () => {
    const h = harness({ lookup });
    await connect(h);
    await h.channel.emit("message", response("completed", [
      write("mail", "update_contact", { contactId: "7", applicationId: "14", email: "a@b.example" }),
    ]));
    await flush(); await flush(); await flush();
    const [card] = cards(h);
    assert.equal(card.children[0].textContent, "Change contact #7 in application #14, which could not be read");
    assert.equal(h.toolCalls.length, 0);
    await tap(card, "Allow");
    assert.deepEqual(h.toolCalls.map((request) => [request.name, request.confirmed]), [["update_contact", true]]);
  });
}

test("two writes in one response get their cards in order, each with its own answer", async () => {
  const h = harness();
  await connect(h);
  await h.channel.emit("message", response("completed", [
    write("archive", "archive_application", { id: "14" }),
    write("revert", "revert_cv", { toRevision: 3, expectedRevision: 5, variant: "default" }),
  ]));
  await flush(); await flush();
  assert.deepEqual(cards(h).map((card) => card.children[0].textContent), [ARCHIVE_14]);
  assert.equal(h.lookups.length, 1);
  await tap(cards(h)[0], "Don’t");
  assert.equal(h.toolCalls.length, 0);
  assert.deepEqual(cards(h).map((card) => card.children[0].textContent), [
    ARCHIVE_14, "Revert the CV to revision 3",
  ]);
  assert.equal(h.lookups.length, 1);
  assert.equal(texts(cards(h)[1])[1], "variant: default");
  // The first card's buttons are spent: tapping them again decides nothing for the second.
  await tap(cards(h)[0], "Allow");
  assert.equal(h.toolCalls.length, 0);
  await tap(cards(h)[1], "Allow");
  assert.deepEqual(h.toolCalls.map((request) => request.name), ["revert_cv"]);
  assert.deepEqual(outputs(h).map((output) => output.error?.code), ["voice_owner_declined", undefined]);
  assert.equal(h.channel.sent.filter((sent) => sent.type === "response.create").length, 1);
});

test("ending the session declines a waiting card, and so does an interrupt", async () => {
  const h = harness();
  await connect(h);
  await h.channel.emit("message", response("completed", [write("first", "archive_application", { id: "14" })]));
  await flush(); await flush();
  await h.elements["voice-interrupt"].emit("click");
  await flush(); await flush();
  const tag = (card) => card.children.find((child) => child.className === "tag").textContent;
  assert.equal(tag(cards(h)[0]), "Declined");
  assert.equal(outputs(h)[0].error.code, "voice_owner_declined");
  assert.equal(h.channel.sent.filter((sent) => sent.type === "response.create").length, 0);

  await h.elements["voice-hold"].emit("click");
  h.advance(500);
  await h.elements["voice-hold"].emit("click");
  await event(h, "response.created", { response: { id: "next" } });
  await event(h, "response.done", { response: {
    id: "next", status: "completed", output: [write("second", "archive_application", { id: "15" })],
  } });
  await flush(); await flush();
  await h.elements["voice-stop"].emit("click");
  await flush(); await flush();
  assert.equal(tag(cards(h)[1]), "Declined");
  await tap(cards(h)[1], "Allow");
  assert.equal(h.toolCalls.length, 0);
});

test("arguments that are no object never reach a card or the server", async () => {
  const h = harness();
  await connect(h);
  await h.channel.emit("message", response("completed", [
    { arguments: "null", call_id: "null", name: "archive_application", type: "function_call" },
  ]));
  await flush(); await flush();
  assert.equal(cards(h).length, 0);
  assert.equal(h.toolCalls.length, 0);
  assert.equal(outputs(h)[0].error.code, "voice_invalid_arguments");
});

const found = (more) => ({ status: 200, json: async () => ({ isError: false, output: JSON.stringify({
  id: "14", company: "Example Robotics", role: "AI Agent Engineer (Python)", ...more,
}) }) });
const tagOf = (card) => card.children.find((child) => child.className === "tag").textContent;

for (const [name, archivedAt, title, said] of [
  ["archive_application", "2026-11-01T00:00:00.000Z", "Already archived:", "already archived"],
  ["unarchive_application", null, "Not archived:", "not archived"],
]) {
  test(`${name} of an application already there asks for no tap and tells the model why`, async () => {
    const h = harness({ lookup: found({ archivedAt }) });
    await connect(h);
    await h.channel.emit("message", response("completed", [write("noop", name, { id: "14" })]));
    await flush(); await flush(); await flush();
    const [card] = cards(h);
    assert.equal(card.children[0].textContent, `${title} application #14: Example Robotics, AI Agent Engineer (Python)`);
    assert.equal(card.children.some((child) => child.className === "action-buttons"), false);
    assert.equal(tagOf(card), "Nothing to do");
    assert.equal(h.toolCalls.length, 0);
    assert.equal(h.updates.length, 0);
    const [output] = outputs(h);
    assert.equal(output.changed, false);
    assert.match(output.message, new RegExp(`Nothing was changed.*${said}.*another application`));
    assert.equal(h.channel.sent.filter((sent) => sent.type === "response.create").length, 1);
  });
}

test("a card for a change to an archived application says that it is archived, and one whose state is unknown still waits", async () => {
  const h = harness({ lookup: found({ archivedAt: "2026-11-01T00:00:00.000Z" }) });
  await connect(h);
  await h.channel.emit("message", response("completed", [
    write("change", "update_application", { id: "14", nextAction: "Call" }),
    write("event", "record_application_event", { applicationId: "14", type: "reply" }),
    write("restore", "unarchive_application", { id: "14" }),
  ]));
  await flush(); await flush(); await flush();
  assert.equal(cards(h)[0].children[0].textContent, "Change application #14: Example Robotics, AI Agent Engineer (Python) (archived)");
  await tap(cards(h)[0], "Don’t");
  await flush();
  assert.equal(cards(h)[1].children[0].textContent, "Record “reply” in the history of application #14: Example Robotics, AI Agent Engineer (Python) (archived)");
  await tap(cards(h)[1], "Don’t");
  await flush();
  assert.equal(cards(h)[2].children[0].textContent, "Restore from the archive: application #14: Example Robotics, AI Agent Engineer (Python)");
  await tap(cards(h)[2], "Allow");
  assert.deepEqual(h.toolCalls.map((request) => request.name), ["unarchive_application"]);

  // The default lookup names no archive state at all: nothing is concluded from that.
  const unknown = harness();
  await connect(unknown);
  await unknown.channel.emit("message", response("completed", [write("restore", "unarchive_application", { id: "14" })]));
  await flush(); await flush(); await flush();
  assert.equal(tagOf(cards(unknown)[0]), "Waiting for your tap");
});

for (const [name, why] of [
  ["archive_application", ". It was already archived"],
  ["unarchive_application", ". It was not archived"],
  ["update_document_links", ""],
]) {
  test(`${name} that the server answers with changed: false reads "Nothing changed" and announces no update`, async () => {
    const output = '{"changed":false,"application":{"id":"14"}}';
    const h = harness({ toolResponse: { status: 200, json: async () => ({ isError: false, output }) } });
    await connect(h);
    await h.channel.emit("message", response("completed", [write("noop", name, { id: "14", applicationIds: [] })]));
    await flush(); await flush(); await flush();
    const [card] = cards(h);
    await tap(card, "Allow");
    assert.equal(h.toolCalls.length, 1);
    assert.equal(tagOf(card), `Nothing changed${why}`);
    assert.equal(card.children.find((child) => child.className === "tag").dataset.state, "done");
    assert.equal(h.updates.length, 0);
    assert.deepEqual(outputs(h), [JSON.parse(output)]);
  });
}

test("a dry run of a write that waits for a tap is shown and sent as the real change", async () => {
  const h = harness();
  await connect(h);
  await h.channel.emit("message", response("completed", [
    write("dry", "archive_application", { id: "14", dryRun: true }),
    write("note", "append_application_note", { applicationId: "14", note: "n" }),
  ]));
  await flush(); await flush(); await flush();
  const [card] = cards(h);
  assert.deepEqual(texts(card).slice(0, 2), [ARCHIVE_14, "Waiting for your tap"]);
  await tap(card, "Allow");
  await flush();
  assert.deepEqual(h.toolCalls[0], { name: "archive_application", arguments: { id: "14" }, confirmed: true });
  assert.equal(tagOf(card), "Saved");
  assert.deepEqual(h.updates, ["archive_application", "append_application_note"]);
});
