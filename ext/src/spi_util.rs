//! Small SPI helpers. Glue only; no decisions here.

use pgrx::prelude::*;

/// Turn a recoverable Rust error into a Postgres ERROR instead of unwrapping.
pub trait OrPgError<T> {
    fn or_pg(self, context: &str) -> T;
}

impl<T, E: core::fmt::Display> OrPgError<T> for Result<T, E> {
    fn or_pg(self, context: &str) -> T {
        match self {
            Ok(v) => v,
            Err(e) => error!("{context}: {e}"),
        }
    }
}

impl<T> OrPgError<T> for Option<T> {
    fn or_pg(self, context: &str) -> T {
        match self {
            Some(v) => v,
            None => error!("{context}"),
        }
    }
}

/// `SELECT` one boolean with text arguments, defaulting to false on NULL.
pub fn one_bool(sql: &str, args: &[&str]) -> bool {
    let datums: Vec<pgrx::datum::DatumWithOid<'_>> = args.iter().map(|a| (*a).into()).collect();
    Spi::connect(|client| {
        client
            .select(sql, Some(1), &datums)
            .or_pg("reading boolean")
            .first()
            .get_one::<bool>()
            .or_pg("reading boolean column")
    })
    .unwrap_or(false)
}
