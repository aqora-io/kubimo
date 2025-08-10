use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use clap::Args;
use flate2::{Compression, write::GzEncoder};
use thiserror::Error;

use super::Command;

#[derive(Args, Debug)]
pub struct PackCommand {
    pub src: PathBuf,
    pub dst: PathBuf,
    #[clap(flatten)]
    pub options: PackOptions,
}

#[async_trait::async_trait]
impl Command for PackCommand {
    type Error = PackError;
    async fn run(self) -> Result<(), PackError> {
        pack_path(self.src, self.dst, &self.options)
    }
}

const DEFAULT_COMPRESSION: u32 = 6;

#[derive(Args, Debug)]
pub struct PackOptions {
    #[clap(long, short = 'c', default_value_t = DEFAULT_COMPRESSION)]
    pub compression: u32,
    #[clap(long, short = 'a')]
    pub allow_symlinks: bool,
}

impl Default for PackOptions {
    fn default() -> Self {
        PackOptions {
            compression: DEFAULT_COMPRESSION,
            allow_symlinks: false,
        }
    }
}

#[derive(Error, Debug)]
pub enum PackError {
    #[error("Error walking the directory: {0}")]
    Walk(#[from] ignore::Error),
    #[error("File out of directory: {0}")]
    FileOutOfDirectory(#[from] std::path::StripPrefixError),
    #[error("Error creating tar archive: {0}")]
    Io(#[from] std::io::Error),
}

pub fn pack(
    src: impl AsRef<Path>,
    dst: impl Write,
    options: &PackOptions,
) -> Result<(), PackError> {
    let src = src.as_ref();
    let mut tar = tar::Builder::new(GzEncoder::new(dst, Compression::new(options.compression)));
    for entry in ignore::WalkBuilder::new(src)
        .follow_links(!options.allow_symlinks)
        .build()
        .skip(1)
    {
        let entry = entry?;
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_file() {
            tar.append_file(
                entry.path().strip_prefix(src)?,
                &mut File::open(entry.path())?,
            )?;
        } else if file_type.is_symlink() {
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

#[inline]
pub fn pack_path(
    src: impl AsRef<Path>,
    dst: impl AsRef<Path>,
    options: &PackOptions,
) -> Result<(), PackError> {
    pack(src, BufWriter::new(File::create(dst)?), options)
}
