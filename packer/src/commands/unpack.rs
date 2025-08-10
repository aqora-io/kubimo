use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};

use clap::Args;

use super::Command;

#[derive(Args, Debug)]
pub struct UnpackCommand {
    pub src: PathBuf,
    pub dst: PathBuf,
}

#[async_trait::async_trait]
impl Command for UnpackCommand {
    type Error = io::Error;
    async fn run(self) -> Result<(), Self::Error> {
        unpack_path(self.src, self.dst)
    }
}

#[inline]
pub fn unpack(src: impl Read, dst: impl AsRef<Path>) -> io::Result<()> {
    tar::Archive::new(flate2::read::GzDecoder::new(src)).unpack(dst.as_ref())
}

#[inline]
pub fn unpack_path(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> io::Result<()> {
    let src_file = BufReader::new(File::open(src)?);
    unpack(src_file, dst)
}
