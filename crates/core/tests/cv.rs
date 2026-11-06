//! The fact/wording split, the lock, and the triggers that keep CV history immutable.

use nexus_core::Db;
use nexus_core::Error;
use nexus_core::repo::applications;
use nexus_core::repo::cv::{self, CvProfileInput};
use nexus_core::repo::cv_facts::{self, OwnerFacts};
use nexus_core::repo::cv_revisions::{self, NewRevision};
use serde_json::{Value, json};
use sqlx::{Connection, SqliteConnection};

fn input() -> CvProfileInput {
    CvProfileInput {
        name: "Mara Example".into(),
        contact: json!({"email": "mara@example.test", "location": "Hamburg"}),
        profile: "Backend engineer.".into(),
        skills: json!([{"category": "Languages", "items": ["Rust", "Go"]}]),
        experience: json!([
            {"id": "brightpath", "company": "Brightpath Logistics", "title": "Software Engineer",
             "date": "03/2019 – 12/2022", "location": "Hamburg", "tier": 1,
             "bullets": ["Cut invoice errors by 40% across 3 depots."]},
            {"id": "brightpath", "company": "Brightpath Logistics", "title": "Intern",
             "date": "2018", "location": "Hamburg", "tier": 3, "bullets": []},
        ]),
        projects: Some(json!([{"name": "tidy", "stack": "Rust", "description": "A linter."}])),
        education: Some(json!([
            {"institution": "Nordsee University", "degree": "BSc Computer Science",
             "date": "2015 – 2018", "location": "Kiel"},
            {"institution": "Elbe School", "degree": "Diploma", "date": "2014", "location": "Kiel",
             "details": ""},
        ])),
    }
}

async fn open() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).await.unwrap();
    (dir, db)
}

async fn upsert(db: &Db, input: &CvProfileInput) -> nexus_core::Result<cv::CvProfile> {
    let mut tx = db.begin_write().await?;
    let profile = cv::upsert_profile(&mut tx, input).await?;
    tx.commit().await?;
    Ok(profile)
}

async fn lock(db: &Db) -> Vec<String> {
    let mut tx = db.begin_write().await.unwrap();
    let (_, added) = cv_facts::lock(&mut tx, "test").await.unwrap();
    tx.commit().await.unwrap();
    added
}

async fn count_in(conn: &mut SqliteConnection, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(conn).await.unwrap()
}

/// A database as M1 left it, before the facts moved out of `cv_profile`.
async fn migrated_to_m1(dir: &std::path::Path) -> SqliteConnection {
    let url = format!("sqlite://{}/old.db?mode=rwc", dir.display());
    let mut conn = SqliteConnection::connect(&url).await.unwrap();
    for sql in [
        include_str!("../migrations/0001_init.sql"),
        include_str!("../migrations/0002_oauth_grants.sql"),
        include_str!("../migrations/0003_import_owner.sql"),
    ] {
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
            .execute(&mut conn)
            .await
            .unwrap();
    }
    conn
}

async fn count(db: &Db, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(db.pool()).await.unwrap()
}

/// All the columns a refused call must leave alone, in one comparable value.
async fn tables(db: &Db) -> Vec<String> {
    let mut out = Vec::new();
    for sql in [
        "SELECT json_object('f', json_group_array(json_object('n', name, 'c', contact, 'e', employment,
             'd', education, 'a', allowed_tokens, 'v', version, 'l', locked_at))) FROM cv_facts",
        "SELECT json_group_array(json_object('p', profile, 's', skills, 'x', experience, 'j', projects,
             'u', updated_at)) FROM cv_profile",
        "SELECT json_group_array(state) FROM cv_facts_history",
    ] {
        out.push(
            sqlx::query_scalar::<_, String>(sql)
                .fetch_one(db.pool())
                .await
                .unwrap(),
        );
    }
    out
}

