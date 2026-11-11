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

const COMPLETE = {
  helpers: [],
  templates: ["classic", "modern", "banner"],
  employment: [],
  education: [],
  metric: [],
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
  await page.route("**/*", (route) => {
    const url = new URL(route.request().url());
    if (url.pathname === "/") return route.fulfill({ contentType: "text/html", body: page_of(source) });
    if (url.pathname === "/assets/app.css") return route.fulfill({ contentType: "text/css", body: asset("../src/style.css") });
    if (url.pathname.startsWith("/assets/")) {
      return route.fulfill({ contentType: "text/javascript", body: asset(`..${url.pathname}`) });
    }
    if (url.pathname.endsWith("/preview.json")) return route.fulfill({ json: { revision: 1, state: "draft", pages: 1 } });
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

  if (process.env.SHOTS) {
    for (const scheme of ["light", "dark"]) {
      await page.emulateMedia({ colorScheme: scheme });
      await page.goto("http://127.0.0.1:9/");
      await page.screenshot({ path: path.join(process.env.SHOTS, `editor-${scheme}.png`) });
    }
  }

  assert.deepEqual(errors, []);
  await browser.close();
  console.log("ok");
})();
