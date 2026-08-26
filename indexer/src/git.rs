pub mod manifest;

use std::{borrow::Cow, cmp, fmt};

use bytes::{BufMut, Bytes, BytesMut};
use sha1::Digest as _;

pub trait WriteBytes {
    fn write(&mut self, bytes: &[u8]);
}

impl WriteBytes for sha1::Sha1 {
    fn write(&mut self, bytes: &[u8]) {
        self.update(bytes);
    }
}

impl WriteBytes for BytesMut {
    fn write(&mut self, bytes: &[u8]) {
        self.put_slice(bytes);
    }
}

#[derive(Clone, Copy, Debug)]
pub enum ObjectKind {
    Blob,
    Tree,
    Commit,
    Tag,
}

impl ObjectKind {
    pub fn write_header(self, size: impl ToString, buf: &mut impl WriteBytes) {
        buf.write(self.into_bytes());
        buf.write(b" ");
        buf.write(size.to_string().as_bytes());
        buf.write(b"\x00");
    }

    pub fn into_bytes(self) -> &'static [u8] {
        match self {
            Self::Blob => b"blob",
            Self::Tree => b"tree",
            Self::Commit => b"commit",
            Self::Tag => b"tag",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TreeEntryKind {
    Tree,
    Blob,
    Executable,
    Link,
}

impl TreeEntryKind {
    pub fn into_octal_str(self) -> &'static str {
        match self {
            Self::Tree => "40000",
            Self::Blob => "100644",
            Self::Executable => "100755",
            Self::Link => "120000",
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct TreeEntry<'a> {
    pub kind: TreeEntryKind,
    pub filename: Cow<'a, str>,
    pub oid: Oid,
}

impl TreeEntry<'_> {
    pub fn encode(&self, buf: &mut impl bytes::BufMut) {
        buf.put_slice(self.kind.into_octal_str().as_bytes());
        buf.put_u8(b' ');
        buf.put_slice(self.filename.as_bytes());
        buf.put_u8(0);
        buf.put(self.oid.0.clone());
    }
}

impl PartialOrd for TreeEntry<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        Some(Ord::cmp(self, other))
    }
}

impl Ord for TreeEntry<'_> {
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        git_cmp_filename(
            &self.filename,
            self.kind == TreeEntryKind::Tree,
            &other.filename,
            other.kind == TreeEntryKind::Tree,
        )
    }
}

fn git_cmp_filename(
    left: &str,
    left_is_dir: bool,
    right: &str,
    right_is_dir: bool,
) -> cmp::Ordering {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let n = left.len().min(right.len());

    left[..n].cmp(&right[..n]).then_with(|| {
        let left = left.get(n).or_else(|| left_is_dir.then_some(&b'/'));
        let right = right.get(n).or_else(|| right_is_dir.then_some(&b'/'));
        left.cmp(&right)
    })
}

#[derive(Clone, Copy, Debug)]
pub enum OidKind {
    Sha1,
    Sha256,
}

impl OidKind {
    pub fn start_hash(self, kind: ObjectKind, size: impl ToString) -> Hash {
        Hash(match self {
            Self::Sha1 => {
                let mut hash = sha1::Sha1::new();
                kind.write_header(size, &mut hash);
                Box::new(hash)
            }

            Self::Sha256 => {
                todo!()
            }
        })
    }

    pub fn hash(self, kind: ObjectKind, data: impl AsRef<[u8]>) -> Oid {
        let data = data.as_ref();
        let mut hash = self.start_hash(kind, data.len());
        hash.update(data);
        hash.finalize()
    }

    pub fn tree<'a>(self, entries: impl IntoIterator<Item = TreeEntry<'a>>) -> Oid {
        let mut buf = BytesMut::new();
        for entry in entries {
            entry.encode(&mut buf);
        }
        self.hash(ObjectKind::Tree, buf.freeze())
    }
}

pub struct Hash(Box<dyn sha1::digest::DynDigest + Send>);

impl Hash {
    pub fn update(&mut self, data: impl AsRef<[u8]>) {
        self.0.update(data.as_ref());
    }

    pub fn finalize(self) -> Oid {
        Oid(Bytes::from_owner(self.0.finalize()))
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct Oid(Bytes);

impl fmt::Debug for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let encoded = hex::encode(&self.0);
        f.write_str(&encoded)
    }
}

impl Oid {
    pub const fn from_static(bytes: &'static [u8]) -> Self {
        Self(Bytes::from_static(bytes))
    }

    pub fn from_hex(str: impl AsRef<str>) -> Result<Self, hex::FromHexError> {
        Ok(Self(Bytes::from_owner(hex::decode(
            str.as_ref().as_bytes(),
        )?)))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_hex_string(self) -> String {
        hex::encode(self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted_entries<'a>(entries: impl IntoIterator<Item = TreeEntry<'a>>) -> Vec<Cow<'a, str>> {
        let mut entries = entries.into_iter().collect::<Vec<_>>();
        entries.sort();
        entries.into_iter().map(|entry| entry.filename).collect()
    }

    fn tree<'a>(filename: &'a str) -> TreeEntry<'a> {
        TreeEntry {
            kind: TreeEntryKind::Tree,
            filename: Cow::Borrowed(filename),
            oid: Oid::from_static(b""),
        }
    }

    fn blob<'a>(filename: &'a str) -> TreeEntry<'a> {
        TreeEntry {
            kind: TreeEntryKind::Blob,
            filename: Cow::Borrowed(filename),
            oid: Oid::from_static(b""),
        }
    }

    #[test]
    fn test_tree_entry_ordering() {
        // Verified against `git ls-tree` on git 2.43.0.
        assert_eq!(
            Vec::from([
                "foobar!",
                "foobar-1",
                "foobar.txt",
                "foobar",
                "foobar0",
                "foobara"
            ]),
            sorted_entries([
                blob("foobara"),
                tree("foobar"),
                blob("foobar0"),
                blob("foobar.txt"),
                blob("foobar!"),
                blob("foobar-1"),
            ])
        );
    }
}
