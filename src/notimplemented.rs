//! `NotImplemented` and its sentinel (`notimplemented.go`).
//!
//! A call that fails with [`Error::NotImplemented`](crate::Error::NotImplemented) asked for
//! something byblos does not do yet. That is a property of the build, not of the document, so a
//! caller changes its pipeline (falls back to another tool) instead of its handling of one
//! file. `capability` comes from the same vocabulary as provenance and `upgrade_candidates`,
//! not free text.

use std::fmt;

/// A capability byblos does not have yet, why not, and where the work is tracked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotImplemented {
    /// A capability string, for example `"linearize"`.
    pub capability: String,
    /// One sentence, in terms of what is missing, not what to do.
    pub why: String,
    /// The tracking id, for example `"byb-k48"`.
    pub issue: String,
}

impl fmt::Display for NotImplemented {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "byblos: {} is not implemented: {} (tracked as {})",
            self.capability, self.why, self.issue
        )
    }
}

impl std::error::Error for NotImplemented {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;

    #[test]
    fn message_and_class() {
        let n = NotImplemented {
            capability: "linearize".into(),
            why: "no hint tables".into(),
            issue: "byb-k48".into(),
        };
        assert_eq!(
            n.to_string(),
            "byblos: linearize is not implemented: no hint tables (tracked as byb-k48)"
        );
        let e = Error::from(n);
        assert!(e.is_not_implemented());
        assert_eq!(e.class(), "not-implemented/linearize");
    }
}
