//! What happens to a referencing array when the parent row is deleted.
//!
//! Core Postgres stalled on this question for array element foreign keys, because
//! `ON DELETE CASCADE` is ambiguous: should it delete the whole child row, or just
//! remove that one element from the array? Both are reasonable. We sidestep the
//! argument by not reusing `CASCADE` at all — removing the element is `scrub`.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnDelete {
    /// Refuse to delete the parent row while any array still references it.
    /// Matches the default for a native foreign key.
    Restrict,
    /// Remove the deleted key from every array that contains it.
    Scrub,
    /// Set the whole array column to NULL in every row that references the key.
    SetNull,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownMode(pub String);

impl core::fmt::Display for UnknownMode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "unknown on_delete mode {:?}; expected one of restrict, scrub, set_null",
            self.0
        )
    }
}

impl OnDelete {
    /// Case-insensitive, and tolerant of `set null` as well as `set_null`.
    pub fn parse(s: &str) -> Result<Self, UnknownMode> {
        match s.trim().to_ascii_lowercase().replace(' ', "_").as_str() {
            "restrict" => Ok(Self::Restrict),
            "scrub" => Ok(Self::Scrub),
            "set_null" => Ok(Self::SetNull),
            _ => Err(UnknownMode(s.to_string())),
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Restrict => "restrict",
            Self::Scrub => "scrub",
            Self::SetNull => "set_null",
        }
    }

    /// `restrict` only needs to know whether a reference exists. The other modes
    /// have to find and rewrite every referencing row.
    #[must_use]
    pub fn rewrites_children(self) -> bool {
        matches!(self, Self::Scrub | Self::SetNull)
    }
}

#[cfg(test)]
mod tests {
    use super::{OnDelete, UnknownMode};

    #[test]
    fn round_trips_through_text() {
        for m in [OnDelete::Restrict, OnDelete::Scrub, OnDelete::SetNull] {
            assert_eq!(OnDelete::parse(m.as_str()), Ok(m));
        }
    }

    #[test]
    fn accepts_sloppy_input() {
        assert_eq!(OnDelete::parse("RESTRICT"), Ok(OnDelete::Restrict));
        assert_eq!(OnDelete::parse("  scrub  "), Ok(OnDelete::Scrub));
        assert_eq!(OnDelete::parse("set null"), Ok(OnDelete::SetNull));
        assert_eq!(OnDelete::parse("SET NULL"), Ok(OnDelete::SetNull));
    }

    #[test]
    fn rejects_cascade_explicitly() {
        // Refusing `cascade` is deliberate: it is the ambiguity that stalled the
        // feature in core, so we do not pretend to have an answer for it.
        assert!(OnDelete::parse("cascade").is_err());
    }

    #[test]
    fn rejects_unknown_modes_with_a_helpful_message() {
        let e = OnDelete::parse("delete_everything").unwrap_err();
        assert_eq!(e, UnknownMode("delete_everything".into()));
        let msg = e.to_string();
        assert!(msg.contains("restrict"), "got {msg}");
        assert!(msg.contains("scrub"), "got {msg}");
    }

    #[test]
    fn only_some_modes_touch_child_rows() {
        assert!(!OnDelete::Restrict.rewrites_children());
        assert!(OnDelete::Scrub.rewrites_children());
        assert!(OnDelete::SetNull.rewrites_children());
    }
}
