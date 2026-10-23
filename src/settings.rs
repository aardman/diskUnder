//! Settings, kept in the per-user store's `settings` table: what the app opens with, defaults
//! for new jobs, named file-type groups (`@images` in any type filter), deletion settings for
//! v6, and the run log's folder and rotation size

use rusqlite::{params, Connection};
use std::{collections::BTreeMap, path::Path};

use crate::filters::parse_path_pattern;
use crate::jobs::open_store;
use crate::output::{write_text_table, Align, Out};
use crate::query::{parse_ext_list, ExtList};

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Kind {
    YesNo,
    Number, // a whole number above 0
    Mode,   // index | update
    Types,  // extension list; @groups allowed
    Paths,  // path patterns, comma-separated
    Text,
}

/// Every setting: (key, default, kind, what it does)
pub(crate) const SETTINGS: [(&str, &str, Kind, &str); 12] = [
    ("start.db", "", Kind::Text, "database the app opens (blank: fileindex.db where you start it)"),
    ("start.select_last_job", "yes", Kind::YesNo, "select the most recently run job when the app opens"),
    ("defaults.mode", "update", Kind::Mode, "mode for new jobs created in the app"),
    ("defaults.threads", "4", Kind::Number, "threads for new jobs created in the app"),
    ("defaults.include_ext", "", Kind::Types, "include types for new jobs created in the app"),
    ("defaults.exclude_ext", "", Kind::Types, "exclude types for new jobs created in the app"),
    ("defaults.exclude_paths", "", Kind::Paths, "exclude paths for new jobs created in the app"),
    ("deletion.dry_run_default", "yes", Kind::YesNo, "v6: preview every dedupe before it acts"),
    ("deletion.confirm_quarantine", "yes", Kind::YesNo, "v6: ask before moving files to quarantine"),
    ("deletion.purge_after_days", "7", Kind::Number, "v6: days before quarantined files can be purged"),
    ("log.folder", "", Kind::Text, "folder for run logs (blank: next to each database)"),
    ("log.rotate_mb", "10", Kind::Number, "start a new run log after this many MB"),
];

/// Built-in type groups; `types.NAME` settings change or add groups (blank removes one)
pub(crate) const BUILTIN_GROUPS: [(&str, &str); 6] = [
    ("images", "jpg,jpeg,png,gif,heic,heif,tif,tiff,webp,bmp,raw,cr2,cr3,nef,arw,dng,psd,exr"),
    ("video", "mov,mp4,m4v,avi,mkv,mxf,mts,m2ts,wmv,webm,prores"),
    ("audio", "wav,mp3,aac,m4a,flac,aif,aiff,ogg"),
    ("documents", "pdf,doc,docx,xls,xlsx,ppt,pptx,key,numbers,pages,txt,md,rtf,csv"),
    ("archives", "zip,tar,gz,tgz,bz2,xz,7z,rar,iso,dmg"),
    ("temporary", "tmp,temp,bak,swp,part,crdownload,log"),
];

#[derive(Clone, Default)]
pub(crate) struct Settings {
    values: BTreeMap<String, String>,
}

fn store_err(e: rusqlite::Error) -> String {
    e.to_string()
}

impl Settings {
    /// Settings from the store; defaults if there is no store yet
    pub(crate) fn load(store: &Path) -> Self {
        let read = || -> Result<BTreeMap<String, String>, String> {
            let Some(conn) = open_store(store, false)? else { return Ok(BTreeMap::new()) };
            let mut stmt = conn.prepare("SELECT key, value FROM settings").map_err(store_err)?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map_err(store_err)?.collect::<rusqlite::Result<_>>();
            rows.map_err(store_err)
        };
        Settings { values: read().unwrap_or_default() }
    }

    /// The setting's value, or its default
    pub(crate) fn get(&self, key: &str) -> &str {
        self.values
            .get(key)
            .map(String::as_str)
            .or_else(|| SETTINGS.iter().find(|s| s.0 == key).map(|s| s.1))
            .unwrap_or("")
    }

