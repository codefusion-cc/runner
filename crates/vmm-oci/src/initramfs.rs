//! The supervisor out of the guest's initramfs: the same verified binary a machine boots as `/init` is what a
//! container starts with, so both modes run one supervisor of one version.

use flate2::read::GzDecoder;
use std::io::Read;

/// The largest initramfs this unpacks: the guest's is a few megabytes.
const UNPACKED_MAX: u64 = 256 << 20;

/// The file `name` of a gzip-compressed `newc` cpio archive.
pub fn file(initramfs: &[u8], name: &str) -> Result<Vec<u8>, String> {
    let mut cpio = Vec::new();
    GzDecoder::new(initramfs)
        .take(UNPACKED_MAX)
        .read_to_end(&mut cpio)
        .map_err(|error| format!("the initramfs does not unpack: {error}"))?;
    let mut at = 0usize;
    let field = |header: &[u8], index: usize| -> Result<usize, String> {
        let text =
            std::str::from_utf8(&header[6 + index * 8..14 + index * 8]).map_err(|_| "a cpio header")?;
        usize::from_str_radix(text, 16).map_err(|_| "a cpio header".to_string())
    };
    loop {
        let header = cpio
            .get(at..at + 110)
            .ok_or_else(|| format!("the initramfs has no {name}"))?;
        if &header[..6] != b"070701" {
            return Err("the initramfs is no newc cpio archive".into());
        }
        let size = field(header, 6)?;
        let name_size = field(header, 11)?;
        let name_at = at + 110;
        let entry = cpio
            .get(name_at..name_at + name_size)
            .ok_or("the initramfs is cut short")?;
        let entry = entry.strip_suffix(b"\0").unwrap_or(entry);
        let data_at = (name_at + name_size).next_multiple_of(4);
        if entry == b"TRAILER!!!" {
            return Err(format!("the initramfs has no {name}"));
        }
        let data = cpio
            .get(data_at..data_at + size)
            .ok_or("the initramfs is cut short")?;
        if entry == name.as_bytes() {
            return Ok(data.to_vec());
        }
        at = (data_at + size).next_multiple_of(4);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;

    fn newc(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut add = |name: &str, data: &[u8]| {
            let fields = [1, 0o100755, 0, 0, 1, 0, data.len(), 0, 0, 0, 0, name.len() + 1, 0];
            out.extend_from_slice(b"070701");
            for value in fields {
                out.extend_from_slice(format!("{value:08X}").as_bytes());
            }
            out.extend_from_slice(name.as_bytes());
            out.push(0);
            out.resize(out.len().next_multiple_of(4), 0);
            out.extend_from_slice(data);
            out.resize(out.len().next_multiple_of(4), 0);
        };
        for (name, data) in entries {
            add(name, data);
        }
        add("TRAILER!!!", b"");
        let mut gz = GzEncoder::new(Vec::new(), Compression::fast());
        gz.write_all(&out).unwrap();
        gz.finish().unwrap()
    }

    #[test]
    fn the_init_is_found_among_the_others() {
        let archive = newc(&[
            ("etc/modules", b"virtio\n"),
            ("init", b"\x7fELF supervisor"),
            ("lib", b""),
        ]);
        assert_eq!(file(&archive, "init").unwrap(), b"\x7fELF supervisor");
        assert_eq!(file(&archive, "etc/modules").unwrap(), b"virtio\n");
        assert!(file(&archive, "missing").unwrap_err().contains("no missing"));
    }

    #[test]
    fn what_is_no_initramfs_is_refused() {
        assert!(file(b"not gzip at all", "init").is_err());
        let mut gz = GzEncoder::new(Vec::new(), Compression::fast());
        gz.write_all(b"070702 not newc").unwrap();
        assert!(file(&gz.finish().unwrap(), "init").is_err());
        let whole = newc(&[("init", b"0123456789")]);
        let mut cut = GzEncoder::new(Vec::new(), Compression::fast());
        let mut plain = Vec::new();
        GzDecoder::new(&whole[..]).read_to_end(&mut plain).unwrap();
        cut.write_all(&plain[..120]).unwrap();
        assert!(file(&cut.finish().unwrap(), "init").is_err());
    }
}
