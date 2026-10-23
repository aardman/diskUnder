//! Indexing filters: which file types and which paths a path's index leaves out. Stored per
//! path in `roots`, so `update` keeps using the filters an index was built with.

use globset::{GlobBuilder, GlobMatcher};
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

// ─── Path patterns ───────────────────────────────────────────────────────────

/// One `.gitignore`-style pattern. Case-sensitive: indexes can live on Linux filesystems even
/// when diskunder runs on a Mac.
#[derive(Clone, Debug)]
struct Pattern {
    anchored: bool,   // contains "/" (or starts with one): matched from the indexed root
    dir_only: bool,   // ends with "/": matches folders only
    glob: GlobMatcher,
    parts: Vec<(String, GlobMatcher)>, // per component, to tell whether it could match deeper
}

fn glob(text: &str) -> Result<GlobMatcher, String> {
    GlobBuilder::new(text)
        .literal_separator(true) // `*` stays within one folder name; `**` crosses folders
        .build()
        .map(|g| g.compile_matcher())
        .map_err(|e| format!("bad pattern '{}': {}", text, e))
}

impl Pattern {
    fn compile(pattern: &str) -> Result<Self, String> {
        let t = pattern.trim();
        let dir_only = t.ends_with('/');
        let t = t.trim_end_matches('/');
        let t = t.strip_prefix("./").unwrap_or(t);
        let anchored = t.starts_with('/') || t.contains('/');
        let t = t.trim_start_matches('/');
        if t.is_empty() {
            return Err(format!("empty path pattern '{}'", pattern));
        }
        let parts = t.split('/').map(|c| Ok((c.to_string(), glob(c)?))).collect::<Result<_, String>>()?;
        Ok(Pattern { anchored, dir_only, glob: glob(t)?, parts })
    }

    /// Whether the pattern matches this path, or any folder above it. `comps` is the path
    /// relative to the indexed root.
    fn matches(&self, comps: &[&str], is_dir: bool) -> bool {
        let n = comps.len();
        // A folders-only pattern can't match the last component of a file
        let eligible = |k: usize| k < n || is_dir || !self.dir_only;
        if self.anchored {
            (1..=n).any(|k| eligible(k) && self.glob.is_match(comps[..k].join("/")))
        } else {
            comps.iter().enumerate().any(|(i, c)| eligible(i + 1) && self.glob.is_match(c))
        }
    }

    /// Whether the pattern could match something inside folder `dir` (relative to the root)
    fn could_match_below(&self, dir: &[&str]) -> bool {
        if !self.anchored {
            return true; // names can match at any depth
        }
        for (i, d) in dir.iter().enumerate() {
            match self.parts.get(i) {
                None => return false,
                Some((text, _)) if text.contains("**") => return true,
                Some((_, g)) if !g.is_match(d) => return false,
                Some(_) => {}
            }
        }
        self.parts.len() > dir.len()
    }
}

/// Paths left out of an index: anything matching an exclude pattern (or under a folder that
/// does), unless an include pattern matches it too — includes are exceptions, not an allowlist
#[derive(Clone, Debug, Default)]
pub(crate) struct PathFilter {
    pub(crate) exclude: Vec<String>,
    pub(crate) include: Vec<String>,
    exclude_pats: Vec<Pattern>,
    include_pats: Vec<Pattern>,
}

impl PartialEq for PathFilter {
    fn eq(&self, other: &Self) -> bool {
        self.exclude == other.exclude && self.include == other.include
    }
}

/// Checks a pattern from the command line; returns it trimmed
pub(crate) fn parse_path_pattern(s: &str) -> Result<String, String> {
    Pattern::compile(s).map(|_| s.trim().to_string())
}

impl PathFilter {
    pub(crate) fn new(exclude: Vec<String>, include: Vec<String>) -> Result<Self, String> {
        let compile = |v: &[String]| v.iter().map(|p| Pattern::compile(p)).collect::<Result<Vec<_>, _>>();
        Ok(PathFilter { exclude_pats: compile(&exclude)?, include_pats: compile(&include)?, exclude, include })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.exclude.is_empty() && self.include.is_empty()
    }

