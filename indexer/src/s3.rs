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
    io::{AsyncRead, AsyncSeek, AsyncSeekExt},
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
        Self {
            builder: Arc::new(AmazonS3Builder::from_env()),
            clients: Arc::new(RwLock::new(BTreeMap::new())),
            cache_markers: Arc::new(RwLock::new(CacheMarkers::new())),
        }
    }

    pub async fn set_cache(&self, cache_markers: CacheMarkers) {
        let mut markers = self.cache_markers.write().await;
        *markers = cache_markers;
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
}
