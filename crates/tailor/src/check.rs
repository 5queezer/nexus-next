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
    /// One word mixing Latin with Cyrillic or Greek lookalikes, which has no honest use on a CV.
    MixedScript,
    /// A bidi control character: the page then draws text in another order than the check reads it.
    BidiControl,
    /// Text rotated, mirrored, skewed, stretched or squeezed: it can draw as something else, like 6
    /// as 9.
    Transform,
    /// An email, URL or link target the contact facts don't back. A reply goes to whoever it names.
    ContactDetails,
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
const CAPITALIZED_CLAIMS: &[&str] = &["Master", "Masters", "Promotion", "Lead", "Manager"];
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

/// CJK numerals with a place value of their own, which stay as they are in a number token.
const CJK_UNITS: &str = "十百千万萬亿億兆拾佰仟";

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
        | \b(?P<alnum>[\p{{L}}\p{{N}}]*[0-9{CJK_UNITS}\p{{Nl}}\p{{No}}][\p{{L}}\p{{N}}]*)
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

/// The value of a CJK numeral with a digit's meaning. `二〇一五` then reads `2015`, a number like any
/// other, so a word holding one is a number token too and must be allowed.
fn cjk_digit(c: char) -> Option<char> {
    Some(match c {
        '〇' | '零' => '0',
        '一' | '壹' => '1',
        '二' | '两' | '兩' | '贰' | '貳' => '2',
        '三' | '叁' | '參' => '3',
        '四' | '肆' => '4',
        '五' | '伍' => '5',
        '六' | '陆' | '陸' => '6',
        '七' | '柒' => '7',
        '八' | '捌' => '8',
        '九' | '玖' => '9',
        _ => return None,
    })
}

/// The value of a Roman numeral sign. NFKC would make `Ⅷ` the letters `VIII`, ordinary text, so
/// these become numbers before it runs.
fn roman_sign(c: char) -> Option<u32> {
    Some(match c as u32 {
        code @ 0x2160..=0x216B => code - 0x2160 + 1,
        code @ 0x2170..=0x217B => code - 0x2170 + 1,
        0x216C | 0x217C => 50,
        0x216D | 0x217D => 100,
        0x216E | 0x217E => 500,
        0x216F | 0x217F | 0x2180 => 1000,
        0x2181 => 5000,
        0x2182 => 10_000,
        _ => return None,
    })
}

/// Unicode folded for matching: NFKC, every decimal digit as ASCII, zero-width and soft-hyphen
/// characters gone, marks on digits gone, every dash `-`, runs of whitespace one space, Roman
/// numeral signs and CJK numerals as digits.
fn normalize(text: &str) -> String {
    let mut signs_as_digits = String::with_capacity(text.len());
    for c in text.chars() {
        match roman_sign(c) {
            Some(value) => signs_as_digits.push_str(&value.to_string()),
            None => signs_as_digits.push(c),
        }
    }
    let mut out = String::with_capacity(text.len());
    for c in signs_as_digits.nfkc() {
        match c.general_category() {
            _ if cjk_digit(c).is_some() => out.extend(cjk_digit(c)),
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

/// The Latin letter a Cyrillic or Greek letter is easily taken for. Lower-case Cyrillic `т`, `м`, `н`
/// and `в` only match Latin small capitals, and are left out so that Russian `сто` doesn't read `cto`.
fn latin_lookalike(c: char) -> Option<char> {
    Some(match c {
        'а' | 'α' => 'a',
        'с' | 'ϲ' => 'c',
        'ԁ' => 'd',
        'е' | 'ё' | 'ε' => 'e',
        'г' => 'r',
        'һ' => 'h',
        'і' | 'ї' | 'ι' => 'i',
        'ј' => 'j',
        'κ' | 'к' => 'k',
        'η' => 'n',
        'о' | 'ο' => 'o',
        'р' | 'ρ' => 'p',
        'ԛ' => 'q',
        'ѕ' => 's',
        'υ' => 'u',
        'ѵ' | 'ν' => 'v',
        'ԝ' => 'w',
        'х' | 'χ' => 'x',
        'у' | 'γ' => 'y',
        'А' | 'Α' => 'A',
        'В' | 'Β' => 'B',
        'С' | 'Ϲ' => 'C',
        'Ԁ' => 'D',
        'Е' | 'Ё' | 'Ε' => 'E',
        'Ԍ' => 'G',
        'Н' | 'Η' => 'H',
        'І' | 'Ї' | 'Ι' => 'I',
        'Ј' => 'J',
        'К' | 'Κ' => 'K',
        'М' | 'Μ' => 'M',
        'Ν' => 'N',
        'О' | 'Ο' => 'O',
        'Р' | 'Ρ' => 'P',
        'Ԛ' => 'Q',
        'Ѕ' => 'S',
        'Т' | 'Τ' => 'T',
        'Ѵ' => 'V',
        'Ԝ' => 'W',
        'Х' | 'Χ' => 'X',
        'У' | 'Ү' | 'Υ' => 'Y',
        'Ζ' => 'Z',
        _ => return None,
    })
}

/// Text as claim and banned-phrase matching reads it: lookalikes as Latin, and runs of three or more
/// single letters closed up, so `S e n i o r` is a word. Initials like `J. R. R.` hold dots and stay.
/// A run starting with a word of one letter is read both ways, since `a P h D` is `a PhD` but
/// `A b i t u r` is `Abitur`.
fn prepare_for_words(text: &str) -> String {
    static SPACED: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\b\p{L}(?: \p{L}){2,}\b").expect("the spaced-letters pattern is valid")
    });
    let folded: String = text
        .chars()
        .map(|c| latin_lookalike(c).unwrap_or(c))
        .collect();
    SPACED
        .replace_all(&folded, |caps: &regex::Captures<'_>| {
            let closed = caps[0].replace(' ', "");
            match closed.chars().next() {
                Some(first @ ('a' | 'A' | 'I')) => {
                    format!("{closed} {first} {}", &closed[first.len_utf8()..])
                }
                _ => closed,
            }
        })
        .into_owned()
}

