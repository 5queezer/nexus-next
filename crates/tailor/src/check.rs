//! The integrity check. Reads the rendered output, not the source, because Typst can compute and
//! rewrite text. See SPEC section 9a for the rules.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;
use unicode_properties::{GeneralCategory, UnicodeGeneralCategory};

use crate::{Facts, RenderedText};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Rule {
    Numbers,
    NumberWords,
    EntityBinding,
    ClaimWords,
    BannedPhrases,
    Images,
    /// Text the pages hold but no reader can see. It could satisfy the other rules unseen.
    HiddenText,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Violation {
    pub rule: Rule,
    /// What is wrong: the token, phrase or company.
    pub token: String,
    /// The surrounding rendered text.
    pub context: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub violations: Vec<Violation>,
    /// New capitalized words. Not refused: the gate cannot judge them, a person can.
    pub review_tokens: Vec<String>,
}

const CONTEXT_CHARS: usize = 40;

/// Time units that make a spelled-out count a claim. A filler may stand between them.
const TIME_UNITS: &[&str] = &[
    "year",
    "years",
    "yr",
    "yrs",
    "month",
    "months",
    "week",
    "weeks",
    "decade",
    "decades",
    "day",
    "days",
    "hour",
    "hours",
    "jahr",
    "jahre",
    "jahren",
    "jahres",
    "monat",
    "monate",
    "monaten",
    "woche",
    "wochen",
    "jahrzehnt",
    "jahrzehnte",
    "jahrzehnten",
    "tag",
    "tage",
    "tagen",
    "stunde",
    "stunden",
];
const COUNT_FILLERS: &[&str] = &[
    "plus",
    "full",
    "long",
    "more",
    "additional",
    "volle",
    "vollen",
    "lange",
    "langen",
    "weitere",
    "weiteren",
];
// No `one` and no German `ein*`: they read as articles far more often than as counts.
const COUNTS: &[&str] = &[
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "eleven",
    "twelve",
    "thirteen",
    "fourteen",
    "fifteen",
    "sixteen",
    "seventeen",
    "eighteen",
    "nineteen",
    "twenty",
    "thirty",
    "forty",
    "fifty",
    "sixty",
    "seventy",
    "eighty",
    "ninety",
    "hundred",
    "dozen",
    "zwei",
    "drei",
    "vier",
    "fünf",
    "sechs",
    "sieben",
    "acht",
    "neun",
    "zehn",
    "elf",
    "zwölf",
    "dreizehn",
    "vierzehn",
    "fünfzehn",
    "sechzehn",
    "siebzehn",
    "achtzehn",
    "neunzehn",
    "zwanzig",
    "dreißig",
    "vierzig",
    "fünfzig",
    "sechzig",
    "siebzig",
    "achtzig",
    "neunzig",
    "hundert",
    "dutzend",
];

/// Words that claim a degree, a seniority or a language level, compared in lower case.
const CLAIMS: &[&str] = &[
    // Degrees, English.
    "bachelor",
    "bachelors",
    "master's",
    "mba",
    "msc",
    "bsc",
    "phd",
    "dphil",
    "doctorate",
    "doctoral",
    "postdoc",
    "postgraduate",
    "undergraduate",
    "graduate",
    "degree",
    "diploma",
    "professor",
    "dr",
    "prof",
    // Degrees, German.
    "magister",
    "diplom",
    "dipl",
    "doktor",
    "doktorand",
    "doktorarbeit",
    "promoviert",
    "habilitation",
    "abitur",
    "fachabitur",
    "staatsexamen",
    "studium",
    "studiert",
    "absolvent",
    "absolventin",
    "ausbildung",
    "lehre",
    "meisterbrief",
    "hochschulabschluss",
    // Seniority.
    "senior",
    "sr",
    "junior",
    "jr",
    "principal",
    "chief",
    "vp",
    "svp",
    "evp",
    "cto",
    "ceo",
    "cfo",
    "coo",
    "cio",
    "ciso",
    "founder",
    "cofounder",
    "co-founder",
    "gründer",
    "mitgründer",
    "gründerin",
    "gruender",
    "director",
    "direktor",
    "direktorin",
    "leiter",
    "leiterin",
    "teamleiter",
    "teamleiterin",
    "abteilungsleiter",
    "abteilungsleiterin",
    "bereichsleiter",
    "bereichsleiterin",
    "projektleiter",
    "projektleiterin",
    "gruppenleiter",
    "gruppenleiterin",
    "geschäftsführer",
    "geschäftsführerin",
    "vorstand",
    "prokurist",
    "prokuristin",
    "oberarzt",
    // Language levels.
    "a1",
    "a2",
    "b1",
    "b2",
    "c1",
    "c2",
    "native",
    "fluent",
    "bilingual",
    "proficient",
    "muttersprache",
    "muttersprachler",
    "muttersprachlerin",
    "muttersprachlich",
    "fließend",
    "fliessend",
    "verhandlungssicher",
    "konversationssicher",
    "grundkenntnisse",
    "toefl",
    "ielts",
];
/// As `CLAIMS`, but only when capitalized: in lower case they are everyday words.
const CAPITALIZED_CLAIMS: &[&str] = &[
    "Master",
    "Masters",
    "Promotion",
    "Lead",
    "Manager",
    "Head",
    "Partner",
];
/// A compound holding one of these still claims it: `Masterstudium`, `Seniorentwickler`.
const CLAIM_PREFIXES: &[&str] = &[
    "bachelor",
    "masterstud",
    "masterab",
    "masterarbeit",
    "senior",
    "doktor",
    "geschäftsführ",
    "geschaeftsfuehr",
    "muttersprach",
    "diplomingenieur",
    "diplominformatik",
];
const CLAIM_SUFFIXES: &[&str] = &[
    "studium",
    "studiengang",
    "studienabschluss",
    "hochschulabschluss",
];
const CLAIM_PHRASES: &[&str] = &[
    "head of",
    "vice president",
    "mother tongue",
    "business fluent",
    "staff engineer",
    "first language",
    "ein jahrzehnt",
    "a decade",
];
/// These claim nothing as one part of a hyphenated word: `cloud-native`, `360-degree`.
const WHOLE_WORD_CLAIMS: &[&str] = &["native", "fluent", "dr", "sr", "jr", "lehre", "degree"];

static TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    const MONTH: &str = "jan(?:uary|uar)?|jän(?:ner)?|feb(?:ruary|ruar)?|mar(?:ch)?|märz|maerz|apr(?:il)?|may|mai|jun(?:e|i)?|jul(?:y|i)?|aug(?:ust)?|sept?(?:ember)?|o[ck]t(?:ober)?|nov(?:ember)?|de[cz](?:ember)?";
    const YEAR: &str = "(?:19|20)[0-9]{2}";
    const CUR_BEFORE: &str = r"€|\$|£|¥|\b(?:eur|euro|usd|chf|gbp)";
    const CUR_AFTER: &str = r"€|\$|£|¥|(?:eur|euro|usd|chf|gbp)\b";
    let pattern = format!(
        r"(?xi)
        \b(?P<d1>[0-9]{{1,2}})\.\s?(?P<m1>[0-9]{{1,2}})\.\s?(?P<y1>{YEAR})\b
        | \b(?P<y2>{YEAR})-(?P<m2>0[1-9]|1[0-2])-(?P<d2>[0-3][0-9])\b
        | \b(?P<d3>[0-9]{{1,2}})\.?\s(?P<n3>{MONTH})\.?\s(?P<y3>{YEAR})\b
        | \b(?P<n4>{MONTH})\.?\s(?P<y4>{YEAR})\b
        | \b(?P<m5>0?[1-9]|1[0-2])[/.](?P<y5>{YEAR})\b
        | \b(?P<y6>{YEAR})-(?P<m6>0[1-9]|1[0-2])\b
        | \b(?P<lit>[0-9]{{1,4}}/[0-9]{{1,4}}|[0-9]+(?:\.[0-9]+){{2,}})\b
        | (?:(?P<cur1>{CUR_BEFORE})\s?)?
          \b(?P<num>[1-9][0-9]{{0,2}}(?:[.,][0-9]{{3}})+|[0-9]+(?:[.,][0-9]+)?)
          (?: \s?(?P<word>mio\.?|mrd\.?|mill?ion(?:en)?|billion(?:en)?|milliarden?|tsd\.?|thousand)\b
            | (?P<suf>[kmb])\b
            | \b )
          (?:\s?(?P<pct>%|(?:percent|prozent|prozentpunkte?)\b)|\s?(?P<cur2>{CUR_AFTER}))?
        | \b(?P<alnum>[\p{{L}}\p{{N}}]*[0-9][\p{{L}}\p{{N}}]*)
        "
    );
    Regex::new(&pattern).expect("the token pattern is valid")
});

