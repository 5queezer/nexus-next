// The source editor in a real browser, on a synthetic page with the real stylesheet and every request
// answered here:
// PLAYWRIGHT=<dir of playwright-core> [CHROME=<a Chromium binary>] node crates/web/tests/editor.browser.js
// With SHOTS=<dir> it also writes pictures of the editor in both themes there.
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { chromium } = require(process.env.PLAYWRIGHT || "playwright-core");

const SOURCE = `#import "/nexus.typ": *
#show: cv.with(lang: "en", template: "modern", pages: 2)

// The top of the page comes from the facts.
#header()

= Profile
Backend engineer with *nine years* in freight software, _mostly_ in Rust. \\#1 in uptime.

= Experience
#employment("brightpath")[
  - Cut invoice errors by #metric("invoice-errors") across 3 depots.
  - Built the routing service in \`Rust\`, see https://example.test/routing for the write-up.
]
#employment("harbourline")

= Skills
#skills("Languages")[Rust, Go, SQL]
#skills("Tools")[Postgres, Kafka]

= Projects
#project("Tidewatch", stack: "Rust, SQLite", url: "example.test/tidewatch")[
  A tide table for the North Sea coast. /* drawn from open data */
]

= Education
#education("nordsee")

= Languages
#languages()

#let gap = 10pt
#set text(size: 9.5pt)
#v(gap)
`;

// As the server writes it, from `HELPERS` and the facts.
const COMPLETE = {
  helpers: [
    { name: "cv", snippet: "show: cv.with(|)", about: "The page setup" },
    { name: "header", snippet: "header()", about: "Your name and contact line" },
    { name: "employment", snippet: 'employment("|")[\n  - \n]', about: "One job by id" },
    { name: "education", snippet: 'education("|")', about: "One qualification by id" },
    { name: "metric", snippet: 'metric("|")', about: "A metric's locked wording" },
    { name: "project", snippet: 'project("|", stack: "", url: "")[]', about: "A side project" },
  ],
  templates: ["classic", "modern", "banner"],
  employment: [
    { id: "brightpath", label: "Brightpath Logistics \u00b7 Software Engineer \u00b7 03/2019 \u2013 12/2022" },
    { id: "harbourline", label: "Harbourline \u00b7 Developer \u00b7 2016 \u2013 2019" },
    { id: "tidal", label: "Tidal Works \u00b7 Intern \u00b7 2015" },
  ],
  education: [{ id: "nordsee", label: "Nordsee University \u00b7 BSc Computer Science \u00b7 2015 \u2013 2018" }],
  metric: [{ id: "invoice-errors", label: "40%" }, { id: "depots", label: "3 depots" }],
};

const page_of = (source) => `<!doctype html><meta charset="utf-8"><meta name="color-scheme" content="light dark">
<link rel="stylesheet" href="/assets/app.css">
<main><div id="cv-workspace" data-variant="default" data-revision="1" data-state="draft" data-view="edit">
<div class="cv-panes"><div class="cv-picture" id="cv-picture-pane">
  <section id="cv-preview"><div id="cv-preview-pages" class="preview-pages"><img alt=""></div>
    <p id="cv-preview-unsaved" hidden></p><ul id="cv-preview-layout" hidden></ul>
    <p id="cv-preview-note" hidden></p></section></div>
<div class="cv-panel"><section id="cv-editor" data-complete="${JSON.stringify(COMPLETE).replace(/&/g, "&amp;").replace(/"/g, "&quot;")}">
  <h2>Source</h2>
  <form class="act" id="cv-editor-form"><input type="hidden" name="csrf" value="token">
    <label class="cv-source">Typst source
      <textarea name="source" class="code" id="cv-source" rows="24" spellcheck="false">${source.replace(/&/g, "&amp;").replace(/</g, "&lt;")}</textarea></label>
    <div class="cv-compile" id="cv-compile-row" hidden><button type="button" class="quiet" id="cv-compile">Compile</button>
      <span id="cv-compile-state" role="status"></span><button type="button" class="cv-error" id="cv-error" hidden></button></div>
    <div class="cv-save-row"><label>What changed<input type="text" name="summary"></label>
      <button type="submit" id="cv-save">Save draft</button></div></form>
  <p class="hint" id="cv-find-miss" hidden>Not in this text.</p>
  <form class="act" id="cv-template-form"><div class="cv-save-row">
    <label>Template<select id="cv-template"><option>classic</option><option>modern</option><option>banner</option></select></label>
    <label>Pages<select id="cv-pages"><option value="">no target</option><option>1</option><option>2</option></select></label>
    <button id="cv-template-save">Save draft with these</button>
    <button type="button" class="secondary" id="cv-fit" hidden>Fit to the pages</button></div>
    <p id="cv-fit-note" hidden></p><p id="cv-template-unreadable" hidden>By hand.</p><p id="cv-template-hint"></p></form>
</section></div></div></div></main>
<script src="/assets/editor.js" defer></script><script src="/assets/cv.js" defer></script>`;

