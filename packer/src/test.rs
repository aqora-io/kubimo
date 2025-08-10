use std::io::Seek;
use std::path::PathBuf;

use crate::commands::{
    pack::{PackOptions, pack},
    unpack::unpack,
};

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
    dst.rewind().unwrap();
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
    dst.rewind().unwrap();
    unpack(&mut dst, tempfile::tempdir().unwrap()).unwrap();
}
