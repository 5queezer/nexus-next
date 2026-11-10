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
  const fetch = async (url, options = {}) => {
    if (url === "/api/voice/session") {
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
      toolCalls.push(JSON.parse(options.body));
      if (toolResponse) return toolResponse;
      return { json: async () => ({ isError: false, output: '{"updated":true}' }) };
    }
    throw new Error(`unexpected fetch: ${url}`);
  };
  const window = { addEventListener() {}, location: { origin: "https://nexus.test" }, opener: null };
  class BroadcastChannel {
    close() {}
    postMessage() {}
  }
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
  return { channel, elements, toolCalls, document, documentListeners, track, advance: (ms) => { now += ms; } };
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

const call = (id) => ({
  arguments: '{"id":"1","notes":"changed"}',
  call_id: id,
  name: "update_application",
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
  assert.deepEqual(h.toolCalls.map((request) => request.arguments.id), ["1"]);
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
