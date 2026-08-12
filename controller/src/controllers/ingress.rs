use kubimo::{Runner, prelude::*};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

#[inline]
pub(crate) fn ingress_path_from_name(name: &str) -> String {
    const ASCII_SET: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'~');
    format!("/{}", utf8_percent_encode(name, ASCII_SET))
}

pub fn ingress_path(runner: &Runner) -> kubimo::Result<String> {
    if let Some(path) = runner
        .spec
        .ingress
        .as_ref()
        .and_then(|ingress| ingress.path.as_ref())
    {
        Ok(path.clone())
    } else {
        Ok(ingress_path_from_name(runner.name()?))
    }
}

/// The path this runner is actually served under.
///
/// A claimed warm pod serves the base-url minted at its birth — marimo cannot
/// change it once booted — so the claim recorded in status overrides whatever
/// the spec asked for. Everything that routes or polls a live runner (the
/// Ingress, the status check) must use this, not [`ingress_path`].
pub fn effective_ingress_path(runner: &Runner) -> kubimo::Result<String> {
    if let Some(claim) = runner.status.as_ref().and_then(|s| s.claim.as_ref()) {
        return Ok(claim.ingress_path.clone());
    }
    ingress_path(runner)
}
