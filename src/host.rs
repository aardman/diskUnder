//! This machine: its drives and mounts (local and network) and a few facts about it — for
//! `diskunder host` and the app's Host menu

use std::path::Path;

use crate::fmt::fmt_bytes;
use crate::output::{write_text_table, Align, Out};

#[derive(Clone)]
pub(crate) struct Mount {
    pub(crate) point: String,  // where it's mounted
    pub(crate) source: String, // device, or server/share for network mounts
    pub(crate) fstype: String,
    pub(crate) total: u64,
    pub(crate) free: u64, // available to unprivileged users
    pub(crate) network: bool,
}

impl Mount {
    pub(crate) fn used(&self) -> u64 {
        self.total.saturating_sub(self.free)
    }
}

/// Filesystems reached over the network
fn is_network(fstype: &str) -> bool {
    let t = fstype.to_lowercase();
    ["smbfs", "cifs", "smb3", "nfs", "nfs4", "afpfs", "webdav", "davfs", "fuse.sshfs", "sshfs", "9p", "ceph", "glusterfs", "fuse.rclone"]
        .iter()
        .any(|n| t == *n)
        || t.starts_with("nfs")
}

/// Mounts that only matter to the operating system — not somewhere you'd index
fn is_system(point: &str, fstype: &str) -> bool {
    const PSEUDO: [&str; 24] = [
        "proc", "sysfs", "devfs", "devtmpfs", "devpts", "tmpfs", "cgroup", "cgroup2", "autofs", "mqueue", "debugfs",
        "tracefs", "securityfs", "pstore", "bpf", "fusectl", "configfs", "hugetlbfs", "binfmt_misc", "nsfs",
        "rpc_pipefs", "squashfs", "overlay", "efivarfs",
    ];
    PSEUDO.contains(&fstype)
        || point.starts_with("/System/Volumes/") && point != "/System/Volumes/Data"
        || point.starts_with("/dev")
        || point.starts_with("/proc")
        || point.starts_with("/sys")
        || point.starts_with("/run")
        || point.starts_with("/snap/")
        || point.starts_with("/private/var/vm")
        || point.starts_with("/Library/Developer/CoreSimulator")
}

#[cfg(target_os = "macos")]
fn raw_mounts() -> Vec<Mount> {
    use std::ffi::CStr;
    let mut buf: *mut libc::statfs = std::ptr::null_mut();
    // SAFETY: getmntinfo returns a count and points `buf` at a static array of that many
    // statfs entries, valid until the next call; we copy what we need out immediately
    let n = unsafe { libc::getmntinfo(&mut buf, libc::MNT_NOWAIT) };
    if n <= 0 || buf.is_null() {
        return Vec::new();
    }
    let entries = unsafe { std::slice::from_raw_parts(buf, n as usize) };
    entries
        .iter()
        .map(|s| {
            let text = |chars: &[libc::c_char]| unsafe { CStr::from_ptr(chars.as_ptr()) }.to_string_lossy().into_owned();
            let fstype = text(&s.f_fstypename);
            let block = s.f_bsize as u64;
            Mount {
                point: text(&s.f_mntonname),
                source: text(&s.f_mntfromname),
                network: is_network(&fstype),
                total: s.f_blocks * block,
                free: s.f_bavail * block,
                fstype,
            }
        })
        .collect()
}

#[cfg(all(unix, not(target_os = "macos")))]
fn raw_mounts() -> Vec<Mount> {
    use std::ffi::CString;
    // /proc/mounts escapes spaces and tabs in paths as octal
    let unescape = |s: &str| s.replace("\\040", " ").replace("\\011", "\t").replace("\\134", "\\");
    std::fs::read_to_string("/proc/mounts")
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            let (source, point, fstype) = (unescape(f.next()?), unescape(f.next()?), f.next()?.to_string());
            let (mut total, mut free) = (0, 0);
            if let Ok(c) = CString::new(point.clone()) {
                let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
                // SAFETY: c is a valid C string and st a valid statvfs to fill in
                if unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0 {
                    total = st.f_blocks as u64 * st.f_frsize as u64;
                    free = st.f_bavail as u64 * st.f_frsize as u64;
                }
            }
            Some(Mount { network: is_network(&fstype), point, source, fstype, total, free })
        })
        .collect()
}

