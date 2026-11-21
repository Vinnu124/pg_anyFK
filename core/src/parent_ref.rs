//! Parses the reference target, written the way a foreign key reads:
//! `tags(id)`, `public.tags(id)`, `"My Schema"."My Table"("My Col")`.
//!
//! Identifier folding matches Postgres: an unquoted identifier is lowercased, a
//! quoted one keeps its case. Getting that wrong would mean `Tags(ID)` resolving to
//! a different table than the same name typed into `psql`.

use crate::quote_ident;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParentRef {
    pub schema: Option<String>,
    pub table: String,
    pub column: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefError {
    Empty,
    EmptyIdentifier,
    UnterminatedQuote,
    MissingOpenParen,
    MissingCloseParen,
    TooManyParts(usize),
    TrailingText(String),
}

impl core::fmt::Display for RefError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Empty => write!(f, "reference target is empty"),
            Self::EmptyIdentifier => write!(f, "reference target has an empty identifier"),
            Self::UnterminatedQuote => {
                write!(f, "reference target has an unterminated double quote")
            }
            Self::MissingOpenParen => {
                write!(f, "reference target must name a column, as in 'tags(id)'")
            }
            Self::MissingCloseParen => write!(f, "reference target is missing its closing ')'"),
            Self::TooManyParts(n) => write!(
                f,
                "reference target has {n} dotted parts; expected 'table(column)' \
                 or 'schema.table(column)'"
            ),
            Self::TrailingText(rest) => {
                write!(f, "unexpected text after the reference target: {rest:?}")
            }
        }
    }
}

fn skip_ws(b: &[u8], i: &mut usize) {
    while matches!(b.get(*i), Some(c) if c.is_ascii_whitespace()) {
        *i += 1;
    }
}

/// Read one identifier, applying Postgres' folding rules.
fn read_ident(s: &str, i: &mut usize) -> Result<String, RefError> {
    let b = s.as_bytes();
    skip_ws(b, i);

    if b.get(*i) == Some(&b'"') {
        *i += 1;
        let mut out = String::new();
        loop {
            match b.get(*i) {
                None => return Err(RefError::UnterminatedQuote),
                Some(&b'"') => {
                    if b.get(*i + 1) == Some(&b'"') {
                        out.push('"');
                        *i += 2;
                    } else {
                        *i += 1;
                        break;
                    }
                }
                Some(_) => {
                    // Copy one whole UTF-8 character. Pushing the byte on its own
                    // would read it as Latin-1 and turn "café" into "cafÃ©".
                    let from = *i;
                    *i += 1;
                    while matches!(b.get(*i), Some(c) if c & 0xC0 == 0x80) {
                        *i += 1;
                    }
                    out.push_str(&s[from..*i]);
                }
            }
        }
        if out.is_empty() {
            return Err(RefError::EmptyIdentifier);
        }
        return Ok(out);
    }

    let start = *i;
    // Postgres allows non-ASCII letters in an unquoted identifier, so accept any
    // byte above ASCII and let the server decide whether the name resolves.
    while matches!(b.get(*i),
        Some(c) if c.is_ascii_alphanumeric() || *c == b'_' || *c == b'$' || *c >= 0x80)
    {
        *i += 1;
    }
    if *i == start {
        return Err(RefError::EmptyIdentifier);
    }
    // Only ASCII folds. Postgres leaves multibyte characters alone, so neither do we.
    Ok(s[start..*i].to_ascii_lowercase())
}

impl ParentRef {
    pub fn parse(input: &str) -> Result<Self, RefError> {
        if input.trim().is_empty() {
            return Err(RefError::Empty);
        }

        let b = input.as_bytes();
        let mut i = 0usize;

        let mut parts = Vec::new();
        loop {
            parts.push(read_ident(input, &mut i)?);
            skip_ws(b, &mut i);
            if b.get(i) == Some(&b'.') {
                i += 1;
                continue;
            }
            break;
        }

        if b.get(i) != Some(&b'(') {
            return Err(RefError::MissingOpenParen);
        }
        i += 1;

        let column = read_ident(input, &mut i)?;

        skip_ws(b, &mut i);
        if b.get(i) != Some(&b')') {
            return Err(RefError::MissingCloseParen);
        }
        i += 1;

        skip_ws(b, &mut i);
        if i != b.len() {
            return Err(RefError::TrailingText(input[i..].to_string()));
        }

        let (schema, table) = match parts.len() {
            1 => (None, parts.remove(0)),
            2 => {
                let t = parts.remove(1);
                (Some(parts.remove(0)), t)
            }
            n => return Err(RefError::TooManyParts(n)),
        };

        Ok(Self {
            schema,
            table,
            column,
        })
    }

    /// The table, quoted, for handing to `to_regclass`. Quoting is what makes a
    /// mixed-case or reserved-word name resolve to the table the user meant.
    #[must_use]
    pub fn quoted_table(&self) -> String {
        match &self.schema {
            Some(s) => format!("{}.{}", quote_ident(s), quote_ident(&self.table)),
            None => quote_ident(&self.table),
        }
    }

    #[must_use]
    pub fn quoted_column(&self) -> String {
        quote_ident(&self.column)
    }
}

impl core::fmt::Display for ParentRef {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}({})", self.quoted_table(), self.quoted_column())
    }
}

#[cfg(test)]
mod tests {
    use super::{ParentRef, RefError};

    fn p(s: &str) -> ParentRef {
        ParentRef::parse(s).unwrap()
    }

