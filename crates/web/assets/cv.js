// The variant page's live preview. Without this file the page still saves, reverts and previews:
// it just needs a reload to show what another writer stored.
(() => {
  const root = document.getElementById("cv-workspace");
  if (!root) return;

  const POLL_MS = 30000;
  const RETRY_STREAM_MS = 60000;
  const variant = root.dataset.variant;
  const base = `/cv/variants/${encodeURIComponent(variant)}`;
  let shown = Number(root.dataset.revision);
  let previewRun = 0;
  let pollTimer = null;
  let stream = null;

  const byId = (id) => document.getElementById(id);

  function setNote(element, text) {
    element.textContent = text;
    element.hidden = !text;
  }

  // Shows `pages` images of a revision, reusing the ones there so the old stays up until the new
  // has loaded.
  function showImages(revision, pages) {
    const box = byId("cv-preview-pages");
    while (box.children.length > pages) box.lastElementChild.remove();
    for (let page = 1; page <= pages; page += 1) {
      let image = box.children[page - 1];
      if (!image) {
        image = document.createElement("img");
        box.append(image);
      }
      image.alt = `Preview of revision ${revision}, page ${page}`;
      image.src = `${base}/preview.png?revision=${revision}&page=${page}`;
    }
  }

  async function loadPreview(revision) {
    const run = ++previewRun;
    let body;
    let ok = false;
    try {
      const response = await fetch(`${base}/preview.json?revision=${revision}`);
      ok = response.ok;
      body = await response.json();
    } catch {
      if (run === previewRun) setNote(byId("cv-preview-note"), "The preview could not be loaded.");
      return;
    }
    if (run !== previewRun) return;
    if (ok) {
      setNote(byId("cv-preview-note"), "");
      showImages(body.revision, body.pages);
    } else {
      const tokens = (body.tokens || []).join(", ");
      setNote(byId("cv-preview-note"), tokens ? `${body.message} Found: ${tokens}` : body.message);
      byId("cv-preview-pages").replaceChildren();
    }
    byId("cv-pdf").href = `${base}/cv.pdf?revision=${revision}`;
  }

  // Takes the revision list and head from a fresh copy of this page. An untouched editor follows
  // the head. One holding someone's typing stays, and its old base makes a save a clear conflict.
  // `kept` marks text a refused save came back with, which is typing though it looks untouched.
  async function followHead(revision, note) {
    let fresh;
    try {
      const response = await fetch(base);
      if (!response.ok) return;
      fresh = new DOMParser().parseFromString(await response.text(), "text/html");
    } catch {
      return;
    }
    for (const id of ["cv-revisions", "cv-head-tokens", "cv-revision"]) {
      const next = fresh.getElementById(id);
      if (next) byId(id).replaceWith(document.adoptNode(next));
    }
    const editor = byId("cv-source");
    const nextEditor = fresh.getElementById("cv-source");
    const nextExpected = fresh.getElementById("cv-expected");
    if (editor && nextEditor && nextExpected) {
      if (editor.value === editor.defaultValue && !("kept" in editor.dataset)) {
        editor.defaultValue = nextEditor.defaultValue;
        editor.value = nextEditor.defaultValue;
        byId("cv-expected").value = nextExpected.value;
        setNote(byId("cv-live-note"), note);
      } else {
        setNote(
          byId("cv-live-note"),
          `${note} Your unsaved text is untouched and still based on the older revision.`,
        );
      }
    }
  }

  function arrived(revision, note) {
    if (revision <= shown) return;
    shown = revision;
    root.dataset.revision = String(revision);
    loadPreview(revision);
    followHead(revision, note);
  }

  async function poll() {
    try {
      const response = await fetch(`/api/cv/${encodeURIComponent(variant)}/head`);
      if (response.ok) {
        const { revision } = await response.json();
        arrived(revision, `Revision ${revision} landed.`);
      }
    } catch {
      // Still offline: the next round tries again.
    }
  }

  function startPolling() {
    if (pollTimer === null) pollTimer = setInterval(poll, POLL_MS);
  }

  function stopPolling() {
    if (pollTimer !== null) clearInterval(pollTimer);
    pollTimer = null;
  }

  function connect() {
    stream = new EventSource(`/api/cv/${encodeURIComponent(variant)}/events`);
    stream.addEventListener("open", stopPolling);
    stream.addEventListener("head", (event) => {
      const { revision } = JSON.parse(event.data);
      arrived(revision, `Revision ${revision} landed.`);
    });
    stream.addEventListener("revision", (event) => {
      const { revision, origin, summary } = JSON.parse(event.data);
      arrived(revision, `Revision ${revision} landed (${origin}: ${summary}).`);
    });
    stream.addEventListener("error", () => {
      startPolling();
      // A refused stream (the 4 are taken, say) stays closed unless asked again.
      if (stream.readyState === EventSource.CLOSED) setTimeout(connect, RETRY_STREAM_MS);
    });
  }

  window.addEventListener("pagehide", () => stream && stream.close());
  window.addEventListener("pageshow", (event) => {
    if (event.persisted) connect();
  });

  loadPreview(shown);
  connect();
})();
