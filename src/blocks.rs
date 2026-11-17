//! Making a copy take no space of its own: sharing its blocks with the kept copy (Linux
//! `FIDEDUPERANGE`, on XFS and Btrfs), or replacing it with a hard link to it

use std::{
    fs,
    io::{self, Read},
    path::Path,
};

/// How sharing a copy's blocks went
#[derive(Debug, PartialEq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) enum Shared {
    Now,     // its blocks are the kept copy's: the space is freed
    Already, // they were before (an earlier dedupe): nothing more to free
    Differs, // the kernel found the bytes differ, so nothing was shared
}

#[cfg(target_os = "linux")]
mod linux {
    use std::{fs::File, os::unix::io::AsRawFd};

    // _IOWR(0x94, 54, struct file_dedupe_range) and _IOWR('f', 11, struct fiemap)
    const FIDEDUPERANGE: libc::c_ulong = 0xC018_9436;
    const FS_IOC_FIEMAP: libc::c_ulong = 0xC020_660B;
    const FILE_DEDUPE_RANGE_DIFFERS: i32 = 1;
    /// The kernel compares and shares at most this much per call on some filesystems
    const CHUNK: u64 = 16 << 20;

    #[repr(C)]
    struct DedupeRange {
        src_offset: u64,
        src_length: u64,
        dest_count: u16,
        reserved1: u16,
        reserved2: u32,
        // one file_dedupe_range_info
        dest_fd: i64,
        dest_offset: u64,
        bytes_deduped: u64,
        status: i32,
        reserved: u32,
    }

