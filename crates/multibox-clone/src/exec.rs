use crate::scan::{DirEntry, Manifest};
use crate::sys::{self, Capability, Outcome};
use crate::{join, thread_count};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const BATCH: usize = 64;
const COPY_BUFFER: usize = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Strategy {
    /// Reflink when the volume supports it, copy otherwise.
    #[default]
    Auto,
    /// Always copy, even on a reflink-capable volume.
    Copy,
    /// Fail up front unless the volume supports reflinks.
    Reflink,
}

/// Live counters, safe to read from another thread while [`execute`] runs.
#[derive(Debug, Default)]
pub struct Progress {
    pub files: AtomicU64,
    pub bytes: AtomicU64,
}

#[derive(Default)]
pub struct Options<'a> {
    pub strategy: Strategy,
    pub threads: Option<usize>,
    pub progress: Option<&'a Progress>,
}

#[derive(Debug)]
pub struct Failure {
    pub path: String,
    pub error: io::Error,
}

#[derive(Debug, Default)]
pub struct Report {
    pub files: u64,
    pub bytes: u64,
    pub reflinked: u64,
    pub copied: u64,
    /// Files where a reflink was attempted and refused, then copied instead.
    pub fallbacks: u64,
    /// Source files that disappeared between the scan and the clone.
    pub vanished: u64,
    pub dirs: u64,
    pub links: u64,
    pub failures: Vec<Failure>,
    pub elapsed: Duration,
    pub capability: Option<Capability>,
}

fn depth(rel: &str) -> usize {
    rel.bytes().filter(|byte| *byte == b'/').count()
}

/// Runs `job` for every item of `items` across `threads`.
fn parallel<T: Sync, S: Send>(
    items: &[T],
    threads: usize,
    batch: usize,
    state: impl Fn() -> S + Sync,
    job: impl Fn(&mut S, &T) + Sync,
) -> Vec<S> {
    let cursor = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads.min(items.len().max(1)))
            .map(|_| {
                scope.spawn(|| {
                    let mut local = state();
                    loop {
                        let start = cursor.fetch_add(batch, Ordering::Relaxed);
                        if start >= items.len() {
                            return local;
                        }
                        for item in &items[start..(start + batch).min(items.len())] {
                            job(&mut local, item);
                        }
                    }
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().expect("clone worker panicked"))
            .collect()
    })
}

fn create_dirs(
    dirs: &[&DirEntry],
    dst: &Path,
    threads: usize,
    failures: &Mutex<Vec<Failure>>,
) {
    let mut start = 0;
    while start < dirs.len() {
        let level = depth(&dirs[start].rel);
        let end = start
            + dirs[start..]
                .iter()
                .take_while(|dir| depth(&dir.rel) == level)
                .count();
        parallel(
            &dirs[start..end],
            threads,
            32,
            || (),
            |_, dir| match fs::create_dir(join(dst, &dir.rel)) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => failures.lock().unwrap().push(Failure {
                    path: dir.rel.clone(),
                    error,
                }),
            },
        );
        start = end;
    }
}

fn strip_verbatim(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(&text))
}

/// An absolute link into the source tree must follow the clone. Left alone it
/// would keep pointing at the source, and writes through the new tree would
/// change the original project.
fn rebase_link(target: &Path, src_root: &Path, dst_root: &Path) -> PathBuf {
    let plain_target = strip_verbatim(target);
    let plain_source = strip_verbatim(src_root);
    match plain_target.strip_prefix(&plain_source) {
        Ok(rest) => strip_verbatim(dst_root).join(rest),
        Err(_) => target.to_path_buf(),
    }
}

fn clone_link(src: &Path, dst: &Path, src_root: &Path, dst_root: &Path) -> io::Result<()> {
    let target = rebase_link(&fs::read_link(src)?, src_root, dst_root);
    sys::create_link(src, dst, &target)
}

#[derive(Default)]
struct Tally {
    reflinked: u64,
    copied: u64,
    fallbacks: u64,
    vanished: u64,
    failures: Vec<Failure>,
}

