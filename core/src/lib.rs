//! Parsing and validation for `pg_anyfk`.
//!
//! No Postgres dependency, so this tests with plain `cargo test` and no server.
//! The `ext` crate is thin glue: it resolves names against the catalogs, installs
//! triggers, and reports errors.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod mode;
pub mod parent_ref;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Render an identifier as a double-quoted SQL name.
///
/// Always quoting is the simplest thing that is always correct: it survives
/// mixed case, reserved words and embedded punctuation.
#[must_use]
pub fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::{quote_ident, VERSION};

    #[test]
    fn version_is_populated() {
        assert!(VERSION.split('.').count() >= 2, "got {VERSION}");
    }

    #[test]
    fn quoting_handles_the_awkward_cases() {
        assert_eq!(quote_ident("tags"), "\"tags\"");
        assert_eq!(quote_ident("My Table"), "\"My Table\"");
        assert_eq!(quote_ident("select"), "\"select\"");
        assert_eq!(quote_ident("we\"ird"), "\"we\"\"ird\"");
    }
}
