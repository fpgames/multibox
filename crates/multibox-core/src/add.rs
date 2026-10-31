use crate::adapter;
use crate::git::{self, Repo};
use crate::index::Index;
use crate::plan::{Plan, PlanSummary, build_plan};
use crate::{Error, Result};
use multibox_clone::{Capability, Options, Progress, Report, Strategy, execute, free_space, probe};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

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
}

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

#[cfg(unix)]
fn refresh_stat(index: &mut Index, worktree: &Path) {
    use crate::index::Stat;
    use std::os::unix::fs::MetadataExt;
    // A clone gets a new inode and ctime, neither of which can be set, so the
    // cached stat data has to be rewritten to match what git will see.
    for at in 0..index.entries.len() {
        let Ok(meta) = fs::symlink_metadata(worktree.join(&index.entries[at].path)) else {
            continue;
        };
        index.set_stat(
            at,
            &Stat {
                ctime_sec: meta.ctime() as u32,
                ctime_nsec: meta.ctime_nsec() as u32,
                mtime_sec: meta.mtime() as u32,
                mtime_nsec: meta.mtime_nsec() as u32,
                dev: meta.dev() as u32,
                ino: meta.ino() as u32,
                uid: meta.uid(),
                gid: meta.gid(),
                size: meta.size() as u32,
            },
        );
    }
}

#[cfg(windows)]
fn refresh_stat(_index: &mut Index, _worktree: &Path) {
    // Git for Windows stores zero for ino, dev, uid and gid, and the creation
    // time as ctime. Both timestamps survive a clone, so the entries still match.
}

/// Installs the source index in the new worktree, then makes the worktree an
/// exact checkout of its HEAD. Files the source had modified, staged or deleted
/// are the only ones git has to touch.
fn finish_checkout(plan: &mut Plan, new: &Repo) -> Result<()> {
    let index = plan
        .index
        .as_mut()
        .ok_or_else(|| Error::new("no index to install"))?;
    refresh_stat(index, &new.worktree);
    fs::write(new.index_path(), index.to_worktree_bytes()?)?;
    git::quiet(&new.worktree, &["reset", "--hard", "--quiet"])
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
    let mut plan = build_plan(&source, options.carry, options.threads)?;
    let target = cwd.join(&options.path);
    let capability = probe(&plan.repo.worktree, &target)?;
    let strategy = resolve_mode(options.mode);
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
    if !plan.manifest.skipped.is_empty() {
        warnings.push(format!(
            "{} entries could not be read during the scan, first: {}",
            plan.manifest.skipped.len(),
            plan.manifest.skipped[0]
        ));
    }
    let volatile: Vec<PathBuf> = plan
        .projects
        .iter()
        .flat_map(|project| adapter::volatile_files(&plan.repo.worktree, project))
        .collect();
    for project in &plan.projects {
        if let Some(editor) = adapter::running_editor(&plan.repo.worktree, project) {
            let who = editor
                .process_id
                .map_or_else(String::new, |pid| format!(" (pid {pid})"));
            observer.warn(&format!(
                "{} editor{who} has {} open; if it writes during the clone the carried cache may need a repair import",
                project.engine,
                plan.repo.worktree.join(&project.root).display()
            ));
        }
    }

    if options.dry_run {
        return Ok(AddOutcome {
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
        let before = snapshot(&volatile);
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
        if before != snapshot(&volatile) {
            warnings.push(
                "the engine's asset database changed during the clone; the first launch may repair it"
                    .to_owned(),
            );
        }
        describe_failures(&report, &mut warnings);
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
