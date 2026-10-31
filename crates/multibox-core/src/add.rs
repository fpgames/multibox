use crate::adapter;
use crate::git::{self, Repo};
use crate::index::{Index, Stat};
use crate::plan::{Plan, PlanSummary, build_plan};
use crate::rules::Rules;
use crate::{Error, Result};
use multibox_clone::{
    Capability, DirEntry, Manifest, Options, Progress, Report, Strategy, execute, free_space, probe,
};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// How files reach the new worktree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Mode {
    /// Reflink where possible, copy otherwise.
    #[default]
    Auto,
    Copy,
    Reflink,
}

pub struct AddOptions {
    /// Arguments for `git worktree add`, passed through verbatim.
    pub git_args: Vec<String>,
    /// The `<path>` among `git_args`.
    pub path: String,
    /// Worktree to clone from; defaults to the one containing `cwd`.
    pub from: Option<PathBuf>,
    pub mode: Mode,
    /// Carry ignored engine caches and `.worktreeinclude` matches.
    pub carry: bool,
    /// Report the plan and stop.
    pub dry_run: bool,
    pub threads: Option<usize>,
}

/// Receives progress from [`add`]. All methods have empty defaults, so an
/// embedding program only implements what it wants to surface.
pub trait Observer: Sync {
    fn note(&self, _message: &str) {}
    fn warn(&self, _message: &str) {}
    fn clone_started(&self, _files: u64, _bytes: u64, _progress: Arc<Progress>) {}
    fn clone_finished(&self) {}
}

#[derive(Debug)]
pub struct AddOutcome {
    pub path: PathBuf,
    pub summary: PlanSummary,
    pub capability: Capability,
    /// False when the source index had to be discarded.
    pub reused_index: bool,
    pub report: Option<Report>,
    pub warnings: Vec<String>,
    /// Variables that keep engine tooling inside the new worktree.
    pub environment: Vec<(String, String)>,
}

const DATABASE_ATTEMPTS: usize = 3;
const DATABASE_RETRY_DELAY: Duration = Duration::from_secs(2);

/// Space kept free on top of the plan, so a copy never fills the volume.
const FREE_SPACE_MARGIN: u64 = 1 << 30;

fn human_bytes(bytes: u64) -> String {
    const GIB: f64 = (1u64 << 30) as f64;
    const MIB: f64 = (1u64 << 20) as f64;
    let value = bytes as f64;
    if value >= GIB {
        format!("{:.1} GiB", value / GIB)
    } else {
        format!("{:.1} MiB", value / MIB)
    }
}

type Snapshot = Vec<(PathBuf, Option<(u64, Option<SystemTime>)>)>;

fn snapshot(paths: &[PathBuf]) -> Snapshot {
    paths
        .iter()
        .map(|path| {
            let state = fs::metadata(path)
                .ok()
                .map(|meta| (meta.len(), meta.modified().ok()));
            (path.clone(), state)
        })
        .collect()
}

