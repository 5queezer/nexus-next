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
    <textarea id="cv-source" name="source">${SOURCE}</textarea></form>
  <form id="cv-template-form">
    <select id="cv-template"><option>classic</option><option>modern</option><option>banner</option></select>
    <select id="cv-pages"><option value="">no target</option><option>1</option><option>2</option></select>
    <button id="cv-template-save">Save draft in this template</button>
    <button type="button" id="cv-fit" hidden>Fit</button><p id="cv-fit-note" hidden></p>
    <p id="cv-template-unreadable" hidden>By hand.</p><p id="cv-template-hint"></p></form>
</div><script src="/assets/cv.js"></script>`;

(async () => {
  const browser = await chromium.launch({ executablePath: process.env.CHROME });
  const page = await browser.newPage();
  const tried = [];
  await page.route("**/*", (route) => {
    const url = new URL(route.request().url());
    if (url.pathname === "/") return route.fulfill({ contentType: "text/html", body: PAGE });
    if (url.pathname === "/assets/cv.js") {
      const body = fs.readFileSync(path.join(__dirname, "../assets/cv.js"), "utf8");
      return route.fulfill({ contentType: "text/javascript", body });
    }
    if (url.pathname.endsWith("/try")) {
      tried.push(new URLSearchParams(route.request().postData()).get("source"));
      return route.fulfill({ json: { key: "k".repeat(64), version: 3, pages: 2, layout: ["2 pages.", "A note."] } });
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

  await browser.close();
  console.log("ok");
})();
