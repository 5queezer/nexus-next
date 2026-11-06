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
  let shownState = root.dataset.state;
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
  function showImages(revision, pages, state) {
    const box = byId("cv-preview-pages");
    while (box.children.length > pages) box.lastElementChild.remove();
    for (let page = 1; page <= pages; page += 1) {
      let image = box.children[page - 1];
      if (!image) {
        image = document.createElement("img");
        box.append(image);
      }
      image.alt = `Preview of revision ${revision}, page ${page}`;
      image.src = `${base}/preview.png?revision=${revision}&page=${page}&state=${state}`;
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
      showImages(body.revision, body.pages, body.state);
    } else {
      const tokens = (body.tokens || []).join(", ");
      setNote(byId("cv-preview-note"), tokens ? `${body.message} Found: ${tokens}` : body.message);
      byId("cv-preview-pages").replaceChildren();
    }
  }

  // A tap can only approve what the owner asked to see. So a form that arrived with a revision by
  // itself never goes on the page: the old one goes, and this link in its place loads the new text.
  function askToRead(revision) {
    const notice = document.createElement("p");
    notice.className = "banner warn";
    notice.id = "cv-approval-stale";
    notice.setAttribute("role", "status");
    const link = document.createElement("a");
    link.href = `${base}?read=${revision}#cv-approval`;
    link.textContent = `Revision ${revision} landed: read it again`;
    notice.append(link);
    const panel = byId("cv-approval");
    const old = byId("cv-approval-stale") || panel.querySelector(".approve-form");
    if (old) old.replaceWith(notice);
    else panel.querySelector(".approval").prepend(notice);
  }

  // Takes the revision list and head from a fresh copy of this page. The approval panel follows only
  // if it brings no form. The editor stays as it is, typed in or not, and its old base makes a save a
  // clear conflict: it must never come to hold, and then save, text the owner didn't read.
  async function followHead(revision, note) {
    let fresh;
    try {
      const response = await fetch(base);
      if (!response.ok) return false;
      fresh = new DOMParser().parseFromString(await response.text(), "text/html");
    } catch {
      return false;
    }
    for (const id of ["cv-revisions", "cv-revision", "cv-state", "cv-preview-actions", "cv-preview-caption", "nav-cv"]) {
      const next = fresh.getElementById(id);
      if (next) byId(id).replaceWith(document.adoptNode(next));
    }
    const panel = fresh.getElementById("cv-approval");
    if (panel && panel.querySelector(".approve-form")) {
      askToRead(fresh.getElementById("cv-workspace").dataset.revision);
    } else if (panel) {
      byId("cv-approval").replaceWith(document.adoptNode(panel));
    }
    const editor = byId("cv-source");
    const nextExpected = fresh.getElementById("cv-expected");
    if (editor && nextExpected) {
      if (byId("cv-expected").value === nextExpected.value) {
        // The same base, approved since: the button says what a save does now. A form that came
        // through a conflict stores a draft, whatever a fresh page's button would say.
        if (byId("cv-from-base")) {
          byId("cv-save").replaceWith(document.adoptNode(fresh.getElementById("cv-save")));
        }
        setNote(byId("cv-live-note"), note);
      } else {
        setNote(
          byId("cv-live-note"),
          `${note} The source below stays on the older revision, so saving it is refused until you reload. Nothing you typed is lost.`,
        );
      }
    }
    return true;
  }

  function arrived(revision, note) {
    if (revision <= shown) return;
    shown = revision;
    root.dataset.revision = String(revision);
    loadPreview(revision);
    followHead(revision, note);
  }

  // The owner approved what is on screen, here or on another device: the stamp goes, in place. Only
  // counted once the page shows it. If the fetch failed the stream may stay open and never say this
  // again, so the slow poll asks until it has been shown.
  async function approved(revision) {
    if (revision !== shown || shownState === "approved") return;
    const followed = await followHead(revision, `Revision ${revision} is approved.`);
    if (!followed) {
      startPolling();
      return;
    }
    if (stream.readyState === EventSource.OPEN) stopPolling();
    if (revision !== shown) return;
    shownState = "approved";
    root.dataset.state = shownState;
    loadPreview(revision);
  }

  // What the head is now, heard after a gap: a poll, or a stream that has just opened.
  function headIs(revision, state) {
    arrived(revision, `Revision ${revision} landed.`);
    if (state === "approved") {
      approved(revision);
    } else if (revision === shown) {
      shownState = state;
      root.dataset.state = state;
    }
  }

  async function poll() {
    try {
      const response = await fetch(`/api/cv/${encodeURIComponent(variant)}/head`);
      if (response.ok) {
        const { revision, state } = await response.json();
        headIs(revision, state);
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
      const { revision, state } = JSON.parse(event.data);
      headIs(revision, state);
    });
    stream.addEventListener("revision", (event) => {
      const { revision, origin, summary, state } = JSON.parse(event.data);
      shownState = state;
      root.dataset.state = state;
      arrived(revision, `Revision ${revision} landed (${origin}: ${summary}).`);
    });
    stream.addEventListener("approved", (event) => {
      approved(JSON.parse(event.data).revision);
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