fn is_latin_letter(c: char) -> bool {
    c.is_alphabetic() && (c < '\u{250}' || ('\u{1E00}'..='\u{1EFF}').contains(&c))
}

fn check_mixed_script(text: &str, known: &Known, findings: &mut Findings) {
    for word in words(text) {
        // Parts of a hyphenated word may honestly differ: `Москва-Berlin`, `α-Test`.
        let mixed = word.split('-').any(|part| {
            part.chars().any(is_latin_letter) && part.chars().any(|c| latin_lookalike(c).is_some())
        });
        if mixed && !known.words.contains(&word_key(word)) {
            let start = word.as_ptr() as usize - text.as_ptr() as usize;
            findings.add(
                Rule::MixedScript,
                word,
                context(text, start, start + word.len()),
            );
        }
    }
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
        let mut add_fact = |text: &str, is_wording: bool| {
            let text = normalize(text);
            if is_wording {
                add_words(&mut known_words, &text);
            }
            phrases.push_str(&text.to_lowercase());
            phrases.push('\n');
            for token in tokens(&text) {
                tokens_known.extend(date_parts(&token.canon).into_iter().map(str::to_owned));
            }
        };
        for text in fact_texts {
            add_fact(text, true);
        }
        for allowed in &facts.allowed_tokens {
            add_fact(allowed, false);
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
        let mut whole: Vec<String> = [&c.email, &c.phone, &c.linkedin, &c.github, &c.location]
            .into_iter()
            .map(|value| normalize(value.trim()))
            .filter(|value| !value.is_empty())
            .collect();
        // A `tel:` link's target, which `check_links` compares by its digits.
        if !c.phone.trim().is_empty() {
            whole.push(format!("tel:{}", normalize(&c.phone).replace(' ', "")));
        }
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
        let targets = text.links.iter().map(|link| &link.target);
        let metadata: Vec<String> = text
            .metadata
            .iter()
            .chain(targets)
            .map(|item| normalize(item))
            .collect();
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

/// An upper-case Roman numeral, which counts only next to a time unit: `CI` and `IV` are ordinary
/// text. `I` never does: it is the pronoun.
fn is_ascii_roman(word: &str) -> bool {
    static ROMAN: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new("^M{0,3}(?:CM|CD|D?C{0,3})(?:XC|XL|L?X{0,3})(?:IX|IV|V?I{0,3})$")
            .expect("the Roman numeral pattern is valid")
    });
    !word.is_empty() && word != "I" && ROMAN.is_match(word)
}

