use std::str::FromStr;

use object_store::path::{Error as ObjectPathError, Path};
use thiserror::Error;
use url::Url;

#[derive(Error, Debug)]
pub enum S3UrlParseError {
    #[error("Invalid url: {0}")]
    Url(#[from] url::ParseError),
    #[error("Missing bucket name")]
    MissingBucket,
    #[error("Invalid scheme {0}")]
    InvalidScheme(String),
    #[error("{0} ({1}) not supported")]
    PartNotSupported(&'static str, String),
    #[error("Invalid object path: {0}")]
    Path(#[from] ObjectPathError),
}

#[derive(Debug, Clone)]
pub struct S3Url {
    pub bucket: String,
    pub path: Path,
}

impl S3Url {
    pub fn parse(s: &str) -> Result<S3Url, S3UrlParseError> {
        let url = Url::parse(s)?;
        Self::parse_from_url(&url)
    }

    pub fn parse_from_url(url: &Url) -> Result<S3Url, S3UrlParseError> {
        if !matches!(url.scheme(), "s3") {
            return Err(S3UrlParseError::InvalidScheme(url.scheme().to_string()));
        }
        if let Some(port) = url.port() {
            return Err(S3UrlParseError::PartNotSupported("port", port.to_string()));
        }
        if let Some(query) = url.query() {
            return Err(S3UrlParseError::PartNotSupported(
                "query",
                query.to_string(),
            ));
        }
        if let Some(fragment) = url.fragment() {
            return Err(S3UrlParseError::PartNotSupported(
                "fragment",
                fragment.to_string(),
            ));
        }
        if !url.username().is_empty() {
            return Err(S3UrlParseError::PartNotSupported(
                "username",
                url.username().to_string(),
            ));
        }
        if let Some(password) = url.password() {
            return Err(S3UrlParseError::PartNotSupported(
                "password",
                password.to_string(),
            ));
        }
        let bucket = url
            .host()
            .ok_or(S3UrlParseError::MissingBucket)?
            .to_string();
        let mut path = url.path();
        if path.starts_with('/') {
            path = &path[1..];
        }
        let path = Path::from_url_path(path)?;
        Ok(S3Url { bucket, path })
    }
}

impl FromStr for S3Url {
    type Err = S3UrlParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        S3Url::parse(s)
    }
}
