use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use flate2::read::GzDecoder;
use futures::prelude::*;
use object_store::{Error as ObjectStoreError, ObjectStore, path::Path as ObjectPath};
use tokio::io::AsyncRead;
use tokio_util::io::{StreamReader, SyncIoBridge};

use clap::Args;

use crate::{Context, Result, S3Url, multipart::MultipartOptions};

#[derive(Args)]
pub struct Command {
    src: S3Url,
    dst: PathBuf,
    #[clap(flatten)]
    multipart: MultipartOptions,
}

struct ChunkIter {
    current: u64,
    end: u64,
    step: u64,
}

impl ChunkIter {
    fn new(end: u64, step: u64) -> Self {
        Self {
            current: 0,
            end,
            step,
        }
    }
}

impl Iterator for ChunkIter {
    type Item = std::ops::Range<u64>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current >= self.end {
            return None;
        }
        let start = self.current;
        let next_end = (start + self.step).min(self.end);
        self.current = next_end;
        Some(start..next_end)
    }
}

async fn download(
    store: impl ObjectStore,
    path: ObjectPath,
    options: &MultipartOptions,
) -> Result<
    StreamReader<impl Stream<Item = Result<Bytes, ObjectStoreError>> + Send + 'static, Bytes>,
    ObjectStoreError,
> {
    let store = Arc::new(store);
    let meta = store.head(&path).await?;
    Ok(StreamReader::new(
        futures::stream::iter(ChunkIter::new(meta.size, options.chunk_size as u64))
            .map(move |range| {
                let path = path.clone();
                let store = Arc::clone(&store);
                async move { store.get_range(&path, range).await }
            })
            .buffered(options.concurrency),
    ))
}

async fn unpack(reader: impl AsyncRead + Send + Unpin + 'static, dst: PathBuf) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        let reader = GzDecoder::new(SyncIoBridge::new(reader));
        tar::Archive::new(reader).unpack(dst)
    })
    .await??;
    Ok(())
}

async fn download_and_unpack(
    store: impl ObjectStore,
    path: ObjectPath,
    dst: PathBuf,
    options: &MultipartOptions,
) -> Result<()> {
    let reader = download(store, path, options).await?;
    unpack(reader, dst).await?;
    Ok(())
}

impl Command {
    pub async fn run(self, context: Context) -> Result<()> {
        let s3 = context.s3.with_bucket_name(self.src.bucket).build()?;
        download_and_unpack(s3, self.src.path, self.dst, &self.multipart).await?;
        Ok(())
    }
}