fn check_number_words(body: &str, known: &Known, findings: &mut Findings) {
    let originals: Vec<&str> = words(body).collect();
    let lower: Vec<String> = originals.iter().map(|word| word.to_lowercase()).collect();
    for (i, word) in lower.iter().enumerate() {
        let mut phrase = None;
        let parts: Vec<&str> = word.split('-').collect();
        let original_parts: Vec<&str> = originals[i].split('-').collect();
        if parts.windows(2).zip(&original_parts).any(|(p, original)| {
            (is_count(p[0]) || is_ascii_roman(original)) && TIME_UNITS.contains(&p[1])
        }) {
            phrase = Some(word.clone());
        } else if let Some((count, _)) =
            word.split_once("jähr").or_else(|| word.split_once("jaehr"))
            && is_count(count)
        {
            phrase = Some(word.clone());
        } else if is_count(word) || is_ascii_roman(originals[i]) {
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
    let without_phrases;
    let text = match &known.vocabulary_phrases {
        Some(phrases) => {
            without_phrases = phrases.replace_all(text, " ");
            without_phrases.as_ref()
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
        &document.body,
        &document.body_with_hyphens,
        &document.metadata,
    ]
    .map(|text| fold_case(&prepare_for_words(text)));
    for phrase in &facts.banned_phrases {
        let folded = fold_case(&prepare_for_words(normalize(phrase).trim()));
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

/// How many lines after a company or institution its date may stand.
const DATE_WINDOW_LINES: usize = 2;

fn check_binding(document: &Document, facts: &Facts, findings: &mut Findings) {
    let anchors = anchors(facts);
    let lower_lines: Vec<String> = document.lines.iter().map(|l| l.to_lowercase()).collect();
    let on_lines: Vec<Vec<&Anchor>> = lower_lines
        .iter()
        .map(|lower| {
            anchors
                .iter()
                .filter(|a| contains_word(lower, &a.lower))
                .collect()
        })
        .collect();
    // Names whose own date stands where a reader would take it for theirs.
    let mut dated: HashSet<&str> = HashSet::new();
    for (i, on_line) in on_lines.iter().enumerate() {
        if on_line.is_empty() {
            continue;
        }
        let line = &document.lines[i];
        let names: Vec<&str> = on_line.iter().map(|a| a.name.as_str()).collect();
        let names = names.join(", ");
        for token in tokens(line) {
            if token_year(&token.canon).is_some()
                && !on_line.iter().any(|a| a.date_tokens.contains(&token.canon))
            {
                findings.add(
                    Rule::EntityBinding,
                    &format!("{names}: {}", &line[token.start..token.end]),
                    line.clone(),
                );
            }
        }
        // Everything up to the next company or institution reads as this one's.
        let end = (i + 1..on_lines.len())
            .find(|&next| !on_lines[next].is_empty())
            .unwrap_or(on_lines.len());
        let window = lower_lines[i..end.min(i + DATE_WINDOW_LINES + 1)].join(" ");
        for anchor in on_line {
            if contains_word(&window, &anchor.date) {
                dated.insert(&anchor.lower);
            }
        }
        let region = lower_lines[i..end].join(" ");
        for other in &anchors {
            // A bare year stays free: "since 2019" in a bullet is normal.
            let bare_year = other.date.chars().all(|c| c.is_ascii_digit());
            let its_own = on_line
                .iter()
                .any(|a| a.lower == other.lower || a.date == other.date);
            if !bare_year && !its_own && contains_word(&region, &other.date) {
                findings.add(
                    Rule::EntityBinding,
                    &format!("{names}: {}", other.date),
                    format!("below {names} stands the date of {}", other.name),
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
            && !same_name.clone().any(|a| {
                (a.detail.is_empty() || present(&a.detail))
                    && (a.date.is_empty() || present(&a.date))
            })
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
        if rendered && !anchor.date.is_empty() && !dated.contains(anchor.lower.as_str()) {
            findings.add(
                Rule::EntityBinding,
                &format!("{}: no date", anchor.name),
                format!(
                    "its date must stand on its line or within the next {DATE_WINDOW_LINES}: {}",
                    anchor.name
                ),
            );
        }
    }
}

/// The numbers and spelled-out counts in `text` that the facts don't vouch for, as written there.
/// Each one, added to `allowed_tokens`, passes the check.
pub fn unverified_numbers(text: &str, facts: &Facts) -> Vec<String> {
    let known = Known::new(facts);
    let masked = known.mask_contact(&normalize(text));
    let mut findings = Findings {
        seen: HashSet::new(),
        violations: Vec::new(),
    };
    check_numbers(&masked, &known, &mut findings);
    check_number_words(&prepare_for_words(&masked), &known, &mut findings);
    findings.violations.into_iter().map(|v| v.token).collect()
}

fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// Right-to-left scripts order themselves by the Unicode algorithm and need none of these. On raw
/// text, because `normalize` removes them.
fn check_bidi(text: &RenderedText, findings: &mut Findings) {
    for item in text.lines.iter().chain(&text.metadata) {
        if let Some((at, control)) = item.char_indices().find(|(_, c)| is_bidi_control(*c)) {
            findings.add(
                Rule::BidiControl,
                &format!("U+{:04X}", control as u32),
                context(item, at, at + control.len_utf8()),
            );
        }
    }
    for run in &text.reversed {
        findings.add(
            Rule::BidiControl,
            run,
            "a bidi override draws this back to front".to_owned(),
        );
    }
}

/// A word only of letters that read as digits, directly before `%` or a time unit: `lOO%`, `lO years`.
/// `IO` and `CI` elsewhere are ordinary words, and `Team of lO` gets past this.
fn check_digit_lookalikes(masked: &str, known: &Known, findings: &mut Findings) {
    static LOOKALIKE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\b([lIOЗО]{2,})\s?(%|\p{L}+)").expect("the lookalike pattern is valid")
    });
    for caps in LOOKALIKE.captures_iter(masked) {
        let unit = caps[2].to_lowercase();
        let is_unit = matches!(unit.as_str(), "%" | "percent" | "prozent")
            || TIME_UNITS.contains(&unit.as_str());
        let word = caps.get(1).expect("group 1 always exists");
        if is_unit && !known.words.contains(&word.as_str().to_lowercase()) {
            findings.add(
                Rule::Numbers,
                word.as_str(),
                context(masked, word.start(), word.end()),
            );
        }
    }
}

static EMAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)[\p{L}\p{N}._%+'-]+\s?(?:@|[\[(]at[\])])\s?[\p{L}\p{N}-]+(?:\.[\p{L}\p{N}-]+)+",
    )
    .expect("the email pattern is valid")
});
/// A scheme or `www.`, or a host in a top-level domain common enough, and not a name of something in
/// tech: `Node.js`, `socket.io`. Upper-case `.NET` is the platform.
static URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:[a-z][a-z0-9+-]*://|www\.)[^\s]+|\b[\p{L}\p{N}-]+(?:\.[\p{L}\p{N}-]+)*\.(?:com|org|(?-i:net)|info|biz|eu|de|at|ch|uk|dev|app|xyz|example|test)\b(?:[/?#][^\s]*)?",
    )
    .expect("the URL pattern is valid")
});

