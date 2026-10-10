//! A minimal `newc` cpio writer: enough for an initramfs, byte for byte reproducible (no timestamps, no owners
//! but root, entries in the order they are added).

use std::io::Write;

/// What kind of entry.
pub enum Kind<'a> {
    Dir,
    File(&'a [u8]),
}

/// Appends entries to a cpio archive.
pub struct Archive<W: Write> {
    out: W,
    next_inode: u32,
}

impl<W: Write> Archive<W> {
    pub fn new(out: W) -> Self {
        Self { out, next_inode: 1 }
    }

    /// Adds an entry; `mode` is the permission bits.
    pub fn add(&mut self, name: &str, kind: Kind<'_>, mode: u32) -> std::io::Result<()> {
        let (type_bits, data): (u32, &[u8]) = match kind {
            Kind::Dir => (0o040000, &[]),
            Kind::File(data) => (0o100000, data),
        };
        let inode = self.next_inode;
        self.next_inode += 1;
        if u32::try_from(data.len()).is_err() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{name} is larger than a cpio entry can be (4 GiB)"),
            ));
        }
        self.header(name, inode, type_bits | mode, data.len())?;
        self.out.write_all(data)?;
        self.pad(data.len())
    }

    /// Writes the trailer and returns the writer.
    pub fn finish(mut self) -> std::io::Result<W> {
        self.header("TRAILER!!!", 0, 0, 0)?;
        Ok(self.out)
    }

    fn header(&mut self, name: &str, inode: u32, mode: u32, size: usize) -> std::io::Result<()> {
        let name_size = name.len() + 1;
        let fields = [
            inode,
            mode,
            0,
            0,
            1,
            0,
            size as u32,
            0,
            0,
            0,
            0,
            name_size as u32,
            0,
        ];
        let mut header = String::from("070701");
        for field in fields {
            header.push_str(&format!("{field:08X}"));
        }
        self.out.write_all(header.as_bytes())?;
        self.out.write_all(name.as_bytes())?;
        self.out.write_all(&[0])?;
        self.pad(header.len() + name_size)
    }

    fn pad(&mut self, written: usize) -> std::io::Result<()> {
        self.out.write_all(&[0u8; 3][..(4 - written % 4) % 4])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads the archive back with the format's own rules: header fields, names and data aligned to four bytes.
    fn read(bytes: &[u8]) -> Vec<(String, u32, Vec<u8>)> {
        let mut at = 0;
        let mut entries = Vec::new();
        loop {
            assert_eq!(&bytes[at..at + 6], b"070701");
            let field = |n: usize| {
                u32::from_str_radix(
                    std::str::from_utf8(&bytes[at + 6 + n * 8..at + 14 + n * 8]).unwrap(),
                    16,
                )
                .unwrap()
            };
            let (mode, size, name_size) = (field(1), field(6) as usize, field(11) as usize);
            let name_start = at + 110;
            let name = String::from_utf8(bytes[name_start..name_start + name_size - 1].to_vec()).unwrap();
            let data_start = (name_start + name_size).next_multiple_of(4);
            if name == "TRAILER!!!" {
                return entries;
            }
            entries.push((name, mode, bytes[data_start..data_start + size].to_vec()));
            at = (data_start + size).next_multiple_of(4);
        }
    }

    #[test]
    fn entries_of_every_alignment_read_back_exactly() {
        let mut archive = Archive::new(Vec::new());
        archive.add("dev", Kind::Dir, 0o755).unwrap();
        for size in [0usize, 1, 2, 3, 4, 5, 1000] {
            archive
                .add(&format!("f{size}"), Kind::File(&vec![7u8; size]), 0o644)
                .unwrap();
        }
        archive.add("init", Kind::File(b"\x7fELF"), 0o755).unwrap();
        let bytes = archive.finish().unwrap();
        assert_eq!(bytes.len() % 4, 0);
        let entries = read(&bytes);
        assert_eq!(entries[0], ("dev".to_string(), 0o040755, vec![]));
        assert_eq!(entries.len(), 9);
        for (index, size) in [0usize, 1, 2, 3, 4, 5, 1000].into_iter().enumerate() {
            let (name, mode, data) = &entries[index + 1];
            assert_eq!(
                (name.as_str(), *mode, data.len()),
                (format!("f{size}").as_str(), 0o100644, size)
            );
        }
        assert_eq!(entries[8], ("init".to_string(), 0o100755, b"\x7fELF".to_vec()));
    }

    #[test]
    fn the_same_input_gives_the_same_bytes() {
        let make = || {
            let mut archive = Archive::new(Vec::new());
            archive.add("a", Kind::File(b"x"), 0o644).unwrap();
            archive.finish().unwrap()
        };
        assert_eq!(make(), make());
    }
}
