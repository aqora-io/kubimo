use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Kube(#[from] kube::Error),
    #[error("Object metadata is missing: {0}")]
    ObjectMetaMissing(&'static str),
    #[error(transparent)]
    Watch(#[from] kube::runtime::watcher::Error),
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
    #[error("Expected a value of type {0} but found {1}")]
    ExpectedType(&'static str, &'static str),
}

impl Error {
    pub(crate) fn expected_json_type(expected: &'static str, found: &serde_json::Value) -> Self {
        match found {
            serde_json::Value::Null => Self::ExpectedType(expected, "null"),
            serde_json::Value::Bool(_) => Self::ExpectedType(expected, "bool"),
            serde_json::Value::Number(_) => Self::ExpectedType(expected, "number"),
            serde_json::Value::String(_) => Self::ExpectedType(expected, "string"),
            serde_json::Value::Array(_) => Self::ExpectedType(expected, "array"),
            serde_json::Value::Object(_) => Self::ExpectedType(expected, "object"),
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum ClientBuildError {
    #[error(transparent)]
    Config(#[from] kube::config::InferConfigError),
    #[error(transparent)]
    Kube(#[from] kube::Error),
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct StatusError {
    pub message: String,
    pub status: Option<String>,
    pub reason: Option<String>,
    pub code: Option<u16>,
}

impl StatusError {
    pub fn new(message: impl ToString) -> Self {
        Self {
            message: message.to_string(),
            status: None,
            reason: None,
            code: None,
        }
    }
}

impl From<&kube::error::ErrorResponse> for StatusError {
    fn from(err: &kube::error::ErrorResponse) -> Self {
        Self {
            message: err.message.clone(),
            status: Some(err.status.clone()),
            reason: Some(err.reason.clone()),
            code: Some(err.code),
        }
    }
}

impl From<&kube::error::Error> for StatusError {
    fn from(err: &kube::error::Error) -> Self {
        match err {
            kube::error::Error::Api(e) => e.into(),
            _ => Self {
                message: err.to_string(),
                status: None,
                reason: None,
                code: None,
            },
        }
    }
}

impl From<&Error> for StatusError {
    fn from(err: &Error) -> Self {
        match err {
            Error::Kube(err) => err.into(),
            _ => Self {
                message: err.to_string(),
                status: None,
                reason: None,
                code: None,
            },
        }
    }
}
