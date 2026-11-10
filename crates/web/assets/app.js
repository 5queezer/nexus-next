// Progressive enhancement only: every page and form works without this file.
if ("serviceWorker" in navigator) {
  window.addEventListener("load", () => {
    navigator.serviceWorker.register("/sw.js", { scope: "/" }).catch(() => {});
  });
}

// A second tap on a slow connection would send the form twice.
document.addEventListener("submit", (event) => {
  const form = event.target;
  if (!(form instanceof HTMLFormElement) || form.method !== "post") return;
  // When this happened is when it was sent, not when the page was built. Once only: the time is part
  // of what makes a resent form the same change.
  const at = form.querySelector("input[name=at]");
  if (at && !at.dataset.sent) {
    at.value = new Date().toISOString();
    at.dataset.sent = "true";
  }
  for (const button of form.querySelectorAll("button[type=submit]")) {
    button.disabled = true;
  }
});

// Back/forward cache restores the page with its buttons still disabled.
window.addEventListener("pageshow", (event) => {
  if (!event.persisted) return;
  for (const button of document.querySelectorAll("form button[type=submit]")) {
    button.disabled = false;
  }
});

// The assistant lives in a named window, so navigating Nexus does not end its WebRTC session. The
// link carries only an immutable record ID (or CV variant name), never page text that could become
// an instruction to the model.
const assistantLink = document.getElementById("nav-assistant");
if (assistantLink && window.location.pathname !== "/assistant") {
  const application = window.location.pathname.match(/^\/applications\/(\d+)$/);
  const cv = window.location.pathname.match(/^\/cv\/variants\/([^/]+)$/);
  const params = new URLSearchParams();
  if (application) params.set("applicationId", application[1]);
  if (cv) {
    try {
      params.set("variant", decodeURIComponent(cv[1]));
    } catch {
      // An invalid path segment is no useful assistant context.
    }
  }
  if (params.size) assistantLink.href = `/assistant?${params}`;

  assistantLink.addEventListener("click", (event) => {
    if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
    event.preventDefault();
    const assistant = window.open("", "nexus-assistant");
    if (!assistant) return;
    // An active assistant keeps its page, WebRTC connection and pinned context. A stopped assistant
    // may move to the new page context without risking an in-flight conversation.
    try {
      const state = assistant.document.getElementById("voice")?.dataset.sessionState;
      if (assistant.location.href === "about:blank" || !["connecting", "live"].includes(state)) {
        assistant.location.replace(assistantLink.href);
      }
      assistant.focus();
    } catch {
      // Nexus never deliberately puts this named window on another origin. If it happened anyway,
      // leave it untouched instead of navigating an unknown page.
    }
  });
}

// A successful write in the assistant never reloads this page by itself: a form may contain
// unsaved work. It gives the person a deliberate refresh instead.
function showAssistantUpdate(message) {
  // Reloading the assistant would end its live voice session.
  if (window.location.pathname === "/assistant") return;
  if (message?.type !== "nexus-assistant-updated") return;
  const note = document.getElementById("assistant-update-note");
  if (note) note.hidden = false;
}

window.addEventListener("message", (event) => {
  if (event.origin !== window.location.origin) return;
  showAssistantUpdate(event.data);
});

if ("BroadcastChannel" in window) {
  const assistantUpdates = new BroadcastChannel("nexus-assistant");
  assistantUpdates.addEventListener("message", (event) => showAssistantUpdate(event.data));
}

document.getElementById("assistant-refresh")?.addEventListener("click", () => {
  window.location.reload();
});
