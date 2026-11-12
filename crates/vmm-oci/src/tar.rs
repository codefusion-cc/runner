//! The little of tar the engine's archive calls need: a few entries written (ustar), and one file read back.

/// One entry to write.
pub struct Entry<'a> {
    /// The path inside the archive, relative, without `..`.
    pub path: &'a str,
    /// The file's bytes, or `None` for a folder.
    pub data: Option<&'a [u8]>,
    /// Permission bits.
    pub mode: u32,
    /// Owner.
    pub uid: u32,
    /// Group.
    pub gid: u32,
}

/// Writes a number in octal, NUL-ended, into a field that has room for it: [`write`] refuses any that would not.
fn octal(field: &mut [u8], value: u64) {
    let digits = field.len() - 1;
    let text = format!("{value:0digits$o}");
    field[..digits].copy_from_slice(&text.as_bytes()[..digits]);
    field[digits] = 0;
}

/// The largest file an entry holds: eleven octal digits.
const SIZE_MAX: u64 = 0o77_777_777_777;
/// The largest owner and mode: seven octal digits.
const ID_MAX: u32 = 0o7_777_777;

/// An archive of these entries, in order. A path longer than 100 bytes, or one with `..`, is refused.
pub fn write(entries: &[Entry<'_>]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    for entry in entries {
        let name = match entry.data {
            Some(_) => entry.path.to_string(),
            None => format!("{}/", entry.path.trim_end_matches('/')),
        };
        if name.len() > 100 || name.starts_with('/') || name.split('/').any(|part| part == "..") {
            return Err(format!("{name:?} is no path for the archive"));
        }
        let size = entry.data.map_or(0, <[u8]>::len) as u64;
        if size > SIZE_MAX || entry.uid > ID_MAX || entry.gid > ID_MAX {
            return Err(format!(
                "{name:?} is too large or its owner's id too high for the archive"
            ));
        }
        let mut header = [0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        octal(&mut header[100..108], u64::from(entry.mode & 0o7777));
        octal(&mut header[108..116], u64::from(entry.uid));
        octal(&mut header[116..124], u64::from(entry.gid));
        octal(&mut header[124..136], size);
        octal(&mut header[136..148], 0);
        header[156] = if entry.data.is_some() { b'0' } else { b'5' };
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        // The checksum counts its own field as spaces.
        header[148..156].fill(b' ');
        let sum: u32 = header.iter().map(|&byte| u32::from(byte)).sum();
        let text = format!("{sum:06o}\0 ");
        header[148..156].copy_from_slice(text.as_bytes());
        out.extend_from_slice(&header);
        if let Some(data) = entry.data {
            out.extend_from_slice(data);
            out.resize(out.len().next_multiple_of(512), 0);
        }
    }
    out.resize(out.len() + 1024, 0);
    Ok(out)
}

fn number(field: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(field).ok()?;
    let text = text.trim_matches(|c: char| c == '\0' || c == ' ');
    if text.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(text, 8).ok()
}

/// The bytes of the first regular file in an archive whose name, without a leading `./` or `/`, ends in `name`
/// after a `/` or is `name`: the engine names a file it was asked for by its last part. `None` when there is none
/// or the archive is cut short.
pub fn read_file(archive: &[u8], name: &str) -> Option<Vec<u8>> {
    let mut at = 0usize;
    while at + 512 <= archive.len() {
        let header = &archive[at..at + 512];
        if header.iter().all(|&byte| byte == 0) {
            return None;
        }
        let end = header[..100].iter().position(|&byte| byte == 0).unwrap_or(100);
        let path = std::str::from_utf8(&header[..end]).ok()?;
        let path = path.trim_start_matches("./").trim_start_matches('/');
        let size = usize::try_from(number(&header[124..136])?).ok()?;
        let data = at.checked_add(512)?;
        let kind = header[156];
        if (kind == b'0' || kind == 0) && (path == name || path.ends_with(&format!("/{name}"))) {
            return archive.get(data..data.checked_add(size)?).map(<[u8]>::to_vec);
        }
        at = data.checked_add(size.next_multiple_of(512))?;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_written_reads_back_with_owner_and_mode() {
        let archive = write(&[
            Entry {
                path: "run/codefusion",
                data: None,
                mode: 0o700,
                uid: 1001,
                gid: 1001,
            },
            Entry {
                path: "run/codefusion/supervisor",
                data: Some(b"\x7fELF and the rest"),
                mode: 0o755,
                uid: 0,
                gid: 0,
            },
        ])
        .unwrap();
        assert_eq!(archive.len() % 512, 0);
        assert_eq!(&archive[..15], b"run/codefusion/");
        assert_eq!(archive[156], b'5');
        assert_eq!(number(&archive[108..116]), Some(1001));
        assert_eq!(number(&archive[100..108]), Some(0o700));
        let file = &archive[512..1024];
        assert_eq!(number(&file[100..108]), Some(0o755));
        assert_eq!(number(&file[108..116]), Some(0));
        assert_eq!(
            read_file(&archive, "supervisor").as_deref(),
            Some(&b"\x7fELF and the rest"[..])
        );
        assert_eq!(read_file(&archive, "codefusion"), None, "a folder is no file");
        // Every header's sum is right, or the engine refuses the archive.
        for header in [&archive[..512], file] {
            let stated = number(&header[148..156]).unwrap();
            let mut copy = header.to_vec();
            copy[148..156].fill(b' ');
            assert_eq!(stated, copy.iter().map(|&b| u64::from(b)).sum::<u64>());
        }
    }

    #[test]
    fn an_owner_a_field_cannot_hold_is_refused_not_cut() {
        let entry = |uid| Entry {
            path: "f",
            data: Some(b""),
            mode: 0o644,
            uid,
            gid: 0,
        };
        assert!(write(&[entry(ID_MAX)]).is_ok());
        assert!(write(&[entry(ID_MAX + 1)]).is_err());
    }

    #[test]
    fn a_path_that_could_leave_the_folder_is_refused() {
        for path in ["../etc/passwd", "/etc/passwd", "a/../../b", &"x".repeat(101)] {
            let entry = Entry {
                path,
                data: Some(b""),
                mode: 0o644,
                uid: 0,
                gid: 0,
            };
            assert!(write(&[entry]).is_err(), "{path}");
        }
    }

    #[test]
    fn a_file_is_found_by_its_last_part_and_a_cut_archive_finds_nothing() {
        let archive = write(&[Entry {
            path: "passwd",
            data: Some(b"runner:x:1001:1001::/home/runner:/bin/bash\n"),
            mode: 0o644,
            uid: 0,
            gid: 0,
        }])
        .unwrap();
        assert!(read_file(&archive, "passwd").unwrap().starts_with(b"runner:"));
        assert_eq!(read_file(&archive, "group"), None);
        assert_eq!(read_file(&archive[..540], "passwd"), None);
        assert_eq!(read_file(&[], "passwd"), None);
        let mut garbage = archive.clone();
        garbage[124..136].copy_from_slice(b"zzzzzzzzzzz\0");
        assert_eq!(read_file(&garbage, "passwd"), None);
    }
}
