//! Slot identity and directory layout on a node's data volume.
//!
//! A *slot* is one workspace's working directory, carved out of the shared
//! per-node volume and handed to a runner pod as a bind mount. Slot ids are
//! generated here and nowhere else: they end up as a path component under
//! `/data/slots/`, so a client-chosen id would be a path-traversal primitive
//! straight into another tenant's files.

use std::fmt;
use std::path::{Path, PathBuf};

use rand::Rng;

/// Length of the random part of a slot id, in base32 characters.
const SLOT_RANDOM_LEN: usize = 26;
const SLOT_PREFIX: &str = "slot-";
/// Crockford-ish lowercase alphabet: no vowels-to-digit confusion, and safe as
/// both a path component and a Kubernetes name fragment.
const SLOT_ALPHABET: &[u8] = b"0123456789abcdefghjkmnpqrstvwxyz";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SlotIdError {
    #[error("slot id must start with {SLOT_PREFIX:?}")]
    MissingPrefix,
    #[error("slot id must be {expected} characters, got {actual}")]
    BadLength { expected: usize, actual: usize },
    #[error("slot id contains an illegal character {0:?}")]
    IllegalCharacter(char),
}

/// A validated slot identifier.
///
/// The only ways to obtain one are [`SlotId::generate`] and [`SlotId::parse`],
/// so an unvalidated string can never reach the filesystem layer.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SlotId(String);

impl SlotId {
    /// Mint a fresh slot id. Slots are never recycled in place — a rebind gets
    /// a new id — so that one tenant cannot inherit another's leftover
    /// `.env`, `.git-credentials` or shell history.
    pub fn generate() -> Self {
        let mut rng = rand::rng();
        let random: String = (0..SLOT_RANDOM_LEN)
            .map(|_| SLOT_ALPHABET[rng.random_range(0..SLOT_ALPHABET.len())] as char)
            .collect();
        Self(format!("{SLOT_PREFIX}{random}"))
    }

    /// Validate an id that came from outside this process (a CSI request, a CR
    /// status field, a directory listing).
    pub fn parse(raw: &str) -> Result<Self, SlotIdError> {
        let Some(random) = raw.strip_prefix(SLOT_PREFIX) else {
            return Err(SlotIdError::MissingPrefix);
        };
        if random.len() != SLOT_RANDOM_LEN {
            return Err(SlotIdError::BadLength {
                expected: SLOT_RANDOM_LEN,
                actual: random.len(),
            });
        }
        if let Some(bad) = random
            .chars()
            .find(|ch| !SLOT_ALPHABET.contains(&(*ch as u8)))
        {
            return Err(SlotIdError::IllegalCharacter(bad));
        }
        Ok(Self(raw.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SlotId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Directory layout of a node data volume.
#[derive(Clone, Debug)]
pub struct SlotLayout {
    root: PathBuf,
}

impl SlotLayout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<root>/slots` — parent of every slot directory.
    pub fn slots_dir(&self) -> PathBuf {
        self.root.join("slots")
    }

    /// `<root>/slots/<id>`.
    ///
    /// Safe by construction: [`SlotId`] cannot contain `/`, `.` or `..`, so
    /// this always yields a direct child of `slots_dir`.
    pub fn slot_dir(&self, id: &SlotId) -> PathBuf {
        self.slots_dir().join(id.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_round_trip_through_parse() {
        for _ in 0..100 {
            let id = SlotId::generate();
            assert_eq!(SlotId::parse(id.as_str()), Ok(id));
        }
    }

    #[test]
    fn generated_ids_are_distinct() {
        let ids: std::collections::HashSet<_> = (0..1000).map(|_| SlotId::generate().0).collect();
        assert_eq!(ids.len(), 1000, "slot ids collided");
    }

    /// The security property: nothing that could escape `slots/` may parse.
    #[test]
    fn parse_rejects_traversal_and_separators() {
        for raw in [
            "slot-../../../etc/passwd",
            "slot-..",
            "slot-/etc/passwd",
            "../slot-aaaaaaaaaaaaaaaaaaaaaaaaaa",
            "slot-aaaaaaaaaaaa/aaaaaaaaaaaaa",
            "slot-aaaaaaaaaaaa\0aaaaaaaaaaaaa",
        ] {
            assert!(SlotId::parse(raw).is_err(), "accepted {raw:?}");
        }
    }

    #[test]
    fn parse_rejects_missing_prefix_and_wrong_length() {
        assert_eq!(SlotId::parse("aaaa"), Err(SlotIdError::MissingPrefix));
        assert!(matches!(
            SlotId::parse("slot-abc"),
            Err(SlotIdError::BadLength { .. })
        ));
    }

    /// Uppercase is excluded so a case-insensitive filesystem can never alias
    /// two distinct slots onto one directory.
    #[test]
    fn parse_rejects_uppercase_and_ambiguous_characters() {
        let upper = format!("{SLOT_PREFIX}{}", "A".repeat(SLOT_RANDOM_LEN));
        assert!(matches!(
            SlotId::parse(&upper),
            Err(SlotIdError::IllegalCharacter('A'))
        ));
        // 'i', 'l', 'o' and 'u' are deliberately absent from the alphabet.
        for ch in ['i', 'l', 'o', 'u'] {
            let raw = format!("{SLOT_PREFIX}{}", ch.to_string().repeat(SLOT_RANDOM_LEN));
            assert!(matches!(
                SlotId::parse(&raw),
                Err(SlotIdError::IllegalCharacter(_)),
            ));
        }
    }

    #[test]
    fn slot_dir_is_always_a_direct_child_of_slots_dir() {
        let layout = SlotLayout::new("/data");
        let id = SlotId::generate();
        let dir = layout.slot_dir(&id);
        assert_eq!(dir.parent(), Some(layout.slots_dir().as_path()));
        assert_eq!(
            dir.strip_prefix(layout.slots_dir()).unwrap(),
            Path::new(id.as_str())
        );
    }
}
