//! Thin persistence functions, ported from old Nexus's `lib/db/prisma-adapter.ts` where that code
//! was plain persistence. Domain rules (event projection, submission policy, lifecycle guards)
//! belong to the callers.
//!
//! Every function takes a `&mut SqliteConnection`, so calls compose inside a transaction: get one
//! from `Db::conn` for reads, or `Db::begin_write` for any read-then-write unit.

pub mod api_tokens;
pub mod applications;
pub mod contacts;
pub mod cv;
pub mod documents;
pub mod events;
pub mod oauth;
pub mod submissions;

use serde::{Serialize, Serializer};

/// Old Nexus's adapter returned every ID as a string; these keep the JSON shape.
pub(crate) fn id_string<S: Serializer>(id: &i64, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(id)
}

pub(crate) fn opt_id_string<S: Serializer>(id: &Option<i64>, s: S) -> Result<S::Ok, S::Error> {
    match id {
        Some(id) => s.collect_str(id),
        None => s.serialize_none(),
    }
}

/// The parent summary embedded in event and document records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct ApplicationRef {
    #[serde(serialize_with = "id_string")]
    pub id: i64,
    pub company: String,
    pub role: String,
}

/// One page of a paginated listing.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Page<T> {
    pub data: Vec<T>,
    pub total: i64,
    pub page: i64,
    pub page_size: i64,
    pub total_pages: i64,
}

/// Declares a patch struct whose `Some` fields become SQL assignments or insert columns. A nullable
/// column has type `Option<T>` here, so a field reads `Option<Option<T>>`: `None` leaves it alone,
/// `Some(None)` writes NULL. Field names are column names.
macro_rules! partial_fields {
    ($(#[$meta:meta])* $name:ident { $($field:ident: $ty:ty),* $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Default, PartialEq)]
        pub struct $name {
            $(pub $field: Option<$ty>,)*
        }

        #[allow(dead_code)]
        impl $name {
            /// Appends `, col = ?` for each set field.
            fn push_assignments(&self, qb: &mut sqlx::QueryBuilder<sqlx::Sqlite>) {
                $(if let Some(value) = &self.$field {
                    qb.push(concat!(", ", stringify!($field), " = ")).push_bind(value.clone());
                })*
            }

            /// Appends `, col` for each set field.
            fn push_columns(&self, qb: &mut sqlx::QueryBuilder<sqlx::Sqlite>) {
                $(if self.$field.is_some() {
                    qb.push(concat!(", ", stringify!($field)));
                })*
            }

            /// Appends `, ?` for each set field, in `push_columns` order.
            fn push_values(&self, qb: &mut sqlx::QueryBuilder<sqlx::Sqlite>) {
                $(if let Some(value) = &self.$field {
                    qb.push(", ").push_bind(value.clone());
                })*
            }
        }
    };
}
pub(crate) use partial_fields;