#[tokio::test]
async fn a_profile_reads_back_exactly_as_written_while_facts_and_wording_part() {
    let (_dir, db) = open().await;
    let input = input();
    let saved = upsert(&db, &input).await.unwrap();
    let as_json = serde_json::to_value(&saved).unwrap();
    for (key, expected) in [
        ("contact", &input.contact),
        ("experience", &input.experience),
        ("education", input.education.as_ref().unwrap()),
        ("projects", input.projects.as_ref().unwrap()),
    ] {
        assert_eq!(
            serde_json::to_string(&as_json[key]).unwrap(),
            serde_json::to_string(expected).unwrap(),
            "{key}, keys in order and absent keys absent"
        );
    }

    let experience: String = sqlx::query_scalar("SELECT experience FROM cv_profile")
        .fetch_one(db.pool())
        .await
        .unwrap();
    let experience: Value = serde_json::from_str(&experience).unwrap();
    assert_eq!(
        experience[0],
        json!({"id": "brightpath", "tier": 1, "bullets": ["Cut invoice errors by 40% across 3 depots."]}),
        "cv_profile keeps only the wording"
    );
    let state = cv_facts::get(&mut db.conn().await.unwrap()).await.unwrap();
    assert_eq!(state.facts.name, "Mara Example");
    assert_eq!(state.facts.employment[0].company, "Brightpath Logistics");
    assert_eq!(state.facts.education[0].id, "edu-1");
    assert_eq!(
        state.facts.education[0].qualification,
        "BSc Computer Science"
    );
    assert_eq!(state.version, 2, "the facts changed once");

    upsert(&db, &input).await.unwrap();
    let state = cv_facts::get(&mut db.conn().await.unwrap()).await.unwrap();
    assert_eq!(state.version, 2, "the same facts again are no change");
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM cv_facts_history WHERE changed_by = 'mcp'"
        )
        .await,
        1,
        "a write that changes a fact is recorded, and the repeat moved nothing"
    );
}

#[tokio::test]
async fn an_m1_database_splits_the_same_way_when_migrated() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = migrated_to_m1(dir.path()).await;
    let input = input();
    sqlx::query(
        "INSERT INTO cv_profile (name, contact, profile, skills, experience, projects, education, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, '2026-01-01T00:00:00.000Z')",
    )
    .bind(&input.name)
    .bind(input.contact.to_string())
    .bind(&input.profile)
    .bind(input.skills.to_string())
    .bind(input.experience.to_string())
    .bind(input.projects.as_ref().unwrap().to_string())
    .bind(input.education.as_ref().unwrap().to_string())
    .execute(&mut conn)
    .await
    .unwrap();
    sqlx::raw_sql(include_str!("../migrations/0004_cv_facts_revisions.sql"))
        .execute(&mut conn)
        .await
        .unwrap();

    let migrated = cv::get_profile(&mut conn).await.unwrap().unwrap();
    assert_eq!(migrated.name, input.name);
    assert_eq!(migrated.contact.0, input.contact);
    assert_eq!(migrated.experience.0, input.experience);
    assert_eq!(migrated.education.0, input.education.clone().unwrap());
    let state = cv_facts::get(&mut conn).await.unwrap();
    assert_eq!(state.facts.education[1].id, "edu-2");
    assert!(!state.is_locked());
    conn.close().await.unwrap();

    let (_dir, db) = open().await;
    let written = upsert(&db, &input).await.unwrap();
    assert_eq!(written.experience.0, migrated.experience.0);
    assert_eq!(written.education.0, migrated.education.0);
}

#[tokio::test]
async fn locking_confirms_the_numbers_in_the_wording_and_records_who() {
    let (_dir, db) = open().await;
    let mut wordy = input();
    wordy.profile = "Eight years of backend work, Q3 2024 included, on S3.".into();
    upsert(&db, &wordy).await.unwrap();
    let added = lock(&db).await;
    for token in ["40%", "3", "eight years", "S3"] {
        assert!(added.iter().any(|a| a == token), "{token} in {added:?}");
    }
    let state = cv_facts::get(&mut db.conn().await.unwrap()).await.unwrap();
    assert!(state.is_locked());
    assert_eq!(state.facts.allowed_tokens, added);
    for text in ["40% and 3", "Eight years", "S3"] {
        assert_eq!(
            nexus_tailor::unverified_numbers(text, &state.facts),
            Vec::<String>::new(),
            "{text}"
        );
    }
    let (change, by, version): (String, String, i64) = sqlx::query_as(
        "SELECT change, changed_by, version FROM cv_facts_history WHERE change = 'lock'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!((change.as_str(), by.as_str()), ("lock", "test"));
    assert_eq!(version, state.version - 1, "history holds the state before");

    let mut tx = db.begin_write().await.unwrap();
    assert!(matches!(
        cv_facts::lock(&mut tx, "test").await,
        Err(Error::CvFacts(_))
    ));
    cv_facts::unlock(&mut tx, "test").await.unwrap();
    assert!(matches!(
        cv_facts::unlock(&mut tx, "test").await,
        Err(Error::CvFacts(_))
    ));
    tx.commit().await.unwrap();
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM cv_facts_history WHERE changed_by = 'test'"
        )
        .await,
        2
    );
}

