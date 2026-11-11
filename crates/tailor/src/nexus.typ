// The Nexus CV template library. Everything here renders hard facts from /facts.json by id.

#let facts = json("/facts.json")

#let _accent = rgb("#1f3a5f")
#let _muted = rgb("#55606e")

#let _find(list, key, value, kind) = {
  let found = list.find(entry => entry.at(key) == value)
  if found == none {
    let known = list.map(entry => entry.at(key)).join(", ")
    panic("no " + kind + " with " + key + " \"" + value + "\" in the facts. Known: " + if known == none { "none" } else { known })
  }
  found
}

#let _url(value) = if value.starts-with("http") { value } else { "https://" + value }

// A section heading over a rule.
#let _ruled(size, colour) = body => {
  set text(size: size, weight: "bold", fill: colour)
  block(above: 1.05em, below: 0.55em, width: 100%, stroke: (bottom: 0.6pt + colour), inset: (bottom: 0.25em), body)
}

// A section heading in capitals.
#let _caps(size, colour) = body => {
  set text(size: size, weight: "bold", fill: colour)
  block(above: 1.7em, below: 0.8em, upper(body))
}

// A section heading on a tinted bar.
#let _barred(size, colour) = body => {
  set text(size: size, weight: "bold", fill: colour)
  block(above: 1.3em, below: 0.75em, width: 100%, fill: colour.lighten(88%), inset: (x: 0.6em, y: 0.4em), body)
}

#let _classic = (
  font: "Libertinus Serif",
  size: 10.5pt,
  small: 9.5pt,
  name: 21pt,
  margin: (x: 2cm, top: 1.7cm, bottom: 1.6cm),
  leading: 0.55em,
  accent: _accent,
  muted: _muted,
  // The colours of the name and of the contact details under it.
  title: _accent,
  detail: _muted,
  heading: _ruled(11.5pt, _accent),
  // How the header is laid out: "plain", "bar", "band" or "line".
  header: "plain",
  // What stands between contact details and between languages.
  sep: [ #h(0.25em)·#h(0.25em) ],
  // The width and colour of a side column, which takes the header, skills and languages.
  side: none,
  panel: none,
)

#let _sans = (font: "Inter", size: 9.5pt, small: 8.5pt, leading: 0.62em)

// The designs a CV can be drawn in. Each draws the same source.
#let _templates = (
  classic: _classic,
  modern: _classic + _sans + (
    name: 25pt,
    accent: rgb("#2457d6"),
    muted: rgb("#5b6472"),
    title: rgb("#111827"),
    detail: rgb("#5b6472"),
    heading: _caps(9pt, rgb("#2457d6")),
    header: "bar",
  ),
  banner: _classic + _sans + (
    name: 23pt,
    accent: rgb("#0f5c55"),
    title: white,
    detail: white,
    heading: _barred(10pt, rgb("#0f5c55")),
    header: "band",
  ),
  sidebar: _classic + _sans + (
    name: 19pt,
    margin: (left: 7cm, right: 1.6cm, top: 1.5cm, bottom: 1.4cm),
    accent: rgb("#1e293b"),
    title: white,
    detail: white,
    heading: _ruled(10.5pt, rgb("#1e293b")),
    sep: linebreak(),
    side: 6cm,
    panel: rgb("#1e293b"),
  ),
  compact: _classic + (
    size: 9.5pt,
    small: 8.5pt,
    name: 15pt,
    margin: (x: 1.4cm, top: 1.2cm, bottom: 1.1cm),
    leading: 0.5em,
    accent: rgb("#111111"),
    muted: rgb("#444444"),
    title: rgb("#111111"),
    detail: rgb("#444444"),
    heading: _ruled(10pt, rgb("#111111")),
    header: "line",
  ),
)

#let _theme = state("nexus-template", _classic)

// Marks what a side column takes.
#let _side = metadata("nexus-side")

// Mark where an entry starts and ends, for the layout notes to tell when a page break cuts one.
#let _entry = metadata("nexus-entry")
#let _entry-end = metadata("nexus-entry-end")

