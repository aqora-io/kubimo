use kubimo::{Runner, prelude::*};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

#[inline]
fn ingress_path_from_name(name: &str) -> String {
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
