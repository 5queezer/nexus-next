use serde::Serialize;
use sqlx::{QueryBuilder, SqliteConnection};

use super::{applications, id_string, partial_fields};
use crate::error::{Error, Result};
use crate::time::Timestamp;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Contact {
    #[serde(serialize_with = "id_string")]
    pub id: i64,
    pub name: String,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub role: Option<String>,
    pub linked_in: Option<String>,
    #[serde(serialize_with = "id_string")]
    pub application_id: i64,
    pub created_at: Timestamp,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewContact {
    pub name: String,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub role: Option<String>,
    pub linked_in: Option<String>,
}

partial_fields! {
    ContactPatch {
        name: String,
        email: Option<String>,
        phone: Option<String>,
        role: Option<String>,
        linked_in: Option<String>,
    }
}

pub async fn list_for_application(
    conn: &mut SqliteConnection,
    application_id: i64,
) -> Result<Vec<Contact>> {
    Ok(
        sqlx::query_as("SELECT * FROM contacts WHERE application_id = ? ORDER BY id")
            .bind(application_id)
            .fetch_all(conn)
            .await?,
    )
}

/// Contacts of many applications at once, ordered by application and then ID.
pub async fn list_for_applications(
    conn: &mut SqliteConnection,
    application_ids: &[i64],
) -> Result<Vec<Contact>> {
    if application_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut qb = QueryBuilder::new("SELECT * FROM contacts WHERE application_id IN (");
    let mut list = qb.separated(", ");
    for id in application_ids {
        list.push_bind(*id);
    }
    qb.push(") ORDER BY application_id, id");
    Ok(qb.build_query_as().fetch_all(conn).await?)
}

pub async fn create(
    conn: &mut SqliteConnection,
    application_id: i64,
    contact: &NewContact,
) -> Result<Contact> {
    if !applications::exists(&mut *conn, application_id).await? {
        return Err(Error::NotFound);
    }
    Ok(sqlx::query_as(
        "INSERT INTO contacts (application_id, name, email, phone, role, linked_in)
         VALUES (?, ?, ?, ?, ?, ?) RETURNING *",
    )
    .bind(application_id)
    .bind(&contact.name)
    .bind(&contact.email)
    .bind(&contact.phone)
    .bind(&contact.role)
    .bind(&contact.linked_in)
    .fetch_one(conn)
    .await?)
}

pub async fn update(
    conn: &mut SqliteConnection,
    id: i64,
    application_id: i64,
    patch: &ContactPatch,
) -> Result<Contact> {
    let mut qb = QueryBuilder::new("UPDATE contacts SET id = id");
    patch.push_assignments(&mut qb);
    qb.push(" WHERE id = ").push_bind(id);
    qb.push(" AND application_id = ").push_bind(application_id);
    qb.push(" RETURNING *");
    qb.build_query_as()
        .fetch_optional(conn)
        .await?
        .ok_or(Error::NotFound)
}

pub async fn delete(conn: &mut SqliteConnection, id: i64, application_id: i64) -> Result<()> {
    let done = sqlx::query("DELETE FROM contacts WHERE id = ? AND application_id = ?")
        .bind(id)
        .bind(application_id)
        .execute(conn)
        .await?;
    if done.rows_affected() == 1 {
        Ok(())
    } else {
        Err(Error::NotFound)
    }
}