/// The value of a decimal digit. Unicode assigns them in contiguous ascending runs from zero.
fn digit_value(c: char) -> u32 {
    let is_digit = |code| {
        char::from_u32(code).is_some_and(|c| c.general_category() == GeneralCategory::DecimalNumber)
    };
    let mut zero = c as u32;
    while is_digit(zero - 1) {
        zero -= 1;
    }
    (c as u32 - zero) % 10
}

/// Unicode folded for matching: NFKC, every decimal digit as ASCII, zero-width and soft-hyphen
/// characters gone, marks on digits gone, every dash `-`, runs of whitespace one space.
fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.nfkc() {
        match c.general_category() {
            GeneralCategory::DecimalNumber => {
                out.push(char::from_digit(digit_value(c), 10).unwrap_or('0'));
            }
            GeneralCategory::Format => {}
            GeneralCategory::NonspacingMark if out.ends_with(|d: char| d.is_ascii_digit()) => {}
            GeneralCategory::DashPunctuation => out.push('-'),
            _ if c == '\u{2212}' => out.push('-'),
            _ if c.is_whitespace() || c.is_control() => {
                if !out.ends_with(' ') {
                    out.push(' ');
                }
            }
            _ => out.push(c),
        }
    }
    out.truncate(out.trim_end().len());
    out
}

