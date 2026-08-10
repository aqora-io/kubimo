//! What makes a workspace file secret, and how the root `.env` is read and
//! written.
//!
//! Secret paths are diverted out of the normal upload pipeline — never a
//! content object, never a `WorkspaceDirectory` entry, never in the manifest's
//! `directories` — and travel instead as the archive's `secrets.json`, whose
//! values a restore may withhold.

use std::path::Path;

use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// Workspace-root pattern file marking additional files as secret, gitignore
/// syntax. A tracked, public file — like `.gitignore` — so clones inherit the
/// marking; values belong in `.env`, never in here (see
/// [`warn_on_assignment_looking_lines`]).
pub const SECRETS_PATTERN_FILE: &str = ".secrets";

/// The dotenv file marimo's secrets panel writes, always secret. The *root*
/// `.env` is exported as KEY/VALUE pairs; any nested `.env` is an ordinary
/// secret file.
pub const DOTENV_FILE_NAME: &str = ".env";

/// Ceiling on a secret file's inline content in `secrets.json`. The object is
/// fetched whole into memory on every `Values` restore and rewritten every
/// sync cycle; secret files are keys and certs measured in kilobytes. An
/// over-cap file stays excluded from the archive but is not value-restorable.
pub const MAX_INLINE_SECRET_FILE_SIZE: u64 = 1024 * 1024;

/// Build the cycle's secret matcher: the built-in `.env` pattern plus the
/// workspace's [`SECRETS_PATTERN_FILE`] when present. A malformed pattern file
/// costs its bad lines a warning, never the cycle — the built-in pattern must
/// keep matching regardless.
pub fn build_matcher(root: &Path) -> Gitignore {
    let mut builder = GitignoreBuilder::new(root);
    builder
        .add_line(None, DOTENV_FILE_NAME)
        .expect("the built-in .env pattern is valid");
    let pattern_file = root.join(SECRETS_PATTERN_FILE);
    if pattern_file.exists() {
        // `add` keeps the valid lines and reports the rest.
        if let Some(err) = builder.add(&pattern_file) {
            tracing::warn!("Malformed lines in {SECRETS_PATTERN_FILE}: {err}");
        }
        if let Ok(content) = std::fs::read_to_string(&pattern_file) {
            warn_on_assignment_looking_lines(&content);
        }
    }
    match builder.build() {
        Ok(matcher) => matcher,
        Err(err) => {
            tracing::warn!("Could not build the secret matcher from {SECRETS_PATTERN_FILE}: {err}");
            let mut builder = GitignoreBuilder::new(root);
            builder
                .add_line(None, DOTENV_FILE_NAME)
                .expect("the built-in .env pattern is valid");
            builder
                .build()
                .expect("the built-in-only matcher always builds")
        }
    }
}

/// Whether `path` (relative to the matcher's root) is secret. Negations in the
/// pattern file whitelist as in gitignore, but never the built-in `.env`
/// pattern — the pattern file only ever *adds* secrets.
pub fn is_secret(matcher: &Gitignore, path: &Path, is_dir: bool) -> bool {
    if path
        .file_name()
        .is_some_and(|name| name == DOTENV_FILE_NAME)
    {
        return true;
    }
    matcher
        .matched_path_or_any_parents(path, is_dir)
        .is_ignore()
}

/// The pattern file is public; a user who mistakes it for a place to put
/// values publishes them. Warn on anything shaped like an assignment.
fn warn_on_assignment_looking_lines(content: &str) {
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, _)) = line.split_once('=')
            && !key.is_empty()
            && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            tracing::warn!(
                "{SECRETS_PATTERN_FILE} contains what looks like an assignment ({key}=…); \
                 it is a *public* pattern file — put secret values in .env instead"
            );
            return;
        }
    }
}

/// Parse dotenv content into KEY/VALUE pairs, duplicate keys deduped with the
/// last value winning, first-seen order.
///
/// Hand-rolled rather than a dotenv crate: dotenvy substitutes `$VAR` from the
/// *process* environment while parsing, which would bake the indexer pod's env
/// into archived values. The grammar inverts marimo's writer and fallback
/// parser (`marimo/_secrets/env_provider.py`, `load_dotenv.py`): skip blanks
/// and `#` comments, split on the first `=`, strip one matching pair of outer
/// quotes, and unescape `\"` in double-quoted values (the writer's only
/// escape).
pub fn parse_dotenv(content: &str) -> Vec<(String, String)> {
    let mut entries: Vec<(String, String)> = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // python-dotenv accepts an `export ` prefix, so the kernel would load
        // such a line; parse it the same way.
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || key.chars().any(char::is_whitespace) {
            continue;
        }
        let value = unquote(value.trim());
        match entries.iter_mut().find(|(existing, _)| existing == key) {
            Some((_, existing)) => *existing = value,
            None => entries.push((key.to_string(), value)),
        }
    }
    entries
}

fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        match (bytes[0], bytes[bytes.len() - 1]) {
            (b'"', b'"') => return value[1..value.len() - 1].replace("\\\"", "\""),
            (b'\'', b'\'') => return value[1..value.len() - 1].to_string(),
            _ => {}
        }
    }
    value.to_string()
}