/// Materialises `manifest` under `dst_root`, which must already exist.
///
/// Per-file errors are collected in [`Report::failures`]; the call itself only
/// fails when the volumes cannot satisfy the requested [`Strategy`].
pub fn execute(
    manifest: &Manifest,
    src_root: &Path,
    dst_root: &Path,
    options: &Options<'_>,
) -> io::Result<Report> {
    let started = Instant::now();
    let src = fs::canonicalize(src_root)?;
    let dst = fs::canonicalize(dst_root)?;
    let threads = thread_count(options.threads);

    let mut capability = sys::probe(&src, &dst)?;
    match options.strategy {
        Strategy::Copy => capability.reflink = false,
        Strategy::Reflink if !capability.reflink => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "reflinks are unavailable between {} and {} ({})",
                    src.display(),
                    dst.display(),
                    capability.filesystem
                ),
            ));
        }
        _ => {}
    }

    let failures = Mutex::new(Vec::new());
    let mut dirs: Vec<&DirEntry> = manifest.dirs.iter().collect();
    dirs.sort_by_key(|dir| depth(&dir.rel));
    create_dirs(&dirs, &dst, threads, &failures);

    let tallies = parallel(
        &manifest.files,
        threads,
        BATCH,
        || (Tally::default(), vec![0u8; COPY_BUFFER]),
        |(tally, buffer), file| {
            let from = join(&src, &file.rel);
            let to = join(&dst, &file.rel);
            match sys::clone_file(&from, &to, &capability, buffer) {
                Ok(Outcome::Reflinked) => tally.reflinked += 1,
                Ok(Outcome::Copied) => tally.copied += 1,
                Ok(Outcome::Fallback) => {
                    tally.copied += 1;
                    tally.fallbacks += 1;
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => tally.vanished += 1,
                Err(error) => tally.failures.push(Failure {
                    path: file.rel.clone(),
                    error,
                }),
            }
            if let Some(progress) = options.progress {
                progress.files.fetch_add(1, Ordering::Relaxed);
                progress.bytes.fetch_add(file.size, Ordering::Relaxed);
            }
        },
    );

    let mut report = Report {
        files: manifest.files.len() as u64,
        bytes: manifest.bytes,
        dirs: manifest.dirs.len() as u64,
        ..Report::default()
    };
    for (tally, _) in tallies {
        report.reflinked += tally.reflinked;
        report.copied += tally.copied;
        report.fallbacks += tally.fallbacks;
        report.vanished += tally.vanished;
        report.failures.extend(tally.failures);
    }

    for link in &manifest.links {
        match clone_link(&join(&src, &link.rel), &join(&dst, &link.rel), &src, &dst) {
            Ok(()) => report.links += 1,
            Err(error) => report.failures.push(Failure {
                path: link.rel.clone(),
                error,
            }),
        }
    }

    // Children touch their parent's mtime, so directories go last and deepest first.
    dirs.reverse();
    parallel(
        &dirs,
        threads,
        64,
        || (),
        |_, dir| {
            if let Some(times) = &dir.times {
                // Directory timestamps are best-effort: nothing relies on them for correctness.
                let _ = sys::set_dir_times(&join(&dst, &dir.rel), times);
            }
        },
    );

    report.failures.extend(failures.into_inner().unwrap());
    report.capability = Some(capability);
    report.elapsed = started.elapsed();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebases_links_into_the_source_tree() {
        let src = Path::new("/projects/game");
        let dst = Path::new("/work/feature");
        assert_eq!(
            rebase_link(Path::new("/projects/game/client/Assets"), src, dst),
            Path::new("/work/feature/client/Assets")
        );
    }

    #[test]
    fn leaves_other_links_alone() {
        let src = Path::new("/projects/game");
        let dst = Path::new("/work/feature");
        for target in ["/projects/gameplay/x", "/opt/sdk", "../relative"] {
            assert_eq!(rebase_link(Path::new(target), src, dst), Path::new(target));
        }
    }
}
