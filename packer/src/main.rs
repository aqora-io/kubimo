use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};
use flate2::{Compression, write::GzEncoder};
use thiserror::Error;

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Cli {
    #[clap(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Pack(PackArgs),
    Unpack(UnpackArgs),
}

#[derive(Args, Debug)]
struct PackArgs {
    src: PathBuf,
    dst: PathBuf,
    #[clap(flatten)]
    options: PackOptions,
}

const DEFAULT_COMPRESSION: u32 = 6;

#[derive(Args, Debug)]
struct PackOptions {
    #[clap(long, short = 'c', default_value_t = DEFAULT_COMPRESSION)]
    compression: u32,
    #[clap(long, short = 'a')]
    allow_symlinks: bool,
}

impl Default for PackOptions {
    fn default() -> Self {
        PackOptions {
            compression: DEFAULT_COMPRESSION,
            allow_symlinks: false,
        }
    }
}

#[derive(Args, Debug)]
struct UnpackArgs {
    src: PathBuf,
    dst: PathBuf,
}

#[derive(Error, Debug)]
enum PackError {
    #[error("Error walking the directory: {0}")]
    Walk(#[from] ignore::Error),
    #[error("File out of directory: {0}")]
    FileOutOfDirectory(#[from] std::path::StripPrefixError),
    #[error("Error creating tar archive: {0}")]
    Io(#[from] std::io::Error),
}

fn pack(src: impl AsRef<Path>, dst: impl Write, options: &PackOptions) -> Result<(), PackError> {
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
fn pack_path(
    src: impl AsRef<Path>,
    dst: impl AsRef<Path>,
    options: &PackOptions,
) -> Result<(), PackError> {
    pack(src, BufWriter::new(File::create(dst)?), options)
}

#[inline]
fn unpack(src: impl Read, dst: impl AsRef<Path>) -> io::Result<()> {
    tar::Archive::new(flate2::read::GzDecoder::new(src)).unpack(dst.as_ref())
}

#[inline]
fn unpack_path(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> io::Result<()> {
    let src_file = BufReader::new(File::open(src)?);
    unpack(src_file, dst)
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Commands::Pack(args) => {
            if let Err(err) = pack_path(&args.src, &args.dst, &args.options) {
                eprintln!("Error packing files: {err}");
                std::process::exit(1);
            }
        }
        Commands::Unpack(args) => {
            if let Err(err) = unpack_path(&args.src, &args.dst) {
                eprintln!("Error unpacking files: {err}");
                std::process::exit(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example_src() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test")
            .join("example")
    }

    #[test]
    fn test_pack() {
        let src = example_src();
        let mut dst = tempfile::tempfile().unwrap();
        pack(src, &mut dst, &Default::default()).unwrap();
        io::Seek::seek(&mut dst, io::SeekFrom::Start(0)).unwrap();
        unpack(&mut dst, tempfile::tempdir().unwrap()).unwrap();
    }

    #[test]
    fn test_pack_symlinks() {
        let src = example_src();
        let mut dst = tempfile::tempfile().unwrap();
        pack(
            src,
            &mut dst,
            &PackOptions {
                allow_symlinks: true,
                ..Default::default()
            },
        )
        .unwrap();
        io::Seek::seek(&mut dst, io::SeekFrom::Start(0)).unwrap();
        unpack(&mut dst, tempfile::tempdir().unwrap()).unwrap();
    }
}
