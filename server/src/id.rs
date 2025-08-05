use std::{fmt, str::FromStr};

use rand::seq::IndexedRandom;
use thiserror::Error;

pub fn gen_name(len: usize) -> String {
    let mut rng = rand::rng();
    let noun = names::NOUNS.choose(&mut rng).unwrap();
    (1..len)
        .map(|_| names::ADJECTIVES.choose(&mut rng).unwrap())
        .chain(std::iter::once(noun))
        .copied()
        .collect::<Vec<_>>()
        .join("-")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Id {
    Workspace(String),
}

impl Id {
    pub fn name(&self) -> &str {
        match self {
            Self::Workspace(name) => name,
        }
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Workspace(name) => write!(f, "kubimo-ws-{name}"),
        }
    }
}

#[derive(Debug, Error)]
pub enum ParseIdError {
    #[error("Invalid prefix for ID: {0:?}")]
    InvalidPrefix(Option<String>),
    #[error("Invalid resource for ID: {0:?}")]
    InvalidResource(Option<String>),
}

impl FromStr for Id {
    type Err = ParseIdError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some((prefix, rest)) = s.split_once('-') {
            if prefix != "kubimo" {
                Err(ParseIdError::InvalidPrefix(Some(prefix.to_string())))
            } else if let Some((resource, name)) = rest.split_once('-') {
                match resource {
                    "ws" => Ok(Id::Workspace(name.to_string())),
                    _ => Err(ParseIdError::InvalidResource(Some(resource.to_string()))),
                }
            } else {
                Err(ParseIdError::InvalidResource(None))
            }
        } else {
            Err(ParseIdError::InvalidPrefix(None))
        }
    }
}

#[async_graphql::Scalar(name = "ID")]
impl async_graphql::ScalarType for Id {
    fn parse(value: async_graphql::Value) -> async_graphql::InputValueResult<Self> {
        match value {
            async_graphql::Value::String(s) => Ok(s.parse()?),
            _ => Err(async_graphql::InputValueError::expected_type(value)),
        }
    }

    fn to_value(&self) -> async_graphql::Value {
        async_graphql::Value::String(self.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_id_display() {
        let name = gen_name(3);
        let id = Id::Workspace(name.clone());
        let string = format!("kubimo-ws-{name}");
        assert_eq!(id.to_string(), string);
        assert_eq!(string.parse::<Id>().expect("Failed to parse Uid"), id)
    }
}
