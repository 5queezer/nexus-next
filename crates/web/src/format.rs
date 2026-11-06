//! Display helpers shared by the dashboard views. Labels come from old Nexus's `messages/en.json`.

use chrono::{NaiveDate, Timelike};
use chrono_tz::Tz;
use nexus_core::Timestamp;

pub fn status_label(status: &str) -> &'static str {
    match status {
        "inbound" => "Inbound",
        "applied" => "Applied",
        "interview" => "Interview",
        "offer" => "Offer",
        "rejected" => "Rejected",
        _ => "Applied",
    }
}

/// The label for an event type, as `application-timeline.tsx` resolves it.
pub fn event_label(event_type: &str) -> String {
    match event_type {
        "application_archived" => "Opportunity archived",
        "application_restored" => "Opportunity restored",
        "bulk_change_undone" => "Bulk changes undone",
        "opportunity_discovered" => "Opportunity discovered",
        "application_submitted" => "Application submitted",
        "recruiter_contacted" => "Recruiter contacted",
        "outbound_contact_recorded" => "Outbound contact recorded",
        "reply_received" => "Reply received",
        "stage_changed" => "Stage changed",
        "interview_invited" => "Interview invitation received",
        "interview_scheduled" => "Interview scheduled",
        "interview_completed" => "Interview completed",
        "feedback_received" => "Feedback received",
        "follow_up_scheduled" => "Follow-up scheduled",
        "offer_received" => "Offer received",
        "application_rejected" => "Application rejected",
        "document_attached" => "Document attached",
        "note_added" => "Timeline note added",
        other => return format!("Unknown event ({other})"),
    }
    .to_owned()
}

/// Metadata keys the timeline shows, in display order, with their labels.
pub const VISIBLE_METADATA: [(&str, &str); 16] = [
    ("fromStage", "Previous stage"),
    ("toStage", "New stage"),
    ("fromStatus", "Previous status"),
    ("toStatus", "New status"),
    ("channel", "Channel"),
    ("interviewType", "Interview type"),
    ("scheduledAt", "Scheduled for"),
    ("followUpAt", "Follow up"),
    ("nextAction", "Next action"),
    ("outcome", "Outcome"),
    ("reason", "Reason"),
    ("note", "Timeline note"),
    ("durationMinutes", "Duration (minutes)"),
    ("answerCount", "Answers"),
    ("documentType", "Document type"),
    ("responseKind", "Response kind"),
];

/// `Nov 06, 2026, 15:30 CET`: an instant in the owner's zone, with the abbreviation in force then.
pub fn date_time(at: &Timestamp, tz: Tz) -> String {
    at.to_datetime()
        .with_timezone(&tz)
        .format("%b %d, %Y, %H:%M %Z")
        .to_string()
}

/// A date stored as UTC midnight means that calendar day, as in old Nexus, and never shifts with
/// the zone.
pub fn is_date_only(at: &Timestamp) -> bool {
    let time = at.to_datetime().time();
    time.num_seconds_from_midnight() == 0 && time.nanosecond() == 0
}

/// The calendar day an instant falls on for the owner.
pub fn day(at: &Timestamp, tz: Tz) -> NaiveDate {
    if is_date_only(at) {
        at.to_datetime().date_naive()
    } else {
        at.to_datetime().with_timezone(&tz).date_naive()
    }
}

pub fn date(at: &Timestamp, tz: Tz) -> String {
    day(at, tz).format("%b %d, %Y").to_string()
}

/// A bare date for date-only values, a zoned date-time otherwise.
pub fn moment(at: &Timestamp, tz: Tz) -> String {
    if is_date_only(at) {
        date(at, tz)
    } else {
        date_time(at, tz)
    }
}

/// Text that looks like an ISO instant is shown as a date-time; anything else verbatim.
pub fn metadata_text(value: &str, tz: Tz) -> String {
    match Timestamp::parse(value) {
        Ok(at) if value.as_bytes().get(10) == Some(&b'T') => moment(&at, tz),
        _ => value.to_owned(),
    }
}