#[cfg(not(unix))]
fn raw_mounts() -> Vec<Mount> {
    Vec::new()
}

/// Mounts you might index, local first, each sorted by mount point
pub(crate) fn mounts() -> Vec<Mount> {
    let mut list: Vec<Mount> = raw_mounts().into_iter().filter(|m| !is_system(&m.point, &m.fstype)).collect();
    list.sort_by(|a, b| a.network.cmp(&b.network).then_with(|| a.point.cmp(&b.point)));
    list.dedup_by(|a, b| a.point == b.point);
    list
}

/// (label, value) facts about this machine and diskunder's set-up
pub(crate) fn host_info(store: &Path, db: &str, log_folder: &str) -> Vec<(&'static str, String)> {
    let cpus = std::thread::available_parallelism().map_or("?".to_string(), |n| n.to_string());
    vec![
        ("host", crate::runlog::host()),
        ("system", format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)),
        ("user", crate::runlog::user()),
        ("CPUs", format!("{} (keep --threads low for network shares)", cpus)),
        ("diskunder", crate::cli::display_version().to_string()),
        ("jobs store", store.display().to_string()),
        ("database", db.to_string()),
        ("run logs", if log_folder.is_empty() { "next to each database".to_string() } else { log_folder.to_string() }),
    ]
}

/// Drives as table rows: mount point, filesystem, size, used, free, source
pub(crate) fn mount_rows(list: &[Mount]) -> Vec<Vec<String>> {
    list.iter()
        .map(|m| {
            let pct = if m.total > 0 { m.used() as f64 * 100.0 / m.total as f64 } else { 0.0 };
            vec![
                m.point.clone(),
                m.fstype.clone(),
                fmt_bytes(m.total),
                format!("{} ({:.0}%)", fmt_bytes(m.used()), pct),
                fmt_bytes(m.free),
                m.source.clone(),
            ]
        })
        .collect()
}

/// `diskunder host`
pub(crate) fn run_host_command(store: &Path, db: &str, log_folder: &str) -> i32 {
    let mut out = Out::new();
    for (label, value) in host_info(store, db, log_folder) {
        out.write(&format!("{:<11} {}\n", label, value));
    }
    let list = mounts();
    let headers = ["MOUNT", "TYPE", "SIZE", "USED", "FREE", "SOURCE"];
    let aligns = [Align::Left, Align::Left, Align::Right, Align::Right, Align::Right, Align::Left];
    for (title, network) in [("Local drives", false), ("Network mounts", true)] {
        let part: Vec<&Mount> = list.iter().filter(|m| m.network == network).collect();
        out.write(&format!("\n{}\n", title));
        if part.is_empty() {
            out.write("  (none)\n");
        } else {
            let owned: Vec<Mount> = part.into_iter().cloned().collect();
            write_text_table(&mut out, &headers, &aligns, &mount_rows(&owned));
        }
    }
    out.flush();
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_and_system_mounts_are_told_apart() {
        assert!(is_network("smbfs") && is_network("nfs4") && is_network("cifs") && is_network("fuse.sshfs"));
        assert!(!is_network("apfs") && !is_network("ext4") && !is_network("xfs"));
        assert!(is_system("/proc", "proc") && is_system("/System/Volumes/VM", "apfs") && is_system("/run/user/1000", "tmpfs"));
        assert!(!is_system("/", "apfs") && !is_system("/Volumes/NAS", "smbfs") && !is_system("/System/Volumes/Data", "apfs"));
    }

    #[test]
    fn this_machine_has_a_root_mount() {
        let list = mounts();
        assert!(list.iter().any(|m| m.point == "/" || m.point == "/System/Volumes/Data"), "{:?}", list.iter().map(|m| &m.point).collect::<Vec<_>>());
        assert!(list.iter().all(|m| m.free <= m.total));
    }
}
