//! A job's disk is a copy of its template: the same bytes and length, this user's alone, whatever was at its path
//! before, and on a file system with holes no larger on disk than the template.

use std::fs;
use std::os::unix::fs::{FileExt as _, MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use cf_vmhost::scratch::{copy, sparse_copy};
use proptest::prelude::*;

const MIB: u64 = 1 << 20;

/// A sparse file of `length` with `pieces` written where they say.
fn template(dir: &Path, length: u64, pieces: &[(u64, &[u8])]) -> PathBuf {
    let path = dir.join("template.img");
    let file = fs::File::create(&path).unwrap();
    file.set_len(length).unwrap();
    for (at, bytes) in pieces {
        file.write_all_at(bytes, *at).unwrap();
    }
    file.sync_all().unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    path
}

fn same(template: &Path, copy: &Path) {
    let (a, b) = (fs::read(template).unwrap(), fs::read(copy).unwrap());
    assert_eq!(a.len(), b.len(), "the copy has another length");
    if let Some(at) = a.iter().zip(&b).position(|(x, y)| x != y) {
        panic!("the copy differs at byte {at}");
    }
}

/// The bytes a file holds on disk.
fn on_disk(path: &Path) -> u64 {
    fs::metadata(path).unwrap().blocks() * 512
}

/// Whether this folder's file system keeps holes: a file of 64 MiB with nothing written holds next to nothing.
fn keeps_holes(dir: &Path) -> bool {
    let probe = dir.join("probe");
    fs::File::create(&probe).unwrap().set_len(64 * MIB).unwrap();
    let holds = on_disk(&probe) < MIB;
    fs::remove_file(probe).unwrap();
    holds
}

fn pieces() -> Vec<(u64, &'static [u8])> {
    vec![
        (0, b"superblock".as_slice()),
        (5000, b"a byte off any block's start"),
        (64 * MIB - 3, b"across a boundary"),
        (100 * MIB, &[7u8; 4096]),
        (255 * MIB, b"near the end"),
    ]
}

#[test]
fn the_copy_is_the_template_byte_for_byte_and_this_users_alone() {
    let dir = tempfile::tempdir().unwrap();
    let from = template(dir.path(), 256 * MIB, &pieces());
    let to = dir.path().join("scratch.img");
    copy(&from, &to).unwrap();
    same(&from, &to);
    assert_eq!(fs::metadata(&to).unwrap().mode() & 0o777, 0o600);
}

#[test]
fn what_was_at_the_path_before_is_replaced_whole() {
    let dir = tempfile::tempdir().unwrap();
    let from = template(dir.path(), 4 * MIB, &[(0, b"new")]);
    let to = dir.path().join("scratch.img");
    // Longer than the template, other bytes, and open to everybody.
    fs::write(&to, vec![9u8; 8 * MIB as usize]).unwrap();
    fs::set_permissions(&to, fs::Permissions::from_mode(0o666)).unwrap();
    copy(&from, &to).unwrap();
    same(&from, &to);
    assert_eq!(fs::metadata(&to).unwrap().mode() & 0o777, 0o600);
}

#[test]
fn the_holes_stay_holes() {
    let dir = tempfile::tempdir().unwrap();
    if !keeps_holes(dir.path()) {
        eprintln!("skipped: this file system keeps no holes");
        return;
    }
    let from = template(dir.path(), 256 * MIB, &pieces());
    let to = dir.path().join("sparse.img");
    let out = fs::File::create(&to).unwrap();
    sparse_copy(&fs::File::open(&from).unwrap(), &out).unwrap();
    drop(out);
    same(&from, &to);
    assert!(
        on_disk(&to) <= on_disk(&from) + MIB,
        "the copy holds {} bytes on disk, its template {}",
        on_disk(&to),
        on_disk(&from)
    );
    // And so does the copy a job gets, whichever way it was made.
    let job = dir.path().join("scratch.img");
    copy(&from, &job).unwrap();
    assert!(
        on_disk(&job) <= on_disk(&from) + MIB,
        "the job's disk holds {} bytes",
        on_disk(&job)
    );
}

#[test]
fn an_empty_template_all_holes_or_all_data_copies_whole() {
    let dir = tempfile::tempdir().unwrap();
    for (name, length, pieces) in [
        ("empty", 0, vec![]),
        ("holes", 256 * MIB, vec![]),
        ("one byte", 1, vec![(0, b"x".as_slice())]),
        ("data to the end", 2 * MIB, vec![(0, [3u8; 2 << 20].as_slice())]),
        (
            "data at the very end",
            8 * MIB,
            vec![(8 * MIB - 1, b"z".as_slice())],
        ),
    ] {
        let from = template(dir.path(), length, &pieces);
        let to = dir.path().join(format!("{name}.img"));
        copy(&from, &to).unwrap_or_else(|error| panic!("{name}: {error}"));
        same(&from, &to);
        let out = dir.path().join(format!("{name}.sparse"));
        sparse_copy(&fs::File::open(&from).unwrap(), &fs::File::create(&out).unwrap()).unwrap();
        same(&from, &out);
    }
}

#[test]
fn a_template_that_is_not_there_is_an_error_and_leaves_no_disk() {
    let dir = tempfile::tempdir().unwrap();
    let to = dir.path().join("scratch.img");
    let error = copy(&dir.path().join("gone.img"), &to).unwrap_err();
    assert!(error.starts_with("the job's disk: "), "{error}");
    assert!(!to.exists());
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    #[test]
    fn any_pieces_anywhere_copy_exactly(
        length in 0u64..(8 * MIB),
        pieces in proptest::collection::vec((0u64..(8 * MIB), proptest::collection::vec(any::<u8>(), 1..9000)), 0..8),
    ) {
        let dir = tempfile::tempdir().unwrap();
        // Pieces past the length make the file longer, as a write there does.
        let owned: Vec<(u64, Vec<u8>)> = pieces;
        let borrowed: Vec<(u64, &[u8])> = owned.iter().map(|(at, bytes)| (*at, bytes.as_slice())).collect();
        let from = template(dir.path(), length, &borrowed);
        let to = dir.path().join("sparse.img");
        sparse_copy(&fs::File::open(&from).unwrap(), &fs::File::create(&to).unwrap()).unwrap();
        prop_assert_eq!(fs::read(&from).unwrap(), fs::read(&to).unwrap());
    }
}
