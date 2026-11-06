//! Follow-up classification, ported from old Nexus's `focus-queue.ts` and `follow-up-policy.ts`.

use std::cmp::Reverse;
use std::sync::LazyLock;

use chrono::{NaiveDate, Utc};
use chrono_tz::Tz;
use nexus_core::repo::applications::Application;
use regex::RegexSet;

const PREFIX: &str = r"^(?:[-*•]\s*)?(?:(?:next\s+action|action|instruction|nächste\s+aktion|nächster\s+schritt|anweisung)\s*:\s*)?(?:(?:please|bitte)\s+)?";

/// The JS patterns end the German one with a lookahead, which `regex` lacks. Consuming that text
/// instead changes nothing, because only whether a clause matches is used.
static HOLD_PATTERNS: LazyLock<RegexSet> = LazyLock::new(|| {
    let bodies = [
        r"(?:(?:do\s+not|don't)\s+(?:send\s+(?:a\s+)?)?follow[ -]?up\b|not\s+yet\s+(?:send\s+(?:a\s+)?)?follow[ -]?up\b)",
        r"no\s+follow[ -]?up(?:\s+(?:until|before)\b|\s*$)",
        r"wait\b.{0,100}\b(?:feedback|repl(?:y|ies)|recruiter|response)\b",
        r"(?:(?:vorerst\s+)?kein(?:e[ns]?)?\s+(?:follow[ -]?up|nachfass\w*|nachfrag\w*)(?:\s*$|\s+(?:bis|vor|bevor)\b)|auf\s+.{0,60}(?:antwort|rückmeldung)\s+warten\b)",
    ];
    RegexSet::new(bodies.map(|body| format!("(?i){PREFIX}{body}"))).expect("hold patterns compile")
});

fn explicit_hold(value: Option<&str>) -> Option<&str> {
    value?
        .split(['\n', '.', '!', '?', ';'])
        .map(str::trim)
        .filter(|clause| !clause.is_empty())
        .find(|clause| HOLD_PATTERNS.is_match(clause))
}

