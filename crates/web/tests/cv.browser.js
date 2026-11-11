// The editor's selects in a real browser, on a synthetic page with every request answered here:
// PLAYWRIGHT=<dir of playwright-core> [CHROME=<a Chromium binary>] node crates/web/tests/cv.browser.js
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { chromium } = require(process.env.PLAYWRIGHT || "playwright-core");

const SOURCE = '#import "/nexus.typ": *\n#show: cv.with(lang: "de")\n\n#header()\n';
const PAGE = `<!doctype html><meta charset="utf-8">
<div id="cv-workspace" data-variant="default" data-revision="1" data-state="draft" data-view="edit">
  <section id="cv-preview"><div id="cv-preview-pages"><img alt=""></div>
    <p id="cv-preview-unsaved" hidden></p><ul id="cv-preview-layout" hidden></ul>
    <p id="cv-preview-note" hidden></p></section>
  <form id="cv-editor-form"><input type="hidden" name="csrf" value="token">
    <textarea id="cv-source" name="source">${SOURCE}</textarea>
    <div id="cv-compile-row" hidden><button type="button" id="cv-compile">Compile</button>
      <span id="cv-compile-state"></span><button type="button" id="cv-error" hidden></button></div></form>
  <form id="cv-template-form">
    <select id="cv-template"><option>classic</option><option>modern</option><option>banner</option></select>
    <select id="cv-pages"><option value="">no target</option><option>1</option><option>2</option></select>
    <button id="cv-template-save">Save draft in this template</button>
    <button type="button" id="cv-fit" hidden>Fit</button><p id="cv-fit-note" hidden></p>
    <p id="cv-find-miss" hidden>Not in this text.</p>
    <p id="cv-template-unreadable" hidden>By hand.</p><p id="cv-template-hint"></p></form>
</div><script src="/assets/cv.js"></script>`;

const chip = (rule, kind, boxes, word) => `<mark class="${kind}" data-rule="${rule}" data-boxes="${boxes}">${word}</mark>`;
const REVIEW = `<!doctype html><meta charset="utf-8">
<div id="cv-workspace" data-variant="default" data-revision="1" data-state="draft" data-view="review">
  <div id="cv-picture-pane"><span id="cv-page-1" style="position:relative;display:block;width:400px;height:560px"></span></div>
  <div class="cv-panel"><section id="cv-approval">
    <ul id="cv-legend"><li id="cv-rule-numbers" class="marked" data-rule="numbers">A number</li>
      <li id="cv-rule-newWords" class="new" data-rule="newWords">A new word</li></ul>
    <p id="cv-legend-hint" hidden>Point at a rule.</p>
    <ul><li>${chip("numbers", "marked", "1:0.1,0.1,0.1,0.02", "2019")}${chip("newWords", "new", "1:0.3,0.1,0.1,0.02", "zebra")}</li>
      <li>${chip("numbers", "marked", "1:0.1,0.2,0.1,0.02;2:0.1,0.2,0.1,0.02", "40%")}</li></ul>
    <details id="cv-more-items"><summary>More</summary><ul><li>${chip("newWords", "new", "1:0.3,0.3,0.1,0.02", "okapi")}</li></ul></details>
  </section></div>
</div><script src="/assets/cv.js"></script>`;

