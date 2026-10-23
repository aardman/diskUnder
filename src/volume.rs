//! Which volume a path is on, as an identity that stays the same however and wherever the
//! volume is mounted: `nfs://server/export`, `smb://server/share`, `uuid:…` for a Linux disk.
//! Lets queries count a file reached through two mounts (or two overlapping indexed paths)
//! once, and tells later dedupe which copies share a filesystem.

use std::path::Path;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Volume {
    pub(crate) id: String,   // the identity, e.g. smb://nas01/projects
    pub(crate) path: String, // where the indexed path is inside the volume, e.g. /2024/film
}

/// Network filesystems, as `host::is_network` lists them, keyed by URL scheme
fn scheme(fstype: &str) -> Option<&'static str> {
    let t = fstype.to_lowercase();
    match t.as_str() {
        "smbfs" | "cifs" | "smb3" => Some("smb"),
        "afpfs" => Some("afp"),
        "webdav" | "davfs" => Some("dav"),
        "fuse.sshfs" | "sshfs" => Some("sftp"),
        _ if t.starts_with("nfs") => Some("nfs"),
        _ => None,
    }
}

/// A server name as written in different ways — `NAS01`, `nas01.studio.local` — reduced to
/// one form: lowercase, without the domain (IP addresses are kept whole)
fn server_name(s: &str) -> String {
    let s = s.trim_matches(|c| c == '[' || c == ']').to_lowercase();
    let s = s.split_once(':').map_or(s.as_str(), |(h, port)| if port.chars().all(|c| c.is_ascii_digit()) { h } else { s.as_str() }).to_string();
    if s.parse::<std::net::IpAddr>().is_ok() || s.contains(':') {
        s
    } else {
        s.split('.').next().unwrap_or(&s).to_string()
    }
}

/// The identity of a network mount's source, or None for local filesystems
pub(crate) fn network_id(fstype: &str, source: &str) -> Option<String> {
    let scheme = scheme(fstype)?;
    let (server, rest) = match scheme {
        // server:/export/path (IPv6 servers in brackets)
        "nfs" => {
            let i = source.find(":/")?;
            (source[..i].to_string(), source[i + 1..].to_string())
        }
        "sftp" => {
            let (host, path) = source.split_once(':').unwrap_or((source, ""));
            (host.rsplit('@').next().unwrap_or(host).to_string(), format!("/{}", path.trim_start_matches('/')))
        }
        // //[domain;][user[:password]@]server/share/path, or a URL for WebDAV
        _ => {
            let s = source.split_once("://").map_or(source, |(_, r)| r).trim_start_matches('/');
            let (host, path) = s.split_once('/').unwrap_or((s, ""));
            let host = host.rsplit('@').next().unwrap_or(host);
            (host.to_string(), format!("/{}", path))
        }
    };
    let mut rest = rest.trim_end_matches('/').to_string();
    if scheme == "smb" || scheme == "afp" {
        rest = rest.to_lowercase(); // share names aren't case-sensitive
    }
    Some(format!("{}://{}{}", scheme, server_name(&server), rest))
}

