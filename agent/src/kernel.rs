//! Kernel version gating.
//!
//! A shared node volume removes the per-workspace filesystem boundary that a
//! PVC per workspace would provide, so kernel filesystem bugs become
//! cross-tenant.
//! [CVE-2026-64600 ("RefluXFS")](https://www.openwall.com/lists/oss-security/2026/07/22/14)
//! is the concrete example the design was reviewed against: an unprivileged
//! local user can reflink-clone a readable file and race `O_DIRECT` writes to
//! overwrite the physical blocks behind it. Its preconditions are exactly this
//! design — XFS with `reflink=1`, plus local unprivileged write access.
//!
//! The patched version is distro-specific (upstream commit `2f4acd0`, backported
//! under each vendor's own numbering), so the operator supplies the minimum
//! rather than the agent guessing it.

use std::cmp::Ordering;

#[derive(Debug, thiserror::Error)]
pub enum KernelError {
    #[error("could not read the kernel release: {0}")]
    Read(#[from] rustix::io::Errno),
    #[error("could not parse kernel release {0:?}")]
    Parse(String),
}

/// The numeric prefix of a kernel release, e.g. `6.8.0-124` from
/// `6.8.0-124-generic`.
///
/// Compared component-wise so `6.8.0-99` sorts before `6.8.0-124`, which a
/// plain string comparison gets wrong.
#[derive(Debug, Clone)]
pub struct KernelVersion {
    /// The string this was parsed from, kept verbatim for display. Spelling the
    /// components back out would print `6.8.0.124` — a release that exists
    /// nowhere, and that an operator cannot match against `uname -r`.
    release: String,
    components: Vec<u64>,
}

impl std::fmt::Display for KernelVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.release)
    }
}

// Equality is defined by `cmp`, not by the underlying Vec: `Ord` treats absent
// trailing components as zero, so `6.8` and `6.8.0` compare Equal and must
// therefore also be equal. Deriving `PartialEq` breaks that contract.
impl PartialEq for KernelVersion {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for KernelVersion {}

impl KernelVersion {
    pub fn parse(release: &str) -> Result<Self, KernelError> {
        let numeric: Vec<u64> = release
            .split(['.', '-'])
            .take_while(|part| part.chars().all(|ch| ch.is_ascii_digit()) && !part.is_empty())
            .filter_map(|part| part.parse().ok())
            .collect();
        if numeric.is_empty() {
            return Err(KernelError::Parse(release.to_string()));
        }
        Ok(Self {
            release: release.to_string(),
            components: numeric,
        })
    }

    pub fn current() -> Result<Self, KernelError> {
        let uname = rustix::system::uname();
        Self::parse(&uname.release().to_string_lossy())
    }
}

impl PartialOrd for KernelVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for KernelVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        // Missing trailing components count as zero, so `6.8` < `6.8.1`.
        let len = self.components.len().max(other.components.len());
        for index in 0..len {
            let ours = self.components.get(index).copied().unwrap_or(0);
            let theirs = other.components.get(index).copied().unwrap_or(0);
            match ours.cmp(&theirs) {
                Ordering::Equal => continue,
                other => return other,
            }
        }
        Ordering::Equal
    }
}

/// Refuse to serve slots on a kernel older than `minimum`.
///
/// Returns the running version so the caller can log it either way.
pub fn require_at_least(minimum: &str) -> Result<KernelVersion, String> {
    let minimum = KernelVersion::parse(minimum)
        .map_err(|err| format!("invalid --min-kernel-version: {err}"))?;
    let current = KernelVersion::current().map_err(|err| err.to_string())?;
    if current < minimum {
        return Err(format!(
            "kernel {current} is older than the required {minimum}. A shared node volume \
             makes kernel filesystem bugs cross-tenant (see CVE-2026-64600); patch the node or \
             pass --allow-unpatched-kernel to accept the risk."
        ));
    }
    Ok(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_distro_release_string() {
        assert_eq!(
            KernelVersion::parse("6.8.0-124-generic")
                .unwrap()
                .components,
            vec![6, 8, 0, 124]
        );
        assert_eq!(
            KernelVersion::parse("6.12.5").unwrap().components,
            vec![6, 12, 5]
        );
    }

    /// The rendered version is read against `uname -r`, so it has to be the
    /// release string itself: neither the components spelled back out
    /// (`6.8.0.124`, a release that exists nowhere) nor the derived `Debug`
    /// (`KernelVersion { .. }`) is something an operator can act on.
    #[test]
    fn renders_the_release_it_was_parsed_from() {
        assert_eq!(
            KernelVersion::parse("6.8.0-124-generic")
                .unwrap()
                .to_string(),
            "6.8.0-124-generic"
        );
        // ...including in the refusal, which is the one an operator sees.
        let err = require_at_least("99.0.0").unwrap_err();
        assert!(err.contains("required 99.0.0"), "got: {err}");
    }

    /// The reason for component-wise comparison: as strings, "6.8.0-99" sorts
    /// *after* "6.8.0-124", which would let an unpatched node through.
    #[test]
    fn compares_numerically_not_lexically() {
        let older = KernelVersion::parse("6.8.0-99").unwrap();
        let newer = KernelVersion::parse("6.8.0-124").unwrap();
        assert!(older < newer);
        assert!("6.8.0-99" > "6.8.0-124", "the lexical trap this avoids");
    }

    #[test]
    fn missing_components_count_as_zero() {
        assert!(KernelVersion::parse("6.8").unwrap() < KernelVersion::parse("6.8.1").unwrap());
        assert_eq!(
            KernelVersion::parse("6.8").unwrap(),
            KernelVersion::parse("6.8.0").unwrap()
        );
    }

    #[test]
    fn rejects_unparseable_releases() {
        assert!(KernelVersion::parse("").is_err());
        assert!(KernelVersion::parse("not-a-version").is_err());
    }

    #[test]
    fn the_running_kernel_parses() {
        assert!(KernelVersion::current().is_ok());
    }

    #[test]
    fn require_at_least_rejects_an_older_kernel() {
        // Far-future minimum: whatever is running must be older.
        let err = require_at_least("99.0.0").unwrap_err();
        assert!(err.contains("CVE-2026-64600"), "got: {err}");
    }

    #[test]
    fn require_at_least_accepts_a_new_enough_kernel() {
        assert!(require_at_least("0.0.1").is_ok());
    }
}
