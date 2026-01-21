use std::path::Path;

use futures::StreamExt;
use object_store::{ObjectStoreExt, PutPayloadMut, PutResult, WriteMultipart, aws::AmazonS3};
use thiserror::Error;
use tokio::sync::{AcquireError, Semaphore, SemaphorePermit};
use tokio_util::io::ReaderStream;

#[derive(Error, Debug)]
pub enum UploadError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Upload(#[from] object_store::Error),
    #[error(transparent)]
    Path(#[from] object_store::path::Error),
    #[error(transparent)]
    Semaphore(#[from] AcquireError),
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

#[tracing::instrument(skip(s3))]
pub async fn upload(
    s3: &AmazonS3,
    key: object_store::path::Path,
    path: &Path,
    size: u64,
    upload_permits: &Semaphore,
) -> Result<PutResult, UploadError> {
    let key = object_store::path::Path::parse(key)?;
    let part_size = std::cmp::max(MIN_PART_SIZE, size.div_ceil(MAX_PARTS));
    let mut stream = ReaderStream::new(tokio::fs::File::open(path).await?);
    if size < part_size {
        let mut out = PutPayloadMut::new();
        while let Some(chunk) = stream.next().await {
            out.push(chunk?)
        }
        Ok(s3.put(&key, out.freeze()).await?)
    } else {
        let mut multipart = WriteMultipart::new(s3.put_multipart(&key).await?);
        while let Some(chunk) = stream.next().await {
            let permits = acquire_permit(upload_permits).await?;
            multipart.wait_for_capacity(permits.len()).await?;
            multipart.put(chunk?);
        }
        Ok(multipart.finish().await?)
    }
}
