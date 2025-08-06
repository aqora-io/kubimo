use std::{fmt, str::FromStr};

use kube::core::object::HasSpec;
use rand::seq::IndexedRandom;
use thiserror::Error;

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

fn calc_word_num(mut bits: usize) -> usize {
    const NOUNS_LEN: u128 = names::NOUNS.len() as u128;
    const ADJECTIVES_LEN: u128 = names::ADJECTIVES.len() as u128;
    let mut target = 1u128;
    while bits > 1 {
        target <<= 1;
        target |= 1;
        bits -= 1;
    }
    target /= NOUNS_LEN;
    let mut len = 1;
    while target > 0 {
        target /= ADJECTIVES_LEN;
        len += 1;
    }
    len
}

const NAME_BITS: usize = u32::BITS as usize;
lazy_static::lazy_static! {
    static ref NAME_LEN: usize = calc_word_num(NAME_BITS);
}

fn gen_name(rng: &mut impl rand::Rng, len: usize) -> String {
    let noun = names::NOUNS.choose(rng).unwrap();
    (1..len)
        .map(|_| names::ADJECTIVES.choose(rng).unwrap())
        .chain(std::iter::once(noun))
        .copied()
        .collect::<Vec<_>>()
        .join("-")
}

pub fn rand_name() -> String {
    gen_name(&mut rand::rng(), *NAME_LEN)
}

pub trait ResourceFactory: HasSpec {
    fn new(name: &str, spec: Self::Spec) -> Self;
}

pub trait ResourceFactoryExt: ResourceFactory + Sized {
    fn create(spec: Self::Spec) -> Self {
        <Self as ResourceFactory>::new(&rand_name(), spec)
    }
}

impl<T> ResourceFactoryExt for T where T: ResourceFactory {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_id_display() {
        for _ in 0..100 {
            let name = rand_name();
            let id = Id::Workspace(name.clone());
            let string = format!("kubimo-ws-{name}");
            assert_eq!(id.to_string(), string);
            assert_eq!(string.parse::<Id>().expect("Failed to parse Uid"), id)
        }
    }

    #[test]
    fn assert_name_len() {
        // this can change if the names library changes, so we assert it here
        // and handle it in the future if necessary
        assert_eq!(*NAME_LEN, 4);
    }

    #[test]
    fn test_gen_name() {
        const K8S_MAX_NAME_SIZE: usize = 253;
        for _ in 0..100 {
            let name = rand_name();
            // some adjectives have hyphens, so we need to ensure that the name has at least 3 hyphens
            assert!(name.matches('-').count() >= *NAME_LEN - 1);
            assert!(name.len() <= K8S_MAX_NAME_SIZE);
        }
    }
}
