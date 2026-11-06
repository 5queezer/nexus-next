use std::fmt;

use chrono::{DateTime, Duration, NaiveDateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

/// A UTC instant as canonical text, `YYYY-MM-DDTHH:MM:SS.sssZ`: the same shape as JavaScript's
/// `toISOString`. Text order is time order, so columns can be compared directly in SQL.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, sqlx::Type,
)]
#[serde(try_from = "String", into = "String")]
#[sqlx(transparent)]
pub struct Timestamp(String);

#[derive(Debug, thiserror::Error)]
#[error("invalid timestamp")]
pub struct InvalidTimestamp;

impl Timestamp {
    pub fn now() -> Self {
        Self::from_datetime(Utc::now())
    }

    /// Later fractional digits are truncated, as `new Date(...)` does.
    pub fn from_datetime(value: DateTime<Utc>) -> Self {
        Self(value.to_rfc3339_opts(SecondsFormat::Millis, true))
    }

    /// Parses an RFC 3339 instant with a `Z` or numeric offset.
    pub fn parse(value: &str) -> Result<Self, InvalidTimestamp> {
        let parsed = DateTime::parse_from_rfc3339(value).map_err(|_| InvalidTimestamp)?;
        Self::in_range(parsed.with_timezone(&Utc))
    }

    /// Parses a zoneless Postgres `timestamp` value as UTC. Prisma stored every `DateTime` in UTC,
    /// and `row_to_json` prints it without a zone and with 0 to 3 fractional digits.
    pub fn parse_naive_utc(value: &str) -> Result<Self, InvalidTimestamp> {
        let parsed = NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f")
            .map_err(|_| InvalidTimestamp)?;
        Self::in_range(parsed.and_utc())
    }

    fn in_range(value: DateTime<Utc>) -> Result<Self, InvalidTimestamp> {
        let canonical = Self::from_datetime(value);
        if canonical.0.len() == 24 {
            Ok(canonical)
        } else {
            Err(InvalidTimestamp)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Columns only check a timestamp's shape, so a stored value can be `2026-99-99T...`. That reads
    /// as the epoch: an odd date on one row beats a panic that takes the whole page with it.
    pub fn to_datetime(&self) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(&self.0)
            .map_or(DateTime::UNIX_EPOCH, |value| value.with_timezone(&Utc))
    }

    pub fn plus(&self, duration: Duration) -> Self {
        Self::from_datetime(self.to_datetime() + duration)
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for Timestamp {
    type Error = InvalidTimestamp;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<Timestamp> for String {
    fn from(value: Timestamp) -> Self {
        value.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn naive_export_values_normalize_to_three_digits_and_z() {
        for (input, expected) in [
            ("2026-01-02T03:04:05", "2026-01-02T03:04:05.000Z"),
            ("2026-01-02T03:04:05.8", "2026-01-02T03:04:05.800Z"),
            ("2026-01-02T03:04:05.82", "2026-01-02T03:04:05.820Z"),
            ("2026-01-02T03:04:05.826", "2026-01-02T03:04:05.826Z"),
            ("2026-01-02T03:04:05.8269", "2026-01-02T03:04:05.826Z"),
        ] {
            assert_eq!(
                Timestamp::parse_naive_utc(input).unwrap().as_str(),
                expected
            );
        }
    }

    #[test]
    fn naive_parser_rejects_zones_dates_and_garbage() {
        for input in [
            "2026-01-02",
            "2026-01-02T03:04:05Z",
            "2026-13-02T03:04:05",
            "",
            "soon",
        ] {
            assert!(Timestamp::parse_naive_utc(input).is_err(), "{input}");
        }
    }

    #[test]
    fn offsets_convert_to_utc() {
        assert_eq!(
            Timestamp::parse("2026-01-02T03:04:05.5+02:00")
                .unwrap()
                .as_str(),
            "2026-01-02T01:04:05.500Z"
        );
        assert_eq!(
            Timestamp::parse("2026-01-02T03:04:05Z").unwrap().as_str(),
            "2026-01-02T03:04:05.000Z"
        );
        assert!(Timestamp::parse("2026-01-02T03:04:05").is_err());
    }

    #[test]
    fn text_order_is_time_order_and_now_is_canonical() {
        let earlier = Timestamp::parse("2026-01-02T03:04:05Z").unwrap();
        let later = earlier.plus(Duration::milliseconds(1));
        assert!(earlier < later);
        assert_eq!(later.as_str(), "2026-01-02T03:04:05.001Z");
        assert_eq!(
            Timestamp::parse(Timestamp::now().as_str())
                .unwrap()
                .as_str()
                .len(),
            24
        );
    }

    #[test]
    fn serde_round_trips_and_rejects_non_canonical_input_shapes() {
        let value: Timestamp = serde_json::from_str("\"2026-01-02T03:04:05+01:00\"").unwrap();
        assert_eq!(
            serde_json::to_string(&value).unwrap(),
            "\"2026-01-02T02:04:05.000Z\""
        );
        assert!(serde_json::from_str::<Timestamp>("\"yesterday\"").is_err());
    }

    #[test]
    fn a_stored_value_that_is_no_real_instant_reads_as_the_epoch() {
        let stored: Timestamp = Timestamp("2026-99-99T99:99:99.999Z".to_owned());
        assert_eq!(stored.to_datetime(), DateTime::UNIX_EPOCH);
    }
}