    /// Whether this path (relative to the root) is left out of the index
    pub(crate) fn excludes(&self, comps: &[&str], is_dir: bool) -> bool {
        !comps.is_empty()
            && self.exclude_pats.iter().any(|p| p.matches(comps, is_dir))
            && !self.include_pats.iter().any(|p| p.matches(comps, is_dir))
    }

    /// Whether the walk can skip this folder entirely: it's excluded, and no include pattern
    /// could match anything inside it
    pub(crate) fn prunes(&self, dir: &[&str]) -> bool {
        self.excludes(dir, true) && !self.include_pats.iter().any(|p| p.could_match_below(dir))
    }

    /// For the `roots` columns: one pattern per line, NULL when empty
    pub(crate) fn to_db(&self) -> (Option<String>, Option<String>) {
        let join = |v: &[String]| (!v.is_empty()).then(|| v.join("\n"));
        (join(&self.exclude), join(&self.include))
    }

    pub(crate) fn from_db(exclude: Option<String>, include: Option<String>) -> Self {
        let split = |s: Option<String>| -> Vec<String> {
            s.map(|s| s.lines().map(str::to_string).filter(|l| !l.is_empty()).collect()).unwrap_or_default()
        };
        // Patterns were checked when they were stored
        PathFilter::new(split(exclude), split(include)).unwrap_or_default()
    }

    /// "exclude .snapshot, @eaDir · except cache/keep", or "no path filters"
    pub(crate) fn describe(&self) -> String {
        let mut parts = Vec::new();
        if !self.exclude.is_empty() {
            parts.push(format!("exclude {}", self.exclude.join(", ")));
        }
        if !self.include.is_empty() {
            parts.push(format!("except {}", self.include.join(", ")));
        }
        if parts.is_empty() {
            "no path filters".to_string()
        } else {
            parts.join(" · ")
        }
    }
}

/// A path relative to the indexed root, as components ("" components never appear)
pub(crate) fn rel_components<'a>(path: &'a Path, root: &Path) -> Vec<std::borrow::Cow<'a, str>> {
    path.strip_prefix(root)
        .map(|rel| rel.components().map(|c| c.as_os_str().to_string_lossy()).collect())
        .unwrap_or_default()
}

// ─── Both together ───────────────────────────────────────────────────────────

/// Everything that decides what an index leaves out
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct IndexFilter {
    pub(crate) types: TypeFilter,
    pub(crate) paths: PathFilter,
}

impl IndexFilter {
    /// Whether a file is left out, by its type or its path
    pub(crate) fn skips_file(&self, path: &Path, root: &Path) -> bool {
        if !self.types.allows(&ext_of(path)) {
            return true;
        }
        let comps = rel_components(path, root);
        let comps: Vec<&str> = comps.iter().map(|c| c.as_ref()).collect();
        self.paths.excludes(&comps, false)
    }

    /// Whether the walk can skip a folder without looking inside
    pub(crate) fn prunes_dir(&self, path: &Path, root: &Path) -> bool {
        let comps = rel_components(path, root);
        let comps: Vec<&str> = comps.iter().map(|c| c.as_ref()).collect();
        !comps.is_empty() && self.paths.prunes(&comps)
    }

    /// Whether an indexed row's path is excluded by the path filters (used for rows the walk
    /// never reached because their folder was skipped)
    pub(crate) fn excludes_row(&self, path: &str, root: &Path) -> bool {
        let comps = rel_components(Path::new(path), root);
        let comps: Vec<&str> = comps.iter().map(|c| c.as_ref()).collect();
        self.paths.excludes(&comps, false)
    }
}

/// Filters given on the command line. Each kind replaces the stored filters of that kind;
/// a kind not given keeps what's stored.
#[derive(Clone, Debug, Default)]
pub(crate) struct Overrides {
    pub(crate) types: Option<TypeFilter>,
    pub(crate) paths: Option<PathFilter>,
}

impl Overrides {
    pub(crate) fn apply(&self, stored: &IndexFilter) -> IndexFilter {
        IndexFilter {
            types: self.types.clone().unwrap_or_else(|| stored.types.clone()),
            paths: self.paths.clone().unwrap_or_else(|| stored.paths.clone()),
        }
    }
}

