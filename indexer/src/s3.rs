use std::collections::BTreeMap;
use std::sync::Arc;

use crc32fast::Hasher as Crc32Hasher;
use futures::StreamExt;
use kubimo::url::Url;
use object_store::{
    Attribute, AttributeValue, Attributes, ObjectStore, ObjectStoreExt, PutMultipartOptions,
    PutOptions, PutPayloadMut, WriteMultipart,
    aws::{AmazonS3, AmazonS3Builder},
    path::Path as Key,
};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncSeek, AsyncSeekExt, AsyncWrite, AsyncWriteExt},
    sync::{AcquireError, RwLock, Semaphore, SemaphorePermit},
};
use tokio_util::io::ReaderStream;

pub struct UploadResult {
    pub crc32: u32,
    pub e_tag: Option<String>,
}

#[derive(Clone)]
pub struct S3Client {
    builder: Arc<AmazonS3Builder>,
    clients: Arc<RwLock<BTreeMap<String, AmazonS3>>>,
    cache_markers: Arc<RwLock<CacheMarkers>>,
}

#[derive(Error, Debug)]
pub enum UploadError {
    #[error(transparent)]
    Url(#[from] ParseS3UrlError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Upload(#[from] object_store::Error),
    #[error(transparent)]
    Semaphore(#[from] AcquireError),
}

#[derive(Debug, Error)]
pub enum DeleteError {
    #[error(transparent)]
    Url(#[from] ParseS3UrlError),
    #[error(transparent)]
    S3(#[from] object_store::Error),
}

#[derive(Debug, Error)]
pub enum DownloadError {
    #[error(transparent)]
    Url(#[from] ParseS3UrlError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    S3(#[from] object_store::Error),
    #[error("crc32 mismatch: expected {expected:08x}, got {actual:08x}")]
    Crc32Mismatch { expected: u32, actual: u32 },
}

#[derive(Debug, Error)]
pub enum CacheMarkerCheckError {
    #[error("URL not found in cache")]
    NotFound,
    #[error("CRC32 checksum does not match")]
    Crc32Mismatch,
    #[error("No remote ETag")]
    NoRemoteETag,
    #[error("ETag does not match")]
    ETagMismatch,
    #[error(transparent)]
    S3(#[from] object_store::Error),
}

impl S3Client {
    pub fn from_env() -> Self {
        Self::from_builder(AmazonS3Builder::from_env())
    }

    /// Build a client from an explicit set of `AWS_*` options.
    ///
    /// The node agent serves workspaces from more than one S3 account at once —
    /// on a shared cluster each environment has its own bucket *and* its own
    /// endpoint — so it cannot use one client built from its own process
    /// environment. kubelet hands it the workspace's own credentials with each
    /// `NodePublishVolume`, and this turns those into a client.
    ///
    /// Keys are matched case-insensitively against `object_store`'s config
    /// names, so a Kubernetes Secret's `AWS_ACCESS_KEY_ID` works as-is.
    /// Anything unrecognised is ignored rather than rejected: a Secret shared
    /// with other consumers may legitimately carry keys that mean nothing here.
    pub fn from_options<K, V>(options: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: AsRef<str>,
        V: Into<String>,
    {
        let mut builder = AmazonS3Builder::new();
        for (key, value) in options {
            if let Ok(key) = key.as_ref().to_ascii_lowercase().parse() {
                builder = builder.with_config(key, value);
            }
        }
        Self::from_builder(builder)
    }

    fn from_builder(builder: AmazonS3Builder) -> Self {
        Self {
            builder: Arc::new(builder),
            clients: Arc::new(RwLock::new(BTreeMap::new())),
            cache_markers: Arc::new(RwLock::new(CacheMarkers::new())),
        }
    }

    pub async fn set_cache(&self, cache_markers: CacheMarkers) {
        let mut markers = self.cache_markers.write().await;
        *markers = cache_markers;
    }

    /// Merge `cache_markers` into the existing set instead of replacing it.
    ///
    /// The standalone indexer owns its client and serves one workspace, so it
    /// can [`Self::set_cache`]. The node agent shares a single client across
    /// every workspace on the node — `cache_markers` lives behind an `Arc`, so
    /// replacing it there would discard the markers of every other slot each
    /// time one was published. Markers are keyed by `(bucket, key)`, which is
    /// globally unique, so merging is always safe.
    pub async fn extend_cache(&self, cache_markers: CacheMarkers) {
        let mut markers = self.cache_markers.write().await;
        markers.extend(cache_markers);
    }

    async fn bucket(&self, bucket: &str) -> object_store::Result<AmazonS3> {
        if let Some(client) = self.clients.read().await.get(bucket) {
            return Ok(client.clone());
        }
        let client = self
            .builder
            .as_ref()
            .clone()
            .with_bucket_name(bucket.to_string())
            .build()?;
        self.clients
            .write()
            .await
            .insert(bucket.to_string(), client.clone());
        Ok(client)
    }

    async fn get_cached(
        &self,
        s3: &AmazonS3,
        bucket: String,
        key: Key,
        crc32: u32,
    ) -> Result<String, CacheMarkerCheckError> {
        let Some(marker) = self
            .cache_markers
            .read()
            .await
            .items
            .get(&(bucket, key.clone()))
            .cloned()
        else {
            return Err(CacheMarkerCheckError::NotFound);
        };
        if marker.0 != crc32 {
            return Err(CacheMarkerCheckError::Crc32Mismatch);
        }
        let Some(e_tag) = s3.head(&key).await?.e_tag else {
            return Err(CacheMarkerCheckError::NoRemoteETag);
        };
        if marker.1 != e_tag {
            return Err(CacheMarkerCheckError::ETagMismatch);
        }
        Ok(e_tag)
    }

    #[tracing::instrument(skip(self, input))]
    pub async fn upload(
        &self,
        url: &Url,
        mut input: impl AsyncRead + AsyncSeek + Unpin,
        size: u64,
        upload_permits: &Semaphore,
    ) -> Result<UploadResult, UploadError> {
        let (bucket, key) = parse_s3_url(url)?;
        let s3 = self.bucket(&bucket).await?;
        let part_size = std::cmp::max(MIN_PART_SIZE, size.div_ceil(MAX_PARTS));
        let res = if size < part_size {
            let mut payload = PutPayloadMut::new();
            let mut hasher = Crc32Hasher::new();
            let mut stream = ReaderStream::new(input);
            while let Some(chunk) = stream.next().await {
                let bytes = chunk?;
                hasher.update(&bytes);
                payload.push(bytes);
            }
            let crc32 = hasher.finalize();
            if let Ok(e_tag) = self
                .get_cached(&s3, bucket.clone(), key.clone(), crc32)
                .await
            {
                return Ok(UploadResult {
                    crc32,
                    e_tag: Some(e_tag),
                });
            }
            let e_tag = s3
                .put_opts(
                    &key,
                    payload.freeze(),
                    PutOptions {
                        attributes: get_attributes(&key),
                        ..Default::default()
                    },
                )
                .await?
                .e_tag;
            UploadResult { crc32, e_tag }
        } else {
            let mut hasher = Crc32Hasher::new();
            let mut stream = ReaderStream::new(&mut input);
            while let Some(chunk) = stream.next().await {
                hasher.update(&chunk?);
            }
            let crc32 = hasher.finalize();
            if let Ok(e_tag) = self
                .get_cached(&s3, bucket.clone(), key.clone(), crc32)
                .await
            {
                return Ok(UploadResult {
                    crc32,
                    e_tag: Some(e_tag),
                });
            }
            input.rewind().await?;
            let mut stream = ReaderStream::new(input);
            let mut multipart = WriteMultipart::new(
                s3.put_multipart_opts(
                    &key,
                    PutMultipartOptions {
                        attributes: get_attributes(&key),
                        ..Default::default()
                    },
                )
                .await?,
            );
            while let Some(chunk) = stream.next().await {
                let permits = acquire_permit(upload_permits).await?;
                multipart.wait_for_capacity(permits.len()).await?;
                multipart.put(chunk?);
            }
            let e_tag = multipart.finish().await?.e_tag;
            UploadResult { crc32, e_tag }
        };
        if let Some(e_tag) = &res.e_tag {
            self.cache_markers
                .write()
                .await
                .insert(url.clone(), res.crc32, e_tag.clone());
        }
        Ok(res)
    }

    #[tracing::instrument(skip(self))]
    pub async fn delete(&self, url: &Url) -> Result<(), DeleteError> {
        let (bucket, key) = parse_s3_url(url)?;
        let s3 = self.bucket(&bucket).await?;
        s3.delete(&key).await?;
        Ok(())
    }

    /// GET a small object fully into memory.
    #[tracing::instrument(skip(self))]
    pub async fn get_bytes(&self, url: &Url) -> Result<bytes::Bytes, DownloadError> {
        let (bucket, key) = parse_s3_url(url)?;
        let s3 = self.bucket(&bucket).await?;
        get_bytes_from_store(&s3, &key).await
    }

    /// Stream a GET to `output`, verifying against `expected_crc32` when
    /// given. Returns the crc32 of the downloaded bytes.
    #[tracing::instrument(skip(self, output))]
    pub async fn download(
        &self,
        url: &Url,
        output: impl AsyncWrite + Unpin,
        expected_crc32: Option<u32>,
    ) -> Result<u32, DownloadError> {
        let (bucket, key) = parse_s3_url(url)?;
        let s3 = self.bucket(&bucket).await?;
        download_from_store(&s3, &key, output, expected_crc32).await
    }
}

async fn get_bytes_from_store(
    store: &impl ObjectStore,
    key: &Key,
) -> Result<bytes::Bytes, DownloadError> {
    Ok(store.get(key).await?.bytes().await?)
}

async fn download_from_store(
    store: &impl ObjectStore,
    key: &Key,
    mut output: impl AsyncWrite + Unpin,
    expected_crc32: Option<u32>,
) -> Result<u32, DownloadError> {
    let mut stream = store.get(key).await?.into_stream();
    let mut hasher = Crc32Hasher::new();
    while let Some(chunk) = stream.next().await {
        let bytes = chunk?;
        hasher.update(&bytes);
        output.write_all(&bytes).await?;
    }
    output.flush().await?;
    let actual = hasher.finalize();
    if let Some(expected) = expected_crc32
        && expected != actual
    {
        return Err(DownloadError::Crc32Mismatch { expected, actual });
    }
    Ok(actual)
}

const MIN_PART_SIZE: u64 = 10 * 1024 * 1024; // 10 MB
const MAX_PARTS: u64 = 10_000;

async fn acquire_permit(
    upload_permits: &Semaphore,
) -> Result<Vec<SemaphorePermit<'_>>, AcquireError> {
    let mut permits = Vec::new();
    permits.push(upload_permits.acquire().await?);
    while let Ok(permit) = upload_permits.try_acquire() {
        permits.push(permit);
    }
    Ok(permits)
}

fn get_attributes(key: &Key) -> Attributes {
    let mut attributes = Attributes::new();
    if let Some(extension) = key.extension() {
        match extension {
            "json" => {
                attributes.insert(
                    Attribute::ContentType,
                    AttributeValue::from("application/json"),
                );
            }
            "ipynb" => {
                attributes.insert(
                    Attribute::ContentType,
                    AttributeValue::from("application/x-ipynb+json"),
                );
            }
            "html" => {
                attributes.insert(Attribute::ContentType, AttributeValue::from("text/html"));
            }
            "md" => {
                attributes.insert(
                    Attribute::ContentType,
                    AttributeValue::from("text/markdown"),
                );
            }
            _ => {}
        }
    }
    attributes
}

#[derive(Debug, Error)]
pub enum ParseS3UrlError {
    #[error("URL scheme is not supported")]
    BadScheme,
    #[error("URL does not contain bucket name")]
    NoBucket,
    #[error(transparent)]
    Path(#[from] object_store::path::Error),
}

fn parse_s3_url(url: &Url) -> Result<(String, Key), ParseS3UrlError> {
    if url.scheme() != "s3" {
        return Err(ParseS3UrlError::BadScheme);
    }
    let Some(bucket) = url.host_str() else {
        return Err(ParseS3UrlError::NoBucket);
    };
    let key = Key::parse(url.path())?;
    Ok((bucket.to_string(), key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::memory::InMemory;

    /// The node agent shares one `S3Client` across every slot on the node, so a
    /// per-slot cache update must not discard the other slots' markers. Keys are
    /// `(bucket, key)`, which is globally unique, so a merge cannot conflate two
    /// workspaces.
    #[tokio::test]
    async fn extending_the_cache_keeps_other_slots_markers() {
        let client = S3Client::from_env();

        let mut first = CacheMarkers::new();
        first.insert("s3://bucket/slot-a".parse().unwrap(), 1, "etag-a".into());
        client.extend_cache(first).await;

        let mut second = CacheMarkers::new();
        second.insert("s3://bucket/slot-b".parse().unwrap(), 2, "etag-b".into());
        client.extend_cache(second).await;

        let markers = client.cache_markers.read().await;
        assert_eq!(markers.items.len(), 2, "second publish evicted the first");

        // `set_cache` is the standalone indexer's behaviour and must stay
        // destructive — this is the contrast the agent must avoid.
        drop(markers);
        client.set_cache(CacheMarkers::new()).await;
        assert_eq!(client.cache_markers.read().await.items.len(), 0);
    }

    async fn store_with(key: &str, contents: &'static [u8]) -> InMemory {
        let store = InMemory::new();
        store
            .put(&Key::parse(key).unwrap(), contents.into())
            .await
            .unwrap();
        store
    }

    #[tokio::test]
    async fn test_download_writes_bytes_and_returns_crc32() {
        let store = store_with("data.csv", b"hello world").await;
        let mut output = std::io::Cursor::new(Vec::new());
        let crc32 = download_from_store(
            &store,
            &Key::parse("data.csv").unwrap(),
            &mut output,
            Some(crc32fast::hash(b"hello world")),
        )
        .await
        .unwrap();
        assert_eq!(output.into_inner(), b"hello world");
        assert_eq!(crc32, crc32fast::hash(b"hello world"));
    }

    #[tokio::test]
    async fn test_download_without_expected_crc32() {
        let store = store_with("data.csv", b"hello world").await;
        let mut output = std::io::Cursor::new(Vec::new());
        download_from_store(&store, &Key::parse("data.csv").unwrap(), &mut output, None)
            .await
            .unwrap();
        assert_eq!(output.into_inner(), b"hello world");
    }

    #[tokio::test]
    async fn test_download_crc32_mismatch() {
        let store = store_with("data.csv", b"hello world").await;
        let mut output = std::io::Cursor::new(Vec::new());
        let err = download_from_store(
            &store,
            &Key::parse("data.csv").unwrap(),
            &mut output,
            Some(crc32fast::hash(b"something else")),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, DownloadError::Crc32Mismatch { .. }));
    }

    #[tokio::test]
    async fn test_get_bytes_from_store() {
        let store = store_with("manifest.json", b"{}").await;
        let bytes = get_bytes_from_store(&store, &Key::parse("manifest.json").unwrap())
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), b"{}");
    }
}

#[derive(Debug, Default)]
pub struct CacheMarkers {
    items: BTreeMap<(String, Key), (u32, String)>,
}

impl CacheMarkers {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, url: Url, crc32: u32, e_tag: String) {
        match parse_s3_url(&url) {
            Ok((bucket, key)) => {
                self.items.insert((bucket, key), (crc32, e_tag));
            }
            Err(err) => {
                tracing::warn!("Failed to parse S3 URL for cache marker: {url}: {err}");
            }
        }
    }

    /// Take every marker from `other`, letting it win on collisions.
    ///
    /// Keys are `(bucket, key)` and an object's content is immutable for a given
    /// key within a run, so a collision means both sides describe the same
    /// object and either value is correct.
    pub fn extend(&mut self, other: CacheMarkers) {
        self.items.extend(other.items);
    }
}