#[tokio::test]
async fn an_unlocked_write_is_recorded_and_a_lock_shows_every_change_since_the_last() {
    let (_dir, db) = open().await;
    upsert(&db, &input()).await.unwrap();
    let mut tx = db.begin_write().await.unwrap();
    let first = cv_facts::lock_preview(&mut tx).await.unwrap();
    assert!(first.needs_confirmation());
    assert!(
        first
            .changes
            .iter()
            .any(|c| c.field == "name" && c.old == "(none)" && c.new == "\"Mara Example\""),
        "{first:?}"
    );
    assert_eq!(
        count_in(
            &mut tx,
            "SELECT count(*) FROM cv_facts_history WHERE changed_by != 'mcp'"
        )
        .await,
        0,
        "a preview writes nothing"
    );
    cv_facts::lock(&mut tx, "cli").await.unwrap();
    cv_facts::unlock(&mut tx, "cli").await.unwrap();
    assert_eq!(
        cv_facts::lock_preview(&mut tx).await.unwrap(),
        cv_facts::LockPreview::default(),
        "nothing changed since the last lock"
    );
    tx.commit().await.unwrap();

    let mut false_title = input();
    false_title.experience = json!([{"id": "brightpath", "company": "Brightpath Logistics",
        "title": "Principal Engineer", "date": "03/2015 – 12/2022", "location": "Hamburg", "tier": 1,
        "bullets": ["Cut invoice errors by 40% across 3 depots."]}]);
    upsert(&db, &false_title).await.unwrap();
    let mut changed_back = false_title.clone();
    changed_back.experience[0]["title"] = json!("Distinguished\u{202e} Principal");
    upsert(&db, &changed_back).await.unwrap();
    upsert(&db, &false_title).await.unwrap();
    upsert(&db, &false_title).await.unwrap();

    let mut tx = db.begin_write().await.unwrap();
    let preview = cv_facts::lock_preview(&mut tx).await.unwrap();
    let change = |field: &str| {
        preview
            .changes
            .iter()
            .find(|c| c.field == field)
            .unwrap_or_else(|| panic!("{field} in {preview:?}"))
    };
    let title = change("employment[0:brightpath].title");
    assert_eq!(title.old, "\"Software Engineer\"");
    assert_eq!(title.new, "\"Principal Engineer\"");
    assert_eq!(
        change("employment[0:brightpath].date").new,
        "\"03/2015 – 12/2022\""
    );
    tx.commit().await.unwrap();

    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT change, changed_by, state ->> '$.employment[0].title' FROM cv_facts_history
         WHERE changed_by = 'mcp' ORDER BY id",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    let titles: Vec<&str> = rows.iter().map(|row| row.2.as_str()).collect();
    assert_eq!(
        titles,
        [
            "",
            "Software Engineer",
            "Principal Engineer",
            "Distinguished\u{202e} Principal"
        ],
        "every write, each with the state it replaced; the repeat moved nothing"
    );
    assert!(rows.iter().all(|row| row.0 == "upsert_cv_profile"));
}

/// Debug quoting escapes what could rewrite the owner's terminal.
#[tokio::test]
async fn a_change_is_printed_escaped() {
    let (_dir, db) = open().await;
    let mut hostile = input();
    hostile.name = "Mara\u{202e} \u{1b}[2J".into();
    upsert(&db, &hostile).await.unwrap();
    let mut tx = db.begin_write().await.unwrap();
    let preview = cv_facts::lock_preview(&mut tx).await.unwrap();
    let name = preview.changes.iter().find(|c| c.field == "name").unwrap();
    assert!(
        name.new.chars().all(|c| !c.is_control() && c != '\u{202e}'),
        "{:?}",
        name.new
    );
}

#[tokio::test]
async fn a_lock_confirms_a_url_in_the_wording() {
    let (_dir, db) = open().await;
    let mut linked = input();
    linked.profile = "Notes at notes.example.com/freight, code at Node.js.".into();
    upsert(&db, &linked).await.unwrap();
    let added = lock(&db).await;
    assert!(
        added
            .iter()
            .any(|token| token == "notes.example.com/freight"),
        "{added:?}"
    );
    assert!(
        !added.iter().any(|token| token.contains("Node")),
        "{added:?}"
    );
}