/// Lines describing the filters a run uses and how they differ from what was stored.
/// `recorded` is None when the path has no recorded runs; `dry` words them for a dry run.
pub(crate) fn filter_notes(recorded: Option<&IndexFilter>, effective: &IndexFilter, overrides: &Overrides, dry: bool) -> Vec<String> {
    let stored = recorded.cloned().unwrap_or_default();
    let mut notes = Vec::new();
    let mut note = |kind: &str, changed: bool, given: bool, empty: bool, old: String, new: String| {
        if recorded.is_some() && changed {
            let verb = if dry { "would change" } else { "changed" };
            notes.push(format!("{} {}: {} → {}", kind, verb, old, new));
        } else if dry && !given && !empty {
            notes.push(format!("using the stored {}: {}", kind, new));
        } else if !empty {
            notes.push(format!("{}: {}", kind, new));
        }
    };
    note(
        "file-type filters",
        stored.types != effective.types,
        overrides.types.is_some(),
        effective.types.is_empty(),
        stored.types.describe(),
        effective.types.describe(),
    );
    note(
        "path filters",
        stored.paths != effective.paths,
        overrides.paths.is_some(),
        effective.paths.is_empty(),
        stored.paths.describe(),
        effective.paths.describe(),
    );
    if !effective.paths.include.is_empty() && effective.paths.exclude.is_empty() {
        notes.push("--include-path only makes exceptions to --exclude-path; with no exclusions it has no effect".to_string());
    }
    notes
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

    fn paths(exclude: &[&str], include: &[&str]) -> PathFilter {
        PathFilter::new(exts(exclude), exts(include)).unwrap()
    }

    fn c(p: &str) -> Vec<&str> {
        p.split('/').collect()
    }

    #[test]
    fn path_patterns() {
        let f = paths(&[".snapshot", "*.tmp", "projects/*/render-cache", "**/node_modules", "build/"], &[]);
        assert!(f.excludes(&c(".snapshot"), true));
        assert!(f.excludes(&c("a/b/.snapshot/x/file.txt"), false), "names match at any depth, and below");
        assert!(f.excludes(&c("deep/junk.tmp"), false));
        assert!(f.excludes(&c("projects/film/render-cache/f.exr"), false), "anchored with a wildcard");
        assert!(!f.excludes(&c("other/projects/film/render-cache/f.exr"), false), "anchored at the root");
        assert!(f.excludes(&c("a/b/node_modules/x.js"), false), "** crosses folders");
        assert!(f.excludes(&c("build/out.o"), false) && !f.excludes(&c("src/build"), false), "trailing / = folders only");
        assert!(!f.excludes(&c("Snapshot.TMP"), false), "case-sensitive");
        assert!(!f.excludes(&c("projects/film/notes.txt"), false));
    }

    #[test]
    fn includes_are_exceptions_and_limit_pruning() {
        let f = paths(&["cache"], &["cache/keep"]);
        assert!(f.excludes(&c("cache/other/x"), false));
        assert!(!f.excludes(&c("cache/keep/x"), false), "the exception wins");
        assert!(!f.prunes(&c("cache")), "must look inside cache to find cache/keep");
        assert!(f.prunes(&c("cache/other")), "nothing under cache/other can be included");
        assert!(!paths(&["cache"], &["*.keep"]).prunes(&c("cache")), "an unanchored include could match anywhere");
        assert!(paths(&["cache"], &[]).prunes(&c("a/cache")));
        assert!(!paths(&[], &["x"]).excludes(&c("x"), false), "include alone excludes nothing");
    }

    #[test]
    fn path_filter_storage_and_validation() {
        let f = paths(&[".snapshot", "a/b"], &["a/b/keep"]);
        assert_eq!(f.to_db(), (Some(".snapshot\na/b".to_string()), Some("a/b/keep".to_string())));
        assert_eq!(PathFilter::from_db(f.to_db().0, f.to_db().1), f);
        assert_eq!(f.describe(), "exclude .snapshot, a/b · except a/b/keep");
        assert!(parse_path_pattern("a/[b").is_err());
        assert!(parse_path_pattern("/").is_err());
    }
}
