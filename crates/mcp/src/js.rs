//! JavaScript semantics that old Nexus's behaviour depends on: UTF-16 string lengths, `new Date`,
//! `new URL`, and its request hash serialization.

use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, Utc};
use nexus_core::Timestamp;
use nexus_core::blob::sha256_hex;
#[cfg(test)]
use serde_json::json;
use serde_json::{Number, Value};
use url::Url;

pub fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

/// `value.slice(0, max)`. A surrogate pair cut in half loses its orphan, which old Nexus could not
/// have stored either.
pub fn slice16(value: &str, max: usize) -> String {
    let mut used = 0;
    let mut out = String::new();
    for ch in value.chars() {
        used += ch.len_utf16();
        if used > max {
            break;
        }
        out.push(ch);
    }
    out
}

/// `String.prototype.trim`, whose whitespace set differs from Rust's by U+FEFF and U+0085.
pub fn trim(value: &str) -> &str {
    value.trim_matches(|c: char| c == '\u{FEFF}' || (c != '\u{85}' && c.is_whitespace()))
}

/// What Prisma's `contains: term, mode: "insensitive"` does in Postgres: `ILIKE '%term%'` with the
/// term unescaped, so `%` and `_` stay wildcards and a backslash escapes.
pub fn ilike_contains(text: &str, term: &str) -> bool {
    enum Token {
        Any,
        One,
        Char(char),
    }
    let mut tokens = vec![Token::Any];
    let pattern = format!("{term}%").to_lowercase();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        tokens.push(match c {
            '%' => Token::Any,
            '_' => Token::One,
            '\\' => Token::Char(chars.next().unwrap_or('\\')),
            c => Token::Char(c),
        });
    }
    let text: Vec<char> = text.to_lowercase().chars().collect();
    let (mut t, mut p) = (0, 0);
    let mut backtrack = None;
    while t < text.len() {
        match tokens.get(p) {
            Some(Token::Any) => {
                backtrack = Some((p + 1, t));
                p += 1;
            }
            Some(Token::One) => {
                t += 1;
                p += 1;
            }
            Some(Token::Char(c)) if *c == text[t] => {
                t += 1;
                p += 1;
            }
            _ => match backtrack {
                Some((after_any, start)) => {
                    p = after_any;
                    t = start + 1;
                    backtrack = Some((after_any, start + 1));
                }
                None => return false,
            },
        }
    }
    tokens[p..].iter().all(|token| matches!(token, Token::Any))
}