/// M1's schema refuses these, but if one got in, `cv_facts` must still read: nothing could repair it.
#[tokio::test]
async fn a_migrated_row_with_odd_json_stays_readable() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = migrated_to_m1(dir.path()).await;
    sqlx::query(
        "INSERT INTO cv_profile (name, contact, profile, skills, experience, projects, education, updated_at)
         VALUES ('Mara', ?, 'Summary', '[]', ?, '[]', ?, '2026-01-01T00:00:00.000Z')",
    )
    .bind(json!({"email": null, "phone": 5, "location": "Hamburg", "extra": 1}).to_string())
    .bind(
        json!([
            {"id": 7, "company": null, "title": ["x"], "date": 2019, "location": true,
             "tier": 1, "bullets": []},
            {"company": "No ID", "title": "Engineer", "date": "2020", "location": "Kiel"},
            "not an entry",
        ])
        .to_string(),
    )
    .bind(
        json!([
            {"institution": null, "degree": 3, "date": {}, "location": false, "details": null},
            {"institution": "Nordsee", "degree": "BSc", "date": "2015", "details": 9},
            "not an entry",
        ])
        .to_string(),
    )
    .execute(&mut conn)
    .await
    .unwrap();
    sqlx::raw_sql(include_str!("../migrations/0004_cv_facts_revisions.sql"))
        .execute(&mut conn)
        .await
        .unwrap();

    let state = cv_facts::get(&mut conn).await.unwrap();
    assert_eq!(state.facts.contact.email, "");
    assert_eq!(state.facts.contact.phone, "");
    assert_eq!(state.facts.contact.location, "Hamburg");
    assert_eq!(state.facts.employment.len(), 2);
    assert_eq!(state.facts.employment[0].id, "");
    assert_eq!(state.facts.employment[0].date, "");
    assert_eq!(state.facts.employment[1].company, "No ID");
    assert_eq!(state.facts.education.len(), 2);
    assert_eq!(state.facts.education[0].institution, "");
    assert_eq!(state.facts.education[1].details, "");
    cv::get_profile(&mut conn).await.unwrap().unwrap();

    let other = tempfile::tempdir().unwrap();
    let mut object_contact = migrated_to_m1(other.path()).await;
    sqlx::query(
        "INSERT INTO cv_profile (name, contact, profile, skills, experience, projects, education, updated_at)
         VALUES ('Mara', 'null', 'Summary', '[]', '[]', '[]', '[]', '2026-01-01T00:00:00.000Z')",
    )
    .execute(&mut object_contact)
    .await
    .unwrap();
    sqlx::raw_sql(include_str!("../migrations/0004_cv_facts_revisions.sql"))
        .execute(&mut object_contact)
        .await
        .unwrap();
    assert_eq!(
        cv_facts::get(&mut object_contact)
            .await
            .unwrap()
            .facts
            .contact,
        nexus_tailor::Contact::default()
    );
}

#[tokio::test]
async fn there_is_nothing_to_lock_before_a_profile() {
    let (_dir, db) = open().await;
    let mut tx = db.begin_write().await.unwrap();
    assert!(matches!(
        cv_facts::lock(&mut tx, "test").await,
        Err(Error::CvFacts(_))
    ));
}

#[tokio::test]
async fn the_owner_sets_only_what_a_profile_cannot_express() {
    let (_dir, db) = open().await;
    upsert(&db, &input()).await.unwrap();
    let fields: OwnerFacts = serde_json::from_value(json!({
        "languages": [{"name": "German", "level": "C1"}],
        "bannedPhrases": ["team player"],
    }))
    .unwrap();
    let mut tx = db.begin_write().await.unwrap();
    let state = cv_facts::set_owner_facts(&mut tx, &fields, "test")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(state.facts.languages[0].level, "C1");
    assert_eq!(state.facts.banned_phrases, ["team player"]);
    assert_eq!(state.facts.name, "Mara Example", "the rest stays");
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM cv_facts_history WHERE changed_by = 'test'"
        )
        .await,
        1
    );
    for refused in [json!({"employment": []}), json!({"name": "Else"})] {
        assert!(serde_json::from_value::<OwnerFacts>(refused).is_err());
    }
}

