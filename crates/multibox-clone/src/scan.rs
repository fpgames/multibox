use crate::{Times, join, thread_count};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::Path;
use std::sync::{Condvar, Mutex};

/// What to do with a path found while walking a tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Take this entry. For a directory, keep walking it.
    Include,
    /// Skip this entry and everything below it.
    Exclude,
    /// Directories only: walk it, but only create it if something inside is taken.
    Descend,
}

pub trait TreeFilter: Sync {
    fn check(&self, rel: &str, is_dir: bool) -> Decision;
}

impl<F: Fn(&str, bool) -> Decision + Sync> TreeFilter for F {
    fn check(&self, rel: &str, is_dir: bool) -> Decision {
        self(rel, is_dir)
    }
}

/// Input to [`scan`]. All paths are relative to the source root and use `/`.
pub struct ScanRequest<'a> {
    /// Individual files the caller already knows about, with their expected size.
    pub files: Vec<(String, u64)>,
    /// Paths that may be symbolic links; they are classified on disk.
    pub links: Vec<String>,
    /// Directories to walk.
    pub trees: Vec<String>,
    /// Applied to every entry found under `trees`.
    pub filter: &'a dyn TreeFilter,
    pub threads: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct FileEntry {
    pub rel: String,
    pub size: u64,
}

#[derive(Clone, Debug)]
pub struct DirEntry {
    pub rel: String,
    pub times: Option<Times>,
}

#[derive(Clone, Debug)]
pub struct LinkEntry {
    pub rel: String,
}

/// Everything [`crate::execute`] will create.
#[derive(Clone, Debug, Default)]
pub struct Manifest {
    pub dirs: Vec<DirEntry>,
    pub files: Vec<FileEntry>,
    pub links: Vec<LinkEntry>,
    pub bytes: u64,
    /// Entries that could not be represented or read during the scan.
    pub skipped: Vec<String>,
}

#[derive(Default)]
struct Found {
    dirs: Vec<DirEntry>,
    files: Vec<FileEntry>,
    links: Vec<LinkEntry>,
    skipped: Vec<String>,
}

struct Queue {
    state: Mutex<(Vec<String>, usize)>,
    wake: Condvar,
}

impl Queue {
    fn next(&self) -> Option<String> {
        let mut state = self.state.lock().unwrap();
        loop {
            if let Some(dir) = state.0.pop() {
                state.1 += 1;
                return Some(dir);
            }
            if state.1 == 0 {
                self.wake.notify_all();
                return None;
            }
            state = self.wake.wait(state).unwrap();
        }
    }

    fn finish(&self, more: Vec<String>) {
        let mut state = self.state.lock().unwrap();
        state.0.extend(more);
        state.1 -= 1;
        self.wake.notify_all();
    }
}

fn times_of(meta: &fs::Metadata) -> Option<Times> {
    Some(Times {
        modified: meta.modified().ok()?,
        created: meta.created().ok(),
    })
}

fn walk_dir(
    root: &Path,
    rel: &str,
    filter: &dyn TreeFilter,
    found: &mut Found,
    more: &mut Vec<String>,
) {
    let entries = match fs::read_dir(join(root, rel)) {
        Ok(entries) => entries,
        Err(err) => {
            found.skipped.push(format!("{rel}: {err}"));
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                found.skipped.push(format!("{rel}: {err}"));
                continue;
            }
        };
        let Ok(name) = entry.file_name().into_string() else {
            found
                .skipped
                .push(format!("{rel}/{:?}: name is not unicode", entry.file_name()));
            continue;
        };
        let child = if rel.is_empty() {
            name
        } else {
            format!("{rel}/{name}")
        };
        let Ok(meta) = entry.metadata() else {
            found.skipped.push(format!("{child}: unreadable"));
            continue;
        };
        let kind = meta.file_type();
        if kind.is_symlink() {
            if filter.check(&child, false) == Decision::Include {
                found.links.push(LinkEntry { rel: child });
            }
        } else if kind.is_dir() {
            match filter.check(&child, true) {
                Decision::Include => {
                    found.dirs.push(DirEntry {
                        rel: child.clone(),
                        times: times_of(&meta),
                    });
                    more.push(child);
                }
                Decision::Descend => more.push(child),
                Decision::Exclude => {}
            }
        } else if filter.check(&child, false) == Decision::Include {
            found.files.push(FileEntry {
                rel: child,
                size: meta.len(),
            });
        }
    }
}

