//! The integrity check. Reads the rendered output, not the source, because Typst can compute and
//! rewrite text. Two tiers, see SPEC section 9a: what a reader would see otherwise than the owner
//! reviews is refused, and everything else the owner should look at is listed for approval.

use std::collections::HashSet;
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use serde::{Deserialize, Serialize};
use similar::{ChangeTag, TextDiff};
use unicode_normalization::UnicodeNormalization;
use unicode_properties::{GeneralCategory, UnicodeGeneralCategory};

use crate::{Facts, RenderedText};

/// In this order a list of review items is read: most serious first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Rule {
    /// A fact the last approved text showed and this one doesn't.
    MissingFact,
    /// A fact's company or institution outside its own entry, or with another's date, title or place.
    EntityBinding,
    /// A fact's language with a level other than its own.
    LanguageLevel,
    ClaimWords,
    /// A word in neither the facts, `vocabulary`, the last locked wording nor the last approved text.
    /// Ahead of numbers: a whole CV lists every number, and a sentence nobody has seen must stay in sight.
    NewWords,
    Numbers,
    NumberWords,
    BannedPhrases,
    Images,
    /// Text the pages hold but no reader can see.
    HiddenText,
    /// One word mixing Latin with Cyrillic or Greek lookalikes, which has no honest use on a CV.
    MixedScript,
    /// A bidi control character: the page then draws text in another order than the check reads it.
    BidiControl,
    /// Text rotated, mirrored, skewed, stretched or squeezed: it can draw as something else, like 6
    /// as 9.
    Transform,
    /// An email, URL, phone number or link target the contact facts don't back. A reply goes to
    /// whoever it names.
    ContactDetails,
    /// A word of letters that read as digits, directly before `%` or a time unit: `lOO%`.
    DigitLookalike,
    /// A letter the fonts don't have, which draws as a crossed box while the text reads as the letter.
    MissingGlyph,
}

/// A hard refusal: a reader of the PDF would see something other than what the owner reviews, so no
/// approval can override it, and nothing is stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Violation {
    pub rule: Rule,
    /// What is wrong: the token, phrase or company.
    pub token: String,
    /// The surrounding rendered text.
    pub context: String,
}

/// Something for the owner to look at before approving. It blocks nothing: no list of words can tell
/// a true sentence from a false one, and a person can.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewItem {
    pub rule: Rule,
    pub token: String,
    pub context: String,
    /// Which line of [`review_text`] it stands on, from 1. None for what is no longer there.
    pub line: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub violations: Vec<Violation>,
    pub review_items: Vec<ReviewItem>,
}

const CONTEXT_CHARS: usize = 40;

/// Time units that make an ASCII Roman numeral a count. Elsewhere `CI` or `IV` are ordinary text.
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
    "half",
    "thousand",
    "tausend",
    "million",
    "millionen",
    "billion",
    "billionen",
    "milliarde",
    "milliarden",
    "trillion",
    "mio",
    "mrd",
    "tsd",
    "bn",
    "mn",
];
/// German counts that hold their number inside a longer word.
const COUNT_PARTS: &[&str] = &["einhalb", "anderthalb"];

