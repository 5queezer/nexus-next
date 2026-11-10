const assert = require("node:assert/strict");
const fs = require("node:fs");
const test = require("node:test");
const vm = require("node:vm");

const source = fs.readFileSync(require.resolve("./app.js"), "utf8");

for (const pathname of ["/assistant", "/applications/42", "/cv/variants/default"]) {
  for (const transport of ["message", "broadcast"]) {
    test(`${transport} update notice stays off the assistant: ${pathname}`, () => {
      const note = { hidden: true };
      const listeners = {};
      let broadcast;
      class BroadcastChannel {
        addEventListener(type, listener) {
          if (type === "message") broadcast = listener;
        }
      }
      const window = {
        BroadcastChannel,
        location: { pathname, origin: "https://nexus.test" },
        addEventListener(type, listener) { listeners[type] = listener; },
      };
      vm.runInNewContext(source, {
        BroadcastChannel,
        URLSearchParams,
        navigator: {},
        window,
        document: {
          addEventListener() {},
          getElementById: (id) => id === "assistant-update-note" ? note : null,
        },
      });
      const receive = transport === "broadcast" ? broadcast : listeners.message;
      receive({ origin: window.location.origin, data: { type: "unrelated" } });
      assert.equal(note.hidden, true);
      if (transport === "message") {
        receive({ origin: "https://other.test", data: { type: "nexus-assistant-updated" } });
        assert.equal(note.hidden, true);
      }
      receive({ origin: window.location.origin, data: { type: "nexus-assistant-updated" } });
      assert.equal(note.hidden, pathname === "/assistant");
    });
  }
}

test("assistant launch preserves a live session and replaces stopped context", () => {
  let click;
  let replacements = 0;
  const voice = { dataset: { sessionState: "stopped" } };
  const assistant = {
    document: { getElementById: (id) => (id === "voice" ? voice : null) },
    focus() {},
    location: {
      href: "about:blank",
      replace(next) {
        this.href = next;
        replacements += 1;
      },
    },
  };
  const link = {
    href: "/assistant",
    addEventListener(type, listener) {
      if (type === "click") click = listener;
    },
  };
  const document = {
    addEventListener() {},
    getElementById(id) {
      return id === "nav-assistant" ? link : null;
    },
  };
  const window = {
    addEventListener() {},
    location: { origin: "https://nexus.test", pathname: "/applications/42", reload() {} },
    open: () => assistant,
  };
  class BroadcastChannel {
    addEventListener() {}
  }
  vm.runInNewContext(source, {
    BroadcastChannel,
    URLSearchParams,
    document,
    navigator: {},
    window,
  });

  const event = {
    altKey: false,
    button: 0,
    ctrlKey: false,
    metaKey: false,
    preventDefault() {},
    shiftKey: false,
  };
  click(event);
  assert.equal(assistant.location.href, "/assistant?applicationId=42");
  assert.equal(replacements, 1);

  voice.dataset.sessionState = "live";
  link.href = "/assistant?applicationId=99";
  assistant.sentinel = "conversation survives";
  click(event);
  assert.equal(assistant.location.href, "/assistant?applicationId=42");
  assert.equal(assistant.sentinel, "conversation survives");
  assert.equal(replacements, 1);

  voice.dataset.sessionState = "stopped";
  click(event);
  assert.equal(assistant.location.href, "/assistant?applicationId=99");
  assert.equal(replacements, 2);
});