fn fold_case(text: &str) -> String {
    text.to_lowercase().replace('ß', "ss")
}

/// Whether `needle` stands in `hay` as a whole word or words.
fn contains_word(hay: &str, needle: &str) -> bool {
    !needle.is_empty()
        && hay.match_indices(needle).any(|(start, _)| {
            let before = hay[..start].chars().next_back();
            let after = hay[start + needle.len()..].chars().next();
            !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
        })
}

struct Token {
    canon: String,
    start: usize,
    end: usize,
}

fn month_number(name: &str) -> &'static str {
    let name = name.to_lowercase();
    match name.chars().take(3).collect::<String>().as_str() {
        "jan" | "jän" => "01",
        "feb" => "02",
        "mar" | "mär" | "mae" => "03",
        "apr" => "04",
        "may" | "mai" => "05",
        "jun" => "06",
        "jul" => "07",
        "aug" => "08",
        "sep" => "09",
        "oct" | "okt" => "10",
        "nov" => "11",
        _ => "12",
    }
}

fn canon_number(raw: &str) -> String {
    let mut groups = raw.split(['.', ',']);
    let first = groups.next().unwrap_or_default();
    let rest: Vec<&str> = groups.collect();
    let thousands = !first.starts_with('0')
        && first.len() <= 3
        && !rest.is_empty()
        && rest.iter().all(|group| group.len() == 3);
    if thousands {
        std::iter::once(first).chain(rest).collect()
    } else {
        raw.replace(',', ".")
    }
}

fn canon_currency(raw: &str) -> String {
    match raw.to_lowercase().as_str() {
        "eur" | "euro" | "€" => "€".to_owned(),
        "usd" | "$" => "$".to_owned(),
        "gbp" | "£" => "£".to_owned(),
        other => other.to_owned(),
    }
}

