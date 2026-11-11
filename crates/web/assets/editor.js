// The source editor on a variant page: colours and line numbers on top of the plain textarea, which
// stays the form's field and the only holder of the text. Without this file the textarea works as it is.
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
  editor.before(box);
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

  editor.addEventListener("input", paint);
  editor.addEventListener("scroll", follow);
  paint();
  follow();
})();