/// A URL as it compares: no scheme, no `www.`, no case, no slash at the end.
fn url_key(url: &str) -> String {
    let lower = url.trim().to_lowercase();
    let bare = lower
        .split_once("://")
        .map_or(lower.as_str(), |(_, rest)| rest);
    bare.strip_prefix("www.")
        .unwrap_or(bare)
        .trim_end_matches('/')
        .to_owned()
}

/// `url` is `base` or a page under it.
fn is_under(url: &str, base: &str) -> bool {
    !base.is_empty()
        && url
            .strip_prefix(base)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

fn digits_of(text: &str) -> String {
    text.chars().filter(char::is_ascii_digit).collect()
}

/// Emails and URLs in `text`, trimmed of the punctuation around them, emails first. An email's domain
/// is not a URL of its own.
fn addresses(text: &str) -> (Vec<String>, Vec<String>) {
    const AROUND: &[char] = &[
        '.', ',', ';', ':', '!', '?', ')', ']', '}', '\'', '"', '»', '”',
    ];
    let trimmed = |found: &str| found.trim_end_matches(AROUND).to_owned();
    let emails = EMAIL.find_iter(text).map(|m| trimmed(m.as_str())).collect();
    let rest = EMAIL.replace_all(text, " ");
    let urls = URL.find_iter(&rest).map(|m| trimmed(m.as_str())).collect();
    (emails, urls)
}

/// The URLs a CV may show: the owner's profiles and pages under them, what the owner listed, and what
/// a fact holds.
fn url_allowed(url: &str, facts: &Facts, known: &Known) -> bool {
    let key = url_key(url);
    let contact = [&facts.contact.linkedin, &facts.contact.github];
    let listed = facts.allowed_tokens.iter().chain(&facts.vocabulary);
    contact
        .into_iter()
        .chain(listed)
        .any(|base| is_under(&key, &url_key(base)))
        || known.phrases.contains(&key)
}

/// A link's target and what it shows must agree with the contact facts. Any `http` or `https` target
/// is fine behind other text: a project's URL comes from the wording, and its name is what shows.
fn check_links(text: &RenderedText, facts: &Facts, findings: &mut Findings) {
    let contact = &facts.contact;
    let email = contact.email.trim().to_lowercase();
    let phone = digits_of(&contact.phone);
    let profiles = [&contact.linkedin, &contact.github].map(|url| url_key(url));
    for link in &text.links {
        let target = link.target.trim().to_lowercase();
        let (mail, tel) = (target.strip_prefix("mailto:"), target.strip_prefix("tel:"));
        let web = target.starts_with("http://") || target.starts_with("https://");
        let target_ok = match (mail, tel) {
            (Some(address), _) => !email.is_empty() && address == email,
            (_, Some(number)) => !phone.is_empty() && digits_of(number) == phone,
            _ => web,
        };
        let shown = normalize(&link.text).to_lowercase();
        let shows = |value: &str| !value.is_empty() && shown.contains(value);
        let key = url_key(&target);
        let shown_ok = !(shows(&email) && mail.is_none())
            && !(shows(&normalize(contact.phone.trim()).to_lowercase()) && tel.is_none())
            && profiles
                .iter()
                .all(|profile| !shows(profile) || (web && is_under(&key, profile)));
        if !target_ok || !shown_ok {
            findings.add(
                Rule::ContactDetails,
                &link.target,
                format!("a link showing \"{}\"", link.text),
            );
        }
    }
}

fn check_contact(
    text: &RenderedText,
    document: &Document,
    facts: &Facts,
    known: &Known,
    findings: &mut Findings,
) {
    let email = facts.contact.email.trim().to_lowercase();
    let metadata = text.metadata.iter().map(|item| normalize(item));
    for said in std::iter::once(document.body.clone()).chain(metadata) {
        let (emails, urls) = addresses(&said);
        for found in emails {
            let key: String = found.split_whitespace().collect::<String>().to_lowercase();
            if key != email && !known.phrases.contains(&key) {
                findings.add(Rule::ContactDetails, &found, found.clone());
            }
        }
        for found in urls {
            if !url_allowed(&found, facts, known) {
                findings.add(Rule::ContactDetails, &found, found.clone());
            }
        }
    }
    check_links(text, facts, findings);
}

/// The URLs in `text` that the facts don't vouch for, as written there. Each one, added to
/// `allowed_tokens`, passes the check.
pub fn unverified_urls(text: &str, facts: &Facts) -> Vec<String> {
    let known = Known::new(facts);
    let (_, urls) = addresses(&normalize(text));
    urls.into_iter()
        .filter(|url| !url_allowed(url, facts, &known))
        .collect()
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
    check_bidi(text, &mut findings);
    check_contact(text, &document, facts, &known, &mut findings);
    for masked in [&body, &metadata] {
        check_numbers(masked, &known, &mut findings);
        check_digit_lookalikes(masked, &known, &mut findings);
        check_mixed_script(masked, &known, &mut findings);
        let prepared = prepare_for_words(masked);
        check_number_words(&prepared, &known, &mut findings);
        check_claims(&prepared, &known, &mut findings);
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
    for run in &text.transformed {
        findings.add(
            Rule::Transform,
            run,
            "rotated, mirrored, skewed, stretched or squeezed text can draw as something else"
                .to_owned(),
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