/// Every number-bearing token in normalized text. Forms of one date or number share a `canon`:
/// a date is `2019-03` or `2019-03-15`, `1,200` and `1.200` are `1200`, `3,5` is `3.5`.
fn tokens(text: &str) -> Vec<Token> {
    TOKEN
        .captures_iter(text)
        .map(|caps| {
            let full = caps.get(0).expect("group 0 always exists");
            let get = |name: &str| caps.name(name).map(|m| m.as_str());
            let canon = if let (Some(y), Some(m), Some(d)) = (get("y1"), get("m1"), get("d1")) {
                format!("{y}-{m:0>2}-{d:0>2}")
            } else if let (Some(y), Some(m), Some(d)) = (get("y2"), get("m2"), get("d2")) {
                format!("{y}-{m}-{d}")
            } else if let (Some(y), Some(n), Some(d)) = (get("y3"), get("n3"), get("d3")) {
                format!("{y}-{}-{d:0>2}", month_number(n))
            } else if let (Some(y), Some(n)) = (get("y4"), get("n4")) {
                format!("{y}-{}", month_number(n))
            } else if let (Some(y), Some(m)) = (get("y5"), get("m5")) {
                format!("{y}-{m:0>2}")
            } else if let (Some(y), Some(m)) = (get("y6"), get("m6")) {
                format!("{y}-{m}")
            } else if let Some(literal) = get("lit") {
                literal.to_owned()
            } else if let Some(alnum) = get("alnum") {
                alnum.to_lowercase()
            } else {
                let mut canon = get("cur1").map(canon_currency).unwrap_or_default();
                canon.push_str(&canon_number(get("num").unwrap_or_default()));
                let scale = get("suf").map(str::to_lowercase).or_else(|| {
                    let word = get("word")?.to_lowercase();
                    Some(
                        match word.as_bytes()[0] {
                            b't' => "k",
                            b'b' => "b",
                            _ if word.starts_with("mrd") || word.starts_with("milliard") => "b",
                            _ => "m",
                        }
                        .to_owned(),
                    )
                });
                canon.push_str(&scale.unwrap_or_default());
                if get("pct").is_some() {
                    canon.push('%');
                }
                if let Some(currency) = get("cur2") {
                    canon = format!("{}{canon}", canon_currency(currency));
                }
                canon
            };
            Token {
                canon,
                start: full.start(),
                end: full.end(),
            }
        })
        .collect()
}

/// The year of a year or date token.
fn token_year(canon: &str) -> Option<&str> {
    let year = canon.get(..4)?;
    let is_year = matches!(canon.len(), 4 | 7 | 10)
        && year.bytes().all(|b| b.is_ascii_digit())
        && (year.starts_with("19") || year.starts_with("20"))
        && canon[4..].chars().all(|c| c.is_ascii_digit() || c == '-')
        && (canon.len() == 4 || canon.as_bytes()[4] == b'-');
    is_year.then_some(year)
}

/// A date's own token and the coarser ones a date implies: `2019-03` also allows `2019`.
fn date_parts(canon: &str) -> Vec<&str> {
    let Some(year) = token_year(canon) else {
        return vec![canon];
    };
    let mut parts = vec![canon, year];
    if canon.len() == 10 {
        parts.push(&canon[..7]);
    }
    parts
}

/// A word's characters. Trims the punctuation it may end in, as in `Dipl.-Ing.` at a sentence end.
fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !(c.is_alphanumeric() || matches!(c, '-' | '.' | '\'' | '’')))
        .map(|word| word.trim_matches(['-', '.', '\'', '’']))
        .filter(|word| !word.is_empty())
}

fn word_key(word: &str) -> String {
    word.to_lowercase().replace('.', "").replace('’', "'")
}

/// What the check compares against. Built once per call.
struct Known {
    tokens: HashSet<String>,
    words: HashSet<String>,
    /// Multi-word vocabulary entries, whose words are not free on their own.
    vocabulary_phrases: Option<Regex>,
    /// All fact wording and allowed tokens in lower case.
    phrases: String,
    /// Contact values, each allowed only whole, so that a postcode frees no small number.
    whole: Vec<String>,
}

fn add_words(set: &mut HashSet<String>, text: &str) {
    for word in words(text) {
        let key = word_key(word);
        for part in key.split('-') {
            set.insert(part.to_owned());
        }
        set.insert(key);
    }
}