(async () => {
  const browser = await chromium.launch({ executablePath: process.env.CHROME });
  const page = await browser.newPage();
  const tried = [];
  await page.route("**/*", (route) => {
    const url = new URL(route.request().url());
    if (url.pathname === "/") return route.fulfill({ contentType: "text/html", body: PAGE });
    if (url.pathname === "/review") return route.fulfill({ contentType: "text/html", body: REVIEW });
    if (url.pathname === "/assets/cv.js") {
      const body = fs.readFileSync(path.join(__dirname, "../assets/cv.js"), "utf8");
      return route.fulfill({ contentType: "text/javascript", body });
    }
    if (url.pathname.endsWith("/try")) {
      tried.push(new URLSearchParams(route.request().postData()).get("source"));
      return route.fulfill({ json: { key: "k".repeat(64), version: 3, pages: 2, layout: ["2 pages.", "A note."], words: [] } });
    }
    if (url.pathname.endsWith("/fit")) {
      const source = new URLSearchParams(route.request().postData()).get("source");
      return route.fulfill({ json: { source: source.replace("pages: 2", "pages: 2, density: 1"), message: "Density 1." } });
    }
    if (url.pathname.endsWith("/preview.json")) {
      return route.fulfill({ json: { revision: 1, state: "draft", pages: 1 } });
    }
    return route.abort();
  });
  await page.goto("http://127.0.0.1:9/");
  const editor = page.locator("#cv-source");
  const select = page.locator("#cv-template");
  const sources = () => page.locator("#cv-preview-pages img").evaluateAll((all) => all.map((image) => image.getAttribute("src")));

  // The fallback button goes, and the select shows what the source says.
  assert.equal(await page.locator("#cv-template-save").isHidden(), true);
  assert.equal(await select.inputValue(), "classic");

  // Choosing rewrites only the line, and the preview is the unsaved text's.
  await select.selectOption("modern");
  assert.equal(await editor.inputValue(), SOURCE.replace('lang: "de"', 'lang: "de", template: "modern"'));
  await page.waitForFunction(() => !document.getElementById("cv-preview-unsaved").hidden);
  assert.equal(tried.at(-1), await editor.inputValue());
  assert.deepEqual(await sources(), [1, 2].map((n) => `/cv/variants/default/try.png?key=${"k".repeat(64)}&version=3&page=${n}`));

  assert.deepEqual(await page.locator("#cv-preview-layout li").allTextContents(), ["2 pages.", "A note."]);

  // The page target is written beside the template, and taken away again.
  const pages = page.locator("#cv-pages");
  await pages.selectOption("2");
  assert.equal(await editor.inputValue(), SOURCE.replace('lang: "de"', 'lang: "de", template: "modern", pages: 2'));
  // A fit needs a target, holds its density in the editor unsaved, and the selects keep it.
  const fit = page.locator("#cv-fit");
  assert.equal(await fit.isVisible(), true);
  await fit.click();
  await page.waitForFunction(() => document.getElementById("cv-fit-note").textContent === "Density 1.");
  assert.equal(await editor.inputValue(), SOURCE.replace('lang: "de"', 'lang: "de", template: "modern", pages: 2, density: 1'));
  assert.equal(tried.at(-1), await editor.inputValue());
  await select.selectOption("banner");
  assert.equal(await editor.inputValue(), SOURCE.replace('lang: "de"', 'lang: "de", template: "banner", pages: 2, density: 1'));
  await editor.fill(SOURCE.replace('lang: "de"', 'lang: "de", template: "banner", pages: 2'));
  assert.equal(await fit.isDisabled(), false);
  await pages.selectOption("");
  assert.equal(await fit.isDisabled(), true);
  await select.selectOption("modern");
  assert.equal(await editor.inputValue(), SOURCE.replace('lang: "de"', 'lang: "de", template: "modern"'));

  // Back to the first template is the stored text again, with the stored picture.
  await select.selectOption("classic");
  assert.equal(await editor.inputValue(), SOURCE);
  await page.waitForFunction(() => document.getElementById("cv-preview-unsaved").hidden);
  assert.equal(await page.locator("#cv-preview-layout").isHidden(), true);
  assert.deepEqual(await sources(), ["/cv/variants/default/preview.png?revision=1&page=1&state=draft"]);

  // The select follows the line typed by hand, an unknown name selects nothing, and typing is drawn.
  await editor.fill(SOURCE.replace('lang: "de"', 'template: "banner"'));
  assert.equal(await select.inputValue(), "banner");
  await page.waitForFunction(() => !document.getElementById("cv-preview-unsaved").hidden);
  await editor.fill(SOURCE.replace('lang: "de"', 'template: "fancy", pages: 1'));
  assert.equal(await select.evaluate((element) => element.selectedIndex), -1);
  assert.equal(await pages.inputValue(), "1");
  // A name the select doesn't know stays when the pages change.
  await pages.selectOption("2");
  assert.equal(await editor.inputValue(), SOURCE.replace('lang: "de"', 'template: "fancy", pages: 2'));
  await editor.fill(SOURCE.replace('lang: "de"', "pages: 3"));
  assert.equal(await pages.isDisabled(), true);

  // A line it can't read turns the select off and leaves the text alone.
  const own = SOURCE.replace(")\n\n", ") // mine\n\n");
  await editor.fill(own);
  assert.equal(await select.isDisabled(), true);
  assert.equal(await page.locator("#cv-template-unreadable").isHidden(), false);
  await editor.fill(SOURCE);
  assert.equal(await select.isDisabled(), false);
  assert.equal(await page.locator("#cv-template-unreadable").isHidden(), true);

  // A word named in the address is selected in the editor, whole and in any case, or said to be missing.
  await page.goto("http://127.0.0.1:9/?find=HEADER");
  assert.deepEqual(await editor.evaluate((e) => [e.value.slice(e.selectionStart, e.selectionEnd), document.activeElement === e]), ["header", true]);
  assert.equal(await page.locator("#cv-find-miss").isHidden(), true);
  await page.goto("http://127.0.0.1:9/?find=head");
  assert.equal(await page.locator("#cv-find-miss").isVisible(), true);

  // A rule in the legend marks all its words on the pages shown, and dims the other words.
  await page.goto("http://127.0.0.1:9/review");
  const marks = () => page.locator(".picture-mark").evaluateAll((all) => all.map((mark) => `${mark.className} ${mark.style.top}`));
  const dimmed = () => page.locator("mark.dim").allTextContents();
  const numbers = page.locator("#cv-rule-numbers");
  const fresh = page.locator("#cv-rule-newWords");
  assert.equal(await page.locator("#cv-legend-hint").isVisible(), true);
  assert.deepEqual(await numbers.evaluate((row) => [row.getAttribute("role"), row.tabIndex, row.getAttribute("aria-pressed")]), ["button", 0, "false"]);
  await numbers.hover();
  assert.deepEqual(await marks(), ["picture-mark marked 10%", "picture-mark marked 20%"]);
  assert.deepEqual(await dimmed(), ["zebra", "okapi"]);
  await page.mouse.move(390, 550);
  assert.deepEqual([await marks(), await dimmed()], [[], []]);

  // A click keeps a rule's words marked, and opens what was folded away if they are in there.
  await fresh.click();
  await page.mouse.move(390, 550);
  assert.equal(await fresh.getAttribute("aria-pressed"), "true");
  assert.deepEqual(await marks(), ["picture-mark new 10%", "picture-mark new 30%"]);
  assert.equal(await page.locator("#cv-more-items").evaluate((details) => details.open), true);
  // Pointing at one word shows that word, and leaving it brings the pinned rule back.
  await page.locator("mark", { hasText: "2019" }).hover();
  assert.deepEqual(await marks(), ["picture-mark marked 10%"]);
  await page.mouse.move(390, 550);
  assert.deepEqual(await marks(), ["picture-mark new 10%", "picture-mark new 30%"]);
  await numbers.click();
  assert.deepEqual([await fresh.getAttribute("aria-pressed"), await numbers.getAttribute("aria-pressed")], ["false", "true"]);
  await page.keyboard.press("Escape");
  await page.mouse.move(390, 550);
  assert.deepEqual([await numbers.getAttribute("aria-pressed"), await marks()], ["false", []]);
  // The keyboard pins too, and a click elsewhere lets go.
  await numbers.focus();
  await page.keyboard.press("Enter");
  assert.equal(await numbers.getAttribute("aria-pressed"), "true");
  await page.locator("#cv-picture-pane").click({ position: { x: 380, y: 540 } });
  assert.equal(await numbers.getAttribute("aria-pressed"), "false");

  await browser.close();
  console.log("ok");
})();
