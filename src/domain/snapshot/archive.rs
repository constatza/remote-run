//! Counting the files in a tar stream, to compare with what the tree should hold.

use anyhow::{Result, bail};

const BLOCK: usize = 512;
/// Where the size field of a tar header starts and ends (octal digits).
const SIZE_FIELD: std::ops::Range<usize> = 124..136;
const TYPE_FLAG: usize = 156;

fn is_end(block: &[u8]) -> bool {
    block.iter().all(|byte| *byte == 0)
}

fn data_len(header: &[u8]) -> Result<usize> {
    let text = std::str::from_utf8(&header[SIZE_FIELD])?.trim_matches(['\0', ' ']);
    if text.is_empty() {
        return Ok(0);
    }
    Ok(usize::from_str_radix(text, 8)?)
}

/// The regular files and symlinks in a tar archive (pure). Directories and the pax headers git
/// writes for long names are not files. A header that cannot be read, or data that is cut short, is
/// an error: a count that might be wrong must not pass for a match.
pub fn count_files(tar: &[u8]) -> Result<usize> {
    let mut at = 0;
    let mut files = 0;
    while at + BLOCK <= tar.len() {
        let header = &tar[at..at + BLOCK];
        if is_end(header) {
            return Ok(files);
        }
        let size = data_len(header)?;
        match header[TYPE_FLAG] {
            b'0' | 0 | b'7' | b'2' => files += 1,
            b'5' | b'x' | b'g' => {}
            other => bail!("unexpected entry type {:?} in the archive", other as char),
        }
        let data = size.div_ceil(BLOCK) * BLOCK;
        at = at
            .checked_add(BLOCK + data)
            .filter(|end| *end <= tar.len())
            .ok_or_else(|| anyhow::anyhow!("the archive ends inside an entry"))?;
    }
    bail!("the archive has no end marker")
}

/// A ustar archive of `entries` for tests: `(type flag, name, content)`.
#[cfg(test)]
pub fn tar_of(entries: &[(u8, &str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (flag, name, data) in entries {
        let mut header = [0u8; BLOCK];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..107].copy_from_slice(b"0000644");
        let size = format!("{:011o}", data.len());
        header[SIZE_FIELD.start..SIZE_FIELD.start + 11].copy_from_slice(size.as_bytes());
        header[TYPE_FLAG] = *flag;
        header[257..262].copy_from_slice(b"ustar");
        header[148..156].copy_from_slice(b"        ");
        let sum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
        header[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
        out.extend_from_slice(&header);
        out.extend_from_slice(data);
        out.resize(out.len().div_ceil(BLOCK) * BLOCK, 0);
    }
    out.extend_from_slice(&[0u8; 2 * BLOCK]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_and_symlinks_count_and_directories_and_pax_headers_do_not() {
        let tar = tar_of(&[
            (b'g', "pax_global_header", b"52 comment=abc\n"),
            (b'5', "src/", b""),
            (b'0', "src/a.py", b"print(1)\n"),
            (b'x', "pax", &[b'x'; 700]),
            (b'0', "src/long", &[b'y'; 1500]),
            (b'2', "link", b""),
            (0, "old-style", b"z"),
        ]);
        assert_eq!(count_files(&tar).unwrap(), 4);
        assert_eq!(count_files(&tar_of(&[])).unwrap(), 0);
    }

    #[test]
    fn a_truncated_or_unreadable_archive_is_an_error_never_a_count() {
        let tar = tar_of(&[(b'0', "a", &[b'x'; 1000])]);
        assert!(count_files(&tar[..BLOCK + 100]).is_err(), "cut inside data");
        assert!(count_files(&tar[..BLOCK]).is_err(), "no data, no end");
        assert!(count_files(&[]).is_err(), "no end marker");
        let hardlink = tar_of(&[(b'1', "h", b"")]);
        assert!(count_files(&hardlink).is_err(), "a type git never writes");
        let mut bad_size = tar_of(&[(b'0', "a", b"")]);
        bad_size[SIZE_FIELD.start] = b'9';
        assert!(count_files(&bad_size).is_err(), "9 is not an octal digit");
    }
}