/// Takes the engine's databases out of the manifest and clones them first.
///
/// The order matters. Imported artifacts are immutable and only ever added, so
/// a database older than the artifact store it is copied with still resolves
/// every reference; a newer one could point at files that were not copied. A
/// write during the copy is detected and retried, which keeps the window to the
/// moment the database is read rather than the whole clone.
///
/// Returns false if the database kept changing through every attempt.
fn capture_databases(
    plan: &mut Plan,
    new: &Repo,
    databases: &[String],
    options: &Options<'_>,
) -> Result<bool> {
    let (first, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut plan.manifest.files)
        .into_iter()
        .partition(|file| databases.contains(&file.rel));
    plan.manifest.files = rest;
    if first.is_empty() {
        return Ok(true);
    }
    let mut dirs: Vec<String> = Vec::new();
    for file in &first {
        let mut path = file.rel.as_str();
        while let Some((parent, _)) = path.rsplit_once('/') {
            dirs.push(parent.to_owned());
            path = parent;
        }
    }
    dirs.sort();
    dirs.dedup();
    let bytes = first.iter().map(|file| file.size).sum();
    plan.manifest.bytes -= bytes;
    let sources: Vec<PathBuf> = first
        .iter()
        .map(|file| plan.repo.worktree.join(&file.rel))
        .collect();
    let manifest = Manifest {
        dirs: dirs
            .into_iter()
            .map(|rel| DirEntry { rel, times: None })
            .collect(),
        files: first,
        links: Vec::new(),
        bytes,
        skipped: Vec::new(),
    };

    for attempt in 0..DATABASE_ATTEMPTS {
        let before = snapshot(&sources);
        execute(&manifest, &plan.repo.worktree, &new.worktree, options)?;
        if before == snapshot(&sources) {
            return Ok(true);
        }
        if attempt + 1 < DATABASE_ATTEMPTS {
            for file in &manifest.files {
                fs::remove_file(new.worktree.join(&file.rel))?;
            }
            std::thread::sleep(DATABASE_RETRY_DELAY);
        }
    }
    Ok(false)
}

fn check_free_space(plan: &Plan, target: &Path, capability: &Capability) -> Result<()> {
    if capability.reflink {
        return Ok(());
    }
    let free = free_space(target)?;
    let needed = plan.manifest.bytes + FREE_SPACE_MARGIN;
    if needed > free {
        return Err(Error::new(format!(
            "not enough space on the target volume: need {}, {} free",
            human_bytes(needed),
            human_bytes(free)
        )));
    }
    Ok(())
}

fn resolve_mode(mode: Mode) -> Strategy {
    match mode {
        Mode::Auto => Strategy::Auto,
        Mode::Copy => Strategy::Copy,
        Mode::Reflink => Strategy::Reflink,
    }
}

fn time_parts(time: std::io::Result<SystemTime>) -> (u32, u32) {
    time.ok()
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or((0, 0), |since| {
            (since.as_secs() as u32, since.subsec_nanos())
        })
}

/// The stat data git would compute for this file.
#[cfg(unix)]
fn stat_of(meta: &fs::Metadata) -> Stat {
    use std::os::unix::fs::MetadataExt;
    Stat {
        ctime_sec: meta.ctime() as u32,
        ctime_nsec: meta.ctime_nsec() as u32,
        mtime_sec: meta.mtime() as u32,
        mtime_nsec: meta.mtime_nsec() as u32,
        dev: meta.dev() as u32,
        ino: meta.ino() as u32,
        uid: meta.uid(),
        gid: meta.gid(),
        size: meta.size() as u32,
    }
}

/// The stat data git would compute for this file. Git for Windows stores zero
/// for dev, ino, uid and gid, and the creation time as ctime.
#[cfg(windows)]
fn stat_of(meta: &fs::Metadata) -> Stat {
    let (ctime_sec, ctime_nsec) = time_parts(meta.created());
    let (mtime_sec, mtime_nsec) = time_parts(meta.modified());
    Stat {
        ctime_sec,
        ctime_nsec,
        mtime_sec,
        mtime_nsec,
        size: meta.len() as u32,
        ..Stat::default()
    }
}

/// Whether the source file is certainly what the index says it is. Git builds
/// differ on nanosecond support, so a recorded zero is not a mismatch. A file
/// no older than the index itself is "racy": its stat data proves nothing.
fn clean_in_source(recorded: &Stat, actual: &Stat, index_time: (u32, u32)) -> bool {
    let time_matches = |recorded: (u32, u32), actual: (u32, u32)| {
        recorded.0 == actual.0 && (recorded.1 == 0 || recorded.1 == actual.1)
    };
    recorded.size == actual.size
        && recorded.ino == actual.ino
        && time_matches(
            (recorded.mtime_sec, recorded.mtime_nsec),
            (actual.mtime_sec, actual.mtime_nsec),
        )
        && time_matches(
            (recorded.ctime_sec, recorded.ctime_nsec),
            (actual.ctime_sec, actual.ctime_nsec),
        )
        && (actual.mtime_sec, actual.mtime_nsec) < index_time
}

