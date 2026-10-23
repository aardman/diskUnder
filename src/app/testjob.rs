//! The test job: a job over generated sample data, to try diskunder without touching a real
//! share. Lives in a `test` folder beside the jobs store.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use crate::filters::{Overrides, PathFilter, TypeFilter};
use crate::jobs::{create_job, delete_job, Job, JobSettings};

pub(crate) const NAME: &str = "test";
const MARKER: &str = ".diskunder-test"; // only a folder with this is ever removed

/// Beside the store, with symlinks resolved — jobs save their paths that way
pub(crate) fn folder(store: &Path) -> PathBuf {
    let dir = store.parent().unwrap_or(Path::new("."));
    fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()).join("test")
}

pub(crate) fn share(store: &Path) -> PathBuf {
    folder(store).join("share")
}

pub(crate) fn db(store: &Path) -> PathBuf {
    folder(store).join("index.db")
}

/// Whether `job` is the test job this module made (not a job of yours that happens to be
/// called "test")
pub(crate) fn is_ours(store: &Path, job: &Job) -> bool {
    job.name == NAME && Path::new(&job.path) == share(store)
}

/// Repeatable pseudo-random bytes, so copies are byte-identical and originals differ
fn bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    (0..len)
        .map(|_| {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (x >> 33) as u8
        })
        .collect()
}

/// Photos with backup copies (duplicates) and a hard link, a film project with a render cache
/// and an old copy, notes, plus a temp file and a snapshot folder the job's filters leave out
fn write_sample(root: &Path) -> std::io::Result<()> {
    let put = |rel: &str, data: &[u8]| -> std::io::Result<()> {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap())?;
        fs::write(p, data)
    };
    for i in 1..=3u64 {
        put(&format!("photos/img{}.jpg", i), &bytes(i, 200_000))?;
    }
    put("photos/thumbs/img1-small.jpg", &bytes(11, 20_000))?;
    put("backup/img1.jpg", &bytes(1, 200_000))?;
    put("backup/img2.jpg", &bytes(2, 200_000))?;
    fs::hard_link(root.join("photos/img3.jpg"), root.join("backup/img3-link.jpg"))?;
    put("projects/film/edit.prproj", &bytes(20, 60_000))?;
    put("projects/film/render.mov", &bytes(21, 800_000))?;
    for i in 1..=3u64 {
        put(&format!("projects/film/render-cache/frame{:04}.exr", i), &bytes(30 + i, 100_000))?;
    }
    put("projects/film/old/edit-v1.prproj", &bytes(20, 60_000))?;
    // Two years old, so the stale report has something to show
    let old = SystemTime::now() - Duration::from_secs(2 * 365 * 24 * 60 * 60);
    fs::File::options().write(true).open(root.join("projects/film/old/edit-v1.prproj"))?.set_modified(old)?;
    put("notes.txt", b"Shopping list: milk, eggs.\n")?;
    put("tmp/scratch.tmp", &bytes(40, 5_000))?;
    put(".snapshot/daily/photos/img1.jpg", &bytes(1, 200_000))?;
    Ok(())
}

/// Make the sample data and the test job. Returns what to say.
pub(crate) fn create(store: &Path) -> Result<String, String> {
    let dir = folder(store);
    if dir.exists() {
        return Err(format!("the test job's folder already exists: {} — remove the test job first", dir.display()));
    }
    fs::create_dir_all(&dir).map_err(|e| format!("can't create {}: {}", dir.display(), e))?;
    fs::write(dir.join(MARKER), "Sample data made by diskunder's test job. Safe to delete.\n").map_err(|e| e.to_string())?;
    write_sample(&share(store)).map_err(|e| format!("can't write the sample files: {}", e))?;
    let (path, db) = (share(store).to_string_lossy().into_owned(), db(store).to_string_lossy().into_owned());
    let settings = JobSettings {
        upsert: Some(true),
        path: Some(&path),
        db: Some(&db),
        threads: Some(2),
        overrides: Overrides {
            types: Some(TypeFilter { include: None, exclude: vec!["tmp".into()] }),
            paths: Some(PathFilter::new(vec![".snapshot".into()], Vec::new()).expect("valid")),
        },
    };
    if let Err(e) = create_job(store, NAME, &settings) {
        let _ = fs::remove_dir_all(&dir);
        return Err(e);
    }
    Ok(format!("created the test job — sample files in {}", path))
}

/// Remove the test job, its index and the sample files
pub(crate) fn remove(store: &Path, job: Option<&Job>) -> Result<String, String> {
    if let Some(job) = job {
        if !is_ours(store, job) {
            return Err("a job called test exists that isn't the test job — leaving it alone".to_string());
        }
        delete_job(store, NAME)?;
    }
    let dir = folder(store);
    if dir.join(MARKER).exists() {
        fs::remove_dir_all(&dir).map_err(|e| format!("can't remove {}: {}", dir.display(), e))?;
    }
    Ok("removed the test job and its sample files".to_string())
}

/// The command that creates the same job
pub(crate) fn add_command(store: &Path) -> String {
    format!(
        "diskunder jobs add test update {} --db {} --threads 2 --exclude-ext tmp --exclude-path .snapshot",
        share(store).display(),
        db(store).display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_copies_match_and_originals_differ() {
        assert_eq!(bytes(1, 64), bytes(1, 64));
        assert_ne!(bytes(1, 64), bytes(2, 64));
    }
}
