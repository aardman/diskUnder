//! File-type filters for indexing: which extensions a path's index includes or excludes.
//! Stored per path in `roots`, so `update` keeps using the filters an index was built with.

use std::path::Path;

/// Extensions are lowercase; "" means "no extension" (`none` on the command line and in the
/// database). An include list is an allowlist; exclude is applied after it.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct TypeFilter {
    pub(crate) include: Option<Vec<String>>,
    pub(crate) exclude: Vec<String>,
}

/// The extension filters match on: lowercase text after the last dot ("archive.tar.gz" → "gz"),
/// or "" when there isn't one
pub(crate) fn ext_of(path: &Path) -> String {
    path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

fn to_list(exts: &[String]) -> String {
    exts.iter().map(|e| if e.is_empty() { "none" } else { e.as_str() }).collect::<Vec<_>>().join(",")
}

fn from_list(s: &str) -> Vec<String> {
    let mut exts: Vec<String> = s
        .split(',')
        .map(|e| e.trim().to_lowercase())
        .filter(|e| !e.is_empty())
        .map(|e| if e == "none" { String::new() } else { e })
        .collect();
    exts.sort();
    exts.dedup();
    exts
}

impl TypeFilter {
    pub(crate) fn is_empty(&self) -> bool {
        self.include.is_none() && self.exclude.is_empty()
    }

    pub(crate) fn allows(&self, ext: &str) -> bool {
        self.include.as_ref().map_or(true, |inc| inc.iter().any(|e| e == ext)) && !self.exclude.iter().any(|e| e == ext)
    }

    /// From the `roots` columns (NULL when unset)
    pub(crate) fn from_db(include: Option<String>, exclude: Option<String>) -> Self {
        TypeFilter {
            include: include.map(|s| from_list(&s)),
            exclude: exclude.map(|s| from_list(&s)).unwrap_or_default(),
        }
    }

    /// For the `roots` columns
    pub(crate) fn to_db(&self) -> (Option<String>, Option<String>) {
        (
            self.include.as_ref().map(|l| to_list(l)),
            (!self.exclude.is_empty()).then(|| to_list(&self.exclude)),
        )
    }

    /// "include jpg,mov · exclude tmp", or "all file types"
    pub(crate) fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(inc) = &self.include {
            parts.push(format!("include {}", to_list(inc)));
        }
        if !self.exclude.is_empty() {
            parts.push(format!("exclude {}", to_list(&self.exclude)));
        }
        if parts.is_empty() {
            "all file types".to_string()
        } else {
            parts.join(" · ")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exts(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn matching() {
        let f = TypeFilter { include: Some(exts(&["jpg", "mov", ""])), exclude: exts(&["mov"]) };
        assert!(f.allows("jpg"));
        assert!(f.allows(""), "no extension, via the none token");
        assert!(!f.allows("mov"), "exclude wins over include");
        assert!(!f.allows("pdf"), "include is an allowlist");
        assert!(TypeFilter::default().allows("anything"));
        assert_eq!(ext_of(Path::new("/a/archive.TAR.GZ")), "gz");
        assert_eq!(ext_of(Path::new("/a/README")), "");
    }

    #[test]
    fn stored_form() {
        let f = TypeFilter { include: Some(exts(&["", "jpg"])), exclude: vec![] };
        assert_eq!(f.to_db(), (Some("none,jpg".to_string()), None));
        assert_eq!(TypeFilter::from_db(Some("JPG, none".into()), None), TypeFilter { include: Some(exts(&["", "jpg"])), exclude: vec![] });
        assert_eq!(f.describe(), "include none,jpg");
        assert_eq!(TypeFilter::default().describe(), "all file types");
    }
}