impl Known {
    fn new(facts: &Facts) -> Self {
        let mut fact_texts: Vec<&str> = vec![&facts.name];
        for e in &facts.employment {
            fact_texts.extend([&e.company, &e.title, &e.date, &e.location].map(String::as_str));
        }
        for e in &facts.education {
            fact_texts.extend(
                [
                    &e.institution,
                    &e.qualification,
                    &e.date,
                    &e.location,
                    &e.details,
                ]
                .map(String::as_str),
            );
        }
        for l in &facts.languages {
            fact_texts.extend([&l.name, &l.level].map(String::as_str));
        }
        for m in &facts.metrics {
            fact_texts.push(&m.text);
        }
        let mut tokens_known = HashSet::new();
        let mut known_words = HashSet::new();
        let mut phrases = String::new();
        for text in fact_texts {
            let text = normalize(text);
            add_words(&mut known_words, &text);
            phrases.push_str(&text.to_lowercase());
            phrases.push('\n');
            for token in tokens(&text) {
                tokens_known.extend(date_parts(&token.canon).into_iter().map(str::to_owned));
            }
        }
        for allowed in &facts.allowed_tokens {
            let text = normalize(allowed);
            phrases.push_str(&text.to_lowercase());
            phrases.push('\n');
            for token in tokens(&text) {
                tokens_known.extend(date_parts(&token.canon).into_iter().map(str::to_owned));
            }
        }
        let mut alternatives = Vec::new();
        for entry in &facts.vocabulary {
            let entry = normalize(entry);
            if entry.trim().contains(' ') {
                alternatives.push(regex::escape(entry.trim()));
            } else {
                add_words(&mut known_words, &entry);
            }
            for token in tokens(&entry) {
                if token.canon.chars().any(char::is_alphabetic) {
                    tokens_known.insert(token.canon);
                }
            }
        }
        let vocabulary_phrases = (!alternatives.is_empty())
            .then(|| Regex::new(&format!("(?i){}", alternatives.join("|"))).ok())
            .flatten();
        let c = &facts.contact;
        let whole = [&c.email, &c.phone, &c.linkedin, &c.github, &c.location]
            .into_iter()
            .map(|value| normalize(value.trim()))
            .filter(|value| !value.is_empty())
            .collect();
        Self {
            tokens: tokens_known,
            words: known_words,
            vocabulary_phrases,
            phrases,
            whole,
        }
    }

    fn mask_contact(&self, text: &str) -> String {
        let mut masked = text.to_owned();
        for value in &self.whole {
            if masked.contains(value.as_str()) {
                masked = masked.replace(value.as_str(), " ");
            }
        }
        masked
    }
}

/// The rendered text normalized and joined, so that checks see across line breaks.
struct Document {
    lines: Vec<String>,
    /// Line-end hyphenation undone.
    body: String,
    /// As `body`, but a hyphen at a line end stays, for a phrase that holds it.
    body_with_hyphens: String,
    metadata: String,
}

impl Document {
    fn new(text: &RenderedText) -> Self {
        let lines: Vec<String> = text.lines.iter().map(|line| normalize(line)).collect();
        let mut body = String::new();
        let mut body_with_hyphens = String::new();
        for line in &lines {
            let hyphen_before = body_with_hyphens
                .strip_suffix('-')
                .and_then(|rest| rest.chars().next_back())
                .is_some_and(char::is_alphabetic);
            let first = line.chars().next();
            if hyphen_before && first.is_some_and(char::is_lowercase) {
                body.pop();
            } else if !(hyphen_before && first.is_some_and(char::is_alphabetic)) && !body.is_empty()
            {
                body.push(' ');
                body_with_hyphens.push(' ');
            }
            body.push_str(line);
            body_with_hyphens.push_str(line);
        }
        let metadata: Vec<String> = text.metadata.iter().map(|item| normalize(item)).collect();
        Self {
            lines,
            body,
            body_with_hyphens,
            metadata: metadata.join(" | "),
        }
    }
}

fn context(text: &str, start: usize, end: usize) -> String {
    let from = text[..start]
        .char_indices()
        .rev()
        .nth(CONTEXT_CHARS)
        .map_or(0, |(i, _)| i);
    let to = text[end..]
        .char_indices()
        .nth(CONTEXT_CHARS)
        .map_or(text.len(), |(i, _)| end + i);
    text[from..to].to_owned()
}

struct Findings {
    seen: HashSet<(Rule, String)>,
    violations: Vec<Violation>,
}

impl Findings {
    fn add(&mut self, rule: Rule, token: &str, context: String) {
        if self.seen.insert((rule, token.to_owned())) {
            self.violations.push(Violation {
                rule,
                token: token.to_owned(),
                context,
            });
        }
    }
}

fn check_numbers(masked: &str, known: &Known, findings: &mut Findings) {
    for token in tokens(masked) {
        if !known.tokens.contains(&token.canon) {
            let raw = &masked[token.start..token.end];
            findings.add(Rule::Numbers, raw, context(masked, token.start, token.end));
        }
    }
}

fn is_count(word: &str) -> bool {
    let last = word.rsplit('-').next().unwrap_or(word);
    let contains = |w: &str| COUNTS.contains(&w);
    contains(last)
        || last
            .split_once("und")
            .is_some_and(|(_, tens)| contains(tens))
}

