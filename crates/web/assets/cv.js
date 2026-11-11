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

  // Shows `pages` images, reusing the ones there so the old stays up until the new has loaded.
  function showPages(pages, alt, src) {
    const box = byId("cv-preview-pages");
    while (box.children.length > pages) box.lastElementChild.remove();
    for (let page = 1; page <= pages; page += 1) {
      let image = box.children[page - 1];
      if (!image) {
        image = document.createElement("img");
        box.append(image);
      }
      image.alt = alt(page);
      image.src = src(page);
    }
  }

  function showImages(revision, pages, state) {
    showPages(
      pages,
      (page) => `Preview of revision ${revision}, page ${page}`,
      (page) => `${base}/preview.png?revision=${revision}&page=${page}&state=${state}`,
    );
  }

  // While a draft is read for approval the picture is the one its form was compiled with, and stays.
  async function loadPreview(revision) {
    if (!byId("cv-preview-pages")) return;
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
    byId("cv-preview-unsaved").hidden = true;
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
    link.href = `${base}?view=review&read=${revision}#cv-approval`;
    link.textContent = `Revision ${revision} landed: read it again`;
    notice.append(link);
    const panel = byId("cv-approval");
    if (!panel) return;
    const old = byId("cv-approval-stale") || panel.querySelector(".approve-form");
    if (old) old.replaceWith(notice);
    else panel.querySelector(".approval").prepend(notice);
  }

  // Takes the revision list and head from a fresh copy of this page, in the view that is open, so
  // only what that view holds is there to take. The approval panel follows only
  // if it brings no form. The editor stays as it is, typed in or not, and its old base makes a save a
  // clear conflict: it must never come to hold, and then save, text the owner didn't read.
  async function followHead(revision, note) {
    let fresh;
    try {
      const response = await fetch(`${base}?view=${root.dataset.view}`);
      if (!response.ok) return false;
      fresh = new DOMParser().parseFromString(await response.text(), "text/html");
    } catch {
      return false;
    }
    const take = (id) => {
      const next = fresh.getElementById(id);
      if (next && byId(id)) byId(id).replaceWith(document.adoptNode(next));
    };
    for (const id of ["cv-revisions", "cv-revision", "cv-state", "cv-modes", "cv-preview-actions", "cv-preview-caption", "nav-cv"]) {
      take(id);
    }
    const panel = fresh.getElementById("cv-approval");
    if (panel && panel.querySelector(".approve-form")) {
      askToRead(fresh.getElementById("cv-workspace").dataset.revision);
    } else if (panel && byId("cv-approval")) {
      // Nothing is left to approve, so the picture that stood for the form gives way to the preview.
      take("cv-approval");
      take("cv-picture-pane");
      loadPreview(revision);
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
    } else {
      setNote(byId("cv-live-note"), note);
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
    const followed = await followHead(revision, `Revision ${revision} of ${variant} is approved.`);
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

  // The button comes after the picture and the list in the page. On a wide screen it is in sight from
  // the start, so it waits there until the list has been scrolled to its end, or fits without scrolling.
  function holdApproval() {
    const button = byId("cv-approve");
    const end = byId("cv-checks-end");
    if (!button || !end || !("IntersectionObserver" in window)) return;
    button.disabled = true;
    byId("cv-approve-hint").hidden = false;
    const watcher = new IntersectionObserver(
      (entries) => {
        if (!entries.some((entry) => entry.isIntersecting)) return;
        button.disabled = false;
        byId("cv-approve-hint").hidden = true;
        watcher.disconnect();
      },
      // On a phone the form covers the bottom of the screen, and nothing under it has been read.
      { rootMargin: `0px 0px -${button.form.offsetHeight}px 0px` },
    );
    watcher.observe(end);
  }

  // A word in the list shows where it stands in the picture while the pointer or the focus is on it.
  // A finger has neither, so a tap shows it and a second tap takes it away.
  function markInPicture() {
    const pane = byId("cv-picture-pane");
    const still = window.matchMedia("(prefers-reduced-motion: reduce)");
    let marks = [];
    let marked = null;
    let tapped = null;
    const clear = () => {
      for (const mark of marks) mark.remove();
      marks = [];
      marked = null;
    };
    const show = (chip) => {
      if (marked === chip) return;
      clear();
      marked = chip;
      for (const place of chip.dataset.boxes.split(";")) {
        const [page, box] = place.split(":");
        const picture = byId(`cv-page-${page}`);
        if (!picture) continue;
        const [left, top, width, height] = box.split(",").map((share) => `${share * 100}%`);
        const mark = document.createElement("span");
        mark.className = `picture-mark ${chip.className}`;
        Object.assign(mark.style, { left, top, width, height });
        picture.append(mark);
        marks.push(mark);
      }
      if (!marks.length) return;
      // Only the picture's own side scrolls: on a narrow screen it has none, and the page stays put.
      const at = marks[0].getBoundingClientRect();
      const seen = pane.getBoundingClientRect();
      const margin = 24;
      const up = Math.min(0, at.top - margin - seen.top);
      const down = Math.max(0, at.bottom + margin - seen.bottom);
      if (up || down) pane.scrollBy({ top: up || down, behavior: still.matches ? "auto" : "smooth" });
    };
    for (const chip of document.querySelectorAll("#cv-approval mark[data-boxes]")) {
      chip.tabIndex = 0;
      chip.addEventListener("pointerenter", (event) => {
        if (event.pointerType === "mouse") show(chip);
      });
      chip.addEventListener("pointerleave", (event) => {
        if (event.pointerType === "mouse") clear();
      });
      chip.addEventListener("focus", () => show(chip));
      chip.addEventListener("blur", clear);
      // The tap's focus comes before its click, so what the tap found is kept from before both.
      chip.addEventListener("pointerdown", (event) => {
        tapped = event.pointerType === "mouse" ? null : marked === chip;
      });
      chip.addEventListener("click", () => {
        if (tapped === null) return;
        if (tapped) clear();
        else show(chip);
        tapped = null;
      });
    }
  }

  // The text in the editor, drawn as it stands. A text that can't be drawn says why and leaves the
  // last picture up. Back at the stored text, the stored revision's picture returns.
  async function tryPreview() {
    const editor = byId("cv-source");
    if (!byId("cv-preview-pages")) return;
    if (editor.value === editor.defaultValue && !byId("cv-editor-form").dataset.unsaved) {
      loadPreview(shown);
      return;
    }
    const run = ++previewRun;
    const form = new URLSearchParams({ csrf: editor.form.elements.csrf.value, source: editor.value });
    let body;
    let ok = false;
    try {
      const response = await fetch(`${base}/try`, { method: "POST", body: form });
      ok = response.ok;
      body = await response.json();
    } catch {
      if (run === previewRun) setNote(byId("cv-preview-note"), "The preview could not be loaded.");
      return;
    }
    if (run !== previewRun) return;
    if (!ok) {
      const tokens = (body.tokens || []).join(", ");
      setNote(byId("cv-preview-note"), tokens ? `${body.message} Found: ${tokens}` : body.message);
      return;
    }
    setNote(byId("cv-preview-note"), "");
    byId("cv-preview-unsaved").hidden = false;
    showPages(
      body.pages,
      (page) => `Preview of the text in the editor, page ${page}`,
      (page) => `${base}/try.png?key=${body.key}&version=${body.version}&page=${page}`,
    );
  }

  const SHOW = "#show: cv";

  // The line that sets up the page, as the server reads it: exactly one, with only `lang` and
  // `template` as quoted words. Null for any other, which is then left alone.
  function showLine(source) {
    let found = null;
    let at = 0;
    for (const line of source.split("\n")) {
      const text = line.trimEnd();
      if (text.startsWith(SHOW)) {
        if (found) return null;
        let args = text.slice(SHOW.length);
        if (args) {
          if (!args.startsWith(".with(") || !args.endsWith(")")) return null;
          args = args.slice(6, -1);
        }
        const set = {};
        for (const arg of args.split(",")) {
          if (!arg.trim()) continue;
          const colon = arg.indexOf(":");
          const key = arg.slice(0, colon).trim();
          const value = /^"([A-Za-z0-9-]*)"$/.exec(arg.slice(colon + 1).trim());
          if (colon < 0 || !value || !["lang", "template"].includes(key) || key in set) return null;
          set[key] = value[1];
        }
        found = { start: at, end: at + text.length, set };
      }
      at += line.length + 1;
    }
    return found;
  }

  // The template select and the editor's `#show: cv` line say the same thing, whichever was changed.
  function chooseTemplate() {
    const editor = byId("cv-source");
    const select = byId("cv-template");
    if (!editor || !select) return;
    let timer = null;
    byId("cv-template-save").hidden = true;
    byId("cv-template-hint").hidden = true;
    const follow = () => {
      const line = showLine(editor.value);
      select.disabled = !line;
      byId("cv-template-unreadable").hidden = !!line;
      // An unknown name selects nothing.
      if (line) select.value = line.set.template || select.options[0].value;
    };
    select.addEventListener("change", () => {
      const line = showLine(editor.value);
      if (!line) return;
      const args = [];
      if ("lang" in line.set) args.push(`lang: "${line.set.lang}"`);
      if (select.value !== select.options[0].value) args.push(`template: "${select.value}"`);
      const show = args.length ? `${SHOW}.with(${args.join(", ")})` : SHOW;
      editor.value = editor.value.slice(0, line.start) + show + editor.value.slice(line.end);
      clearTimeout(timer);
      tryPreview();
    });
    editor.addEventListener("input", () => {
      follow();
      clearTimeout(timer);
      timer = setTimeout(tryPreview, 700);
    });
    follow();
  }

  // A view is another page, so leaving the editor with unsaved text asks first.
  function guardTyping() {
    const editor = byId("cv-source");
    if (!editor) return;
    const form = byId("cv-editor-form");
    let sending = false;
    form.addEventListener("submit", () => {
      sending = true;
    });
    // Text a refused save brought back is unsaved too, though the page came with it.
    window.addEventListener("beforeunload", (event) => {
      const unsaved = form.dataset.unsaved || editor.value !== editor.defaultValue;
      if (!sending && unsaved) event.preventDefault();
    });
  }

  window.addEventListener("pagehide", () => stream && stream.close());
  window.addEventListener("pageshow", (event) => {
    if (event.persisted) connect();
  });

  holdApproval();
  markInPicture();
  guardTyping();
  chooseTemplate();
  loadPreview(shown);
  connect();
})();
