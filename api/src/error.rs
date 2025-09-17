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
    #[error(transparent)]
    ClientBuildError(#[from] ClientBuildError),
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