/// Render KEY/VALUE pairs exactly as marimo's writer does — `KEY="value"`,
/// with `"` escaped as `\"` — so a `Values` restore round-trips what the
/// secrets panel wrote.
pub fn render_dotenv<'a>(entries: impl IntoIterator<Item = (&'a str, &'a str)>) -> String {
    let mut out = String::new();
    for (key, value) in entries {
        out.push_str(key);
        out.push_str("=\"");
        out.push_str(&value.replace('"', "\\\""));
        out.push_str("\"\n");
    }
    out
}

/// The names-only `.env`: every key present with an empty value, so marimo's
/// secrets panel shows what the notebook needs without carrying the values.
pub fn render_placeholders<'a>(keys: impl IntoIterator<Item = &'a str>) -> String {
    render_dotenv(keys.into_iter().map(|key| (key, "")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matcher_with(dir: &Path, patterns: Option<&str>) -> Gitignore {
        if let Some(patterns) = patterns {
            std::fs::write(dir.join(SECRETS_PATTERN_FILE), patterns).unwrap();
        }
        build_matcher(dir)
    }

    #[test]
    fn dotenv_is_secret_at_any_depth_without_a_pattern_file() {
        let dir = tempfile::tempdir().unwrap();
        let matcher = matcher_with(dir.path(), None);
        assert!(is_secret(&matcher, Path::new(".env"), false));
        assert!(is_secret(&matcher, Path::new("sub/.env"), false));
        assert!(!is_secret(&matcher, Path::new("notebook.py"), false));
        assert!(!is_secret(&matcher, Path::new(".envrc"), false));
        assert!(!is_secret(&matcher, Path::new(SECRETS_PATTERN_FILE), false));
    }

    #[test]
    fn pattern_file_marks_files_and_directories() {
        let dir = tempfile::tempdir().unwrap();
        let matcher = matcher_with(dir.path(), Some("*.pem\ncreds/\n"));
        assert!(is_secret(&matcher, Path::new("key.pem"), false));
        assert!(is_secret(&matcher, Path::new("sub/key.pem"), false));
        // Children of a matched directory are secret via the parent.
        assert!(is_secret(&matcher, Path::new("creds/token.txt"), false));
        assert!(!is_secret(&matcher, Path::new("notebook.py"), false));
    }

    #[test]
    fn negations_whitelist_but_never_the_dotenv() {
        let dir = tempfile::tempdir().unwrap();
        let matcher = matcher_with(dir.path(), Some("*.pem\n!public.pem\n!.env\n"));
        assert!(is_secret(&matcher, Path::new("key.pem"), false));
        assert!(!is_secret(&matcher, Path::new("public.pem"), false));
        assert!(is_secret(&matcher, Path::new(".env"), false));
    }

    #[test]
    fn malformed_pattern_file_keeps_the_builtin() {
        let dir = tempfile::tempdir().unwrap();
        // `**foo**bar**` style lines are invalid globs; the cycle must survive
        // and `.env` must stay secret.
        let matcher = matcher_with(dir.path(), Some("a**b**c\n*.pem\n"));
        assert!(is_secret(&matcher, Path::new(".env"), false));
        assert!(is_secret(&matcher, Path::new("key.pem"), false));
    }

    #[test]
    fn parse_dotenv_inverts_marimos_writer() {
        // Exactly what `DotEnvSecretsProvider.write_key` produces.
        let content = "A=\"one\"\nB=\"say \\\"hi\\\"\"\n";
        assert_eq!(
            parse_dotenv(content),
            vec![
                ("A".to_string(), "one".to_string()),
                ("B".to_string(), "say \"hi\"".to_string()),
            ]
        );
    }

    #[test]
    fn parse_dotenv_handles_hand_written_files() {
        let content = r#"
# comment
export A=exported
B='single quoted'
C=bare
C=last wins
NOT A KEY=skipped
=skipped
also-skipped
D=
"#;
        assert_eq!(
            parse_dotenv(content),
            vec![
                ("A".to_string(), "exported".to_string()),
                ("B".to_string(), "single quoted".to_string()),
                ("C".to_string(), "last wins".to_string()),
                ("D".to_string(), "".to_string()),
            ]
        );
    }

    #[test]
    fn render_parse_round_trips() {
        let entries = vec![
            ("KEY".to_string(), "value".to_string()),
            ("QUOTED".to_string(), "say \"hi\"".to_string()),
            ("EMPTY".to_string(), "".to_string()),
        ];
        let rendered = render_dotenv(entries.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        assert_eq!(parse_dotenv(&rendered), entries);
    }

    #[test]
    fn placeholders_keep_the_keys_visible() {
        let rendered = render_placeholders(["A", "B"]);
        assert_eq!(rendered, "A=\"\"\nB=\"\"\n");
        assert_eq!(
            parse_dotenv(&rendered),
            vec![
                ("A".to_string(), "".to_string()),
                ("B".to_string(), "".to_string()),
            ]
        );
    }
}