    pub(crate) fn is_set(&self, key: &str) -> bool {
        self.values.contains_key(key)
    }

    pub(crate) fn yes(&self, key: &str) -> bool {
        self.get(key) == "yes"
    }

    pub(crate) fn number(&self, key: &str) -> u64 {
        self.get(key).parse().unwrap_or_else(|_| SETTINGS.iter().find(|s| s.0 == key).map_or(0, |s| s.1.parse().unwrap_or(0)))
    }

    /// Every type group: built-ins (unless removed or changed) and your own, by name
    pub(crate) fn groups(&self) -> Vec<(String, String)> {
        let mut groups: BTreeMap<String, String> =
            BUILTIN_GROUPS.iter().map(|(n, v)| (n.to_string(), v.to_string())).collect();
        for (k, v) in &self.values {
            if let Some(name) = k.strip_prefix("types.") {
                groups.insert(name.to_string(), v.clone());
            }
        }
        groups.into_iter().filter(|(_, v)| !v.is_empty()).collect()
    }

    /// Replace `@name` tokens in a comma-separated extension list with the group's extensions
    pub(crate) fn expand_types(&self, list: &str) -> Result<String, String> {
        let groups: BTreeMap<String, String> = self.groups().into_iter().collect();
        let mut out = Vec::new();
        for token in list.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            match token.strip_prefix('@') {
                Some(name) => match groups.get(&name.to_lowercase()) {
                    Some(exts) => out.push(exts.clone()),
                    None => {
                        let known: Vec<String> = groups.keys().map(|k| format!("@{}", k)).collect();
                        return Err(format!("no type group @{} — groups: {}", name, known.join(", ")));
                    }
                },
                None => out.push(token.to_string()),
            }
        }
        Ok(out.join(","))
    }

    /// Expand any `@groups` in an already-parsed extension list
    pub(crate) fn expand_list(&self, list: &mut ExtList) -> Result<(), String> {
        if list.0.iter().any(|e| e.starts_with('@')) {
            let joined = list.0.iter().map(|e| if e.is_empty() { "none" } else { e.as_str() }).collect::<Vec<_>>().join(",");
            *list = parse_ext_list(&self.expand_types(&joined)?)?;
        }
        Ok(())
    }
}

/// Check a value for a key; returns it normalised
pub(crate) fn check(settings: &Settings, key: &str, value: &str) -> Result<String, String> {
    let v = value.trim();
    if let Some(name) = key.strip_prefix("types.") {
        let ok = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !ok {
            return Err(format!("group names use letters, digits, - and _: '{}'", name));
        }
        if v.is_empty() {
            return Ok(String::new()); // removes the group
        }
        let exts = parse_ext_list(&settings.expand_types(v)?)?;
        return Ok(exts.0.iter().map(|e| if e.is_empty() { "none" } else { e.as_str() }).collect::<Vec<_>>().join(","));
    }
    let Some((_, _, kind, _)) = SETTINGS.iter().find(|s| s.0 == key) else {
        return Err(format!("no setting {} — see `diskunder settings`", key));
    };
    match kind {
        Kind::YesNo if v == "yes" || v == "no" => Ok(v.to_string()),
        Kind::YesNo => Err(format!("{} is yes or no", key)),
        Kind::Number => match v.parse::<u64>() {
            Ok(n) if n > 0 => Ok(n.to_string()),
            _ => Err(format!("{} is a whole number above 0", key)),
        },
        Kind::Mode if v == "index" || v == "update" => Ok(v.to_string()),
        Kind::Mode => Err(format!("{} is index or update", key)),
        Kind::Types if v.is_empty() => Ok(String::new()),
        Kind::Types => {
            settings.expand_types(v)?; // groups must exist; kept as typed so @groups stay live
            parse_ext_list(&v.replace('@', ""))?;
            Ok(v.to_string())
        }
        Kind::Paths => {
            for p in v.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                parse_path_pattern(p)?;
            }
            Ok(v.to_string())
        }
        Kind::Text => Ok(v.to_string()),
    }
}