fn walk_trees(root: &Path, req: &ScanRequest<'_>, threads: usize) -> Vec<Found> {
    let queue = Queue {
        state: Mutex::new((req.trees.clone(), 0)),
        wake: Condvar::new(),
    };
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut found = Found::default();
                    while let Some(rel) = queue.next() {
                        let mut more = Vec::new();
                        walk_dir(root, &rel, req.filter, &mut found, &mut more);
                        queue.finish(more);
                    }
                    found
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().expect("scan worker panicked"))
            .collect()
    })
}

/// Collects the times of `rels` from disk, in parallel.
fn stat_dirs(root: &Path, rels: Vec<String>, threads: usize) -> Vec<DirEntry> {
    let cursor = std::sync::atomic::AtomicUsize::new(0);
    let rels = &rels;
    let cursor = &cursor;
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(move || {
                    let mut out = Vec::new();
                    loop {
                        let start = cursor.fetch_add(128, std::sync::atomic::Ordering::Relaxed);
                        if start >= rels.len() {
                            return out;
                        }
                        for rel in &rels[start..(start + 128).min(rels.len())] {
                            let times = fs::metadata(join(root, rel))
                                .ok()
                                .and_then(|meta| times_of(&meta));
                            out.push(DirEntry {
                                rel: rel.clone(),
                                times,
                            });
                        }
                    }
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|worker| worker.join().expect("scan worker panicked"))
            .collect()
    })
}

/// Expands `req` into a [`Manifest`]. Unreadable entries end up in
/// [`Manifest::skipped`] instead of failing the whole scan.
pub fn scan(src_root: &Path, req: &ScanRequest<'_>) -> io::Result<Manifest> {
    let root = fs::canonicalize(src_root)?;
    let threads = thread_count(req.threads);
    let mut manifest = Manifest::default();
    let mut dirs: HashMap<String, DirEntry> = HashMap::new();

    for found in walk_trees(&root, req, threads) {
        manifest.files.extend(found.files);
        manifest.links.extend(found.links);
        manifest.skipped.extend(found.skipped);
        for dir in found.dirs {
            dirs.insert(dir.rel.clone(), dir);
        }
    }
    manifest
        .files
        .extend(req.files.iter().map(|(rel, size)| FileEntry {
            rel: rel.clone(),
            size: *size,
        }));
    for rel in &req.links {
        match fs::symlink_metadata(join(&root, rel)) {
            Ok(meta) if meta.file_type().is_symlink() => {
                manifest.links.push(LinkEntry { rel: rel.clone() });
            }
            // A tracked link checked out as a plain file (core.symlinks=false).
            Ok(meta) => manifest.files.push(FileEntry {
                rel: rel.clone(),
                size: meta.len(),
            }),
            Err(_) => {}
        }
    }

    manifest.files.sort_unstable_by(|a, b| a.rel.cmp(&b.rel));
    manifest.files.dedup_by(|a, b| a.rel == b.rel);
    manifest.links.sort_unstable_by(|a, b| a.rel.cmp(&b.rel));
    manifest.links.dedup_by(|a, b| a.rel == b.rel);
    manifest.bytes = manifest.files.iter().map(|file| file.size).sum();

    let mut parents = HashSet::new();
    let leaves = manifest
        .files
        .iter()
        .map(|file| file.rel.as_str())
        .chain(manifest.links.iter().map(|link| link.rel.as_str()));
    for leaf in leaves {
        let mut path = leaf;
        while let Some((parent, _)) = path.rsplit_once('/') {
            if !parents.insert(parent.to_owned()) {
                break;
            }
            path = parent;
        }
    }
    let unknown = parents
        .into_iter()
        .filter(|parent| !dirs.contains_key(parent))
        .collect();
    manifest.dirs = dirs.into_values().collect();
    manifest.dirs.extend(stat_dirs(&root, unknown, threads));
    Ok(manifest)
}