pub fn file_size(bytes: i64) -> String {
    match bytes {
        b if b < 1024 => format!("{b} B"),
        b if b < 1024 * 1024 => format!("{:.1} KB", b as f64 / 1024.0),
        b => format!("{:.1} MB", b as f64 / (1024.0 * 1024.0)),
    }
}

/// Only web links become anchors, so a stored `javascript:` URL stays text.
pub fn web_url(value: &str) -> Option<&str> {
    let lower = value.get(..8).unwrap_or(value).to_ascii_lowercase();
    (lower.starts_with("http://") || lower.starts_with("https://")).then_some(value)
}

pub fn salary(
    min: Option<i64>,
    max: Option<i64>,
    currency: Option<&str>,
    period: Option<&str>,
) -> Option<String> {
    let amount = match (min, max) {
        (Some(min), Some(max)) if min != max => format!("{} to {}", thousands(min), thousands(max)),
        (Some(value), _) | (_, Some(value)) => thousands(value),
        (None, None) => return None,
    };
    let mut text = amount;
    if let Some(currency) = currency.filter(|c| !c.is_empty()) {
        text = format!("{text} {currency}");
    }
    if let Some(period) = period.filter(|p| !p.is_empty()) {
        text = format!("{text} / {period}");
    }
    Some(text)
}

fn thousands(value: i64) -> String {
    let digits = value.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if value < 0 { format!("-{out}") } else { out }
}

pub fn yes_no(value: bool) -> &'static str {
    if value { "Yes" } else { "No" }
}

/// `inbound` to `Inbound`, `interview_scheduled` to `Interview scheduled`.
pub fn humanize(value: &str) -> String {
    let spaced = value.replace('_', " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MADRID: Tz = chrono_tz::Europe::Madrid;

    fn ts(value: &str) -> Timestamp {
        Timestamp::parse(value).unwrap()
    }

    #[test]
    fn times_use_the_abbreviation_in_force_across_dst_changes() {
        for (utc, shown) in [
            ("2026-03-28T12:00:00Z", "Mar 28, 2026, 13:00 CET"),
            ("2026-03-29T00:59:00Z", "Mar 29, 2026, 01:59 CET"),
            ("2026-03-29T01:00:00Z", "Mar 29, 2026, 03:00 CEST"),
            ("2026-10-25T00:30:00Z", "Oct 25, 2026, 02:30 CEST"),
            ("2026-10-25T01:30:00Z", "Oct 25, 2026, 02:30 CET"),
        ] {
            assert_eq!(date_time(&ts(utc), MADRID), shown);
        }
    }

    #[test]
    fn late_evening_utc_is_the_next_day_locally() {
        assert_eq!(date(&ts("2026-11-06T23:30:00Z"), MADRID), "Nov 07, 2026");
        assert_eq!(date(&ts("2026-07-01T22:00:00Z"), MADRID), "Jul 02, 2026");
        assert_eq!(date(&ts("2026-07-01T21:59:00Z"), MADRID), "Jul 01, 2026");
    }

    #[test]
    fn date_only_values_never_shift_with_the_zone() {
        let at = ts("2026-11-06T00:00:00.000Z");
        let west: Tz = chrono_tz::America::Los_Angeles;
        for tz in [MADRID, west, chrono_tz::Pacific::Auckland] {
            assert_eq!(date(&at, tz), "Nov 06, 2026");
            assert_eq!(moment(&at, tz), "Nov 06, 2026");
        }
        assert_eq!(
            metadata_text("2026-11-06T00:00:00.000Z", west),
            "Nov 06, 2026"
        );
        assert_eq!(
            metadata_text("2026-11-06T14:00:00.000Z", MADRID),
            "Nov 06, 2026, 15:00 CET"
        );
    }
}