fn check_number_words(body: &str, known: &Known, findings: &mut Findings) {
    let lower: Vec<String> = words(body).map(str::to_lowercase).collect();
    for (i, word) in lower.iter().enumerate() {
        let mut phrase = None;
        let parts: Vec<&str> = word.split('-').collect();
        if parts
            .windows(2)
            .any(|p| is_count(p[0]) && TIME_UNITS.contains(&p[1]))
        {
            phrase = Some(word.clone());
        } else if let Some((count, _)) =
            word.split_once("jähr").or_else(|| word.split_once("jaehr"))
            && is_count(count)
        {
            phrase = Some(word.clone());
        } else if is_count(word) {
            let next = lower.get(i + 1).map(String::as_str);
            let after = lower.get(i + 2).map(String::as_str);
            if next.is_some_and(|w| TIME_UNITS.contains(&w)) {
                phrase = Some(lower[i..=i + 1].join(" "));
            } else if next.is_some_and(|w| COUNT_FILLERS.contains(&w))
                && after.is_some_and(|w| TIME_UNITS.contains(&w))
            {
                phrase = Some(lower[i..=i + 2].join(" "));
            }
        }
        if let Some(phrase) = phrase
            && !known.phrases.contains(&phrase)
        {
            findings.add(Rule::NumberWords, &phrase, phrase.clone());
        }
    }
}

fn is_claim(key: &str) -> bool {
    static CLAIM_SET: LazyLock<HashSet<&str>> = LazyLock::new(|| CLAIMS.iter().copied().collect());
    CLAIM_SET.contains(key)
        || CLAIM_PREFIXES.iter().any(|prefix| key.starts_with(prefix))
        || CLAIM_SUFFIXES.iter().any(|suffix| key.ends_with(suffix))
}

fn check_claims(text: &str, known: &Known, findings: &mut Findings) {
    let unmasked;
    let text = match &known.vocabulary_phrases {
        Some(phrases) => {
            unmasked = phrases.replace_all(text, " ");
            unmasked.as_ref()
        }
        None => text,
    };
    for word in words(text) {
        let key = word_key(word);
        if known.words.contains(&key) {
            continue;
        }
        let hit = is_claim(&key)
            || key.split('-').count() > 1
                && key.split('-').any(|part| {
                    is_claim(part)
                        && !WHOLE_WORD_CLAIMS.contains(&part)
                        && !known.words.contains(part)
                });
        let capitalized = word.split('-').any(|part| {
            CAPITALIZED_CLAIMS.contains(&part) && !known.words.contains(&part.to_lowercase())
        });
        if hit || capitalized {
            let start = word.as_ptr() as usize - text.as_ptr() as usize;
            findings.add(
                Rule::ClaimWords,
                word,
                context(text, start, start + word.len()),
            );
        }
    }
    let lower = text.to_lowercase();
    for phrase in CLAIM_PHRASES {
        if contains_word(&lower, phrase) && !known.phrases.contains(phrase) {
            findings.add(Rule::ClaimWords, phrase, (*phrase).to_owned());
        }
    }
}

fn check_banned(document: &Document, facts: &Facts, findings: &mut Findings) {
    let texts = [
        fold_case(&document.body),
        fold_case(&document.body_with_hyphens),
        fold_case(&document.metadata),
    ];
    for phrase in &facts.banned_phrases {
        let folded = fold_case(normalize(phrase).trim());
        if !folded.is_empty()
            && let Some(found) = texts.iter().find_map(|text| {
                text.find(&folded)
                    .map(|start| context(text, start, start + folded.len()))
            })
        {
            findings.add(Rule::BannedPhrases, phrase, found);
        }
    }
}

/// A company or institution, the details that must render with it, and the entry's date tokens.
struct Anchor {
    name: String,
    lower: String,
    detail: String,
    date: String,
    date_tokens: HashSet<String>,
}

fn anchors(facts: &Facts) -> Vec<Anchor> {
    let employment = facts
        .employment
        .iter()
        .map(|e| (&e.company, &e.title, &e.date));
    let education = facts
        .education
        .iter()
        .map(|e| (&e.institution, &e.qualification, &e.date));
    employment
        .chain(education)
        .filter(|(name, _, _)| !name.trim().is_empty())
        .map(|(name, detail, date)| {
            let name = normalize(name.trim());
            let date = normalize(date.trim());
            let mut date_tokens = HashSet::new();
            for token in tokens(&date) {
                date_tokens.extend(date_parts(&token.canon).into_iter().map(str::to_owned));
            }
            Anchor {
                lower: name.to_lowercase(),
                name,
                detail: normalize(detail.trim()).to_lowercase(),
                date: date.to_lowercase(),
                date_tokens,
            }
        })
        .collect()
}

