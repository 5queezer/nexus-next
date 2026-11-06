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
