use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::Args;
use flate2::{Compression, write::GzEncoder};
use futures::prelude::*;
use object_store::{ObjectStore, WriteMultipart, path::Path as ObjectPath};
use tokio::io::{AsyncRead, AsyncWriteExt, ReadHalf, SimplexStream};
use tokio_util::io::SyncIoBridge;

use crate::{Context, Error, Result, S3Url, multipart::MultipartOptions};

const INTERNAL_BUFFER_SIZE: usize = 128 * 1024;

#[derive(Args, Debug)]
pub struct Command {
    src: PathBuf,
    dst: S3Url,
    #[clap(flatten)]
    pack: PackOptions,
    #[clap(flatten)]
    multipart: MultipartOptions,
}

#[derive(Args, Debug, Default)]
pub struct PackOptions {
    #[clap(long, short = 'l', default_value_t = 6)]
    compression_level: u32,
    #[clap(long, short = 's')]
    follow_symlinks: bool,
}

pub fn pack(src: impl AsRef<Path>, dst: impl Write, options: &PackOptions) -> Result<()> {
    let src = src.as_ref();
    let mut tar = tar::Builder::new(GzEncoder::new(
        dst,
        Compression::new(options.compression_level),
    ));
    for entry in ignore::WalkBuilder::new(src)
        .follow_links(options.follow_symlinks)
        .build()
        .skip(1)
    {
        let entry = entry?;
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_file() {
            tracing::info!("Adding file {}", entry.path().display());
            tar.append_file(
                entry.path().strip_prefix(src)?,
                &mut File::open(entry.path())?,
            )?;
        } else if file_type.is_symlink() {
            tracing::info!("Adding link {}", entry.path().display());
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            tar.append_link(
                &mut header,
                entry.path().strip_prefix(src)?,
                std::fs::read_link(entry.path())?,
            )?;
        }
    }
    let mut writer = tar.into_inner()?.finish()?;
    writer.flush()?;
    Ok(())
}

fn pack_reader(
    src: PathBuf,
    options: PackOptions,
) -> (ReadHalf<SimplexStream>, impl Future<Output = Result<()>>) {
    let (read, write) = tokio::io::simplex(INTERNAL_BUFFER_SIZE);
    let writer = tokio::task::spawn_blocking(move || {
        let mut writer = SyncIoBridge::new(write);
        pack(src, &mut writer, &options)?;
        Ok::<_, Error>(writer.into_inner())
    })
    .map_err(Error::from)
    .and_then(|res| async move {
        let mut writer = res?;
        writer.flush().await?;
        writer.shutdown().await?;
        Ok(())
    });
    (read, writer)
}

async fn create_multipart_writer(
    store: &impl ObjectStore,
    location: &ObjectPath,
    options: &MultipartOptions,
) -> Result<WriteMultipart> {
    let upload = store.put_multipart(location).await?;
    Ok(WriteMultipart::new_with_chunk_size(
        upload,
        options.chunk_size,
    ))
}

async fn upload_reader(
    reader: impl AsyncRead + Unpin,
    writer: &mut WriteMultipart,
    options: &MultipartOptions,
) -> Result<()> {
    let mut stream = tokio_util::io::ReaderStream::with_capacity(reader, INTERNAL_BUFFER_SIZE);
    while let Some(chunk) = stream.try_next().await? {
        writer.wait_for_capacity(options.concurrency).await?;
        writer.put(chunk);
    }
    Ok(())
}

async fn pack_and_upload(
    src: PathBuf,
    mut writer: WriteMultipart,
    pack: PackOptions,
    multipart: MultipartOptions,
) -> Result<()> {
    let (reader, pack_fut) = pack_reader(src, pack);
    let upload_fut = upload_reader(reader, &mut writer, &multipart);
    if let Err(err) = futures::future::try_join(pack_fut, upload_fut).await {
        if let Err(err) = writer.abort().await {
            tracing::error!("Failed to abort multipart upload: {}", err);
        }
        return Err(err);
    }
    writer.finish().await?;
    Ok(())
}

impl Command {
    pub async fn run(self, context: Context) -> Result<()> {
        let s3 = context.s3.with_bucket_name(self.dst.bucket).build()?;
        let writer = create_multipart_writer(&s3, &self.dst.path, &self.multipart).await?;
        pack_and_upload(self.src, writer, self.pack, self.multipart).await?;
        Ok(())
    }
}