const asset = (name) => fs.readFileSync(path.join(__dirname, name), "utf8");

// What the tokenizer makes of a source: each coloured stretch as "kinds:text", plain text left out.
const CASES = [
  ["// a note\ntext", ["comment:// a note"]],
  ["a /* b /* c */ d */ e", ["comment:/* b /* c */ d */"]],
  ['#import "/nexus.typ": *\n*bold*', ["kw:#import", 'str:"/nexus.typ"', "strong:*bold*"]],
  ['#show: cv.with(lang: "de", pages: 2)', ["kw:#show", "fn:with", "punct:(", "key:lang", 'str:"de"', "key:pages", "num:2", "punct:)"]],
  ['#f("a\\"b", x)', ["fn:#f", "punct:(", 'str:"a', 'esc:\\"', 'str:b"', "punct:)"]],
  ["#set text(size: 10pt, tracking: 2cm, fill: 50%, top: 1.5em)", ["kw:#set", "fn:text", "punct:(", "key:size", "num:10pt", "key:tracking", "num:2cm", "key:fill", "num:50%", "key:top", "num:1.5em", "punct:)"]],
  ["#let on = true\n#let off = none", ["kw:#let", "atom:true", "kw:#let", "atom:none"]],
  ["#if x { 1 } else [two]\n#for i in (1, 2) [#i]", ["kw:#if", "punct:{", "num:1", "punct:}", "kw:else", "punct:[", "punct:]", "kw:#for", "kw:in", "punct:(", "num:1", "num:2", "punct:)", "punct:[", "var:#i", "punct:]"]],
  ['#employment("a")[- one #metric("m")\n  - two]', ["fn:#employment", "punct:(", 'str:"a"', "punct:)", "punct:[", "list:-", "fn:#metric", "punct:(", 'str:"m"', "punct:)", "list:-", "punct:]"]],
  ["= Title here\nbody", ["heading:= Title here"]],
  ["== Sub *loud*\n+ one\n1. two\na - b", ["heading:== Sub ", "heading strong:*loud*", "list:+", "list:1."]],
  ["some *strong* and _emph_ and snake_case_name and 2*3*4", ["strong:*strong*", "emph:_emph_"]],
  ["raw `a # b` and ```\n#x\n``` end", ["raw:`a # b`", "raw:```\n#x\n```"]],
  ["price $x^2 + #y$ and \\$5 \\# \\u{1f600}", ["math:$x^2 + #y$", "esc:\\$", "esc:\\#", "esc:\\u{1f600}"]],
  ["see @fig:one. and <tab-1> and a < b", ["ref:@fig:one", "label:<tab-1>"]],
  ["go to https://example.test/a//b now // then", ["comment:// then"]],
  // A quote or a bracket that is never closed costs its line or its call, not the rest of the file.
  ['#f("open\n= Next', ["fn:#f", "punct:(", 'str:"open', "heading:= Next"]],
  ["#f(a, b\n#header()\n= Next", ["fn:#f", "punct:(", "fn:#header", "punct:(", "punct:)", "heading:= Next"]],
  ["#f[a (b] c *d*", ["fn:#f", "punct:[", "punct:]", "strong:*d*"]],
  ["#f[a [b] c] *d*", ["fn:#f", "punct:[", "punct:]", "strong:*d*"]],
  ["#facts.name. Then #a.b(1).c[x]", ["var:#facts", "var:name", "var:#a", "fn:b", "punct:(", "num:1", "punct:)", "fn:c", "punct:[", "punct:]"]],
];

