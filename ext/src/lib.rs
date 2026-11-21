//! `pg_anyfk` — foreign keys for the elements of an array column.
//!
//! Postgres has no syntax for this: a `FOREIGN KEY` constrains a whole column, not
//! the elements inside an array. Core attempted the feature from 2017 to 2021 and
//! stalled on semantics, not difficulty. This builds it on top with triggers, and
//! picks an opinionated answer where core needed consensus.

#![cfg_attr(
    any(test, feature = "pg_test"),
    allow(clippy::unwrap_used, clippy::expect_used)
)]

// Emits the symbol Postgres checks before loading the library.
::pgrx::pg_module_magic!(name, version);

/// Declares the `anyfk` schema to pgrx's SQL generator. The schema itself is created
/// by the bootstrap SQL below, which must run first because the catalog lives in it.
#[pgrx::pg_schema]
mod anyfk {}

mod api;
mod spi_util;

#[cfg(any(test, feature = "pg_test"))]
mod pgtests;

// Schema and the constraints catalog. `bootstrap` puts this before everything.
::pgrx::extension_sql_file!("../sql/schema.sql", name = "schema", bootstrap);

// Trigger functions and helpers. `finalize` puts this last.
::pgrx::extension_sql_file!("../sql/api.sql", name = "api", finalize);

/// Required by `cargo pgrx test`. Must stay visible at the crate root.
#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {}

    #[must_use]
    pub fn postgresql_conf_options() -> Vec<&'static str> {
        vec![]
    }
}