/// A clone differs from its source in ways that cannot be set back: a new inode
/// and ctime on Unix, a new creation time on Windows. Cached stat data therefore
/// has to be rewritten to match what git will see in the new worktree.
///
/// Only entries that are clean in the source qualify. A file edited there must
/// keep its stale entry: rewriting it would tell git the clone's content is
/// what the index records, and the edit would survive `reset --hard` unseen.
pub(crate) fn refresh_stat(index: &mut Index, source: &Path, index_path: &Path, worktree: &Path) {
    let Ok(index_meta) = fs::metadata(index_path) else {
        return;
    };
    let index_time = time_parts(index_meta.modified());
    let shared: &Index = index;
    let entries = shared.entries.len();
    let threads = std::thread::available_parallelism().map_or(4, usize::from);
    let chunk = (entries / threads.min(16)).max(1);
    let updates: Vec<(usize, Stat)> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..entries)
            .step_by(chunk)
            .map(|start| {
                scope.spawn(move || {
                    (start..(start + chunk).min(entries))
                        .filter_map(|at| {
                            let path = &shared.entries[at].path;
                            let original = fs::symlink_metadata(source.join(path)).ok()?;
                            let clone = fs::symlink_metadata(worktree.join(path)).ok()?;
                            let recorded = shared.stat(at).ok()?;
                            clean_in_source(&recorded, &stat_of(&original), index_time)
                                .then(|| (at, stat_of(&clone)))
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|worker| worker.join().expect("stat worker panicked"))
            .collect()
    });
    for (at, stat) in updates {
        index.set_stat(at, &stat);
    }
}

/// Installs the source index in the new worktree, then makes the worktree an
/// exact checkout of its HEAD. Files the source had modified, staged or deleted
/// are the only ones git has to touch.
fn finish_checkout(plan: &mut Plan, new: &Repo) -> Result<()> {
    let index = plan
        .index
        .as_mut()
        .ok_or_else(|| Error::new("no index to install"))?;
    refresh_stat(
        index,
        &plan.repo.worktree,
        &plan.repo.index_path(),
        &new.worktree,
    );
    fs::write(new.index_path(), index.to_worktree_bytes()?)?;
    git::quiet(&new.worktree, &["reset", "--hard", "--quiet"])
}

/// How files spell a path: with forward slashes, escaped as in a JSON string,
/// and natively. Only Windows paths differ between the three.
fn spellings(path: &Path) -> [String; 3] {
    let text = path.display().to_string();
    let native = text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned();
    if cfg!(windows) {
        let native = native.replace('/', "\\");
        [
            native.replace('\\', "/"),
            native.replace('\\', r"\\"),
            native,
        ]
    } else {
        [native.clone(), native.clone(), native]
    }
}

/// Pairs every spelling of the source, as given and as the filesystem resolves
/// it, with the target spelt the same way. Longest first, so that no spelling
/// is replaced inside another.
fn relocations(source: &Path, target: &Path) -> Vec<(String, String)> {
    let resolved = |path: &Path| fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let to = spellings(&resolved(target));
    let mut pairs: Vec<(String, String)> = Vec::new();
    for from in [spellings(source), spellings(&resolved(source))] {
        for (from, to) in from.into_iter().zip(&to) {
            if !pairs.iter().any(|(seen, _)| *seen == from) {
                pairs.push((from, to.clone()));
            }
        }
    }
    pairs.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
    pairs
}

/// Replaces `from` wherever it names the whole path, not the start of a
/// longer name. Windows paths compare without case.
fn replace_path(text: &str, from: &str, to: &str) -> Option<String> {
    let fold = |text: &str| {
        if cfg!(windows) {
            text.to_ascii_lowercase()
        } else {
            text.to_owned()
        }
    };
    let (haystack, needle) = (fold(text), fold(from));
    let in_name = |c: char| c.is_alphanumeric() || "-_.~".contains(c);
    let mut out = String::with_capacity(text.len());
    let (mut copied, mut at) = (0, 0);
    while let Some(found) = haystack[at..].find(&needle) {
        let start = at + found;
        let end = start + needle.len();
        let before = text[..start].chars().next_back();
        let after = text[end..].chars().next();
        if !before.is_some_and(in_name) && !after.is_some_and(|c| in_name(c) || c == ' ') {
            out.push_str(&text[copied..start]);
            out.push_str(to);
            copied = end;
            at = end;
        } else {
            at = start + text[start..].chars().next().map_or(1, char::len_utf8);
        }
    }
    (copied > 0).then(|| {
        out.push_str(&text[copied..]);
        out
    })
}

fn relocate_text(text: &str, pairs: &[(String, String)]) -> Option<String> {
    let mut changed: Option<String> = None;
    for (from, to) in pairs {
        let current = changed.as_deref().unwrap_or(text);
        if let Some(next) = replace_path(current, from, to) {
            changed = Some(next);
        }
    }
    changed
}

fn relocate_file(path: &Path, pairs: &[(String, String)]) -> io::Result<()> {
    let Some(text) = relocate_text(&fs::read_to_string(path)?, pairs) else {
        return Ok(());
    };
    let modified = fs::metadata(path)?.modified()?;
    let mut file = fs::File::options().write(true).truncate(true).open(path)?;
    file.write_all(text.as_bytes())?;
    file.set_modified(modified)
}

/// Points carried files that record the project's location at the new
/// worktree. Left alone, their tools would treat the worktree as a moved
/// project, or reach back into the source.
fn relocate(plan: &Plan, target: &Path, warnings: &mut Vec<String>) -> Result<()> {
    let patterns: Vec<String> = plan.projects.iter().flat_map(adapter::relocated).collect();
    if patterns.is_empty() {
        return Ok(());
    }
    let rules = Rules::new(&patterns)?;
    let pairs = relocations(&plan.repo.worktree, target);
    for file in &plan.manifest.files {
        if rules.includes(&file.rel, false) {
            if let Err(err) = relocate_file(&target.join(&file.rel), &pairs) {
                warnings.push(format!("could not update the paths in {}: {err}", file.rel));
            }
        }
    }
    Ok(())
}

/// `git worktree add --no-checkout` skips the hook, so it runs here to behave
/// like a real checkout. As in git, a failing hook does not fail the command.
fn run_post_checkout(new: &Repo, observer: &dyn Observer) {
    let Ok(head) = git::output(&new.worktree, &["rev-parse", "HEAD"]) else {
        return;
    };
    let zero = "0".repeat(if new.hash_len == 32 { 64 } else { 40 });
    let args = [
        "hook",
        "run",
        "--ignore-missing",
        "post-checkout",
        "--",
        zero.as_str(),
        head.trim(),
        "1",
    ];
    if let Err(err) = git::quiet(&new.worktree, &args) {
        observer.warn(&format!("post-checkout hook failed: {err}"));
    }
}

fn rollback(cwd: &Path, target: &Path, observer: &dyn Observer) {
    let path = target.to_string_lossy();
    let args = ["worktree", "remove", "--force", path.as_ref()];
    if let Err(err) = git::quiet(cwd, &args) {
        observer.warn(&format!("could not clean up {path}: {err}"));
    }
}

fn describe_failures(report: &Report, warnings: &mut Vec<String>) {
    if report.failures.is_empty() {
        return;
    }
    let sample: Vec<String> = report
        .failures
        .iter()
        .take(5)
        .map(|failure| format!("{}: {}", failure.path, failure.error))
        .collect();
    warnings.push(format!(
        "{} file(s) could not be cloned; tracked files were restored by git, cached ones will be rebuilt by the engine:\n  {}",
        report.failures.len(),
        sample.join("\n  ")
    ));
}

/// Creates a worktree, as `git worktree add` would, carrying over everything
/// the plan selected. `cwd` is where a relative `path` is resolved.
pub fn add(cwd: &Path, options: &AddOptions, observer: &dyn Observer) -> Result<AddOutcome> {
    let source = options
        .from
        .as_ref()
        .map_or_else(|| cwd.to_path_buf(), |from| cwd.join(from));
    let target = cwd.join(&options.path);
    let capability = probe(&Repo::discover(&source)?.worktree, &target)?;
    let strategy = resolve_mode(options.mode);
    let frugal = !capability.reflink || strategy == Strategy::Copy;
    let mut plan = build_plan(&source, options.carry, frugal, options.threads)?;
    if strategy == Strategy::Reflink && !capability.reflink {
        return Err(Error::new(format!(
            "reflinks are unavailable here ({}, same volume: {}); run `git multibox doctor`",
            capability.filesystem, capability.same_volume
        )));
    }

    let mut warnings = Vec::new();
    if let Some(problem) = &plan.index_problem {
        warnings.push(format!(
            "source index not reused ({problem}); git will do a full checkout"
        ));
    }
    if let Some(first) = plan.refused.first() {
        let more = match plan.refused.len() {
            1 => String::new(),
            n => format!(" and {} more", n - 1),
        };
        warnings.push(format!(
            "{first}{more} left behind: reinstalling is faster than a copy, so it is only carried as a reflink"
        ));
    }
    if !plan.manifest.skipped.is_empty() {
        warnings.push(format!(
            "{} entries could not be read during the scan, first: {}",
            plan.manifest.skipped.len(),
            plan.manifest.skipped[0]
        ));
    }
    let databases: Vec<String> = plan
        .projects
        .iter()
        .flat_map(adapter::volatile_files)
        .collect();
    for project in &plan.projects {
        if let Some(editor) = adapter::running_editor(&plan.repo.worktree, project) {
            let who = editor
                .process_id
                .map_or_else(String::new, |pid| format!(" (pid {pid})"));
            observer.warn(&format!(
                "{} editor{who} has {} open",
                project.engine,
                plan.repo.worktree.join(&project.root).display()
            ));
        }
    }

    if options.dry_run {
        return Ok(AddOutcome {
            environment: adapter::environment(&target, &plan.projects),
            path: target,
            summary: plan.summary,
            capability,
            reused_index: plan.index.is_some(),
            report: None,
            warnings,
        });
    }
    if !capability.reflink {
        check_free_space(&plan, &target, &capability)?;
    }

    // With no reusable index, git performs the checkout and only caches are cloned.
    let mut git_args: Vec<&str> = vec!["worktree", "add"];
    if plan.index.is_some() {
        git_args.push("--no-checkout");
    }
    git_args.extend(options.git_args.iter().map(String::as_str));
    git::run(cwd, &git_args)?;

    let hook_needed = plan.index.is_some();
    let result = (|| -> Result<(Repo, Report)> {
        let new = Repo::discover(&target)?;
        let quiet = Options {
            strategy,
            threads: options.threads,
            progress: None,
        };
        if !capture_databases(&mut plan, &new, &databases, &quiet)? {
            warnings.push(
                "the engine was writing to its asset database throughout the clone; the first launch may need to repair it"
                    .to_owned(),
            );
        }
        let progress = Arc::new(Progress::default());
        observer.clone_started(
            plan.manifest.files.len() as u64,
            plan.manifest.bytes,
            progress.clone(),
        );
        let report = execute(
            &plan.manifest,
            &plan.repo.worktree,
            &new.worktree,
            &Options {
                strategy,
                threads: options.threads,
                progress: Some(&progress),
            },
        );
        observer.clone_finished();
        let report = report?;
        describe_failures(&report, &mut warnings);
        relocate(&plan, &new.worktree, &mut warnings)?;
        if plan.index.is_some() {
            let started = Instant::now();
            finish_checkout(&mut plan, &new)?;
            observer.note(&format!(
                "checked out in {:.1}s",
                started.elapsed().as_secs_f32()
            ));
        }
        Ok((new, report))
    })();

    match result {
        Ok((new, report)) => {
            if hook_needed {
                run_post_checkout(&new, observer);
            }
            Ok(AddOutcome {
                environment: adapter::environment(&new.worktree, &plan.projects),
                path: new.worktree,
                summary: plan.summary,
                capability: report.capability.clone().unwrap_or(capability),
                reused_index: plan.index.is_some(),
                report: Some(report),
                warnings,
            })
        }
        Err(err) => {
            rollback(cwd, &target, observer);
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(mtime: (u32, u32), size: u32) -> Stat {
        Stat {
            mtime_sec: mtime.0,
            mtime_nsec: mtime.1,
            ctime_sec: mtime.0,
            ctime_nsec: mtime.1,
            ino: 7,
            size,
            ..Stat::default()
        }
    }

    #[test]
    fn an_unchanged_older_file_is_clean() {
        let recorded = stat((100, 5), 10);
        assert!(clean_in_source(&recorded, &stat((100, 5), 10), (200, 0)));
    }

    #[test]
    fn a_zero_recorded_nanosecond_is_not_a_mismatch() {
        let recorded = stat((100, 0), 10);
        assert!(clean_in_source(&recorded, &stat((100, 5), 10), (200, 0)));
    }

    #[test]
    fn any_difference_means_dirty() {
        let recorded = stat((100, 5), 10);
        assert!(!clean_in_source(&recorded, &stat((100, 6), 10), (200, 0)));
        assert!(!clean_in_source(&recorded, &stat((101, 5), 10), (200, 0)));
        assert!(!clean_in_source(&recorded, &stat((100, 5), 11), (200, 0)));
    }

    fn pair(from: &str, to: &str) -> Vec<(String, String)> {
        vec![(from.to_owned(), to.to_owned())]
    }

    #[test]
    fn replaces_whole_paths_only() {
        let pairs = pair("/src/game", "/wt/feature");
        let text = r#"{"a": "/src/game/node_modules", "b": "/src/game", "c": "/src/games", "d": "/x/src/game"}"#;
        assert_eq!(
            relocate_text(text, &pairs).unwrap(),
            r#"{"a": "/wt/feature/node_modules", "b": "/wt/feature", "c": "/src/games", "d": "/x/src/game"}"#
        );
        assert_eq!(relocate_text("/src/game copy/x", &pairs), None);
        assert_eq!(relocate_text("nothing here", &pairs), None);
    }

    #[test]
    fn a_target_inside_the_source_is_replaced_once() {
        let pairs = pair("/src/game", "/src/game/.worktrees/feature");
        assert_eq!(
            relocate_text("dir: /src/game/node_modules", &pairs).unwrap(),
            "dir: /src/game/.worktrees/feature/node_modules"
        );
    }

    #[cfg(windows)]
    #[test]
    fn relocates_every_spelling_of_a_windows_path() {
        let pairs = relocations(Path::new("E:/Games/main"), Path::new("E:/Games/feature"));
        let text = r#"{"virtualStoreDir": "e:\\games\\main\\node_modules\\.pnpm", "dir": "E:/Games/main", "raw": "E:\Games\main\x"}"#;
        assert_eq!(
            relocate_text(text, &pairs).unwrap(),
            r#"{"virtualStoreDir": "E:\\Games\\feature\\node_modules\\.pnpm", "dir": "E:/Games/feature", "raw": "E:\Games\feature\x"}"#
        );
    }

    #[test]
    fn a_file_as_new_as_the_index_proves_nothing() {
        let recorded = stat((100, 5), 10);
        assert!(!clean_in_source(&recorded, &stat((100, 5), 10), (100, 5)));
        assert!(!clean_in_source(&recorded, &stat((100, 5), 10), (100, 4)));
    }
}