// The sections with something marked for the side column, and the rest, each in the order written.
// A `#show` or `#set` rule in the source wraps all that follows it, which stays whole in the main column.
#let _split(body) = {
  let side = ()
  let main = ()
  let section = ()
  let beside = false
  for child in body.at("children", default: (body,)) + (none,) {
    let wrapped = child != none and repr(child.func()) == "styled"
    if child == none or wrapped or (child.func() == heading and child.at("depth", default: 1) == 1) {
      if beside { side += section } else { main += section }
      section = ()
      beside = false
    }
    if wrapped {
      main.push(child)
    } else if child != none {
      section.push(child)
      beside = beside or child == _side or child.at("children", default: ()).contains(_side)
    }
  }
  (side.join(), main.join())
}

// A template drawn tighter, for a density above 0, or looser: the text, its line spacing and the
// margins above and below. The text never goes under 9 pt, or under its own size if that is smaller.
#let _dense(t, density) = {
  let scaled(size, least) = calc.max(calc.min(size, least), size * (1 - 0.04 * density))
  let margin = t.margin
  for side in ("top", "bottom") { margin.insert(side, margin.at(side) * (1 - 0.1 * density)) }
  t + (
    size: scaled(t.size, 9pt),
    // The small text stays as it is when the rest grows: a side column has no room for a wider address.
    small: if density > 0 { scaled(t.small, 8pt) } else { t.small },
    name: t.name * (1 - 0.04 * density),
    leading: t.leading * (1 - 0.1 * density),
    margin: margin,
  )
}

// The page setup. Start a CV with `#show: cv`, `#show: cv.with(lang: "de")` or
// `#show: cv.with(template: "modern")`. `pages` is how many full pages the CV aims for, which draws
// nothing: the layout notes read it and say how far off the CV is. `density` from -2 to 2 draws the
// same text looser or tighter, to close a small gap to that.
#let cv(lang: "en", template: "classic", pages: none, density: 0, body) = {
  if template not in _templates {
    panic("no template \"" + template + "\". Known: " + _templates.keys().join(", "))
  }
  if density not in (-2, -1, 0, 1, 2) {
    panic("no density " + repr(density) + ". It goes from -2, the loosest, to 2, the tightest")
  }
  let t = _templates.at(template)
  if density != 0 { t = _dense(t, density) }
  set document(title: facts.name, author: facts.name)
  set page(
    paper: "a4",
    margin: t.margin,
    background: if t.side != none { place(top + left, rect(width: t.side, height: 100%, fill: t.panel)) },
  )
  set text(font: t.font, size: t.size, lang: lang)
  set par(leading: t.leading, spacing: 0.7em, justify: false)
  set list(marker: [•], indent: 0.4em, body-indent: 0.5em, spacing: 0.45em)
  show link: set text(fill: t.accent)
  show heading.where(level: 1): it => (t.heading)(it.body)
  _theme.update(t)
  if t.side == none {
    body
  } else {
    let (side, main) = _split(body)
    place(top + left, dx: 0.8cm - t.margin.left, block(width: t.side - 1.6cm, {
      set text(fill: white)
      show link: set text(fill: white)
      show heading.where(level: 1): it => (_ruled(10pt, white))(it.body)
      side
    }))
    main
  }
}

// The width of the text on a page.
#let _room(t) = 21cm - 2 * t.margin.at("x", default: 0cm) - t.margin.at("left", default: 0cm) - t.margin.at("right", default: 0cm)

// `width` is the room the details have.
#let _contact(t, fill, width) = {
  let c = facts.contact
  let parts = ()
  if c.email != "" { parts.push(link("mailto:" + c.email, c.email)) }
  if c.phone != "" { parts.push(c.phone) }
  if c.location != "" { parts.push(c.location) }
  if c.linkedin != "" { parts.push(link(_url(c.linkedin), c.linkedin)) }
  if c.github != "" { parts.push(link(_url(c.github), c.github)) }
  // Details that need two lines are shared out evenly, so that none stands alone on the second.
  let line(parts) = text(size: t.small, fill: fill, parts.map(box).join(t.sep))
  let widths = range(1, parts.len()).map(n => (n, calc.max(measure(line(parts.slice(0, n))).width, measure(line(parts.slice(n))).width)))
  let even = widths.filter(w => w.at(1) <= width).sorted(key: w => w.at(1)).at(0, default: none)
  if t.sep == linebreak() or even == none or measure(line(parts)).width <= width {
    line(parts)
  } else {
    line(parts.slice(0, even.at(0)))
    linebreak()
    line(parts.slice(even.at(0)))
  }
}