/// JSON numbers as JavaScript sees them: `5.0` is the integer `5`.
pub fn normalize_numbers(value: &mut Value) {
    match value {
        Value::Number(n) => {
            if let Some(f) = n.as_f64()
                && n.is_f64()
                && f.fract() == 0.0
                && f.abs() < 9_007_199_254_740_992.0
            {
                *n = Number::from(f as i64);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(normalize_numbers),
        Value::Object(map) => map.values_mut().for_each(normalize_numbers),
        _ => {}
    }
}

/// Port of `nid`: the adapter's numeric ID parser.
pub fn nid(id: &str) -> Result<i64, String> {
    let bytes = id.as_bytes();
    let invalid = || format!("Invalid numeric id: {id}");
    if bytes.is_empty() || bytes[0] == b'0' || !bytes.iter().all(u8::is_ascii_digit) {
        return Err(invalid());
    }
    match id.parse::<i64>() {
        Ok(parsed) if parsed <= 2_147_483_647 => Ok(parsed),
        _ => Err(invalid()),
    }
}

fn digits(value: &str, count: usize) -> Option<i64> {
    (value.len() == count && value.bytes().all(|b| b.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
}

const LEGACY_DATES: [&str; 12] = [
    "%Y/%m/%d",
    "%Y-%m-%d",
    "%m/%d/%Y",
    "%m-%d-%Y",
    "%b %d, %Y",
    "%B %d, %Y",
    "%b %d %Y",
    "%B %d %Y",
    "%d %b %Y",
    "%d %B %Y",
    "%a, %d %b %Y",
    "%a %b %d %Y",
];
const LEGACY_TIMES: [&str; 4] = ["%H:%M:%S%.f", "%H:%M", "%I:%M %p", "%I:%M:%S %p"];

/// The forms of V8's legacy parser an agent might plausibly send, plus its reading of a bare number
/// and of a date without a year, which is 2001. A value it would accept in some other shape is
/// rejected here instead.
fn parse_legacy(value: &str) -> Option<DateTime<Utc>> {
    if let Ok(number) = value.parse::<u32>() {
        let (year, month) = match number {
            0 => (2000, 1),
            1..=12 => (2001, number),
            32..=49 => (2000 + number as i32, 1),
            50..=99 => (1900 + number as i32, 1),
            100..=9999 => (number as i32, 1),
            _ => return None,
        };
        return Some(
            NaiveDate::from_ymd_opt(year, month, 1)?
                .and_hms_opt(0, 0, 0)?
                .and_utc(),
        );
    }
    if let Ok(parsed) = DateTime::parse_from_rfc2822(value) {
        return Some(parsed.with_timezone(&Utc));
    }
    let upper = value.to_ascii_uppercase();
    let local = upper
        .strip_suffix(" UTC")
        .or_else(|| upper.strip_suffix(" GMT"))
        .unwrap_or(&upper);
    for date in LEGACY_DATES {
        if let Ok(parsed) = NaiveDate::parse_from_str(local, date) {
            return Some(parsed.and_hms_opt(0, 0, 0)?.and_utc());
        }
        for time in LEGACY_TIMES {
            if let Ok(parsed) = NaiveDateTime::parse_from_str(local, &format!("{date} {time}")) {
                return Some(parsed.and_utc());
            }
        }
    }
    let with_year = format!("{local} 2001");
    [
        "%b %d %Y", "%B %d %Y", "%d %b %Y", "%d %B %Y", "%m/%d %Y", "%m-%d %Y",
    ]
    .iter()
    .find_map(|format| NaiveDate::parse_from_str(&with_year, format).ok())
    .and_then(|date| date.and_hms_opt(0, 0, 0))
    .map(|parsed| parsed.and_utc())
}

/// `new Date(value)`. A value without a zone is local time in JavaScript, and old Nexus's containers
/// ran in UTC.
pub fn parse_date(value: &str) -> Option<Timestamp> {
    let value = value.trim();
    let parsed = parse_iso(&value.to_ascii_uppercase()).or_else(|| parse_legacy(value))?;
    let stamp = Timestamp::from_datetime(parsed);
    (stamp.as_str().len() == 24).then_some(stamp)
}

fn parse_iso(value: &str) -> Option<DateTime<Utc>> {
    let (date, time) = match value.split_once('T') {
        Some((date, time)) => (date, Some(time)),
        None => (value, None),
    };
    let mut parts = date.split('-');
    let year = digits(parts.next()?, 4)?;
    let month = parts.next().map_or(Some(1), |m| digits(m, 2))?;
    let day = parts.next().map_or(Some(1), |d| digits(d, 2))?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // V8 accepts a day past the end of a shorter month and rolls it over.
    let base = NaiveDate::from_ymd_opt(year as i32, month as u32, 1)? + Duration::days(day - 1);
    let Some(time) = time else {
        return Some(base.and_hms_opt(0, 0, 0)?.and_utc());
    };
    let (clock, offset_seconds) = if let Some(clock) = time.strip_suffix('Z') {
        (clock, 0)
    } else if let Some(at) = time.rfind(['+', '-']) {
        let (clock, zone) = time.split_at(at);
        let sign = if zone.starts_with('+') { 1 } else { -1 };
        let zone = zone[1..].replace(':', "");
        let hours = digits(zone.get(..2)?, 2)?;
        let minutes = digits(zone.get(2..)?, 2)?;
        (clock, sign * (hours * 3600 + minutes * 60))
    } else {
        (time, 0)
    };
    let mut fields = clock.split(':');
    let hours = digits(fields.next()?, 2)?;
    let minutes = digits(fields.next()?, 2)?;
    let (seconds, millis) = match fields.next() {
        None => (0, 0),
        Some(field) => {
            let (seconds, fraction) = field.split_once('.').unwrap_or((field, ""));
            if field.contains('.')
                && (fraction.is_empty() || !fraction.bytes().all(|b| b.is_ascii_digit()))
            {
                return None;
            }
            let padded = format!("{fraction:0<3}");
            (digits(seconds, 2)?, digits(&padded[..3], 3)?)
        }
    };
    let end_of_day = hours == 24 && minutes == 0 && seconds == 0 && millis == 0;
    if fields.next().is_some() || (hours > 23 && !end_of_day) || minutes > 59 || seconds > 59 {
        return None;
    }
    let midnight = base.and_hms_opt(0, 0, 0)?.and_utc();
    Some(
        midnight
            + Duration::seconds(hours * 3600 + minutes * 60 + seconds - offset_seconds)
            + Duration::milliseconds(millis),
    )
}

fn is_leap(year: u32) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

fn two(value: &str) -> Option<u32> {
    digits(value, 2).map(|n| n as u32)
}

/// zod v3's `z.string().datetime()`, with seconds optional and any fractional precision. `offset`
/// is `datetime({ offset: true })`, which also allows `+hh:mm` and `+hhmm`.
pub fn is_zod_datetime(value: &str, offset: bool) -> bool {
    let check = || -> Option<bool> {
        let (date, time) = value.split_once('T')?;
        let year = digits(date.get(..4)?, 4)? as u32;
        if date.as_bytes().get(4) != Some(&b'-') || date.as_bytes().get(7) != Some(&b'-') {
            return Some(false);
        }
        let month = two(date.get(5..7)?)?;
        let day = two(date.get(8..)?)?;
        let last = match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if is_leap(year) => 29,
            2 => 28,
            _ => return Some(false),
        };
        if day == 0 || day > last {
            return Some(false);
        }
        let clock = if let Some(clock) = time.strip_suffix('Z') {
            clock
        } else if offset {
            let at = time.rfind(['+', '-'])?;
            let (clock, zone) = time.split_at(at);
            let zone = &zone[1..];
            let valid = match zone.len() {
                4 => digits(zone, 4).is_some(),
                5 => {
                    zone.as_bytes()[2] == b':'
                        && two(&zone[..2]).is_some()
                        && two(&zone[3..]).is_some()
                }
                _ => false,
            };
            if !valid {
                return Some(false);
            }
            clock
        } else {
            return Some(false);
        };
        let hours = two(clock.get(..2)?)?;
        let minutes = two(clock.get(3..5)?)?;
        if clock.as_bytes().get(2) != Some(&b':') || hours > 23 || minutes > 59 {
            return Some(false);
        }
        let rest = clock.get(5..)?;
        if rest.is_empty() {
            return Some(true);
        }
        let seconds = rest.strip_prefix(':')?;
        let (whole, fraction) = seconds.split_once('.').unwrap_or((seconds, "0"));
        Some(
            two(whole)? <= 59
                && !fraction.is_empty()
                && fraction.bytes().all(|b| b.is_ascii_digit()),
        )
    };
    check().unwrap_or(false)
}

/// Port of `stableSerialize`: keys sorted, `undefined` already absent from `Value`s.
fn stable_serialize(value: &Value) -> String {
    match value {
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(stable_serialize).collect();
            format!("[{}]", items.join(","))
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            // JavaScript's default sort compares UTF-16 code units.
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            let fields: Vec<String> = keys
                .into_iter()
                .map(|key| {
                    format!(
                        "{}:{}",
                        Value::from(key.as_str()),
                        stable_serialize(&map[key])
                    )
                })
                .collect();
            format!("{{{}}}", fields.join(","))
        }
        other => other.to_string(),
    }
}

/// Port of `submissionRequestHash`.
pub fn request_hash(value: &Value) -> String {
    sha256_hex(stable_serialize(value).as_bytes())
}

const TRACKING_PARAMS: [&str; 7] = [
    "ref", "source", "campaign", "mc_cid", "mc_eid", "gclid", "fbclid",
];

/// Port of `canonicalizeJobUrl`. The errors are the messages old Nexus throws.
pub fn canonicalize_job_url(raw: &str) -> Result<Option<String>, String> {
    let raw = trim(raw);
    if raw.is_empty() {
        return Ok(None);
    }
    let mut url = Url::parse(raw).map_err(|_| "Invalid URL".to_owned())?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err("Job URL must use http or https".to_owned());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("Job URL must not contain credentials".to_owned());
    }
    url.set_fragment(None);
    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(key, _)| {
            let key = key.to_lowercase();
            !key.starts_with("utm_") && !TRACKING_PARAMS.contains(&key.as_str())
        })
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    pairs.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
    if pairs.is_empty() {
        url.set_query(None);
    } else {
        url.query_pairs_mut().clear().extend_pairs(pairs);
    }
    if url.path().len() > 1 {
        let stripped = url.path().trim_end_matches('/').to_owned();
        url.set_path(&stripped);
    }
    let is_root = url.path() == "/";
    let text = url.to_string();
    let text = text.strip_suffix('?').unwrap_or(&text);
    Ok(Some(if is_root {
        text.to_owned()
    } else {
        text.strip_suffix('/').unwrap_or(text).to_owned()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Expected values generated with Node, zod v3 and old Nexus's own functions. Reports every
    /// mismatch at once.
    #[test]
    fn ports_match_real_javascript() {
        let vectors: Value =
            serde_json::from_str(include_str!("../tests/fixtures/js_vectors.json")).unwrap();
        let list = |key: &str| vectors[key].as_array().unwrap();
        let mut failures = Vec::new();
        for case in list("urls") {
            let input = case["input"].as_str().unwrap();
            let got = match canonicalize_job_url(input) {
                Ok(canonical) => json!({"ok": canonical}),
                Err(message) => json!({"err": message}),
            };
            let expected = match case.get("ok") {
                Some(ok) => json!({"ok": ok}),
                None => json!({"err": case["err"]}),
            };
            if got != expected {
                failures.push(format!("canonicalize {input:?}: {got} vs {expected}"));
            }
            let parses = Url::parse(input).is_ok();
            if parses != case["zodUrl"] {
                failures.push(format!("z.url {input:?}: {parses}"));
            }
        }
        for case in list("dates") {
            let input = case["input"].as_str().unwrap();
            let got = parse_date(input);
            let got = got.as_ref().map(Timestamp::as_str);
            if got != case["iso"].as_str() {
                failures.push(format!("Date {input:?}: {got:?} vs {}", case["iso"]));
            }
            if is_zod_datetime(input, false) != case["zod"] {
                failures.push(format!("datetime {input:?}"));
            }
            if is_zod_datetime(input, true) != case["zodOffset"] {
                failures.push(format!("datetime offset {input:?}"));
            }
        }
        for case in list("trims") {
            let input = case["input"].as_str().unwrap();
            if trim(input) != case["trimmed"] {
                failures.push(format!("trim {input:?}"));
            }
        }
        for case in list("hashes") {
            let mut input = case["input"].clone();
            normalize_numbers(&mut input);
            if request_hash(&input) != case["hash"] {
                failures.push(format!("hash {input}"));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn dates_follow_new_date() {
        for (input, expected) in [
            ("2026-01-02", Some("2026-01-02T00:00:00.000Z")),
            ("2026-01", Some("2026-01-01T00:00:00.000Z")),
            ("2026-01-02T03:04Z", Some("2026-01-02T03:04:00.000Z")),
            (
                "2026-01-02T03:04:05.123456Z",
                Some("2026-01-02T03:04:05.123Z"),
            ),
            (
                "2026-01-02T03:04:05.5+02:00",
                Some("2026-01-02T01:04:05.500Z"),
            ),
            ("2026-01-02T03:04:05+0200", Some("2026-01-02T01:04:05.000Z")),
            ("2026-02-30", Some("2026-03-02T00:00:00.000Z")),
            ("Jan 5, 2026", Some("2026-01-05T00:00:00.000Z")),
            ("2026-13-01", None),
            ("soon", None),
            ("", None),
        ] {
            assert_eq!(
                parse_date(input).as_ref().map(Timestamp::as_str),
                expected,
                "{input}"
            );
        }
    }

    #[test]
    fn zod_datetime_matches_the_v3_regex() {
        for value in [
            "2026-01-02T03:04:05Z",
            "2026-01-02T03:04Z",
            "2024-02-29T00:00:00.123456Z",
        ] {
            assert!(is_zod_datetime(value, false), "{value}");
        }
        for value in [
            "2026-01-02T03:04:05+02:00",
            "2026-01-02",
            "2026-02-29T00:00:00Z",
            "2026-01-02T24:00:00Z",
            "2026-01-02T03:04:5Z",
            "2026-01-02T03:04:05.Z",
            "",
        ] {
            assert!(!is_zod_datetime(value, false), "{value}");
        }
        assert!(is_zod_datetime("2026-01-02T03:04:05+02:00", true));
        assert!(is_zod_datetime("2026-01-02T03:04:05-0200", true));
        assert!(!is_zod_datetime("2026-01-02T03:04:05+2", true));
    }

    #[test]
    fn canonical_urls_match_old_nexus() {
        for (input, expected) in [
            (
                "https://Jobs.Example.com/a/b/?utm_source=x&b=2&a=1#frag",
                "https://jobs.example.com/a/b?a=1&b=2",
            ),
            ("https://example.com/", "https://example.com/"),
            ("https://example.com/?", "https://example.com/"),
            ("https://example.com/x?ref=1", "https://example.com/x"),
            ("http://example.com/x?q=a b", "http://example.com/x?q=a+b"),
        ] {
            assert_eq!(
                canonicalize_job_url(input).unwrap().as_deref(),
                Some(expected),
                "{input}"
            );
        }
        assert_eq!(canonicalize_job_url("  ").unwrap(), None);
        assert!(canonicalize_job_url("ftp://example.com").is_err());
        assert!(canonicalize_job_url("https://user:pw@example.com").is_err());
    }

    #[test]
    fn hashing_sorts_keys_and_keeps_nulls() {
        let serialized =
            stable_serialize(&serde_json::json!({"b": [1, {"d": null, "c": "é"}], "a": true}));
        assert_eq!(serialized, r#"{"a":true,"b":[1,{"c":"é","d":null}]}"#);
    }

    #[test]
    fn ilike_is_unicode_insensitive_with_wildcards() {
        assert!(ilike_contains("München GmbH", "MÜNCHEN"));
        assert!(ilike_contains("abc", ""));
        assert!(ilike_contains("Acme 50% off", "50%"));
        assert!(ilike_contains("Acme 50 off", "50%off"));
        assert!(ilike_contains("a1c", "a_c"));
        assert!(!ilike_contains("ac", "a_c"));
        assert!(ilike_contains("100%", "0\\%"));
        assert!(!ilike_contains("100x", "0\\%"));
        assert!(!ilike_contains("Acme", "Beta"));
        assert!(ilike_contains("aab", "ab"));
    }

    #[test]
    fn strings_count_utf16_units() {
        assert_eq!(utf16_len("\u{1F600}"), 2);
        assert_eq!(slice16("ab\u{1F600}c", 3), "ab");
        assert_eq!(slice16("abc", 2), "ab");
        assert_eq!(trim("\u{FEFF} x\n"), "x");
    }
}
