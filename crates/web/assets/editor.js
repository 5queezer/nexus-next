// The source editor on a variant page: colours, line numbers, completion and a few keys on top of the
// plain textarea, which stays the form's field and the only holder of the text. Without this file
// the textarea works as it is.
(() => {
  const editor = document.getElementById("cv-source");
  if (!editor) return;

  const KEYWORDS = new Set([
    "let", "show", "set", "import", "include", "if", "else", "for", "in", "while", "return", "break",
    "continue", "context", "not", "and", "or", "as",
  ]);
  const ATOMS = new Set(["true", "false", "none", "auto"]);
  const IDENT = /[\p{L}_][\p{L}\p{N}_-]*/uy;
  const NUMBER = /(?:0x[\da-f]+|\d+(?:\.\d+)?(?:e[+-]?\d+)?)(?:pt|mm|cm|in|em|fr|deg|rad|%)?/iy;
  const LABEL = /<[\p{L}\p{N}_.:-]+>/uy;
  const REF = /@[\p{L}\p{N}_-]+(?:[.:][\p{L}\p{N}_-]+)*/uy;
  const ESCAPE = /\\(?:u\{[\da-f]*\}|[^\n])?/iy;
  const HEADING = /=+(?=[ \n]|$)/y;
  const NUMBERED = /\d+\.(?= )/y;
  const LINK = /https?:\/\/[^\s\]]+/y;
  const WORD = /[\p{L}\p{N}]/u;

  // Typst as a CV writes it, cut into coloured stretches: `emit(kind, from, to)` for each, in order,
  // and what lies between them is plain text. Markup and code call each other as the brackets nest.
  function tokenize(text, emit) {
    const n = text.length;
    let i = 0;
    // Code that meets a `#` or a heading, which only markup has, was opened and never closed: it gives
    // up to the markup around it, so that one missing bracket doesn't turn the rest of the file into code.
    let lost = false;
    const match = (shape, at = i) => {
      shape.lastIndex = at;
      const found = shape.exec(text);
      return found ? found[0].length : 0;
    };
    const lineEnd = (at) => {
      const end = text.indexOf("\n", at);
      return end < 0 ? n : end;
    };
    const take = (kind, to) => {
      emit(kind, i, to);
      i = to;
    };

    function comment() {
      if (text.startsWith("//", i)) {
        take("comment", lineEnd(i));
        return true;
      }
      if (!text.startsWith("/*", i)) return false;
      let depth = 1;
      let at = i + 2;
      while (at < n && depth) {
        if (text.startsWith("/*", at)) depth += 1;
        else if (text.startsWith("*/", at)) depth -= 1;
        else at -= 1;
        at += 2;
      }
      take("comment", Math.min(at, n));
      return true;
    }

    // A string that is never closed ends with its line, so a stray quote colours one line.
    function string() {
      let at = i + 1;
      while (at < n && text[at] !== '"' && text[at] !== "\n") {
        const escape = text[at] === "\\" ? match(ESCAPE, at) : 0;
        if (escape > 1) {
          take("str", at);
          take("esc", at + escape);
          at = i;
        } else {
          at += 1;
        }
      }
      take("str", text[at] === '"' ? at + 1 : at);
    }

    // One backtick ends on its line or not at all. Three or more end where as many come again.
    function raw() {
      let ticks = 1;
      while (text[i + ticks] === "`") ticks += 1;
      if (ticks === 2) return take("raw", i + 2);
      const limit = ticks === 1 ? lineEnd(i) : n;
      const close = text.indexOf("`".repeat(ticks), i + ticks);
      take("raw", close < 0 || close >= limit ? limit : close + ticks);
    }

    function math() {
      let at = i + 1;
      while (at < n && text[at] !== "$") at += text[at] === "\\" ? 2 : 1;
      take("math", at < n ? at + 1 : lineEnd(i));
    }

    function group() {
      const open = text[i];
      take("punct", i + 1);
      if (open === "[") markup(true);
      else code(open === "(" ? ")" : "}");
      if (!lost && i < n) take("punct", i + 1);
    }

    // Code up to `close`, which stays for the caller. A statement after `#let` and the like closes
    // with "\n": its line ends it, and so do a `;` and the `]` of the block it stands in.
    function code(close) {
      while (i < n && !lost) {
        const c = text[i];
        if (c === close || (close === "\n" && (c === ";" || c === "]"))) return;
        let length;
        if (c === "#" || (c === "\n" && match(HEADING, i + 1))) lost = true;
        else if (comment()) continue;
        else if (c === '"') string();
        else if (c === "`") raw();
        else if (c === "$") math();
        else if (c === "(" || c === "{" || c === "[") group();
        else if ((length = match(NUMBER))) take("num", i + length);
        else if ((length = match(LABEL))) take("label", i + length);
        else if ((length = match(IDENT))) {
          const name = text.slice(i, i + length);
          const end = i + length;
          let after = end;
          while (text[after] === " ") after += 1;
          if (KEYWORDS.has(name)) emit("kw", i, end);
          else if (ATOMS.has(name)) emit("atom", i, end);
          else if (text[end] === "(" || text[end] === "[") emit("fn", i, end);
          else if (close === ")" && text[after] === ":") emit("key", i, end);
          i = end;
        } else i += 1;
      }
    }

    // What follows a name or a closed bracket with nothing between: arguments, a body, a field.
    function tail() {
      while (!lost) {
        const c = text[i];
        const field = c === "." ? match(IDENT, i + 1) : 0;
        if (c === "(" || c === "[") group();
        else if (field) {
          const end = i + 1 + field;
          emit(text[end] === "(" || text[end] === "[" ? "fn" : "var", i + 1, end);
          i = end;
        } else return;
      }
    }

    // A `#` in markup and the code it starts.
    function hash() {
      const length = match(IDENT, i + 1);
      const c = text[i + 1];
      const end = i + 1 + length;
      if (length && KEYWORDS.has(text.slice(i + 1, end))) {
        take("kw", end);
        code("\n");
      } else if (length) {
        take(text[end] === "(" || text[end] === "[" ? "fn" : "var", end);
        tail();
      } else if (c === "(" || c === "{" || c === "[") {
        take("punct", i + 1);
        group();
        tail();
      } else if (c === '"') {
        take("str", i + 1);
        string();
      } else if (match(NUMBER, i + 1)) {
        take("num", i + 1 + match(NUMBER, i + 1));
      } else {
        i += 1;
      }
    }

    // Markup to the end, or to the `]` that closes the block it is the content of, which stays for
    // the caller.
    function markup(nested) {
      // Square brackets in the text close in pairs before the block does.
      let depth = 0;
      // Only spaces so far on this line, so a heading or a list item can start here.
      let fresh = true;
      let heading = false;
      let strong = false;
      let emph = false;
      let from = i;
      // The text since the last token takes the styles it stands in.
      const flush = () => {
        const kinds = [heading && "heading", strong && "strong", emph && "emph"].filter(Boolean);
        if (kinds.length) emit(kinds.join(" "), from, i);
        from = i;
      };
      const token = (run) => {
        flush();
        run();
        from = i;
      };
      // A `*` or `_` inside a word is a plain character.
      const toggles = () => !(WORD.test(text[i - 1] || "") && WORD.test(text[i + 1] || ""));
      while (i < n) {
        const c = text[i];
        if (c === " " || c === "\t") {
          i += 1;
          continue;
        }
        const first = fresh;
        fresh = c === "\n";
        let length;
        if (c === "\n") {
          flush();
          heading = false;
          // An empty line ends the paragraph, and with it what was left open.
          if (first) strong = emph = false;
          i += 1;
        } else if (c === "\\") token(() => take("esc", i + match(ESCAPE)));
        else if (c === "/" && (text[i + 1] === "/" || text[i + 1] === "*")) token(comment);
        else if (c === "#") {
          token(hash);
          lost = false;
        } else if (first && (length = match(HEADING))) {
          flush();
          heading = true;
          i += length;
        } else if (first && "-+/".includes(c) && /[ \n]/.test(text[i + 1] || "\n")) {
          token(() => take("list", i + 1));
        } else if (first && (length = match(NUMBERED))) token(() => take("list", i + length));
        else if ((c === "*" || c === "_") && toggles()) {
          const on = c === "*" ? strong : emph;
          // The mark itself is in the style it opens or closes.
          if (on) i += 1;
          flush();
          if (c === "*") strong = !on;
          else emph = !on;
          if (!on) i += 1;
        } else if (c === "`") token(raw);
        else if (c === "$") token(math);
        else if (c === "<" && (length = match(LABEL))) token(() => take("label", i + length));
        else if (c === "@" && (length = match(REF))) token(() => take("ref", i + length));
        // A web address holds a `//` that is no comment.
        else if (c === "h" && (length = match(LINK))) i += length;
        else if (c === "]" && !depth && nested) break;
        else {
          if (c === "[") depth += 1;
          else if (c === "]" && depth) depth -= 1;
          i += 1;
        }
      }
      flush();
    }

    markup(false);
  }

  const box = document.createElement("div");
  box.className = "cv-code";
  const gutter = document.createElement("div");
  gutter.className = "cv-gutter";
  gutter.setAttribute("aria-hidden", "true");
  const numbers = gutter.appendChild(document.createElement("div"));
  const pane = document.createElement("div");
  pane.className = "cv-text";
  const layer = document.createElement("pre");
  layer.className = "cv-layer";
  layer.setAttribute("aria-hidden", "true");
  // The label keeps only its words, so that nothing drawn around the textarea is read out as its name.
  const label = editor.closest("label");
  label.htmlFor = editor.id;
  label.after(box);
  pane.append(layer, editor);
  box.append(gutter, pane);

  // The template select and the fit write the text without an event, so the textarea's own `value`
  // repaints as well.
  const nativeValue = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value");
  Object.defineProperty(editor, "value", {
    configurable: true,
    get: () => nativeValue.get.call(editor),
    set: (text) => {
      nativeValue.set.call(editor, text);
      paint();
    },
  });

  const escaped = (text) => text.replace(/&/g, "&amp;").replace(/</g, "&lt;");

  // The copy under the textarea, coloured, and a number for each line. The last line break is there
  // because a `pre` draws no line after its final one, and a textarea does.
  function paint() {
    const text = editor.value;
    let html = "";
    let at = 0;
    tokenize(text, (kind, from, to) => {
      if (to <= from) return;
      html += `${escaped(text.slice(at, from))}<span class="t-${kind.replaceAll(" ", " t-")}">${escaped(text.slice(from, to))}</span>`;
      at = to;
    });
    layer.innerHTML = `${html}${escaped(text.slice(at))}\n`;
    let lines = 1;
    for (let line = text.indexOf("\n"); line >= 0; line = text.indexOf("\n", line + 1)) lines += 1;
    while (numbers.children.length > lines) numbers.lastElementChild.remove();
    while (numbers.children.length < lines) {
      numbers.appendChild(document.createElement("div")).textContent = numbers.children.length;
    }
  }

  // The copy has no scrollbars of its own, so it is moved, not scrolled: a scrollbar in the textarea
  // would otherwise leave it short of the end.
  function follow() {
    layer.style.transform = `translate(${-editor.scrollLeft}px, ${-editor.scrollTop}px)`;
    numbers.style.transform = `translateY(${-editor.scrollTop}px)`;
  }

  // A text that doesn't compile has its line marked, and the message under the editor leads there.
  const problem = document.getElementById("cv-error");
  document.getElementById("cv-workspace").addEventListener("cv-compiled", () => {
    numbers.querySelector(".error")?.classList.remove("error");
    if (!problem.hidden) numbers.children[problem.dataset.line - 1]?.classList.add("error");
  });
  problem.addEventListener("click", () => {
    const lines = editor.value.split("\n");
    const line = Math.min(Number(problem.dataset.line), lines.length);
    const start = lines.slice(0, line - 1).join("\n").length + (line > 1 ? 1 : 0);
    const at = start + Math.min(problem.dataset.column - 1, lines[line - 1].length);
    editor.focus();
    editor.setSelectionRange(at, at);
    // A textarea doesn't scroll to its caret by itself: the line goes to the middle.
    const height = parseFloat(getComputedStyle(editor).lineHeight);
    editor.scrollTop = Math.max(0, (line - 1) * height - editor.clientHeight / 2);
  });

  // Every change this file makes goes through the browser's own editing, the only way that keeps its
  // undo history: `setRangeText` and `value` both drop it. The selection then goes to `start`.
  let quiet = false;
  function edit(from, to, text, start, end = start) {
    quiet = true;
    editor.setSelectionRange(from, to);
    if (!document.execCommand(text ? "insertText" : "delete", false, text)) {
      editor.setRangeText(text);
      editor.dispatchEvent(new Event("input", { bubbles: true }));
    }
    quiet = false;
    editor.setSelectionRange(start, end);
  }

  const known = JSON.parse(document.getElementById("cv-editor").dataset.complete);
  // What can follow a `#`: the helpers of /nexus.typ, which the server lists, and Typst's own words.
  const helpers = [
    ...known.helpers,
    { snippet: "let ", about: "Name a value or a function" },
    { snippet: "set ", about: "Set a default, such as #set text(size: 10pt)" },
    { snippet: 'import "/nexus.typ": *', about: "The helpers, at the top of a source" },
  ].map(({ name = "", snippet, about }) => ({
    name,
    label: `#${snippet.split("(")[0].trim()}`,
    detail: about,
    text: snippet.replace("|", ""),
    caret: snippet.includes("|") ? snippet.indexOf("|") : snippet.length,
  }));
  const quoted = (words) => words.map((word) => `"${word}"`);
  // The named arguments of a call, and the values of those that have only a few.
  const ARGS = {
    "cv.with": {
      lang: quoted(["en", "de"]),
      template: quoted(known.templates),
      pages: ["1", "2"],
      density: ["-2", "-1", "0", "1", "2"],
    },
    "#project": { stack: null, url: null },
  };

  // Whether a `#` at `at` starts code, which it doesn't in a string, a comment, raw text or math.
  function startsCode(text, at) {
    let inside = false;
    tokenize(text.slice(0, at + 1), (kind, from, to) => {
      if (from < at && to > at) inside = true;
    });
    return !inside;
  }

  // The `(` the caret stands in, looking back over closed pairs, or -1.
  function opener(text, at) {
    let depth = 0;
    for (let open = at - 1; open >= 0 && at - open < 2000; open -= 1) {
      if (text[open] === ")") depth += 1;
      else if (text[open] === "(" && depth-- === 0) return open;
    }
    return -1;
  }

  // What can be written at the caret: the choices, and the stretch of text one of them replaces.
  // Argument names come only once a letter is typed, or when `asked`.
  function completions(asked) {
    const text = editor.value;
    const at = editor.selectionStart;
    if (at !== editor.selectionEnd) return null;
    const line = text.slice(text.lastIndexOf("\n", at - 1) + 1, at);
    let found;
    if ((found = /#(employment|education|metric)\("([^"\n]*)$/.exec(line))) {
      const [, kind, typed] = found;
      const from = at - typed.length;
      // The whole id goes, up to the quote that closes it.
      const rest = /^[^"\n]*(?=")/.exec(text.slice(at));
      const to = at + (rest ? rest[0].length : 0);
      const others = text.slice(0, from) + text.slice(to);
      const used = new Set([...others.matchAll(new RegExp(`#${kind}\\("([^"\n]*)"`, "g"))].map((call) => call[1]));
      const items = known[kind]
        .filter(({ id, label }) => id.startsWith(typed) || label.toLowerCase().includes(typed.toLowerCase()))
        .map(({ id, label }) => ({
          label: id,
          detail: label,
          used: used.has(id),
          text: id,
        }));
      // The caret then goes past what closes the call.
      return { from, to, items, skip: text.startsWith('")', to) ? 2 : 0 };
    }
    if ((found = /#([\p{L}_-]*)$/u.exec(line)) && line[found.index - 1] !== "\\" && startsCode(text, at - found[0].length)) {
      const typed = found[1];
      const items = helpers.filter((helper) => helper.label.startsWith(`#${typed}`) || helper.name.startsWith(typed));
      return { from: at - typed.length, to: at, items };
    }
    const open = opener(text, at);
    const call = /(cv\.with|#project)$/.exec(text.slice(Math.max(0, open - 20), Math.max(0, open)));
    const args = call && ARGS[call[1]];
    if (!args) return null;
    const said = text.slice(open + 1, at);
    const last = said.slice(said.lastIndexOf(",") + 1);
    if ((found = /^\s*([a-z]+):\s*("?)([\w-]*)$/.exec(last)) && args[found[1]]) {
      const [, name, quote, typed] = found;
      // A value half typed goes whole, with the quote that closes it.
      const rest = (quote ? /^[\w-]*"?/ : /^[\w-]*/).exec(text.slice(at))[0];
      const items = args[name]
        .filter((value) => value.replaceAll('"', "").startsWith(typed))
        .map((value) => ({ label: value, detail: "", text: value }));
      return { from: at - quote.length - typed.length, to: at + rest.length, items };
    }
    // Inside a string there is no name to write.
    if (said.split('"').length % 2 === 0) return null;
    found = /^\s*([a-z]*)$/.exec(last);
    if (!found || !(found[1] || asked)) return null;
    const whole = said + text.slice(at).split(")")[0];
    const items = Object.keys(args)
      .filter((name) => name.startsWith(found[1]) && !new RegExp(`\\b${name}\\s*:`).test(whole))
      .map((name) => ({
        label: name,
        detail: args[name] ? args[name].join(", ").replaceAll('"', "") : "",
        text: args[name] ? `${name}: ` : `${name}: ""`,
        caret: args[name] ? undefined : name.length + 3,
      }));
    return { from: at - found[1].length, to: at, items };
  }

  const list = document.createElement("ul");
  list.id = "cv-complete";
  list.className = "cv-complete";
  list.setAttribute("role", "listbox");
  list.setAttribute("aria-label", "Completions");
  list.hidden = true;
  box.append(list);
  editor.setAttribute("aria-controls", list.id);
  const measure = document.createElement("canvas").getContext("2d");
  let offered = null;
  let chosen = 0;

  function choose(index) {
    chosen = (index + offered.items.length) % offered.items.length;
    [...list.children].forEach((row, at) => row.setAttribute("aria-selected", String(at === chosen)));
    const row = list.children[chosen];
    list.scrollTop = Math.min(row.offsetTop, Math.max(list.scrollTop, row.offsetTop + row.offsetHeight - list.clientHeight));
    editor.setAttribute("aria-activedescendant", row.id);
  }

  // Under the start of what a choice replaces, or over it where the editor ends too soon for the list.
  function place() {
    const style = getComputedStyle(editor);
    const before = editor.value.slice(0, offered.from).split("\n");
    measure.font = `${style.fontSize} ${style.fontFamily}`;
    const left = pane.offsetLeft + parseFloat(style.paddingLeft) + measure.measureText(before.at(-1).replaceAll("\t", "  ")).width - editor.scrollLeft;
    const line = parseFloat(style.lineHeight);
    const top = parseFloat(style.paddingTop) + before.length * line - editor.scrollTop;
    // Measured from the left edge, where the list is as wide as it gets.
    list.style.left = "0";
    const over = top - line - list.offsetHeight;
    list.style.left = `${Math.max(0, Math.min(left, box.clientWidth - list.offsetWidth))}px`;
    list.style.top = `${top + list.offsetHeight > box.clientHeight && over >= 0 ? over : top}px`;
  }

  function close() {
    offered = null;
    list.hidden = true;
    editor.removeAttribute("aria-activedescendant");
  }

  function offer(found) {
    offered = found;
    list.replaceChildren(
      ...found.items.map((item, index) => {
        const row = document.createElement("li");
        row.id = `cv-complete-${index}`;
        row.setAttribute("role", "option");
        const part = (tag, text) => Object.assign(document.createElement(tag), { textContent: text });
        row.append(part("b", item.label));
        // Right after the id, where a long line about it can't push it out of sight.
        if (item.used) row.append(part("em", "already used"));
        row.append(part("small", item.detail));
        return row;
      }),
    );
    list.hidden = false;
    place();
    choose(0);
  }

  // A single choice that is already written is no choice.
  function suggest(asked) {
    const found = completions(asked);
    const written = found && found.items.length === 1 && found.items[0].text === editor.value.slice(found.from, found.to);
    if (found && found.items.length && !written) offer(found);
    else close();
  }

  // Writes a choice, and offers what it leads to: the ids inside a new call, the values after a name.
  function accept(item) {
    const { from, to, skip = 0 } = offered;
    close();
    edit(from, to, item.text, from + (item.caret ?? item.text.length + skip));
    suggest(true);
  }

  // A press on the list must not take the focus, and with it the caret, from the textarea.
  list.addEventListener("mousedown", (event) => event.preventDefault());
  list.addEventListener("click", (event) => {
    const row = event.target.closest("li");
    if (row) accept(offered.items[[...list.children].indexOf(row)]);
  });

  // Tab moves the lines of the selection in by two spaces, and Shift+Tab out. With nothing selected,
  // Tab writes the two spaces where the caret is.
  function indent(out) {
    const text = editor.value;
    const { selectionStart: a, selectionEnd: b } = editor;
    close();
    if (!out && a === b) return edit(a, b, "  ", a + 2);
    const from = text.lastIndexOf("\n", a - 1) + 1;
    // A selection that ends at the start of a line doesn't hold that line.
    const end = text.indexOf("\n", b > a && text[b - 1] === "\n" ? b - 1 : b);
    const to = end < 0 ? text.length : end;
    const lines = text.slice(from, to).split("\n");
    const moved = lines.map((line) => (out ? line.replace(/^ {1,2}|^\t/, "") : line && `  ${line}`));
    const block = moved.join("\n");
    if (block.length === to - from) return;
    const start = Math.max(from, a + moved[0].length - lines[0].length);
    edit(from, to, block, start, Math.max(start, b + block.length - (to - from)));
  }

  const PAIRS = { "(": ")", "[": "]", "{": "}", '"': '"' };

  // Enter keeps the line's indentation and goes on with its list. An item left empty ends the list,
  // and between a pair of brackets the caret gets a line of its own, one step in.
  function newline() {
    const text = editor.value;
    const { selectionStart: a, selectionEnd: b } = editor;
    const from = text.lastIndexOf("\n", a - 1) + 1;
    const [, pad, mark = "", rest] = /^([ \t]*)([-+] )?(.*)$/.exec(text.slice(from, a));
    const opened = text[a - 1];
    if (a === b && mark && !rest && (a === text.length || text[a] === "\n")) edit(from, a, "", from);
    else if (a === b && opened !== '"' && PAIRS[opened] && PAIRS[opened] === text[a]) {
      edit(a, b, `\n${pad}  \n${pad}`, a + pad.length + 3);
    } else edit(a, b, `\n${pad}${mark}`, a + 1 + pad.length + mark.length);
  }

  // Brackets and quotes come in pairs: one typed brings the other, or goes round the selection.
  // Typing the one that is already there steps over it, and Backspace between the two takes both.
  // Says whether the key was dealt with.
  function paired(key) {
    const text = editor.value;
    const { selectionStart: a, selectionEnd: b } = editor;
    const before = text[a - 1] || "";
    const next = text[a] || "";
    if (key === "Backspace") {
      if (a !== b || !next || PAIRS[before] !== next) return false;
      edit(a - 1, a + 1, "", a - 1);
    } else if (a === b && next === key && Object.values(PAIRS).includes(key) && before !== "\\") {
      editor.setSelectionRange(a + 1, a + 1);
      close();
      return true;
    } else if (!(key in PAIRS)) return false;
    else if (a !== b) edit(a, b, key + text.slice(a, b) + PAIRS[key], a + 1, b + 1);
    // Not in front of a word. And a quote after a letter or a backslash closes or is written out.
    else if (/[^\s)\]}]/.test(next) || (key === '"' && /[\p{L}\p{N}\\"]/u.test(before))) return false;
    else edit(a, a, key + PAIRS[key], a + 1);
    suggest(false);
    return true;
  }

  // Set by Escape: the next Tab then leaves the editor, as it does anywhere else on the page.
  let leaving = false;
  document.getElementById("cv-keys").hidden = false;

  editor.addEventListener("keydown", (event) => {
    if (event.isComposing) return;
    const { key } = event;
    const leave = leaving;
    if (key !== "Shift") leaving = false;
    const command = event.ctrlKey || event.metaKey;
    if (event.ctrlKey && key === " ") suggest(true);
    else if (command && key === "s") editor.form.requestSubmit(document.getElementById("cv-save"));
    else if (command && key === "Enter") document.getElementById("cv-compile").click();
    else if (command || event.altKey) return;
    else if (offered && (key === "ArrowDown" || key === "ArrowUp")) choose(chosen + (key === "ArrowDown" ? 1 : -1));
    else if (offered && (key === "Enter" || (key === "Tab" && !event.shiftKey))) accept(offered.items[chosen]);
    else if (key === "Escape") {
      if (offered) close();
      else leaving = true;
    } else if (key === "Tab" && !leave) indent(event.shiftKey);
    else if (key === "Enter") newline();
    else if (!paired(key)) {
      // The caret leaves what the list was for.
      if (key.startsWith("Arrow") || key === "Home" || key === "End") close();
      return;
    }
    event.preventDefault();
  });

  editor.addEventListener("input", (event) => {
    paint();
    if (quiet) return;
    // Typing offers what fits. A paste, an undo or a text set from outside does not.
    if (/^(insertText|insertCompositionText|deleteContentBackward)$/.test(event.inputType)) suggest(false);
    else close();
  });
  editor.addEventListener("scroll", () => {
    follow();
    if (offered) place();
  });
  editor.addEventListener("blur", close);
  editor.addEventListener("mousedown", close);
  paint();
  follow();
})();
