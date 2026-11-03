//! The eval set's lock: its version and the digest of its case files.
//!
//! The lock is committed next to the set. A case edited under an unchanged version no longer
//! matches it, so a set cannot change without its version being raised.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use super::set::EvalSet;
use crate::skills;

/// The lock file of an eval set.
pub const LOCK_FILE: &str = "set.lock";

/// The content digest of an eval set's case files: the hex SHA-256 over them sorted by path,
/// encoded as a skill's files are.
#[must_use]
pub fn digest(files: &BTreeMap<String, String>) -> String {
    skills::digest(files)
}

/// A set's version with the digest of its cases at that version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lock {
    /// The set's version.
    pub version: u32,
    /// The digest of its case files.
    pub digest: String,
}

/// Why a set does not match its lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockError {
    /// The cases changed while the version stayed.
    Changed {
        /// The version both declare.
        version: u32,
        /// The digest the lock holds.
        locked: String,
        /// The digest of the cases as they are.
        actual: String,
    },
    /// The set's version differs from the lock's.
    NotRenewed {
        /// The set's declared version.
        declared: u32,
        /// The lock's version.
        locked: u32,
    },
}

impl fmt::Display for LockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Changed {
                version,
                locked,
                actual,
            } => write!(
                f,
                "the eval set's cases changed under version {version} (locked {locked}, now \
                 {actual}): raise the version in set.yaml, then renew the lock"
            ),
            Self::NotRenewed { declared, locked } => write!(
                f,
                "the eval set declares version {declared} but its lock is for version {locked}: \
                 renew the lock"
            ),
        }
    }
}

impl std::error::Error for LockError {}

impl Lock {
    /// The lock `set` would have now.
    #[must_use]
    pub fn of(set: &EvalSet) -> Self {
        Self {
            version: set.manifest.version,
            digest: digest(&set.files),
        }
    }

    /// Parse a lock file.
    ///
    /// # Errors
    /// A message when the text is not a lock.
    pub fn parse(text: &str) -> Result<Self, String> {
        serde_saphyr::from_str(text).map_err(|why| format!("{LOCK_FILE}: {why}"))
    }

    /// The lock file's text.
    #[must_use]
    pub fn to_text(&self) -> String {
        format!("version: {}\ndigest: {}\n", self.version, self.digest)
    }

    /// Check that `set` is the one this lock was written for.
    ///
    /// # Errors
    /// [`LockError`] when the cases changed under the locked version, or the version changed
    /// and the lock was not renewed.
    pub fn check(&self, set: &EvalSet) -> Result<(), LockError> {
        let actual = Self::of(set);
        if actual.version != self.version {
            return Err(LockError::NotRenewed {
                declared: actual.version,
                locked: self.version,
            });
        }
        if actual.digest != self.digest {
            return Err(LockError::Changed {
                version: self.version,
                locked: self.digest.clone(),
                actual: actual.digest,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::super::set::tests::{MANIFEST, case};
    use super::*;

    const PLAN: &str = "expect: { outcome: plan }\n";

    fn set(manifest: &str, files: &[(String, String)]) -> EvalSet {
        EvalSet::parse(manifest, files.iter().cloned().collect()).expect("set")
    }

    #[test]
    fn the_digest_ignores_file_order_and_follows_paths_and_bytes() {
        let (a, b) = (case("a", "late", PLAN), case("b", "late", PLAN));
        let forward: BTreeMap<_, _> = [a.clone(), b.clone()].into_iter().collect();
        let backward: BTreeMap<_, _> = [b.clone(), a.clone()].into_iter().collect();
        assert_eq!(digest(&forward), digest(&backward));
        assert_eq!(digest(&forward).len(), 64);

        let renamed: BTreeMap<_, _> = [("cases/c.yaml".to_owned(), a.1.clone()), b.clone()]
            .into_iter()
            .collect();
        assert_ne!(digest(&forward), digest(&renamed));
        let edited: BTreeMap<_, _> = [(a.0.clone(), format!("{} ", a.1)), b]
            .into_iter()
            .collect();
        assert_ne!(digest(&forward), digest(&edited));
    }

    #[test]
    fn a_lock_round_trips_and_accepts_its_own_set() {
        let set = set(MANIFEST, &[case("a", "late", PLAN)]);
        let lock = Lock::parse(&Lock::of(&set).to_text()).expect("lock");
        assert_eq!(lock, Lock::of(&set));
        lock.check(&set).expect("matches");
    }

    #[test]
    fn a_case_edited_under_the_same_version_is_refused_with_both_digests() {
        let lock = Lock::of(&set(MANIFEST, &[case("a", "late", PLAN)]));
        let (path, content) = case("a", "late", PLAN);
        let edited = set(MANIFEST, &[(path, content.replace("Why.", "Because."))]);
        let error = lock.check(&edited).expect_err("changed");
        let LockError::Changed { locked, actual, .. } = &error else {
            panic!("{error}");
        };
        assert_eq!(locked, &lock.digest);
        assert_ne!(locked, actual);
        assert!(error.to_string().contains("raise the version"), "{error}");
    }

    #[test]
    fn a_raised_version_needs_a_renewed_lock() {
        let files = [case("a", "late", PLAN)];
        let lock = Lock::of(&set(MANIFEST, &files));
        let raised = set(&MANIFEST.replace("version: 1", "version: 2"), &files);
        assert_eq!(
            lock.check(&raised),
            Err(LockError::NotRenewed {
                declared: 2,
                locked: 1
            })
        );
        Lock::of(&raised).check(&raised).expect("renewed");
    }
}