/// Words that may claim a degree, a seniority or a language level, compared in lower case. Generous,
/// since they only highlight: `lead` is also a verb, and the owner can tell.
const CLAIMS: &[&str] = &[
    // Degrees, English.
    "bachelor",
    "bachelors",
    "master",
    "masters",
    "master's",
    "mba",
    "msc",
    "bsc",
    "phd",
    "dphil",
    "doctorate",
    "doctoral",
    "graduated",
    "honors",
    "honours",
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
    "promotion",
    "promoviert",
    "promovierte",
    "habilitation",
    "habilitierte",
    "abitur",
    "fachabitur",
    "staatsexamen",
    "studium",
    "studiert",
    "studierte",
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
    "lead",
    "manager",
    "head",
    "staff",
    "distinguished",
    "leitender",
    "leitende",
    "inhaber",
    "inhaberin",
    "mitinhaber",
    "mitinhaberin",
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
/// Degrees written with dots. Without them they are everyday letters: `ms`, `ma`.
const DOTTED_CLAIMS: &[&str] = &["ms", "ma", "ba", "bs", "llm", "meng", "beng"];
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
    "chef",
    "fließend",
    "fliessend",
    "fluent",
    "verhandlungssicher",
    "konversationssicher",
];
const CLAIM_SUFFIXES: &[&str] = &[
    "studium",
    "studiengang",
    "studienabschluss",
    "hochschulabschluss",
    "kenntnisse",
    "leitung",
    "leiter",
    "leiterin",
];
const CLAIM_PHRASES: &[&str] = &[
    "head of",
    "vice president",
    "mother tongue",
    "business fluent",
    "staff engineer",
    "first language",
    "cum laude",
    "native level",
    "native-level",
    "business level",
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
        | \b(?P<n4>{MONTH})[.,]?\s(?:of\s|des\sJahres\s)?(?P<y4>{YEAR})\b
        | \b(?P<y7>{YEAR})\s(?P<n7>{MONTH})\b
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
/// other, so a word holding one is a number token too.
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
            } else if let (Some(y), Some(n)) = (get("y7"), get("n7")) {
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
    words: HashSet<String>,
    /// All fact wording and allowed tokens in lower case, which a URL can be vouched for by.
    phrases: String,
    /// Each fact's wording and vocabulary entry, in lower case. A claim word is the fact's own only
    /// beside the rest of it: a true `Senior Engineer` frees no `Senior Director`.
    fact_phrases: Vec<String>,
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
        let mut known_words = HashSet::new();
        let mut phrases = String::new();
        let mut fact_phrases = Vec::new();
        for text in fact_texts {
            let text = normalize(text);
            add_words(&mut known_words, &text);
            let lower = fold_case(&prepare_for_words(&text));
            if !lower.is_empty() {
                fact_phrases.push(lower);
            }
            phrases.push_str(&text.to_lowercase());
            phrases.push('\n');
        }
        for allowed in &facts.allowed_tokens {
            phrases.push_str(&normalize(allowed).to_lowercase());
            phrases.push('\n');
        }
        for entry in &facts.vocabulary {
            let entry = normalize(entry);
            add_words(&mut known_words, &entry);
            fact_phrases.push(fold_case(&prepare_for_words(entry.trim())));
        }
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
            words: known_words,
            phrases,
            fact_phrases,
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

#[derive(Default)]
struct Items {
    seen: HashSet<(String, Option<usize>)>,
    list: Vec<ReviewItem>,
}

impl Items {
    /// One item per word and line. The first rule to name it says most.
    fn add(&mut self, rule: Rule, token: &str, context: &str, at: Option<usize>) {
        if self.seen.insert((token.to_lowercase(), at)) {
            self.list.push(ReviewItem {
                rule,
                token: token.to_owned(),
                context: context.to_owned(),
                line: at.map(|at| at as u32 + 1),
            });
        }
    }
}

fn is_count(word: &str) -> bool {
    let contains = |w: &str| COUNTS.contains(&w);
    contains(word)
        || COUNT_PARTS.iter().any(|part| word.contains(part))
        || word
            .split_once("und")
            .is_some_and(|(_, tens)| contains(tens))
        || word
            .split_once("jähr")
            .or_else(|| word.split_once("jaehr"))
            .is_some_and(|(count, _)| contains(count))
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

/// How far ahead a time unit still makes a Roman numeral a count: `XII long years`.
const ROMAN_UNIT_WORDS: usize = 3;

fn check_number_words(line: &str, at: usize, items: &mut Items) {
    let originals: Vec<&str> = words(line).collect();
    for (i, original) in originals.iter().enumerate() {
        let lower = original.to_lowercase();
        let parts: Vec<&str> = lower.split('-').collect();
        let original_parts: Vec<&str> = original.split('-').collect();
        let is_unit = |word: &str| TIME_UNITS.contains(&word.to_lowercase().as_str());
        let hit = parts.iter().copied().any(is_count)
            || parts
                .windows(2)
                .zip(&original_parts)
                .any(|(pair, part)| is_ascii_roman(part) && TIME_UNITS.contains(&pair[1]))
            || is_ascii_roman(original)
                && originals[i + 1..]
                    .iter()
                    .take(ROMAN_UNIT_WORDS)
                    .any(|word| is_unit(word));
        if hit {
            items.add(Rule::NumberWords, original, line, Some(at));
        }
    }
}

fn is_claim(key: &str) -> bool {
    static CLAIM_SET: LazyLock<HashSet<&str>> = LazyLock::new(|| CLAIMS.iter().copied().collect());
    CLAIM_SET.contains(key)
        || CLAIM_PREFIXES.iter().any(|prefix| key.starts_with(prefix))
        || CLAIM_SUFFIXES.iter().any(|suffix| key.ends_with(suffix))
}

/// `line` is prepared for words. A claim word that is part of a fact standing whole on this line is
/// that fact, and says nothing new.
fn check_claims(line: &str, at: usize, known: &Known, items: &mut Items) {
    let folded = fold_case(line);
    let on_line: Vec<&String> = known
        .fact_phrases
        .iter()
        .filter(|phrase| contains_word(&folded, phrase))
        .collect();
    let is_own = |key: &str| {
        let key = fold_case(key);
        on_line.iter().any(|phrase| {
            phrase.contains(&key) || words(phrase).any(|word| fold_case(&word_key(word)) == key)
        })
    };
    for word in words(line) {
        let key = word_key(word);
        let dotted = word.contains('.') && DOTTED_CLAIMS.contains(&key.as_str());
        let hit = dotted
            || is_claim(&key)
            || key.split('-').count() > 1
                && key
                    .split('-')
                    .any(|part| is_claim(part) && !WHOLE_WORD_CLAIMS.contains(&part));
        if hit && !is_own(&key) {
            items.add(Rule::ClaimWords, word, line, Some(at));
        }
    }
    for phrase in CLAIM_PHRASES {
        if contains_word(&folded, phrase) && !is_own(phrase) {
            items.add(Rule::ClaimWords, phrase, line, Some(at));
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
                    .or_else(|| closed_up(text, &folded))
            })
        {
            findings.add(Rule::BannedPhrases, phrase, found);
        }
    }
}

/// Finds `phrase` where closing up spaced letters took its word gaps with them: `r o c k s t a r  n i n
/// j a` is one run of letters by then. It must start and end on word ends, so `go to` isn't in `Hugo
/// took`, and it says what it found.
fn closed_up(text: &str, phrase: &str) -> Option<String> {
    let phrase: String = phrase.split_whitespace().collect();
    let words: Vec<&str> = text.split_whitespace().collect();
    (0..words.len()).find_map(|first| {
        let mut joined = String::new();
        for (last, word) in words.iter().enumerate().skip(first) {
            joined.push_str(word.trim_matches(|c: char| !c.is_alphanumeric()));
            if joined == phrase {
                return Some(words[first..=last].join(" "));
            }
            if !phrase.starts_with(&joined) {
                break;
            }
        }
        None
    })
}

/// A company or institution, and what belongs with it.
struct Anchor {
    name: String,
    lower: String,
    /// The name without its legal form, and its first word: `Brightpath Logistics GmbH` is also
    /// `Brightpath Logistics`, and `Brightpath`. Dropping a legal form is how people write a company.
    short: Vec<String>,
    detail: String,
    date: String,
    location: String,
    date_tokens: HashSet<String>,
}

const LEGAL_FORMS: &[&str] = &[
    "gmbh", "mbh", "ag", "se", "kg", "ug", "ohg", "ev", "ltd", "llc", "inc", "plc", "corp", "bv",
    "co", "&",
];
/// So that a first word binds a name and not a sentence: `The` binds nothing.
const MIN_FIRST_WORD_CHARS: usize = 4;

fn short_names(name: &str) -> Vec<String> {
    let mut parts: Vec<&str> = name.split_whitespace().collect();
    while parts.len() > 1
        && parts.last().is_some_and(|last| {
            LEGAL_FORMS.contains(
                &last
                    .trim_matches(['.', ','])
                    .replace('.', "")
                    .to_lowercase()
                    .as_str(),
            )
        })
    {
        parts.pop();
    }
    let mut short = Vec::new();
    if parts.join(" ") != name {
        short.push(parts.join(" "));
    }
    if parts.len() > 1 && parts[0].chars().count() >= MIN_FIRST_WORD_CHARS {
        short.push(parts[0].trim_matches(',').to_owned());
    }
    short
}

fn anchors(facts: &Facts) -> Vec<Anchor> {
    let employment = facts
        .employment
        .iter()
        .map(|e| (&e.company, &e.title, &e.date, &e.location));
    let education = facts
        .education
        .iter()
        .map(|e| (&e.institution, &e.qualification, &e.date, &e.location));
    employment
        .chain(education)
        .filter(|(name, _, _, _)| !name.trim().is_empty())
        .map(|(name, detail, date, location)| {
            let name = normalize(name.trim());
            let date = normalize(date.trim());
            let mut date_tokens = HashSet::new();
            for token in tokens(&date) {
                date_tokens.extend(date_parts(&token.canon).into_iter().map(str::to_owned));
            }
            Anchor {
                lower: name.to_lowercase(),
                short: short_names(&name),
                name,
                detail: normalize(detail.trim()).to_lowercase(),
                date: date.to_lowercase(),
                location: normalize(location.trim()).to_lowercase(),
                date_tokens,
            }
        })
        .collect()
}

impl Anchor {
    /// How this one is named on `line`, if it is. Its first word only as written, or in capitals:
    /// a company called Data must not bind every "data".
    fn named_on(&self, line: &str, lower: &str) -> Option<String> {
        if contains_word(lower, &self.lower) {
            return Some(self.name.clone());
        }
        self.short.iter().enumerate().find_map(|(index, short)| {
            let is_first_word = index + 1 == self.short.len() && !short.contains(' ');
            let found = if is_first_word {
                contains_word(line, short) || contains_word(line, &short.to_uppercase())
            } else {
                contains_word(lower, &short.to_lowercase())
            };
            found.then(|| short.clone())
        })
    }
}

/// How many lines after a company or institution what belongs to it may stand, or be read as its.
const DATE_WINDOW_LINES: usize = 2;

/// Lists a fact's name wherever it stands outside its own entry, and whatever a reader would take
/// for its date, title or place that isn't. Only where a line of it changed.
fn check_binding(lines: &[String], changed: &[bool], anchors: &[Anchor], items: &mut Items) {
    let lower_lines: Vec<String> = lines.iter().map(|l| l.to_lowercase()).collect();
    let on_lines: Vec<Vec<(&Anchor, String)>> = lines
        .iter()
        .zip(&lower_lines)
        .map(|(line, lower)| {
            anchors
                .iter()
                .filter_map(|a| a.named_on(line, lower).map(|named| (a, named)))
                .collect()
        })
        .collect();
    for (i, on_line) in on_lines.iter().enumerate() {
        if on_line.is_empty() {
            continue;
        }
        // Everything up to the next company or institution reads as this one's.
        let end = (i + 1..on_lines.len())
            .find(|&next| !on_lines[next].is_empty())
            .unwrap_or(on_lines.len());
        if !changed[i..end].iter().any(|changed| *changed) {
            continue;
        }
        let window_end = end.min(i + DATE_WINDOW_LINES + 1);
        let window = lower_lines[i..window_end].join(" ");
        let region = lower_lines[i..end].join(" ");
        let is_on_line = |other: &Anchor| on_line.iter().any(|(a, _)| a.lower == other.lower);
        let line = &lines[i];
        for (anchor, named) in on_line {
            let is_entry = contains_word(&lower_lines[i], &anchor.lower)
                && anchors.iter().filter(|a| a.lower == anchor.lower).any(|a| {
                    (a.date.is_empty() || contains_word(&window, &a.date))
                        && (a.detail.is_empty() || contains_word(&window, &a.detail))
                });
            if !is_entry {
                items.add(Rule::EntityBinding, named, line, Some(i));
            }
        }
        for (at, near) in lines.iter().enumerate().take(window_end).skip(i) {
            for token in tokens(near) {
                let own = anchors
                    .iter()
                    .any(|a| is_on_line(a) && a.date_tokens.contains(&token.canon));
                if token_year(&token.canon).is_some() && !own {
                    items.add(
                        Rule::EntityBinding,
                        &near[token.start..token.end],
                        near,
                        Some(at),
                    );
                }
            }
        }
        for other in anchors.iter().filter(|other| !is_on_line(other)) {
            // A bare year stays out of this: the token above lists one near the name.
            let bare_year = other.date.chars().all(|c| c.is_ascii_digit());
            let own = |pick: fn(&Anchor) -> &str, value: &str| {
                anchors
                    .iter()
                    .any(|a| is_on_line(a) && pick(a).contains(value))
            };
            if !bare_year && !own(|a| &a.date, &other.date) && contains_word(&region, &other.date) {
                items.add(Rule::EntityBinding, &other.date, line, Some(i));
            }
            if !own(|a| &a.detail, &other.detail) && contains_word(&window, &other.detail) {
                items.add(Rule::EntityBinding, &other.detail, line, Some(i));
            }
            if !own(|a| &a.location, &other.location) && contains_word(&window, &other.location) {
                items.add(Rule::EntityBinding, &other.location, line, Some(i));
            }
        }
    }
}

/// A level after a fact's language name, up to the next such name, must be that language's own.
fn check_languages(lines: &[String], changed: &[bool], facts: &Facts, items: &mut Items) {
    static LEVEL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"\b(?:[abc][12]|native|fluent|bilingual|proficient|business|basic|muttersprach\w*|fließend\w*|fliessend\w*|verhandlungssicher\w*|konversationssicher\w*|grundkenntnisse)\b",
        )
        .expect("the language level pattern is valid")
    });
    let names: Vec<(String, String)> = facts
        .languages
        .iter()
        .map(|l| {
            (
                normalize(&l.name).to_lowercase(),
                normalize(&l.level).to_lowercase(),
            )
        })
        .filter(|(name, _)| !name.is_empty())
        .collect();
    for (at, line) in lines.iter().enumerate().filter(|(at, _)| changed[*at]) {
        let lower = line.to_lowercase();
        let mut found: Vec<(usize, usize, &str)> = names
            .iter()
            .flat_map(|(name, level)| {
                lower.match_indices(name.as_str()).filter_map(|(start, _)| {
                    let end = start + name.len();
                    contains_word(&lower[start..], name).then_some((start, end, level.as_str()))
                })
            })
            .collect();
        found.sort_unstable();
        for (index, (_, end, level)) in found.iter().enumerate() {
            let until = found.get(index + 1).map_or(lower.len(), |next| next.0);
            if let Some(said) = LEVEL.find(&lower[*end..until])
                && !level.contains(said.as_str())
            {
                let token = lower[found[index].0..*end + said.end()].to_owned();
                items.add(Rule::LanguageLevel, &token, line, Some(at));
            }
        }
    }
}

