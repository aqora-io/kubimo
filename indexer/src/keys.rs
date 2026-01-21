use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::PathBuf;

use kubimo::url::{ParseError as UrlParseError, Url};
use rand::distr::{Distribution, StandardUniform};
use thiserror::Error;

const MAX_KEY_GENERATION_TRIES: usize = 1000000;
const BASE32_ALPHABET: base32::Alphabet = base32::Alphabet::Crockford;

pub struct WorkspaceDirNameSet {
    key: WorkspaceDirKey,
    set: KeySet<PathBuf, u32>,
}

impl WorkspaceDirNameSet {
    pub fn new(name: String) -> Self {
        WorkspaceDirNameSet {
            key: WorkspaceDirKey::new(name),
            set: KeySet::new(),
        }
    }

    pub fn insert(&mut self, item: PathBuf, key: &str) -> Result<(), InvalidKey> {
        let parsed_key = self.key.parse(key)?;
        self.set.insert(item, parsed_key);
        Ok(())
    }

    pub fn get_or_insert(&mut self, item: PathBuf) -> String {
        let key = self.set.get_or_insert(item);
        self.key.format(key)
    }
}

pub struct WorkspaceFileUrlSet {
    key: WorkspaceFileKey,
    set: KeySet<PathBuf, u64>,
}

impl WorkspaceFileUrlSet {
    pub fn new(bucket: String, prefix: Option<String>) -> Result<Self, UrlParseError> {
        Ok(WorkspaceFileUrlSet {
            key: WorkspaceFileKey::new(bucket, prefix)?,
            set: KeySet::new(),
        })
    }

    pub fn insert(&mut self, item: PathBuf, url: &Url) -> Result<(), InvalidKey> {
        let (parsed_key, format) = self.key.parse(url)?;
        if Some(OsStr::new(&format)) != item.extension() {
            return Err(InvalidKey::BadFormat);
        }
        self.set.insert(item, parsed_key);
        Ok(())
    }

    pub fn get_or_insert(&mut self, item: PathBuf) -> Result<Url, UrlParseError> {
        let format = item
            .extension()
            .and_then(OsStr::to_str)
            .ok_or(UrlParseError::IdnaError)?
            .to_string();
        let key = self.set.get_or_insert(item);
        self.key.format(key, &format)
    }
}

struct KeySet<T, K> {
    keys: BTreeMap<T, K>,
    values: BTreeMap<K, T>,
}

impl<T, K> KeySet<T, K>
where
    T: Eq + Ord + Clone,
    K: Eq + Ord + Clone,
    StandardUniform: Distribution<K>,
{
    fn new() -> Self {
        KeySet {
            keys: BTreeMap::new(),
            values: BTreeMap::new(),
        }
    }
    fn insert(&mut self, item: T, key: K) {
        self.keys.insert(item.clone(), key.clone());
        self.values.insert(key, item);
    }

    fn get_or_insert(&mut self, item: T) -> K {
        if self.keys.contains_key(&item) {
            self.keys.get(&item).unwrap().clone()
        } else {
            let mut tries = 0;
            let mut key = rand::random();
            while self.values.contains_key(&key) {
                key = rand::random();
                tries += 1;
                if tries > MAX_KEY_GENERATION_TRIES {
                    panic!("Failed to generate unique key after {MAX_KEY_GENERATION_TRIES} tries");
                }
            }
            self.insert(item, key.clone());
            key
        }
    }
}

#[derive(Error, Debug)]
pub enum InvalidKey {
    #[error("Invalid workspace directory key format")]
    BadFormat,
    #[error("Invalid base32 encoding in workspace directory key")]
    BadBase32,
    #[error("Invalid key length in workspace directory key")]
    BadLength,
    #[error("Invalid workspace name")]
    BadWorkspaceName,
}

struct WorkspaceDirKey {
    name: String,
}

impl WorkspaceDirKey {
    fn new(name: String) -> Self {
        WorkspaceDirKey { name }
    }

    fn format(&self, key: u32) -> String {
        format!(
            "{}-{}",
            self.name,
            base32::encode(BASE32_ALPHABET, &key.to_be_bytes()).to_lowercase()
        )
    }

    fn parse(&self, key: &str) -> Result<u32, InvalidKey> {
        let (name_part, key_part) = key.rsplit_once('-').ok_or(InvalidKey::BadFormat)?;
        if name_part != self.name {
            return Err(InvalidKey::BadWorkspaceName);
        }
        let key_bytes = base32::decode(BASE32_ALPHABET, key_part).ok_or(InvalidKey::BadBase32)?;
        let key_array: [u8; 4] = key_bytes.try_into().map_err(|_| InvalidKey::BadLength)?;
        Ok(u32::from_be_bytes(key_array))
    }
}

struct WorkspaceFileKey {
    base: Url,
    prefix: Option<String>,
}

impl WorkspaceFileKey {
    pub fn new(bucket: String, prefix: Option<String>) -> Result<Self, UrlParseError> {
        let base = Url::parse(&format!("s3://{bucket}/"))?;
        if let Some(prefix) = prefix.as_ref() {
            let _ = base.join(prefix)?;
        }
        Ok(WorkspaceFileKey { base, prefix })
    }

    fn format(&self, key: u64, format: &str) -> Result<Url, UrlParseError> {
        let full_path = format!(
            "{prefix}{name}.{format}",
            prefix = self.prefix.as_deref().unwrap_or(""),
            name = base32::encode(BASE32_ALPHABET, &key.to_be_bytes()).to_lowercase(),
        );
        self.base.join(&full_path)
    }

    fn parse(&self, url: &Url) -> Result<(u64, String), InvalidKey> {
        let relative_path = url
            .path()
            .strip_prefix(&format!("/{}", self.prefix.as_deref().unwrap_or("")))
            .ok_or(InvalidKey::BadFormat)?;
        let (name_part, format_part) = relative_path
            .rsplit_once('.')
            .ok_or(InvalidKey::BadFormat)?;
        let key_bytes = base32::decode(BASE32_ALPHABET, name_part).ok_or(InvalidKey::BadBase32)?;
        let key_array: [u8; 8] = key_bytes.try_into().map_err(|_| InvalidKey::BadLength)?;
        Ok((u64::from_be_bytes(key_array), format_part.to_string()))
    }
}