/// Change a setting (checked first)
pub(crate) fn set(store: &Path, key: &str, value: &str) -> Result<String, String> {
    let current = Settings::load(store);
    let value = check(&current, key, value)?;
    let conn: Connection = open_store(store, true)?.expect("created");
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )
    .map_err(store_err)?;
    Ok(value)
}

/// Back to the default (or, for a built-in group, back to the built-in list)
pub(crate) fn unset(store: &Path, key: &str) -> Result<(), String> {
    if !key.starts_with("types.") && !SETTINGS.iter().any(|s| s.0 == key) {
        return Err(format!("no setting {} — see `diskunder settings`", key));
    }
    if let Some(conn) = open_store(store, false)? {
        conn.execute("DELETE FROM settings WHERE key = ?1", [key]).map_err(store_err)?;
    }
    Ok(())
}

/// `diskunder settings` — every setting and type group
fn list(store: &Path) {
    let s = Settings::load(store);
    let mut rows: Vec<Vec<String>> = SETTINGS
        .iter()
        .map(|(key, default, _, about)| {
            let value = s.get(key);
            let shown = if value.is_empty() { "—".to_string() } else { value.to_string() };
            let mark = if s.is_set(key) && value != *default { "" } else { " (default)" };
            vec![key.to_string(), format!("{}{}", shown, mark), about.to_string()]
        })
        .collect();
    for (name, exts) in s.groups() {
        let builtin = BUILTIN_GROUPS.iter().any(|(n, v)| *n == name && *v == exts);
        rows.push(vec![format!("types.{}", name), exts, format!("@{} in any type filter{}", name, if builtin { " (built in)" } else { "" })]);
    }
    let mut out = Out::new();
    write_text_table(&mut out, &["SETTING", "VALUE", "WHAT IT DOES"], &[Align::Left, Align::Left, Align::Left], &rows);
    out.flush();
}

pub(crate) enum SettingsCommand<'a> {
    List,
    Set { key: &'a str, value: &'a str },
    Unset { key: &'a str },
}

pub(crate) fn run_settings_command(store: &Path, cmd: SettingsCommand) -> i32 {
    let result = match cmd {
        SettingsCommand::List => {
            list(store);
            Ok(())
        }
        SettingsCommand::Set { key, value } => set(store, key, value).map(|v| {
            if v.is_empty() && key.starts_with("types.") {
                println!("{} removed", key);
            } else {
                println!("{} = {}", key, if v.is_empty() { "—" } else { &v });
            }
        }),
        SettingsCommand::Unset { key } => unset(store, key).map(|_| println!("{} back to its default", key)),
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {}", e);
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_expand_and_values_are_checked() {
        let s = Settings::default();
        assert!(s.expand_types("@video,psd").unwrap().starts_with("mov,mp4"));
        assert!(s.expand_types("@nope").unwrap_err().contains("no type group @nope"));
        let mut list = parse_ext_list("@archives,txt").unwrap();
        s.expand_list(&mut list).unwrap();
        assert!(list.0.contains(&"iso".to_string()) && list.0.contains(&"txt".to_string()));
        assert_eq!(check(&s, "defaults.threads", "8"), Ok("8".to_string()));
        assert!(check(&s, "defaults.threads", "0").is_err());
        assert!(check(&s, "start.select_last_job", "maybe").is_err());
        assert!(check(&s, "defaults.exclude_paths", "a/[b").is_err());
        assert_eq!(check(&s, "types.raw", "CR2, nef"), Ok("cr2,nef".to_string()));
        assert!(check(&s, "nope", "1").is_err());
        assert_eq!(s.get("log.rotate_mb"), "10");
    }
}