/// Facts the last approved text showed and this one doesn't. A tailored CV may leave out a job, and
/// the owner should still hear of it.
fn check_missing(previous: &str, current: &str, facts: &Facts, items: &mut Items) {
    let (previous, current) = (
        normalize(previous).to_lowercase(),
        normalize(current).to_lowercase(),
    );
    let contact = &facts.contact;
    let names = facts
        .employment
        .iter()
        .map(|e| &e.company)
        .chain(facts.education.iter().map(|e| &e.institution))
        .chain(facts.languages.iter().map(|l| &l.name))
        .chain(facts.metrics.iter().map(|m| &m.text))
        .chain([
            &facts.name,
            &contact.email,
            &contact.phone,
            &contact.linkedin,
            &contact.github,
        ]);
    for name in names {
        let needle = normalize(name.trim()).to_lowercase();
        if contains_word(&previous, &needle) && !contains_word(&current, &needle) {
            let context = format!(
                "On the last approved CV, and not on this one: {}",
                name.trim()
            );
            items.add(Rule::MissingFact, name.trim(), &context, None);
        }
    }
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
                Rule::DigitLookalike,
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
/// tech: `Node.js`, `socket.io`. Only all upper-case `.NET` is the platform.
static URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:[a-z][a-z0-9+-]*://|www\.)[^\s]+|\b[\p{L}\p{N}-]+(?:\.[\p{L}\p{N}-]+)*\.(?:com|org|(?-i:net|Net|nEt|neT|NEt|NeT|nET)|info|biz|eu|de|at|ch|uk|dev|app|xyz|example|test)\b(?:[/?#][^\s]*)?",
    )
    .expect("the URL pattern is valid")
});

