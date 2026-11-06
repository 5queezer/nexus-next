use std::collections::HashMap;

use nexus_core::Timestamp;
use nexus_core::repo::{applications, documents, submissions};
use serde_json::{Value, json};

use super::{Ctx, ToolResult, json_result};

fn finding(
    code: &str,
    severity: &str,
    ids: (&str, String),
    message: &str,
    remediation: &str,
) -> Value {
    let mut finding = json!({"code": code, "severity": severity});
    finding[ids.0] = ids.1.into();
    finding["message"] = message.into();
    finding["remediation"] = remediation.into();
    finding
}

/// Port of `computeApplicationHealth` and the `pipeline_healthcheck` handler.
pub async fn check(ctx: &Ctx) -> ToolResult {
    if let Some(error) = ctx.require_submissions() {
        return Ok(error);
    }
    let mut conn = ctx.state.db.conn().await?;
    let applications = applications::list(&mut conn).await?;
    let submissions = submissions::list(&mut conn).await?;
    let documents = documents::list(&mut conn).await?;
    let now = Timestamp::now();
    let mut findings = Vec::new();
    let mut submission_counts: HashMap<i64, usize> = HashMap::new();

    for submission in &submissions {
        *submission_counts
            .entry(submission.application_id)
            .or_default() += 1;
        let with_submission = |code, severity, message, remediation| {
            let mut value = finding(
                code,
                severity,
                ("applicationId", submission.application_id.to_string()),
                message,
                remediation,
            );
            value["submissionId"] = submission.id.to_string().into();
            value
        };
        if submission.answers.0.as_array().is_some_and(Vec::is_empty) {
            findings.push(with_submission(
                "submission_without_answers",
                "warning",
                "The submission does not preserve any application answers.",
                "Record the exact submitted answers or explicitly confirm the form had none.",
            ));
        }
        if submission
            .document_ids
            .0
            .as_array()
            .is_some_and(Vec::is_empty)
        {
            findings.push(with_submission(
                "submission_without_materials",
                "error",
                "The submission has no preserved submitted materials.",
                "Link the exact submitted CV and other uploaded files.",
            ));
        }
    }

    for application in &applications {
        let id = || ("applicationId", application.id.to_string());
        let status = application.status.as_str();
        let is_applied = matches!(status, "applied" | "interview" | "offer" | "rejected");
        if is_applied && application.applied_at.is_none() {
            findings.push(finding(
                "applied_without_date",
                "error",
                id(),
                "The application is applied-or-later but has no appliedAt timestamp.",
                "Record the confirmed submission timestamp.",
            ));
        }
        if !is_applied && application.applied_at.is_some() {
            findings.push(finding(
                "date_without_applied_status",
                "warning",
                id(),
                "The lead has an appliedAt timestamp but is not in an applied-or-later status.",
                "Correct the status or clear the application timestamp.",
            ));
        }
        if matches!(status, "applied" | "interview" | "offer") {
            if !submission_counts.contains_key(&application.id) {
                findings.push(finding(
                    "applied_without_submission",
                    "error",
                    id(),
                    "No structured submission package is stored for this active application.",
                    "Record the submitted answers and exact submitted materials.",
                ));
            }
            match &application.follow_up_at {
                None => findings.push(finding(
                    "missing_next_action",
                    "warning",
                    id(),
                    "The active application has no follow-up date.",
                    "Set the next follow-up or interview action.",
                )),
                Some(follow_up) if *follow_up < now => findings.push(finding(
                    "overdue_follow_up",
                    "warning",
                    id(),
                    "The follow-up date is overdue.",
                    "Complete or reschedule the follow-up.",
                )),
                Some(_) => {}
            }
        }
    }

    for document in documents
        .iter()
        .filter(|document| document.applications.is_empty())
    {
        findings.push(finding(
            "orphan_document",
            "info",
            ("documentId", document.id.to_string()),
            "The document is not linked to an application.",
            "Link, classify as intentionally orphaned, archive, or delete after review.",
        ));
    }

    Ok(json_result(
        json!({"healthy": findings.is_empty(), "findingCount": findings.len(), "findings": findings}),
        false,
    ))
}
