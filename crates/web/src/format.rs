//! Display helpers shared by the dashboard views. Labels come from old Nexus's `messages/en.json`.

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

/// `Nov 06, 2026, 14:30 UTC`, as `formatEventDateTime` prints it.
pub fn date_time(at: &Timestamp) -> String {
    at.to_datetime().format("%b %d, %Y, %H:%M UTC").to_string()
}

pub fn date(at: &Timestamp) -> String {
    at.to_datetime().format("%b %d, %Y").to_string()
}

/// Text that looks like an ISO instant is shown as a date-time; anything else verbatim.
pub fn metadata_text(value: &str) -> String {
    match Timestamp::parse(value) {
        Ok(at) if value.as_bytes().get(10) == Some(&b'T') => date_time(&at),
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