#[tokio::test]
async fn locked_facts_refuse_every_change_and_write_nothing() {
    let (_dir, db) = open().await;
    upsert(&db, &input()).await.unwrap();
    lock(&db).await;
    let before = tables(&db).await;

    type Change = (&'static str, fn(&mut CvProfileInput));
    let changes: [Change; 10] = [
        ("name", |i| i.name = "Mara Prime".into()),
        ("contact", |i| i.contact["phone"] = "+1 555".into()),
        ("education", |i| {
            i.education.as_mut().unwrap()[0]["degree"] = "PhD".into()
        }),
        ("education", |i| i.education = None),
        ("experience.brightpath.title", |i| {
            i.experience[0]["title"] = "Senior Engineer".into()
        }),
        ("experience.brightpath.date", |i| {
            i.experience[0]["date"] = "2017 – 2022".into()
        }),
        ("experience.brightpath.company", |i| {
            i.experience[0]["company"] = "Bright Path".into()
        }),
        ("experience.new-job", |i| {
            i.experience[1]["id"] = "new-job".into()
        }),
        ("profile", |i| i.profile = "Eight years of work.".into()),
        ("experience.brightpath.bullets", |i| {
            i.experience[0]["bullets"] = json!(["Cut invoice errors by 90%."])
        }),
    ];
    for (expected, change) in changes {
        let mut changed = input();
        change(&mut changed);
        match upsert(&db, &changed).await {
            Err(Error::CvFactsLocked { fields, .. }) => {
                assert_eq!(fields, [expected]);
            }
            other => panic!("{expected}: {other:?}"),
        }
        assert_eq!(tables(&db).await, before, "{expected}");
    }

    let mut wording = input();
    wording.profile = "Backend engineer, freight.".into();
    wording.experience[0]["bullets"] = json!(["Reduced invoice errors by 40%."]);
    wording.experience[0]["tier"] = 2.into();
    let saved = upsert(&db, &wording).await.unwrap();
    assert_eq!(saved.profile, "Backend engineer, freight.");
    assert_eq!(saved.experience.0[0]["tier"], 2);
    assert_eq!(saved.experience.0[0]["company"], "Brightpath Logistics");
    let after = tables(&db).await;
    assert_eq!(after[0], before[0], "wording changes leave the facts alone");
    assert_eq!(after[2], before[2]);

    let mut fewer = input();
    fewer.experience.as_array_mut().unwrap().truncate(1);
    let saved = upsert(&db, &fewer).await.unwrap();
    assert_eq!(saved.experience.0.as_array().unwrap().len(), 1);
    assert_eq!(
        cv_facts::get(&mut db.conn().await.unwrap())
            .await
            .unwrap()
            .facts
            .employment
            .len(),
        2,
        "dropping a job from the profile keeps its facts"
    );
}

async fn revision_on(db: &Db, variant_id: i64, parent_id: Option<i64>) -> i64 {
    let mut tx = db.begin_write().await.unwrap();
    let revision = cv_revisions::append(
        &mut tx,
        variant_id,
        &NewRevision {
            parent_id,
            source: "= CV",
            facts_version: 1,
            origin: "test",
            summary: "s",
            warnings: &["Word".to_owned()],
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    revision.id
}

#[tokio::test]
async fn revisions_and_history_cannot_change_and_a_deleted_application_takes_its_own() {
    let (_dir, db) = open().await;
    upsert(&db, &input()).await.unwrap();
    lock(&db).await;
    let app = applications::create(
        &mut db.conn().await.unwrap(),
        "Acme",
        "Engineer",
        Default::default(),
    )
    .await
    .unwrap();
    let mut tx = db.begin_write().await.unwrap();
    let bound = cv_revisions::create_variant(&mut tx, "application-x", Some(app.id))
        .await
        .unwrap();
    let default = cv_revisions::get_variant(&mut tx, "default")
        .await
        .unwrap()
        .unwrap();
    tx.commit().await.unwrap();
    let first = revision_on(&db, bound.id, None).await;
    let second = revision_on(&db, bound.id, Some(first)).await;
    let on_default = revision_on(&db, default.id, Some(second)).await;

    for sql in [
        "UPDATE cv_revisions SET source = 'x'",
        "UPDATE cv_revisions SET summary = 'x' WHERE id = 1",
        "DELETE FROM cv_revisions",
        "DELETE FROM cv_variants",
        "DELETE FROM cv_variants WHERE name = 'default'",
        "UPDATE cv_variants SET name = 'y' WHERE name = 'default'",
        "UPDATE cv_variants SET application_id = NULL WHERE name = 'application-x'",
        "UPDATE cv_facts_history SET change = 'x'",
        "DELETE FROM cv_facts_history",
        "DELETE FROM cv_facts",
    ] {
        assert!(
            sqlx::query(sql).execute(db.pool()).await.is_err(),
            "{sql} must be refused"
        );
    }

    applications::delete(&mut db.conn().await.unwrap(), app.id)
        .await
        .unwrap();
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM cv_variants WHERE name = 'application-x'"
        )
        .await,
        0
    );
    let left: Vec<i64> = sqlx::query_scalar("SELECT id FROM cv_revisions")
        .fetch_all(db.pool())
        .await
        .unwrap();
    assert_eq!(left, [on_default], "the other variant keeps its revision");
    let violations: Vec<(String,)> =
        sqlx::query_as("SELECT \"table\" FROM pragma_foreign_key_check")
            .fetch_all(db.pool())
            .await
            .unwrap();
    assert!(violations.is_empty(), "{violations:?}");
}