    #[test]
    fn parses_a_bare_table() {
        let r = p("tags(id)");
        assert_eq!(r.schema, None);
        assert_eq!(r.table, "tags");
        assert_eq!(r.column, "id");
    }

    #[test]
    fn parses_a_qualified_table() {
        let r = p("public.tags(id)");
        assert_eq!(r.schema.as_deref(), Some("public"));
        assert_eq!(r.table, "tags");
        assert_eq!(r.column, "id");
    }

    #[test]
    fn tolerates_whitespace_everywhere() {
        let r = p("  public . tags ( id )  ");
        assert_eq!(r.schema.as_deref(), Some("public"));
        assert_eq!(r.table, "tags");
        assert_eq!(r.column, "id");
    }

    #[test]
    fn folds_unquoted_identifiers_to_lower_case() {
        // Postgres does this, so we must too, or Tags(ID) resolves to the wrong table.
        let r = p("Public.Tags(ID)");
        assert_eq!(r.schema.as_deref(), Some("public"));
        assert_eq!(r.table, "tags");
        assert_eq!(r.column, "id");
    }

    #[test]
    fn preserves_the_case_of_quoted_identifiers() {
        let r = p("\"My Schema\".\"My Table\"(\"My Col\")");
        assert_eq!(r.schema.as_deref(), Some("My Schema"));
        assert_eq!(r.table, "My Table");
        assert_eq!(r.column, "My Col");
    }

    #[test]
    fn handles_doubled_quotes_inside_an_identifier() {
        let r = p("\"we\"\"ird\"(id)");
        assert_eq!(r.table, "we\"ird");
    }

    #[test]
    fn a_quoted_identifier_may_contain_punctuation() {
        let r = p("\"a.b\"(\"c(d)\")");
        assert_eq!(r.schema, None);
        assert_eq!(r.table, "a.b");
        assert_eq!(r.column, "c(d)");
    }

    #[test]
    fn renders_back_as_a_safely_quoted_name() {
        assert_eq!(p("public.tags(id)").quoted_table(), "\"public\".\"tags\"");
        assert_eq!(p("tags(id)").quoted_table(), "\"tags\"");
        assert_eq!(p("tags(id)").quoted_column(), "\"id\"");
        assert_eq!(
            p("\"My Schema\".\"T\"(c)").quoted_table(),
            "\"My Schema\".\"T\""
        );
        assert_eq!(
            p("public.tags(id)").to_string(),
            "\"public\".\"tags\"(\"id\")"
        );
    }

    #[test]
    fn rejects_empty_input() {
        assert_eq!(ParentRef::parse(""), Err(RefError::Empty));
        assert_eq!(ParentRef::parse("   "), Err(RefError::Empty));
    }

    #[test]
    fn rejects_a_missing_column() {
        assert_eq!(ParentRef::parse("tags"), Err(RefError::MissingOpenParen));
        assert_eq!(
            ParentRef::parse("public.tags"),
            Err(RefError::MissingOpenParen)
        );
    }

    #[test]
    fn rejects_an_unclosed_paren() {
        assert_eq!(
            ParentRef::parse("tags(id"),
            Err(RefError::MissingCloseParen)
        );
    }

    #[test]
    fn rejects_an_empty_identifier() {
        assert_eq!(ParentRef::parse("tags()"), Err(RefError::EmptyIdentifier));
        assert_eq!(
            ParentRef::parse(".tags(id)"),
            Err(RefError::EmptyIdentifier)
        );
        assert_eq!(ParentRef::parse("\"\"(id)"), Err(RefError::EmptyIdentifier));
    }

    #[test]
    fn rejects_an_unterminated_quote() {
        assert_eq!(
            ParentRef::parse("\"tags(id)"),
            Err(RefError::UnterminatedQuote)
        );
    }

    #[test]
    fn rejects_too_many_dotted_parts() {
        assert_eq!(
            ParentRef::parse("db.public.tags(id)"),
            Err(RefError::TooManyParts(3))
        );
    }

    #[test]
    fn rejects_trailing_junk() {
        assert_eq!(
            ParentRef::parse("tags(id) DROP TABLE x"),
            Err(RefError::TrailingText("DROP TABLE x".into()))
        );
        assert_eq!(
            ParentRef::parse("tags(id);"),
            Err(RefError::TrailingText(";".into()))
        );
    }

    #[test]
    fn error_messages_name_the_expected_shape() {
        let msg = RefError::MissingOpenParen.to_string();
        assert!(msg.contains("tags(id)"), "got {msg}");
    }
    #[test]
    fn a_quoted_non_ascii_identifier_survives_intact() {
        // Reading the bytes one at a time made this "cafÃ©".
        let r = p("\"café\"(id)");
        assert_eq!(r.table, "café");
        assert_eq!(
            p("\"naïve\".\"café\"(\"id\")").schema.as_deref(),
            Some("naïve")
        );
    }

    #[test]
    fn an_unquoted_non_ascii_identifier_is_accepted_and_ascii_folded() {
        // Postgres allows non-ASCII letters unquoted, and folds only the ASCII part.
        assert_eq!(p("café(id)").table, "café");
        assert_eq!(p("CafÉ(ID)").table, "cafÉ");
        assert_eq!(p("CafÉ(ID)").column, "id");
    }

    #[test]
    fn non_ascii_input_does_not_break_byte_indexing() {
        assert_eq!(
            ParentRef::parse("\"tágs\"(id) junk"),
            Err(RefError::TrailingText("junk".into()))
        );
    }
}