/// Whether `key` stands whole in `phrases`: `docs.example.org/guide` holds neither `e.org` nor the
/// parent host `example.org`, and `mara.long@host` doesn't hold `long@host`. It may stop short of a
/// path, since that is the same host.
fn stands_whole(phrases: &str, key: &str) -> bool {
    let inside = |c: char| c.is_alphanumeric() || "._%+'-@".contains(c);
    !key.is_empty()
        && phrases.match_indices(key).any(|(start, _)| {
            let before = &phrases[..start];
            let rest = &phrases[start + key.len()..];
            let starts = before.ends_with("//")
                || !before
                    .chars()
                    .next_back()
                    .is_some_and(|c| inside(c) || c == '/');
            let rest = rest.strip_prefix('.').unwrap_or(rest);
            starts && !rest.chars().next().is_some_and(inside)
        })
}

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

/// A phone number with its country code: `+49 170 5550137`, `0049 170 5550137`. A local form can't be
/// told from a date range, so it is only listed for review, as every number is.
static PHONE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:\+|\b00)\s*[0-9][0-9 ()/.-]{6,}[0-9]").expect("the phone pattern is valid")
});

fn check_phones(said: &str, facts: &Facts, findings: &mut Findings) {
    let digits = |text: &str| digits_of(text).trim_start_matches('0').to_owned();
    let own = digits(&facts.contact.phone);
    for found in PHONE.find_iter(said) {
        if own.is_empty() || digits(found.as_str()) != own {
            findings.add(
                Rule::ContactDetails,
                found.as_str(),
                found.as_str().to_owned(),
            );
        }
    }
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
        || stands_whole(&known.phrases, &key)
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
        // What a reader takes it to say: two links can split a value, a space between its pieces or not.
        let shown: String = normalize(&link.together)
            .to_lowercase()
            .split_whitespace()
            .collect();
        let shows = |value: &str| !value.is_empty() && shown.contains(value);
        let key = url_key(&target);
        let shows_phone = !phone.is_empty() && digits_of(&shown).contains(&phone);
        let shown_ok = !(shows(&email) && mail.is_none())
            && !(shows_phone && tel.is_none())
            && profiles
                .iter()
                .all(|profile| !shows(profile) || (web && is_under(&key, profile)));
        if !target_ok || !shown_ok {
            findings.add(
                Rule::ContactDetails,
                &link.target,
                format!("a link showing \"{}\"", link.together),
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
    let metadata: Vec<String> = text.metadata.iter().map(|item| normalize(item)).collect();
    // Line by line: a number starting the next line is no part of a phone number ending this one.
    for line in text
        .lines
        .iter()
        .map(|line| normalize(line))
        .chain(metadata.iter().cloned())
    {
        check_phones(&line, facts, findings);
    }
    for said in std::iter::once(document.body.clone()).chain(metadata) {
        let (emails, urls) = addresses(&said);
        for found in emails {
            let key: String = found.split_whitespace().collect::<String>().to_lowercase();
            if key != email && !stands_whole(&known.phrases, &key) {
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

const LINK_LABEL: &str = "Link: ";
const PDF_INFO_LABEL: &str = "PDF info: ";
/// Words that can't state a fact by themselves, in the CV's two languages. Listing them would bury the
/// ones that can.
const FUNCTION_WORDS: &str = "a an and at as by for from in into of on or the to with und oder bei bis der die das den dem des ein eine einen einem einer für im in mit nach von vor zu zum zur";
/// What the template and these lines write themselves, which is no one's new word.
const TEMPLATE_WORDS: &str = "Summary Skills Experience Projects Education Languages Profil Kenntnisse Berufserfahrung Projekte Ausbildung Sprachen https http mailto tel www";

/// What the owner reviews and approves, one line each: the pages as a reader sees them, then what the
/// PDF says beyond them. Control characters draw nothing, and could rewrite a terminal.
pub fn review_lines(text: &RenderedText) -> Vec<String> {
    let one_line = |item: String| -> String {
        item.chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect()
    };
    let links = text
        .links
        .iter()
        .map(|link| format!("{LINK_LABEL}{} ({})", link.target, link.text));
    let metadata = text.metadata.iter().map(|m| format!("{PDF_INFO_LABEL}{m}"));
    text.lines
        .iter()
        .cloned()
        .chain(links)
        .chain(metadata)
        .map(one_line)
        .collect()
}

/// [`review_lines`] as one text, as an approval stores it.
pub fn review_text(text: &RenderedText) -> String {
    review_lines(text).join("\n")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineChange {
    Same,
    Added,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub change: LineChange,
    pub text: String,
    /// Its line in the new text, from 1, as [`ReviewItem::line`] counts them.
    pub line: Option<u32>,
}

/// Past this a diff gets coarser, which shows more as changed, never less.
const DIFF_TIMEOUT: Duration = Duration::from_millis(500);

/// `current` against what was last approved, or against nothing, so that a first approval shows all
/// of it. A line moved elsewhere counts as changed: a bullet moved to another job says something new.
pub fn review_diff(previous: Option<&str>, current: &str) -> Vec<DiffLine> {
    let old: Vec<&str> = previous.map_or_else(Vec::new, |text| text.split('\n').collect());
    let new: Vec<&str> = current.split('\n').collect();
    TextDiff::configure()
        .timeout(DIFF_TIMEOUT)
        .diff_slices(&old, &new)
        .iter_all_changes()
        .map(|change| DiffLine {
            change: match change.tag() {
                ChangeTag::Equal => LineChange::Same,
                ChangeTag::Insert => LineChange::Added,
                ChangeTag::Delete => LineChange::Removed,
            },
            text: change.value().to_owned(),
            line: change.new_index().map(|index| index as u32 + 1),
        })
        .collect()
}

fn strip_label(line: &str) -> &str {
    line.strip_prefix(LINK_LABEL)
        .or_else(|| line.strip_prefix(PDF_INFO_LABEL))
        .unwrap_or(line)
}

fn review(
    text: &RenderedText,
    facts: &Facts,
    known: &Known,
    previous: Option<&str>,
    owners_wording: &str,
) -> Vec<ReviewItem> {
    let raw = review_lines(text);
    let current = raw.join("\n");
    let mut changed = vec![false; raw.len()];
    for line in review_diff(previous, &current) {
        if let (LineChange::Added, Some(at)) = (line.change, line.line) {
            changed[at as usize - 1] = true;
        }
    }
    let lines: Vec<String> = raw
        .iter()
        .map(|line| normalize(strip_label(line)))
        .collect();
    let mut items = Items::default();
    let masked: Vec<String> = lines.iter().map(|line| known.mask_contact(line)).collect();
    check_binding(&masked, &changed, &anchors(facts), &mut items);
    check_languages(&masked, &changed, facts, &mut items);
    let mut familiar = known.words.clone();
    add_words(&mut familiar, &known.whole.join(" "));
    add_words(&mut familiar, TEMPLATE_WORDS);
    add_words(&mut familiar, FUNCTION_WORDS);
    add_words(&mut familiar, &normalize(owners_wording));
    if let Some(previous) = previous {
        add_words(&mut familiar, &normalize(&previous.replace('\n', " ")));
    }
    for (at, line) in masked.iter().enumerate().filter(|(at, _)| changed[*at]) {
        let prepared = prepare_for_words(line);
        check_claims(&prepared, at, known, &mut items);
        for token in tokens(line) {
            items.add(Rule::Numbers, &line[token.start..token.end], line, Some(at));
        }
        check_number_words(&prepared, at, &mut items);
        for word in words(line) {
            let key = word_key(word);
            let new = !word.chars().any(|c| c.is_ascii_digit())
                && !familiar.contains(&key)
                && !key.split('-').all(|part| familiar.contains(part));
            if new && familiar.insert(key) {
                items.add(Rule::NewWords, word, line, Some(at));
            }
        }
    }
    if let Some(previous) = previous {
        check_missing(previous, &current, facts, &mut items);
    }
    items.list.sort_by_key(|item| item.rule);
    items.list
}

/// Judges a rendered document. Hard refusals look at all of it. Review items look at the lines that
/// differ from `previous`, the text last approved, because a line the owner has approved needs no
/// second look. A word is new if neither that text, the facts, nor `owners_wording`, what the owner
/// last locked, holds it. Linear in the text, for facts of a fixed size.
pub fn check(
    text: &RenderedText,
    facts: &Facts,
    previous: Option<&str>,
    owners_wording: &str,
) -> Report {
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
        check_digit_lookalikes(masked, &known, &mut findings);
        check_mixed_script(masked, &known, &mut findings);
    }
    check_banned(&document, facts, &mut findings);
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
            "tiny, clipped, off-page, covered or too pale text: a CV shows everything it says"
                .to_owned(),
        );
    }
    for letter in &text.missing {
        findings.add(
            Rule::MissingGlyph,
            letter,
            "the CV's font has no such letter or symbol, so it draws as a crossed box. It holds Latin, \
             Greek, Cyrillic and Hebrew letters, and digits but no letters in math. Use a word or an \
             embedded symbol instead, or tell the owner the font lacks this"
                .to_owned(),
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
    Report {
        violations: findings.violations,
        review_items: review(text, facts, &known, previous, owners_wording),
    }
}