    /// Share `dest`'s blocks with `src` from `offset`: Ok(Some(bytes)) when shared, Ok(None)
    /// when the bytes differ
    pub(super) fn dedupe(src: &File, dest: &File, offset: u64, len: u64) -> std::io::Result<Option<u64>> {
        let mut r = DedupeRange {
            src_offset: offset,
            src_length: len,
            dest_count: 1,
            reserved1: 0,
            reserved2: 0,
            dest_fd: dest.as_raw_fd() as i64,
            dest_offset: offset,
            bytes_deduped: 0,
            status: 0,
            reserved: 0,
        };
        // SAFETY: r is a file_dedupe_range with room for its one info record
        if unsafe { libc::ioctl(src.as_raw_fd(), FIDEDUPERANGE as _, &mut r) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        match r.status {
            FILE_DEDUPE_RANGE_DIFFERS => Ok(None),
            s if s < 0 => Err(std::io::Error::from_raw_os_error(-s)),
            _ => Ok(Some(r.bytes_deduped)),
        }
    }

    pub(super) fn chunk() -> u64 {
        CHUNK
    }

    #[repr(C)]
    struct Fiemap {
        start: u64,
        length: u64,
        flags: u32,
        mapped_extents: u32,
        extent_count: u32,
        reserved: u32,
        extents: [Extent; 32],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Extent {
        logical: u64,
        physical: u64,
        length: u64,
        reserved64: [u64; 2],
        flags: u32,
        reserved: [u32; 3],
    }

    const FIEMAP_FLAG_SYNC: u32 = 0x1;
    const EXTENT_LAST: u32 = 0x1;
    // Unknown, delayed, encoded or inline: no physical place to compare
    const EXTENT_UNPLACED: u32 = 0x2 | 0x4 | 0x8 | 0x200;

    /// Where a file's data is on disk: (logical, physical, length) per extent; None when that
    /// can't be told
    pub(super) fn extents(f: &File) -> Option<Vec<(u64, u64, u64)>> {
        let mut out = Vec::new();
        let mut start = 0u64;
        loop {
            let mut m = Fiemap {
                start,
                length: u64::MAX - start,
                flags: FIEMAP_FLAG_SYNC,
                mapped_extents: 0,
                extent_count: 32,
                reserved: 0,
                extents: [Extent { logical: 0, physical: 0, length: 0, reserved64: [0; 2], flags: 0, reserved: [0; 3] }; 32],
            };
            // SAFETY: m is a fiemap with room for extent_count extents
            if unsafe { libc::ioctl(f.as_raw_fd(), FS_IOC_FIEMAP as _, &mut m) } != 0 {
                return None;
            }
            let found = &m.extents[..m.mapped_extents as usize];
            for e in found {
                if e.flags & EXTENT_UNPLACED != 0 {
                    return None;
                }
                out.push((e.logical, e.physical, e.length));
            }
            match found.last() {
                Some(e) if e.flags & EXTENT_LAST == 0 => start = e.logical + e.length,
                _ => return Some(out),
            }
        }
    }
}

/// Share `copy`'s blocks with `keep` — the kernel checks the bytes match first, and every path
/// keeps working. Linux only, on a filesystem that can (XFS with reflink, Btrfs).
#[cfg(target_os = "linux")]
pub(crate) fn share(keep: &Path, copy: &Path, size: u64) -> io::Result<Shared> {
    if already_shared(keep, copy) {
        return Ok(Shared::Already);
    }
    let src = crate::scan::open_for_reading(keep)?;
    let dest = crate::scan::open_for_reading(copy)?;
    let mut offset = 0;
    while offset < size {
        let len = linux::chunk().min(size - offset);
        match linux::dedupe(&src, &dest, offset, len)? {
            None => return Ok(Shared::Differs),
            Some(0) => return Err(io::Error::other("the kernel shared nothing")),
            Some(n) => offset += n,
        }
    }
    Ok(Shared::Now)
}

/// Whether two files' data is already in the same blocks on disk (shared earlier)
#[cfg(target_os = "linux")]
pub(crate) fn already_shared(a: &Path, b: &Path) -> bool {
    let extents = |p: &Path| crate::scan::open_for_reading(p).ok().and_then(|f| linux::extents(&f));
    matches!((extents(a), extents(b)), (Some(x), Some(y)) if !x.is_empty() && x == y)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn already_shared(_a: &Path, _b: &Path) -> bool {
    false
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn share(_keep: &Path, _copy: &Path, _size: u64) -> io::Result<Shared> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "sharing blocks needs Linux"))
}

/// Whether two files hold the same bytes, read in full
pub(crate) fn same_bytes(a: &Path, b: &Path) -> io::Result<bool> {
    let (mut fa, mut fb) = (crate::scan::open_for_reading(a)?, crate::scan::open_for_reading(b)?);
    if fa.metadata()?.len() != fb.metadata()?.len() {
        return Ok(false);
    }
    let (mut ba, mut bb) = (vec![0u8; 1 << 20], vec![0u8; 1 << 20]);
    loop {
        let n = read_full(&mut fa, &mut ba)?;
        if n != read_full(&mut fb, &mut bb)? || ba[..n] != bb[..n] {
            return Ok(false);
        }
        if n == 0 {
            return Ok(true);
        }
    }
}

fn read_full(f: &mut fs::File, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// Replace `copy` with a hard link to `keep`, in one step: a link made beside it is renamed
/// over it, so the path is never missing
pub(crate) fn link_over(keep: &Path, copy: &Path) -> io::Result<()> {
    let name = copy.file_name().ok_or_else(|| io::Error::other("no file name"))?;
    let temp = copy.with_file_name(format!(".{}.diskunder-link-{}", name.to_string_lossy(), std::process::id()));
    fs::hard_link(keep, &temp)?;
    fs::rename(&temp, copy).inspect_err(|_| {
        let _ = fs::remove_file(&temp);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linking_over_a_copy() {
        let dir = std::env::temp_dir().join(format!("diskunder-blocks-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let (a, b, c) = (dir.join("a"), dir.join("b"), dir.join("c"));
        fs::write(&a, "same bytes").unwrap();
        fs::write(&b, "same bytes").unwrap();
        fs::write(&c, "same bytez").unwrap();
        assert!(same_bytes(&a, &b).unwrap());
        assert!(!same_bytes(&a, &c).unwrap());
        link_over(&a, &b).unwrap();
        use std::os::unix::fs::MetadataExt;
        assert_eq!(fs::metadata(&a).unwrap().ino(), fs::metadata(&b).unwrap().ino());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 3, "no temporary link left");
        fs::remove_dir_all(&dir).unwrap();
    }
}