(async () => {
  const browser = await chromium.launch({ executablePath: process.env.CHROME });
  const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
  let source = SOURCE;
  const tried = [];
  // While set, an answer to `/try` waits for it.
  let held = null;
  await page.route("**/*", async (route) => {
    const url = new URL(route.request().url());
    if (url.pathname === "/") return route.fulfill({ contentType: "text/html", body: page_of(source) });
    if (url.pathname === "/assets/app.css") return route.fulfill({ contentType: "text/css", body: asset("../src/style.css") });
    if (url.pathname.startsWith("/assets/")) {
      return route.fulfill({ contentType: "text/javascript", body: asset(`..${url.pathname}`) });
    }
    if (url.pathname.endsWith("/preview.json")) return route.fulfill({ json: { revision: 1, state: "draft", pages: 1 } });
    if (url.pathname.endsWith("/try")) {
      const text = new URLSearchParams(route.request().postData()).get("source");
      tried.push(text);
      await held;
      // A text with `#broken` in it doesn't compile, at that word.
      const line = text.split("\n").findIndex((row) => row.includes("#broken"));
      if (line < 0) return route.fulfill({ json: { key: "k".repeat(64), version: 3, pages: 1, layout: [] } });
      const problem = { severity: "error", message: "unknown variable: broken", file: "/main.typ", line: line + 1, column: text.split("\n")[line].indexOf("#broken") + 2, hints: ["Is it spelled right?"] };
      return route.fulfill({ status: 422, json: { message: "This text doesn't compile.", tokens: [], diagnostics: [problem] } });
    }
    return route.abort();
  });
  const errors = [];
  page.on("pageerror", (error) => errors.push(error));
  await page.goto("http://127.0.0.1:9/");
  const editor = page.locator("#cv-source");
  const layer = page.locator(".cv-layer");

  // The copy under the textarea holds the same text, with one line break more, however it changes.
  assert.equal(await layer.textContent(), `${SOURCE}\n`);
  assert.equal(await page.locator(".cv-gutter div div").count(), SOURCE.split("\n").length);
  await editor.focus();
  await page.keyboard.press("Control+End");
  await page.keyboard.type("more *text*");
  assert.equal(await layer.textContent(), `${SOURCE}more *text*\n`);
  assert.equal(await page.locator(".cv-layer .t-strong").last().textContent(), "*text*");
  await editor.evaluate((element) => {
    element.value = "one\ntwo";
  });
  assert.equal(await layer.textContent(), "one\ntwo\n");
  assert.equal(await page.locator(".cv-gutter div div").count(), 2);

  const kinds = () =>
    layer.evaluate((pre) => [...pre.querySelectorAll("span")].map((span) => `${span.className.replaceAll("t-", "")}:${span.textContent}`));
  for (const [text, expected] of CASES) {
    await editor.fill(text);
    assert.equal(await layer.textContent(), `${text}\n`);
    assert.deepEqual(await kinds(), expected, text);
  }

  // A long source is coloured within a frame, and the copy and the numbers move with the textarea.
  const long = `${SOURCE.repeat(10)}// ${"wide ".repeat(80)}`;
  await editor.fill(long);
  const took = await editor.evaluate((element) => {
    const from = performance.now();
    for (let round = 0; round < 20; round += 1) element.dispatchEvent(new Event("input"));
    return (performance.now() - from) / 20;
  });
  console.log(`${long.split("\n").length} lines coloured in ${took.toFixed(2)} ms`);
  assert.ok(took < 16, `${took} ms`);
  const places = () =>
    page.evaluate(() => {
      const top = (element) => element.getBoundingClientRect().top;
      const text = document.getElementById("cv-source");
      const numbers = document.querySelectorAll(".cv-gutter div div");
      const line = parseFloat(getComputedStyle(text).lineHeight);
      return {
        scroll: [text.scrollLeft, text.scrollTop],
        layer: top(document.querySelector(".cv-layer")) - top(text),
        left: document.querySelector(".cv-layer").getBoundingClientRect().left - text.getBoundingClientRect().left,
        // Where line 200 stands in the gutter, and where the textarea draws it.
        number: top(numbers[199]) - top(text),
        expected: 8 + 199 * line - text.scrollTop,
        heights: [document.querySelector(".cv-layer").offsetHeight, text.scrollHeight],
      };
    });
  await editor.evaluate((element) => element.scrollTo(40, 3000));
  await page.waitForFunction(() => document.querySelector(".cv-layer").style.transform.includes("-3000px"));
  let at = await places();
  assert.deepEqual(at.scroll, [40, 3000]);
  assert.deepEqual([at.layer, at.left], [-3000, -40]);
  assert.ok(Math.abs(at.number - at.expected) < 1, JSON.stringify(at));
  assert.ok(Math.abs(at.heights[0] - at.heights[1]) < 1, JSON.stringify(at));
  await editor.evaluate((element) => element.scrollTo(0, element.scrollHeight));
  await page.waitForFunction(() => !document.querySelector(".cv-layer").style.transform.includes("-3000px"));
  at = await places();
  assert.equal(at.layer, -at.scroll[1]);

  // A text that doesn't compile says so beside the button, marks its line, and the message leads there.
  const state = page.locator("#cv-compile-state");
  const marked = () => page.locator(".cv-gutter .error").allTextContents();
  await page.goto("http://127.0.0.1:9/");
  assert.equal(await page.locator("#cv-compile").isVisible(), true);
  await editor.focus();
  await page.keyboard.press("Control+End");
  await page.keyboard.type("x");
  assert.equal(await state.textContent(), "Not compiled yet");
  await page.waitForFunction(() => document.getElementById("cv-compile-state").textContent === "Preview is up to date");
  assert.deepEqual(await marked(), []);
  await editor.fill(SOURCE.replace("#header()", "  #broken"));
  let release;
  held = new Promise((resolve) => {
    release = resolve;
  });
  await page.locator("#cv-compile").click();
  assert.equal(await state.textContent(), "Compiling\u2026");
  release();
  await page.waitForFunction(() => document.getElementById("cv-compile-state").textContent === "Does not compile:");
  const message = page.locator("#cv-error");
  assert.equal(await message.textContent(), "Line 5, column 4: unknown variable: broken Is it spelled right?");
  assert.deepEqual(await marked(), ["5"]);
  await page.locator("#cv-save").focus();
  await message.click();
  assert.deepEqual(await editor.evaluate((e) => [document.activeElement === e, e.selectionStart, e.selectionEnd]), [true, SOURCE.indexOf("#header()") + 3, SOURCE.indexOf("#header()") + 3]);
  // The next text that compiles takes the mark and the message away.
  await editor.fill(SOURCE.replace("#header()", "#contact()"));
  await page.locator("#cv-compile").click();
  await page.waitForFunction(() => document.getElementById("cv-compile-state").textContent === "Preview is up to date");
  assert.deepEqual([await marked(), await message.isHidden()], [[], true]);
  // Back at the stored text the stored picture is the preview.
  await editor.fill(SOURCE);
  await page.locator("#cv-compile").click();
  await page.waitForFunction(() => document.getElementById("cv-compile-state").textContent === "Preview is up to date");
  assert.equal(tried.at(-1), SOURCE.replace("#header()", "#contact()"));

  // The text with `|` where the caret stands, written and read back that way.
  const put = async (text) => {
    await editor.fill(text.replace("|", ""));
    await editor.evaluate((element, at) => element.setSelectionRange(at, at), text.indexOf("|"));
  };
  const seen = () => editor.evaluate((e) => `${e.value.slice(0, e.selectionStart)}|${e.value.slice(e.selectionEnd)}`);
  const offered = () =>
    page.locator("#cv-complete:not([hidden]) li").evaluateAll((rows) => rows.map((row) => row.querySelector("b").textContent));
  const picked = () => page.locator('#cv-complete li[aria-selected="true"]').textContent();

  // After a `#` come the helpers, narrowed by what is typed. Enter writes one with the caret where
  // its id goes, and the ids in the facts follow, each saying whose it is and whether it is used.
  await put('#employment("brightpath")\n|\n');
  await page.keyboard.type("#");
  assert.deepEqual(await offered(), ["#show: cv.with", "#header", "#employment", "#education", "#metric", "#project", "#let", "#set", '#import "/nexus.typ": *']);
  await page.keyboard.type("e");
  assert.deepEqual(await offered(), ["#employment", "#education"]);
  assert.equal(await picked(), "#employmentOne job by id");
  assert.equal(await editor.getAttribute("aria-activedescendant"), "cv-complete-0");
  await page.keyboard.press("Enter");
  assert.equal(await seen(), '#employment("brightpath")\n#employment("|")[\n  - \n]\n');
  assert.deepEqual(await offered(), ["brightpath", "harbourline", "tidal"]);
  assert.equal(await picked(), "brightpathBrightpath Logistics \u00b7 Software Engineer \u00b7 03/2019 \u2013 12/2022 \u00b7 already used");
  assert.equal(await page.locator("#cv-complete li.used").count(), 1);
  // Down and Up go round, and Tab writes too. The caret lands after the call's bracket.
  await page.keyboard.press("ArrowUp");
  assert.equal(await picked(), "tidalTidal Works \u00b7 Intern \u00b7 2015");
  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("Tab");
  assert.equal(await seen(), '#employment("brightpath")\n#employment("harbourline")|[\n  - \n]\n');
  assert.deepEqual(await offered(), []);
  assert.equal(await layer.textContent(), `${await editor.inputValue()}\n`);
  // Each choice is one step back for the browser's undo.
  await page.keyboard.press("Control+z");
  assert.equal(await editor.inputValue(), '#employment("brightpath")\n#employment("")[\n  - \n]\n');
  await page.keyboard.press("Control+z");
  assert.equal(await editor.inputValue(), '#employment("brightpath")\n#e\n');
  await page.keyboard.press("Control+y");
  assert.equal(await editor.inputValue(), '#employment("brightpath")\n#employment("")[\n  - \n]\n');
  assert.equal(await layer.textContent(), `${await editor.inputValue()}\n`);

  // An id can be found by the name it stands for, and choosing replaces the one that was there.
  await put('#employment("brightpath|")');
  await page.keyboard.press("Control+Space");
  assert.deepEqual(await offered(), []);
  await put('#employment("|brightpath")');
  await page.keyboard.type("tid");
  assert.deepEqual(await offered(), ["tidal"]);
  await page.keyboard.press("Enter");
  assert.equal(await seen(), '#employment("tidal")|');
  await put('#metric("|")\n#education("")');
  await page.keyboard.press("Control+Space");
  assert.deepEqual(await offered(), ["invoice-errors", "depots"]);
  await page.keyboard.press("Escape");
  assert.deepEqual(await offered(), []);
  await put('#metric("")\n#education("|")');
  await page.keyboard.type("nord");
  assert.deepEqual(await offered(), ["nordsee"]);

  // Escape closes the list, and then Enter is a line break again. So is one when no list was open.
  await put("|");
  await page.keyboard.type("#he");
  assert.deepEqual(await offered(), ["#header"]);
  await page.keyboard.press("Escape");
  assert.deepEqual(await offered(), []);
  await page.keyboard.press("Enter");
  assert.equal(await seen(), "#he\n|");
  // A click writes a choice as well, and the textarea keeps the focus.
  await page.keyboard.type("#he");
  await page.locator("#cv-complete li").click();
  assert.equal(await seen(), "#he\n#header()|");
  assert.equal(await editor.evaluate((element) => document.activeElement === element), true);

  // No helpers where a `#` is no code: in a string, a comment, raw text, or after a backslash.
  for (const text of ['#skills("|")', "// note |", "`raw |`", "\\|"]) {
    await put(text);
    await page.keyboard.type("#");
    assert.deepEqual(await offered(), [], text);
  }

  // In the page setup come its arguments, without the ones written, and then their values. The
  // select beside the editor follows what a choice writes.
  await put('#show: cv.with(lang: "en", |)');
  await page.keyboard.type("x");
  assert.deepEqual(await offered(), []);
  await page.keyboard.press("Backspace");
  await page.keyboard.press("Control+Space");
  assert.deepEqual(await offered(), ["template", "pages", "density"]);
  assert.equal(await picked(), "templateclassic, modern, banner");
  await page.keyboard.press("Enter");
  assert.equal(await seen(), '#show: cv.with(lang: "en", template: |)');
  assert.deepEqual(await offered(), ['"classic"', '"modern"', '"banner"']);
  await page.keyboard.type('"b');
  assert.deepEqual(await offered(), ['"banner"']);
  await page.keyboard.press("Enter");
  assert.equal(await seen(), '#show: cv.with(lang: "en", template: "banner"|)');
  assert.equal(await page.locator("#cv-template").inputValue(), "banner");
  await page.keyboard.type(", p");
  assert.deepEqual(await offered(), ["pages"]);
  await page.keyboard.press("Enter");
  assert.deepEqual(await offered(), ["1", "2"]);
  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("Enter");
  assert.equal(await seen(), '#show: cv.with(lang: "en", template: "banner", pages: 2|)');
  assert.deepEqual(await offered(), []);
  assert.equal(await page.locator("#cv-pages").inputValue(), "2");
  await page.keyboard.type(", density: ");
  assert.deepEqual(await offered(), ["-2", "-1", "0", "1", "2"]);
  await page.keyboard.type("-");
  assert.deepEqual(await offered(), ["-2", "-1"]);
  await put('#show: cv.with(lang: "|en")');
  await page.keyboard.press("Control+Space");
  assert.deepEqual(await offered(), ['"en"', '"de"']);
  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("Enter");
  assert.equal(await seen(), '#show: cv.with(lang: "de"|)');
  // A new setup line offers its arguments at once.
  await put("|");
  await page.keyboard.type("#sh");
  await page.keyboard.press("Enter");
  assert.equal(await seen(), "#show: cv.with(|)");
  assert.deepEqual(await offered(), ["lang", "template", "pages", "density"]);

  // A project's named arguments, with the caret between their quotes, and none inside a string.
  await put('#project("Tidewatch", |)[]');
  await page.keyboard.type("s");
  assert.deepEqual(await offered(), ["stack"]);
  await page.keyboard.press("Enter");
  assert.equal(await seen(), '#project("Tidewatch", stack: "|")[]');
  assert.deepEqual(await offered(), []);
  await page.keyboard.type("Rust, u");
  assert.deepEqual(await offered(), []);
  await put('#project("Tidewatch", stack: "Rust", |)[]');
  await page.keyboard.press("Control+Space");
  assert.deepEqual(await offered(), ["url"]);
  // Elsewhere there is nothing to offer, and asking says so by showing nothing.
  await put("Plain text (in brackets|)");
  await page.keyboard.press("Control+Space");
  assert.deepEqual(await offered(), []);

  if (process.env.SHOTS) {
    for (const scheme of ["light", "dark"]) {
      await page.emulateMedia({ colorScheme: scheme });
      await page.goto("http://127.0.0.1:9/");
      await page.screenshot({ path: path.join(process.env.SHOTS, `editor-${scheme}.png`) });
      // A problem marked, some text selected and the list of ids open.
      await put(SOURCE.replace("#header()", "#broken").replace('#employment("harbourline")', '#employment("|")'));
      await page.locator("#cv-compile").click();
      await page.locator("#cv-error").waitFor();
      await editor.focus();
      await editor.evaluate((element) => {
        element.setSelectionRange(element.value.indexOf("Backend"), element.value.indexOf(" in freight"));
        element.scrollTop = 0;
      });
      await page.screenshot({ path: path.join(process.env.SHOTS, `editor-${scheme}-selected.png`) });
      await editor.evaluate((element) => element.setSelectionRange(element.value.indexOf('("")') + 2, element.value.indexOf('("")') + 2));
      await page.keyboard.press("Control+Space");
      await page.screenshot({ path: path.join(process.env.SHOTS, `editor-${scheme}-list.png`) });
    }
  }

  assert.deepEqual(errors, []);
  await browser.close();
  console.log("ok");
})();