/// The mount a path is on: (mount point, source, filesystem type, the mount's own root within
/// its filesystem — "/" except for bind mounts)
#[cfg(target_os = "macos")]
fn mount_of(path: &Path) -> Option<(String, String, String, String)> {
    use std::ffi::{CStr, CString};
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: c is a valid C string and st a valid statfs to fill in
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let text = |chars: &[libc::c_char]| unsafe { CStr::from_ptr(chars.as_ptr()) }.to_string_lossy().into_owned();
    Some((text(&st.f_mntonname), text(&st.f_mntfromname), text(&st.f_fstypename), "/".into()))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn mount_of(path: &Path) -> Option<(String, String, String, String)> {
    // mountinfo: id parent major:minor root mount-point options [optional…] - fstype source super
    let unescape = |s: &str| s.replace("\\040", " ").replace("\\011", "\t").replace("\\012", "\n").replace("\\134", "\\");
    let text = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    let mut best: Option<(String, String, String, String)> = None;
    for line in text.lines() {
        let (left, right) = line.split_once(" - ")?;
        let f: Vec<&str> = left.split(' ').collect();
        let r: Vec<&str> = right.split(' ').collect();
        if f.len() < 5 || r.len() < 2 {
            continue;
        }
        let point = unescape(f[4]);
        if path.starts_with(&point) && best.as_ref().is_none_or(|b| point.len() >= b.0.len()) {
            best = Some((point, unescape(r[1]), r[0].to_string(), unescape(f[3])));
        }
    }
    best
}

#[cfg(not(unix))]
fn mount_of(_path: &Path) -> Option<(String, String, String, String)> {
    None
}

/// A local disk's identity: its filesystem UUID where Linux lists one, else this machine and
/// the device
fn local_id(source: &str) -> String {
    #[cfg(all(unix, not(target_os = "macos")))]
    if let (Ok(dev), Ok(dir)) = (std::fs::canonicalize(source), std::fs::read_dir("/dev/disk/by-uuid")) {
        for e in dir.flatten() {
            if std::fs::canonicalize(e.path()).is_ok_and(|p| p == dev) {
                return format!("uuid:{}", e.file_name().to_string_lossy().to_lowercase());
            }
        }
    }
    format!("local://{}{}", crate::runlog::host().to_lowercase(), if source.starts_with('/') { source.to_string() } else { format!("/{}", source) })
}

/// The volume an (absolute, canonical) path is on
pub(crate) fn volume_of(path: &Path) -> Option<Volume> {
    let (point, source, fstype, mount_root) = mount_of(path)?;
    // macOS firmlinks put the Data volume's folders at / (e.g. /Users), not under its mount
    // point, so a path outside its mount point is already its place on the volume
    let inside = path.strip_prefix(&point).unwrap_or(path.strip_prefix("/").unwrap_or(path));
    let mut within = Path::new(&mount_root).join(inside).to_string_lossy().into_owned();
    if !within.starts_with('/') {
        within.insert(0, '/');
    }
    let id = network_id(&fstype, &source).unwrap_or_else(|| local_id(&source));
    Some(Volume { id, path: within })
}

/// Whether two indexed paths overlap: on the same volume, one inside (or the same as) the other
pub(crate) fn overlaps(a: &Volume, b: &Volume) -> bool {
    let inside = |x: &str, y: &str| x == y || y == "/" || x.starts_with(&format!("{}/", y));
    a.id == b.id && (inside(&a.path, &b.path) || inside(&b.path, &a.path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_sources_are_normalised() {
        assert_eq!(network_id("smbfs", "//jo@NAS01.studio.local/Projects").as_deref(), Some("smb://nas01/projects"));
        assert_eq!(network_id("cifs", "//nas01/projects/2024").as_deref(), Some("smb://nas01/projects/2024"));
        assert_eq!(network_id("smbfs", "//STUDIO;jo@nas01/Projects/").as_deref(), Some("smb://nas01/projects"));
        assert_eq!(network_id("nfs", "nas01.studio.local:/export/Proj").as_deref(), Some("nfs://nas01/export/Proj"));
        assert_eq!(network_id("nfs4", "10.0.0.5:/export").as_deref(), Some("nfs://10.0.0.5/export"));
        assert_eq!(network_id("nfs", "[fe80::1]:/data").as_deref(), Some("nfs://fe80::1/data"));
        assert_eq!(network_id("fuse.sshfs", "jo@nas01:/srv/x").as_deref(), Some("sftp://nas01/srv/x"));
        assert_eq!(network_id("apfs", "/dev/disk3s5"), None);
    }

    #[test]
    fn overlapping_paths_on_a_volume() {
        let v = |id: &str, p: &str| Volume { id: id.into(), path: p.into() };
        assert!(overlaps(&v("smb://nas/p", "/a"), &v("smb://nas/p", "/a/b")));
        assert!(overlaps(&v("smb://nas/p", "/"), &v("smb://nas/p", "/x")));
        assert!(!overlaps(&v("smb://nas/p", "/a"), &v("smb://nas/p", "/ab")));
        assert!(!overlaps(&v("smb://nas/p", "/a"), &v("smb://nas/q", "/a")));
    }

    #[test]
    fn this_machine_knows_its_volumes() {
        let dir = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let v = volume_of(&dir).expect("a volume for the temp folder");
        assert!(!v.id.is_empty() && v.path.starts_with('/'), "{:?}", v);
        assert!(v.path.ends_with(dir.file_name().unwrap().to_str().unwrap()), "{:?}", v);
    }
}
