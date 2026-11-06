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

// The page setup. Start a CV with `#show: cv` or `#show: cv.with(lang: "de")`.
#let cv(lang: "en", body) = {
  set document(title: facts.name, author: facts.name)
  set page(paper: "a4", margin: (x: 2cm, top: 1.7cm, bottom: 1.6cm))
  set text(font: "Libertinus Serif", size: 10.5pt, lang: lang)
  set par(leading: 0.55em, spacing: 0.7em, justify: false)
  set list(marker: [•], indent: 0.4em, body-indent: 0.5em, spacing: 0.45em)
  show link: set text(fill: _accent)
  show heading.where(level: 1): it => {
    set text(size: 11.5pt, weight: "bold", fill: _accent)
    block(above: 1.05em, below: 0.55em, width: 100%, stroke: (bottom: 0.6pt + _accent), inset: (bottom: 0.25em), it.body)
  }
  body
}

// One line of contact details: email, phone, location, LinkedIn, GitHub.
#let contact() = {
  let c = facts.contact
  let parts = ()
  if c.email != "" { parts.push(link("mailto:" + c.email, c.email)) }
  if c.phone != "" { parts.push(c.phone) }
  if c.location != "" { parts.push(c.location) }
  if c.linkedin != "" { parts.push(link(_url(c.linkedin), c.linkedin)) }
  if c.github != "" { parts.push(link(_url(c.github), c.github)) }
  text(size: 9.5pt, fill: _muted, parts.map(box).join([ #h(0.25em)·#h(0.25em) ]))
}

// The name and contact line at the top of a CV.
#let header() = {
  block(below: 0.9em)[
    #text(size: 21pt, weight: "bold", fill: _accent, facts.name)
    #v(-0.45em)
    #contact()
  ]
}

#let _dated(left, date) = grid(
  columns: (1fr, auto),
  column-gutter: 1em,
  left,
  text(fill: _muted, date),
)

// One job. With a body it is a full entry: `#employment("id")[- a bullet]`. Without one, a single
// compact line.
#let employment(id, ..body) = {
  let e = _find(facts.employment, "id", id, "employment")
  let bullets = body.pos().at(0, default: none)
  if bullets == none {
    let place = if e.location != "" { [ · #e.location] }
    block(below: 0.55em, _dated([#strong(e.company) · #e.title#place], e.date))
  } else {
    block(above: 0.85em, below: 0.75em)[
      #_dated([#strong(e.company) · #e.title], e.date)
      #if e.location != "" { v(-0.35em); text(size: 9.5pt, style: "italic", fill: _muted, e.location) }
      #v(-0.1em)
      #bullets
    ]
  }
}

// One qualification, by id.
#let education(id) = {
  let e = _find(facts.education, "id", id, "education")
  let place = if e.location != "" { [ · #e.location] }
  block(above: 0.7em, below: 0.7em)[
    #_dated([#strong(e.institution)#place], e.date)
    #v(-0.35em)
    #e.qualification
    #if e.details != "" { linebreak(); text(size: 9.5pt, fill: _muted, e.details) }
  ]
}

// All languages with their levels.
#let languages() = facts.languages.map(l => [#l.name (#l.level)]).join([ #h(0.25em)·#h(0.25em) ])

// A metric's locked wording, by key.
#let metric(key) = _find(facts.metrics, "key", key, "metric").text

// One line of skills: `#skills("Languages")[Rust, Go]`.
#let skills(category, items) = block(above: 0.5em, below: 0.5em)[#strong(category): #items]

// A side project: `#project("Name", stack: "Rust", url: "example.org")[What it does.]`
#let project(name, stack: "", url: "", description) = block(above: 0.7em, below: 0.7em)[
  #strong(if url != "" { link(_url(url), name) } else { name })#if stack != "" [ · #text(fill: _muted, stack)]
  #linebreak()
  #description
]