// One line of contact details: email, phone, location, LinkedIn, GitHub.
#let contact() = context _contact(_theme.get(), _theme.get().muted, _room(_theme.get()))

// The name and contact line at the top of a CV.
#let header() = _side + context {
  let t = _theme.get()
  let name = text(size: t.name, weight: "bold", fill: t.title, facts.name)
  show link: it => if t.detail == white { text(fill: white, it) } else { it }
  if t.header == "band" {
    block(width: 100%, below: 1.6em, fill: t.accent, outset: (x: t.margin.x, top: t.margin.top, bottom: 1em))[
      #name
      #v(-0.3em)
      #_contact(t, t.detail, _room(t))
    ]
  } else if t.header == "line" {
    grid(
      columns: (auto, 1fr),
      column-gutter: 1em,
      align: (bottom + left, bottom + right),
      name,
      _contact(t, t.detail, _room(t) - measure(name).width - 1em.to-absolute()),
    )
  } else {
    block(below: 0.9em)[
      #name
      #v(-0.45em)
      #_contact(t, t.detail, _room(t))
      #if t.header == "bar" { v(0.15em); line(length: 2.4cm, stroke: 2.5pt + t.accent) }
    ]
  }
}

// The date stands by the last line, so that a wrapped title reads whole, line after line.
#let _dated(t, left, date) = grid(
  columns: (1fr, auto),
  column-gutter: 1em,
  align: bottom,
  left,
  text(fill: t.muted, date),
)

#let _employer(e) = if e.title == "" { strong(e.company) } else { [#strong(e.company) · #e.title] }

// One job. With a body it is a full entry: `#employment("id")[- a bullet]`. Without one, a single
// compact line.
#let employment(id, ..body) = {
  let e = _find(facts.employment, "id", id, "employment")
  let bullets = body.pos().at(0, default: none)
  _entry + context {
    let t = _theme.get()
    if bullets == none {
      let place = if e.location != "" { [ · #e.location] }
      block(below: 0.55em, _dated(t, [#_employer(e)#place], e.date))
    } else {
      block(above: 0.85em, below: 0.75em)[
        #_dated(t, _employer(e), e.date)
        #if e.location != "" { v(-0.35em); text(size: t.small, style: "italic", fill: t.muted, e.location) }
        #v(-0.1em)
        #bullets
      ]
    }
  } + _entry-end
}

// One qualification, by id.
#let education(id) = {
  let e = _find(facts.education, "id", id, "education")
  let place = if e.location != "" { [ · #e.location] }
  _entry + context {
    let t = _theme.get()
    block(above: 0.7em, below: 0.7em)[
      #_dated(t, [#strong(e.institution)#place], e.date)
      #v(-0.35em)
      #e.qualification
      #if e.details != "" { linebreak(); text(size: t.small, fill: t.muted, e.details) }
    ]
  } + _entry-end
}

// All languages with their levels.
#let languages() = _side + context facts.languages.map(l => [#l.name (#l.level)]).join(_theme.get().sep)

// A metric's locked wording, by key.
#let metric(key) = _find(facts.metrics, "key", key, "metric").text

// One line of skills: `#skills("Languages")[Rust, Go]`.
#let skills(category, items) = _side + block(above: 0.5em, below: 0.5em)[#strong(category): #items]

// A side project: `#project("Name", stack: "Rust", url: "example.org")[What it does.]`
#let project(name, stack: "", url: "", description) = _entry + context block(above: 0.7em, below: 0.7em)[
  #strong(if url != "" { link(_url(url), name) } else { name })#if stack != "" [ · #text(fill: _theme.get().muted, stack)]
  #linebreak()
  #description
] + _entry-end