fn check_binding(document: &Document, facts: &Facts, findings: &mut Findings) {
    let anchors = anchors(facts);
    for line in &document.lines {
        let lower = line.to_lowercase();
        let on_line: Vec<&Anchor> = anchors
            .iter()
            .filter(|a| contains_word(&lower, &a.lower))
            .collect();
        if on_line.is_empty() {
            continue;
        }
        for token in tokens(line) {
            if token_year(&token.canon).is_some()
                && !on_line.iter().any(|a| a.date_tokens.contains(&token.canon))
            {
                let names: Vec<&str> = on_line.iter().map(|a| a.name.as_str()).collect();
                findings.add(
                    Rule::EntityBinding,
                    &format!("{}: {}", names.join(", "), &line[token.start..token.end]),
                    line.clone(),
                );
            }
        }
    }
    let lower_body = document.body.to_lowercase();
    let lower_kept = document.body_with_hyphens.to_lowercase();
    let present =
        |needle: &str| contains_word(&lower_body, needle) || contains_word(&lower_kept, needle);
    for anchor in &anchors {
        // Lower case is prose: a company called Data must not bind every "data".
        let rendered = contains_word(&document.body, &anchor.name)
            || contains_word(&document.body, &anchor.name.to_uppercase());
        let same_name = anchors.iter().filter(|a| a.lower == anchor.lower);
        if rendered
            && !same_name
                .clone()
                .any(|a| present(&a.detail) && (a.date.is_empty() || present(&a.date)))
        {
            findings.add(
                Rule::EntityBinding,
                &anchor.name,
                format!(
                    "rendered without its exact details and date: {}",
                    anchor.name
                ),
            );
        }
    }
}

/// Judges a rendered document against the facts. `previous` is the parent revision's text, for
/// `review_tokens`. Linear in the text, for facts of a fixed size.
pub fn check(text: &RenderedText, facts: &Facts, previous: Option<&RenderedText>) -> Report {
    let known = Known::new(facts);
    let document = Document::new(text);
    let mut findings = Findings {
        seen: HashSet::new(),
        violations: Vec::new(),
    };
    let body = known.mask_contact(&document.body);
    let metadata = known.mask_contact(&document.metadata);
    for masked in [&body, &metadata] {
        check_numbers(masked, &known, &mut findings);
        check_number_words(masked, &known, &mut findings);
        check_claims(masked, &known, &mut findings);
    }
    check_banned(&document, facts, &mut findings);
    check_binding(&document, facts, &mut findings);
    if text.image_count > 0 {
        findings.add(
            Rule::Images,
            &text.image_count.to_string(),
            "a CV has no images: the check cannot read them".to_owned(),
        );
    }

    for run in &text.hidden {
        findings.add(
            Rule::HiddenText,
            run,
            "white, tiny, clipped or off-page text: a CV shows everything it says".to_owned(),
        );
    }

    let mut familiar = known.words.clone();
    add_words(&mut familiar, &known.whole.join(" "));
    for allowed in &facts.allowed_tokens {
        add_words(&mut familiar, &normalize(allowed));
    }
    if let Some(entry) = facts
        .vocabulary
        .iter()
        .map(|v| normalize(v))
        .reduce(|a, b| a + " " + &b)
    {
        add_words(&mut familiar, &entry);
    }
    if let Some(previous) = previous {
        let previous = Document::new(previous);
        add_words(&mut familiar, &previous.body);
        add_words(&mut familiar, &previous.metadata);
    }
    let mut review_tokens = Vec::new();
    for word in words(&body).chain(words(&metadata)) {
        let key = word_key(word);
        let new = word.chars().next().is_some_and(char::is_uppercase)
            && !word.chars().any(|c| c.is_ascii_digit())
            && !familiar.contains(&key)
            && !key.split('-').all(|part| familiar.contains(part));
        if new && familiar.insert(key) {
            review_tokens.push(word.to_owned());
        }
    }
    Report {
        violations: findings.violations,
        review_tokens,
    }
}