/// Explicit current instructions take precedence over a suggested reminder date.
pub fn follow_up_hold_reason(application: &Application) -> Option<&str> {
    explicit_hold(application.next_action.as_deref())
        .or_else(|| explicit_hold(application.notes.as_deref()))
        .or_else(|| explicit_hold(application.job_summary.as_deref()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    Overdue,
    DueSoon,
    Waiting,
    NewLeads,
    Completed,
}

impl Group {
    pub fn title(self) -> &'static str {
        match self {
            Group::Overdue => "Overdue",
            Group::DueSoon => "Due soon",
            Group::Waiting => "Waiting",
            Group::NewLeads => "New leads",
            Group::Completed => "Completed",
        }
    }
}

/// The owner's zone and today's date in it.
#[derive(Clone, Copy)]
pub struct Clock {
    pub tz: Tz,
    pub today: NaiveDate,
}

impl Clock {
    pub fn now(tz: Tz) -> Self {
        Self {
            tz,
            today: Utc::now().with_timezone(&tz).date_naive(),
        }
    }
}

pub fn follow_up_day(application: &Application, tz: Tz) -> Option<NaiveDate> {
    application
        .follow_up_at
        .as_ref()
        .map(|at| crate::format::day(at, tz))
}

pub fn group_of(application: &Application, clock: Clock) -> Group {
    let due = follow_up_day(application, clock.tz);
    if matches!(application.status.as_str(), "offer" | "rejected") {
        Group::Completed
    } else if follow_up_hold_reason(application).is_some() {
        Group::Waiting
    } else if due.is_some_and(|due| due <= clock.today) {
        Group::Overdue
    } else if due.is_some() {
        Group::DueSoon
    } else if application.status == "inbound" {
        Group::NewLeads
    } else {
        Group::Waiting
    }
}

/// Non-empty groups in display order. Within a group: earliest follow-up first (none last), then
/// higher triage quality, then ID.
pub fn build<'a>(
    applications: impl IntoIterator<Item = &'a Application>,
    clock: Clock,
) -> Vec<(Group, Vec<&'a Application>)> {
    let mut groups: Vec<(Group, Vec<&Application>)> = [
        Group::Overdue,
        Group::DueSoon,
        Group::Waiting,
        Group::NewLeads,
        Group::Completed,
    ]
    .map(|group| (group, Vec::new()))
    .into();
    for application in applications {
        let group = group_of(application, clock);
        groups
            .iter_mut()
            .find(|(candidate, _)| *candidate == group)
            .expect("every group is listed")
            .1
            .push(application);
    }
    for (_, items) in &mut groups {
        items.sort_by(|a, b| {
            let key = |x: &Application| {
                (
                    x.follow_up_at.is_none(),
                    x.follow_up_at.clone(),
                    Reverse(x.triage_quality.unwrap_or(0)),
                    x.id,
                )
            };
            key(a).cmp(&key(b))
        });
    }
    groups.retain(|(_, items)| !items.is_empty());
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::Timestamp;

    fn app(id: i64, status: &str) -> Application {
        Application {
            id,
            company: format!("Co{id}"),
            role: "Engineer".into(),
            status: status.into(),
            applied_at: None,
            last_contact: None,
            notes: None,
            follow_up_at: None,
            job_description: None,
            source: None,
            remote: false,
            salary_min: None,
            salary_max: None,
            rating: None,
            job_url: None,
            canonical_job_url: None,
            resume_id: None,
            company_size: None,
            salary_band_mentioned: false,
            triage_quality: None,
            triage_reason: None,
            incoming_source: None,
            auto_rejected: false,
            auto_reject_reason: None,
            archived_at: None,
            work_mode: None,
            eligible_countries: Default::default(),
            primary_locations: Default::default(),
            office_days_min: None,
            travel_percent: None,
            visa_sponsorship: None,
            right_to_work_required: None,
            timezone_overlap: None,
            salary_currency: None,
            salary_period: None,
            salary_type: None,
            ats_name: None,
            requisition_id: None,
            job_captured_at: None,
            job_verified_at: None,
            job_posted_at: None,
            job_closed_at: None,
            job_content_hash: None,
            job_liveness: None,
            job_summary: None,
            current_stage: None,
            next_action: None,
            event_version: 0,
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
        }
    }

    fn clock(value: &str) -> Clock {
        Clock {
            tz: chrono_tz::Europe::Madrid,
            today: value.parse().unwrap(),
        }
    }

    fn due(mut application: Application, at: &str) -> Application {
        application.follow_up_at = Some(Timestamp::parse(at).unwrap());
        application
    }

    #[test]
    fn hold_instructions_are_recognized_in_english_and_german() {
        for text in [
            "Do not follow up",
            "next action: please don't send a follow-up until Monday",
            "No follow-up before the offer call",
            "Wait for recruiter feedback",
            "Nächste Aktion: kein Follow-up bis Freitag",
            "Bitte auf die Antwort warten",
            "Sent CV. Wait for their reply",
        ] {
            assert!(explicit_hold(Some(text)).is_some(), "{text}");
        }
        for text in [
            "Follow up on Friday",
            "They will wait lists",
            "Kein Problem",
        ] {
            assert!(explicit_hold(Some(text)).is_none(), "{text}");
        }
    }

    #[test]
    fn groups_follow_old_nexus_rules() {
        let today = clock("2026-11-06");
        let overdue = due(app(1, "applied"), "2026-11-06T00:00:00.000Z");
        let soon = due(app(2, "interview"), "2026-11-07T00:00:00.000Z");
        let lead = app(3, "inbound");
        let idle = app(4, "applied");
        let closed = due(app(5, "rejected"), "2026-01-01T00:00:00.000Z");
        let mut held = due(app(6, "applied"), "2026-01-01T00:00:00.000Z");
        held.next_action = Some("do not follow up".into());
        let kinds: Vec<_> = [&overdue, &soon, &lead, &idle, &closed, &held]
            .map(|a| group_of(a, today))
            .into();
        assert_eq!(
            kinds,
            [
                Group::Overdue,
                Group::DueSoon,
                Group::NewLeads,
                Group::Waiting,
                Group::Completed,
                Group::Waiting
            ]
        );
    }

    #[test]
    fn build_orders_groups_and_members() {
        let today = clock("2026-11-06");
        let later = due(app(1, "applied"), "2026-11-03T00:00:00.000Z");
        let earlier = due(app(2, "applied"), "2026-11-01T00:00:00.000Z");
        let groups = build([&later, &earlier], today);
        assert_eq!(groups.len(), 1);
        let ids: Vec<_> = groups[0].1.iter().map(|a| a.id).collect();
        assert_eq!(ids, [2, 1]);
    }

    #[test]
    fn day_boundaries_use_the_local_date_but_date_only_follow_ups_do_not_shift() {
        let today = clock("2026-11-06");
        let late = due(app(1, "applied"), "2026-11-06T23:30:00Z");
        assert_eq!(group_of(&late, today), Group::DueSoon, "Nov 07 in Madrid");
        let early = due(app(2, "applied"), "2026-11-05T23:30:00Z");
        assert_eq!(group_of(&early, today), Group::Overdue, "Nov 06 in Madrid");
        let date_only = due(app(3, "applied"), "2026-11-06T00:00:00.000Z");
        assert_eq!(group_of(&date_only, today), Group::Overdue);
        let west = Clock {
            tz: chrono_tz::America::Los_Angeles,
            today: "2026-11-05".parse().unwrap(),
        };
        assert_eq!(group_of(&date_only, west), Group::DueSoon, "still Nov 06");
    }
}
