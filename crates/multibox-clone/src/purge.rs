use crate::scan::{Decision, ScanRequest, scan};
use crate::{join, thread_count};
use std::fs;
use std::io;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug, Default)]
pub struct PurgeReport {
    pub files: u64,
    pub dirs: u64,
    pub failures: Vec<String>,
}

/// Treats an entry that is already gone as done: another purge of the same
/// directory may be running after an interrupted removal.
fn tolerate_missing(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

fn remove_file(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
            let mut permissions = fs::symlink_metadata(path)?.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            fs::set_permissions(path, permissions)?;
            fs::remove_file(path)
        }
        other => other,
    }
}

/// Removes an empty directory. On Windows a file deleted while another process
/// has it open, an indexer or a virus scanner for instance, lingers until that
/// handle closes, and so does a handle on the directory itself; both make the
/// removal fail for a moment. A few short retries ride that out.
fn remove_dir_patiently(path: &Path) -> io::Result<()> {
    let mut delay = std::time::Duration::from_millis(20);
    for _ in 0..6 {
        match tolerate_missing(fs::remove_dir(path)) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => {
                std::thread::sleep(delay);
                delay *= 2;
            }
            done => return done,
        }
    }
    tolerate_missing(fs::remove_dir(path))
}

/// Deletes `root` and everything below it, in parallel. Links are removed, never
/// followed, so a junction inside the tree cannot reach data outside it.
pub fn purge(root: &Path, threads: Option<usize>) -> io::Result<PurgeReport> {
    let threads = thread_count(threads);
    let everything = |_: &str, _: bool| Decision::Include;
    let manifest = scan(
        root,
        &ScanRequest {
            files: Vec::new(),
            links: Vec::new(),
            trees: vec![String::new()],
            filter: &everything,
            threads: Some(threads),
        },
    )?;
    let root = fs::canonicalize(root)?;
    let failures = Mutex::new(Vec::new());

    let leaves: Vec<&str> = manifest
        .files
        .iter()
        .map(|file| file.rel.as_str())
        .chain(manifest.links.iter().map(|link| link.rel.as_str()))
        .collect();
    let cursor = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let start = cursor.fetch_add(64, Ordering::Relaxed);
                    if start >= leaves.len() {
                        return;
                    }
                    for rel in &leaves[start..(start + 64).min(leaves.len())] {
                        let path = join(&root, rel);
                        // A directory link needs remove_dir on Windows.
                        let removed = remove_file(&path).or_else(|_| fs::remove_dir(&path));
                        if let Err(err) = tolerate_missing(removed) {
                            failures.lock().unwrap().push(format!("{rel}: {err}"));
                        }
                    }
                }
            });
        }
    });

    let mut dirs: Vec<&str> = manifest.dirs.iter().map(|dir| dir.rel.as_str()).collect();
    dirs.sort_unstable_by_key(|rel| std::cmp::Reverse(rel.len()));
    for rel in dirs.iter().copied().chain([""]) {
        let path = if rel.is_empty() {
            root.clone()
        } else {
            join(&root, rel)
        };
        if let Err(err) = remove_dir_patiently(&path) {
            let rel = if rel.is_empty() { "." } else { rel };
            failures.lock().unwrap().push(format!("{rel}: {err}"));
        }
    }

    Ok(PurgeReport {
        files: leaves.len() as u64,
        dirs: dirs.len() as u64,
        failures: failures.into_inner().unwrap(),
    })
}
