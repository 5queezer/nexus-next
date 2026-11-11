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
  // Says whether the picture is up, or null if a later call took over.
  async function loadPreview(revision) {
    if (!byId("cv-preview-pages")) return null;
    const run = ++previewRun;
    let body;
    let ok = false;
    try {
      const response = await fetch(`${base}/preview.json?revision=${revision}`);
      ok = response.ok;
      body = await response.json();
    } catch {
      if (run !== previewRun) return null;
      setNote(byId("cv-preview-note"), "The preview could not be loaded.");
      return false;
    }
    if (run !== previewRun) return null;
    byId("cv-preview-unsaved").hidden = true;
    byId("cv-preview-layout").hidden = true;
    if (ok) {
      setNote(byId("cv-preview-note"), "");
      showImages(body.revision, body.pages, body.state);
    } else {
      const tokens = (body.tokens || []).join(", ");
      setNote(byId("cv-preview-note"), tokens ? `${body.message} Found: ${tokens}` : body.message);
      byId("cv-preview-pages").replaceChildren();
    }
    return ok;
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
  // A finger has neither, so a tap shows it and a second tap takes it away. A rule in the legend shows
  // all its words at once, and a click keeps them shown until another click or Escape.
  function markInPicture() {
    const pane = byId("cv-picture-pane");
    const still = window.matchMedia("(prefers-reduced-motion: reduce)");
    const chips = [...document.querySelectorAll("#cv-approval mark[data-rule], #cv-approval mark[data-boxes]")];
    let marks = [];
    let marked = null;
    let tapped = null;
    let pinned = null;
    const clear = () => {
      for (const mark of marks) mark.remove();
      for (const chip of chips) chip.classList.remove("dim");
      marks = [];
      marked = null;
    };
    // Only a pane's own side scrolls: on a narrow screen it has none, and the page stays put.
    const bring = (side, element) => {
      const at = element.getBoundingClientRect();
      const seen = side.getBoundingClientRect();
      const margin = 24;
      const up = Math.min(0, at.top - margin - seen.top);
      const down = Math.max(0, at.bottom + margin - seen.bottom);
      if (up || down) side.scrollBy({ top: up || down, behavior: still.matches ? "auto" : "smooth" });
    };
    const draw = (from) => {
      for (const place of (from.dataset.boxes || "").split(";").filter(Boolean)) {
        const [page, box] = place.split(":");
        const picture = byId(`cv-page-${page}`);
        if (!picture) continue;
        const [left, top, width, height] = box.split(",").map((share) => `${share * 100}%`);
        const mark = document.createElement("span");
        mark.className = `picture-mark ${from.className}`;
        Object.assign(mark.style, { left, top, width, height });
        picture.append(mark);
        marks.push(mark);
      }
    };
    const show = (chip) => {
      if (marked === chip) return;
      clear();
      marked = chip;
      draw(chip);
      if (marks.length) bring(pane, marks[0]);
    };
    const showRule = (row) => {
      if (marked === row) return;
      clear();
      marked = row;
      for (const chip of chips) {
        if (chip.dataset.rule === row.dataset.rule) draw(chip);
        else chip.classList.add("dim");
      }
    };
    // What shows when nothing is pointed at: the pinned rule's words, or nothing.
    const rest = () => {
      if (pinned) showRule(pinned);
      else clear();
    };
    const pin = (row) => {
      for (const other of document.querySelectorAll("#cv-legend li")) {
        other.setAttribute("aria-pressed", String(other === row));
      }
      pinned = row;
      rest();
    };
    for (const chip of chips.filter((chip) => chip.dataset.boxes)) {
      chip.tabIndex = 0;
      chip.addEventListener("pointerenter", (event) => {
        if (event.pointerType === "mouse") show(chip);
      });
      chip.addEventListener("pointerleave", (event) => {
        if (event.pointerType === "mouse") rest();
      });
      chip.addEventListener("focus", () => show(chip));
      chip.addEventListener("blur", rest);
      // The tap's focus comes before its click, so what the tap found is kept from before both.
      chip.addEventListener("pointerdown", (event) => {
        tapped = event.pointerType === "mouse" ? null : marked === chip;
      });
      chip.addEventListener("click", () => {
        if (tapped === null) return;
        if (tapped) rest();
        else show(chip);
        tapped = null;
      });
    }
    const rows = document.querySelectorAll("#cv-legend li[data-rule]");
    if (!rows.length) return;
    byId("cv-legend-hint").hidden = false;
    for (const row of rows) {
      row.tabIndex = 0;
      row.setAttribute("role", "button");
      row.setAttribute("aria-pressed", "false");
      row.addEventListener("pointerenter", (event) => {
        if (event.pointerType === "mouse") showRule(row);
      });
      row.addEventListener("pointerleave", (event) => {
        if (event.pointerType === "mouse") rest();
      });
      row.addEventListener("focus", () => showRule(row));
      row.addEventListener("blur", rest);
      const press = () => {
        if (pinned === row) {
          pin(null);
          return;
        }
        pin(row);
        const own = chips.filter((chip) => chip.dataset.rule === row.dataset.rule);
        const first = own[0];
        if (!first) return;
        // Some of a rule's words can be among the ones folded away.
        for (const chip of own) {
          const folded = chip.closest("details");
          if (folded) folded.open = true;
        }
        const side = first.closest(".cv-panel");
        if (side) bring(side, first);
        if (marks.length) bring(pane, marks[0]);
      };
      row.addEventListener("click", press);
      row.addEventListener("keydown", (event) => {
        if (event.key !== "Enter" && event.key !== " ") return;
        event.preventDefault();
        press();
      });
    }
    document.addEventListener("keydown", (event) => {
      if (event.key === "Escape" && pinned) pin(null);
    });
    document.addEventListener("click", (event) => {
      if (pinned && !event.target.closest("#cv-legend li, #cv-approval mark, .picture-mark")) pin(null);
    });
  }

  // A link from a word in the review names it in `find`: the editor opens with the word selected, or
  // says that the text doesn't hold it. Whole words only, in any case.
  function findInSource() {
    const editor = byId("cv-source");
    const word = new URLSearchParams(location.search).get("find");
    if (!editor || !word) return;
    const text = editor.value.toLowerCase();
    const letter = /[\p{L}\p{N}]/u;
    let at = -1;
    do {
      at = text.indexOf(word.toLowerCase(), at + 1);
    } while (at >= 0 && (letter.test(text[at - 1] || "") || letter.test(text[at + word.length] || "")));
    if (at < 0) {
      byId("cv-find-miss").hidden = false;
      return;
    }
    editor.focus();
    editor.setSelectionRange(at, at + word.length);
    // A textarea doesn't scroll to its selection by itself: the word's line goes to the middle.
    const line = parseFloat(getComputedStyle(editor).lineHeight) || 1.4 * parseFloat(getComputedStyle(editor).fontSize);
    const above = editor.value.slice(0, at).split("\n").length - 1;
    editor.scrollTop = Math.max(0, above * line - editor.clientHeight / 2);
  }

  // What the preview is to the editor's text, said beside the Compile button. A `problem` with a line
  // goes on a button that editor.js takes to that line, and null takes the last one away.
  function compileState(text, problem) {
    byId("cv-compile-state").textContent = text;
    if (problem === undefined) return;
    const button = byId("cv-error");
    button.hidden = !problem;
    if (problem) {
      button.textContent = `Line ${problem.line}, column ${problem.column}: ${[problem.message, ...problem.hints].join(" ")}`;
      button.dataset.line = problem.line;
      button.dataset.column = problem.column;
    }
    root.dispatchEvent(new Event("cv-compiled"));
  }

  // The text in the editor, drawn as it stands. A text that can't be drawn says why and leaves the
  // last picture up. Back at the stored text, the stored revision's picture returns.
  async function tryPreview() {
    const editor = byId("cv-source");
    if (!byId("cv-preview-pages")) return;
    compileState("Compiling\u2026");
    if (editor.value === editor.defaultValue && !byId("cv-editor-form").dataset.unsaved) {
      const drawn = await loadPreview(shown);
      if (drawn !== null) compileState(drawn ? "Preview is up to date" : "", null);
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
      if (run !== previewRun) return;
      setNote(byId("cv-preview-note"), "The preview could not be loaded.");
      compileState("The preview could not be loaded.");
      return;
    }
    if (run !== previewRun) return;
    if (!ok) {
      const tokens = (body.tokens || []).join(", ");
      const note = tokens ? `${body.message} Found: ${tokens}` : body.message;
      setNote(byId("cv-preview-note"), note);
      // An error inside the template library has no line here that says more than its message.
      const first = (body.diagnostics || [])[0];
      const placed = first && first.line && first.file === "/main.typ" ? first : null;
      compileState(placed ? "Does not compile:" : first ? `Does not compile: ${first.message}` : note, placed);
      return;
    }
    compileState("Preview is up to date", null);
    setNote(byId("cv-preview-note"), "");
    byId("cv-preview-unsaved").hidden = false;
    const notes = byId("cv-preview-layout");
    notes.replaceChildren(
      ...body.layout.map((text) => Object.assign(document.createElement("li"), { textContent: text })),
    );
    notes.hidden = !body.layout.length;
    showPages(
      body.pages,
      (page) => `Preview of the text in the editor, page ${page}`,
      (page) => `${base}/try.png?key=${body.key}&version=${body.version}&page=${page}`,
    );
  }

  const SHOW = "#show: cv";

  // The line that sets up the page, as the server reads it: exactly one, with only `lang` and
  // `template` as quoted words, `pages` as 1 or 2 and `density` from -2 to 2. Null for any other,
  // which is then left alone.
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
          const said = arg.slice(colon + 1).trim();
          const shape = { pages: /^([12])$/, density: /^(-?[12]|0)$/ }[key] || /^"([A-Za-z0-9-]*)"$/;
          const value = shape.exec(said);
          if (colon < 0 || !value || !["lang", "template", "pages", "density"].includes(key) || key in set) return null;
          set[key] = value[1];
        }
        found = { start: at, end: at + text.length, set };
      }
      at += line.length + 1;
    }
    return found;
  }

  // The template and pages selects and the editor's `#show: cv` line say the same thing, whichever was
  // changed.
  function chooseTemplate() {
    const editor = byId("cv-source");
    const select = byId("cv-template");
    const pages = byId("cv-pages");
    const fitButton = byId("cv-fit");
    if (!editor || !select) return;
    let timer = null;
    byId("cv-template-save").hidden = true;
    byId("cv-template-hint").hidden = true;
    const follow = () => {
      const line = showLine(editor.value);
      select.disabled = !line;
      pages.disabled = !line;
      fitButton.disabled = !line;
      byId("cv-template-unreadable").hidden = !!line;
      if (!line) return;
      // An unknown name selects nothing.
      select.value = line.set.template || select.options[0].value;
      pages.value = line.set.pages || "";
      fitButton.disabled = !line.set.pages;
    };
    // Asks for the density at which this text comes closest to its pages, and holds it unsaved.
    const fit = async () => {
      const form = new URLSearchParams({ csrf: editor.form.elements.csrf.value, source: editor.value });
      fitButton.disabled = true;
      let body = { message: "The fit could not be tried." };
      try {
        const response = await fetch(`${base}/fit`, { method: "POST", body: form });
        body = await response.json();
      } catch {
        // The note says so.
      }
      if (body.source) editor.value = body.source;
      setNote(byId("cv-fit-note"), body.message);
      follow();
      if (body.source) tryPreview();
    };
    const chosen = () => {
      const line = showLine(editor.value);
      if (!line) return;
      const args = [];
      if ("lang" in line.set) args.push(`lang: "${line.set.lang}"`);
      // With nothing selected, the name typed by hand stays.
      const template = select.selectedIndex < 0 ? line.set.template : select.value;
      if (template && template !== select.options[0].value) args.push(`template: "${template}"`);
      if (pages.value) args.push(`pages: ${pages.value}`);
      if (Number(line.set.density)) args.push(`density: ${line.set.density}`);
      const show = args.length ? `${SHOW}.with(${args.join(", ")})` : SHOW;
      editor.value = editor.value.slice(0, line.start) + show + editor.value.slice(line.end);
      follow();
      clearTimeout(timer);
      tryPreview();
    };
    select.addEventListener("change", chosen);
    pages.addEventListener("change", chosen);
    fitButton.hidden = false;
    fitButton.addEventListener("click", fit);
    byId("cv-compile-row").hidden = false;
    byId("cv-compile").addEventListener("click", () => {
      clearTimeout(timer);
      tryPreview();
    });
    editor.addEventListener("input", () => {
      follow();
      compileState("Not compiled yet");
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
  findInSource();
  loadPreview(shown);
  connect();
})();
