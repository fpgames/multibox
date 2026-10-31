//! `git multibox init`: gets a project onto a volume with reflinks.
//!
//! Reflinks only work within one volume, so on Windows the project has to live
//! on the Dev Drive. `init` does whatever is left to do and can be run again at
//! any point:
//!
//! 1. Create the Dev Drive if the machine has none.
//! 2. Copy the project across. The source is only read, so the editor can stay
//!    open and work can continue during the copy.
//! 3. Once the editor is closed, bring the copy up to date with whatever changed
//!    meanwhile, then redirect the old path to the new location.
//!
//! Nothing is deleted. The original keeps its contents under a new name, for the
//! user to remove once they are satisfied.

use crate::adapter;
use crate::add::{Observer, refresh_stat};
use crate::git::{self, Repo};
use crate::index::Index;
use crate::setup::{Mount, SetupOptions, plan_setup, run_setup};
use crate::{Error, Result};
use multibox_clone::{
    Decision, Options, Progress, Report, ScanRequest, TreeFilter, execute, free_space, probe, scan,
    sync, volumes,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct MigrateOptions {
    pub destination: PathBuf,
    /// Paths relative to the repository root to leave behind, typically build
    /// output that can be regenerated.
    pub exclude: Vec<String>,
    pub dry_run: bool,
    pub threads: Option<usize>,
}

#[derive(Debug)]
pub struct MigrateOutcome {
    pub destination: PathBuf,
    pub files: u64,
    pub bytes: u64,
    pub report: Option<Report>,
}

/// Skips per-instance engine state and, crucially, the registrations of linked
/// worktrees: they point back at the source, so a copy that kept them would
/// believe it owned those worktrees and could prune them.
struct CopyFilter {
    skipped: Vec<String>,
}

impl CopyFilter {
    fn new(repo: &Repo, exclude: &[String]) -> CopyFilter {
        let mut skipped = vec![".git/worktrees".to_owned()];
        skipped.extend(exclude.iter().map(|path| path.trim_matches('/').to_owned()));
        for project in projects(repo) {
            skipped.push(project.path("Temp"));
            skipped.push(project.path("Logs"));
        }
        CopyFilter { skipped }
    }
}

impl TreeFilter for CopyFilter {
    fn check(&self, rel: &str, _is_dir: bool) -> Decision {
        let instance_state = rel.ends_with("/Library/EditorInstance.json")
            || rel == "Library/EditorInstance.json"
            || rel.ends_with("/Library/Pipeline")
            || rel == "Library/Pipeline"
            || (rel.contains("Library/") && (rel.ends_with("-lock") || rel.ends_with(".pid")));
        if instance_state || self.skipped.iter().any(|path| path == rel) {
            Decision::Exclude
        } else {
            Decision::Include
        }
    }
}

fn same_directory(a: &Path, b: &Path) -> bool {
    matches!((fs::canonicalize(a), fs::canonicalize(b)), (Ok(a), Ok(b)) if a == b)
}

fn main_worktree(cwd: &Path) -> Result<Repo> {
    let repo = Repo::discover(cwd)?;
    let list = git::worktrees(cwd)?;
    let main = list
        .first()
        .ok_or_else(|| Error::new("repository has no worktrees"))?;
    Repo::discover(&main.path).or(Ok(repo))
}

fn projects(repo: &Repo) -> Vec<adapter::Project> {
    let Ok(index) = Index::read(&repo.index_path(), repo.hash_len) else {
        return Vec::new();
    };
    let tracked: Vec<&str> = index.entries.iter().map(|e| e.path.as_str()).collect();
    adapter::detect(&repo.worktree, &tracked)
}

fn open_editor(repo: &Repo) -> Option<String> {
    projects(repo).iter().find_map(|project| {
        adapter::running_editor(&repo.worktree, project).map(|_| {
            format!(
                "the {} editor has {} open",
                project.engine,
                repo.worktree.join(&project.root).display()
            )
        })
    })
}

/// Rewrites the copied index for its new files, so the first `git status` after
/// a move does not re-hash the whole project. An index that cannot be reused is
/// left as copied: git simply rebuilds the stat data on its first run.
fn refresh_copied_index(repo: &Repo, destination: &Path) -> Result<()> {
    let Ok(mut index) = Index::read(&repo.index_path(), repo.hash_len) else {
        return Ok(());
    };
    refresh_stat(&mut index, &repo.worktree, &repo.index_path(), destination);
    fs::write(
        destination.join(".git").join("index"),
        index.to_worktree_bytes()?,
    )?;
    Ok(())
}

fn selection(
    repo: &Repo,
    exclude: &[String],
    threads: Option<usize>,
) -> Result<multibox_clone::Manifest> {
    let filter = CopyFilter::new(repo, exclude);
    Ok(scan(
        &repo.worktree,
        &ScanRequest {
            files: Vec::new(),
            links: Vec::new(),
            trees: vec![String::new()],
            filter: &filter,
            threads,
        },
    )?)
}

/// Copies the main worktree, history included, to `destination`. The source is
/// only read, so an open editor is fine: [`switch`] picks up later changes.
pub fn migrate(
    cwd: &Path,
    options: &MigrateOptions,
    observer: &dyn Observer,
) -> Result<MigrateOutcome> {
    let repo = main_worktree(cwd)?;
    if options.destination.exists() {
        return Err(Error::new(format!(
            "{} already exists",
            options.destination.display()
        )));
    }

    let manifest = selection(&repo, &options.exclude, options.threads)?;
    let files = manifest.files.len() as u64;
    let bytes = manifest.bytes;

    if options.dry_run {
        return Ok(MigrateOutcome {
            destination: options.destination.clone(),
            files,
            bytes,
            report: None,
        });
    }
    let free = free_space(&options.destination)?;
    if bytes + (1 << 30) > free {
        return Err(Error::new(format!(
            "not enough space at {}: need {bytes} bytes, {free} free",
            options.destination.display()
        )));
    }

    fs::create_dir_all(&options.destination)?;
    let progress = Arc::new(Progress::default());
    observer.clone_started(files, bytes, progress.clone());
    let report = execute(
        &manifest,
        &repo.worktree,
        &options.destination,
        &Options {
            threads: options.threads,
            progress: Some(&progress),
            ..Options::default()
        },
    );
    observer.clone_finished();
    let report = report?;
    if !report.failures.is_empty() {
        return Err(Error::new(format!(
            "{} file(s) could not be copied, first: {}: {}; the source is unchanged",
            report.failures.len(),
            report.failures[0].path,
            report.failures[0].error
        )));
    }
    refresh_copied_index(&repo, &options.destination)?;
    git::quiet(&options.destination, &["worktree", "prune"])?;
    Ok(MigrateOutcome {
        destination: options.destination.clone(),
        files,
        bytes,
        report: Some(report),
    })
}

#[derive(Debug)]
pub struct SwitchOutcome {
    /// Where the original project now lives.
    pub kept_at: PathBuf,
    /// Files that changed after the copy was made and were brought across.
    pub updated: u64,
}

/// Brings a migrated copy up to date, then points the old project path at it, so
/// Unity Hub, IDE history and scripts keep working. The original is renamed, not
/// deleted. Linked worktrees stay where they are and are repointed.
pub fn switch(cwd: &Path, destination: &Path, exclude: &[String]) -> Result<SwitchOutcome> {
    let repo = main_worktree(cwd)?;
    if same_directory(destination, &repo.worktree) {
        return Err(Error::new(format!(
            "{} is already the project; nothing to switch",
            destination.display()
        )));
    }
    let destination = fs::canonicalize(destination)?;
    if !destination.join(".git").is_dir() {
        return Err(Error::new(format!(
            "{} is not a migrated repository",
            destination.display()
        )));
    }
    if let Some(reason) = open_editor(&repo) {
        return Err(Error::new(format!("close it first: {reason}")));
    }

    let name = repo
        .worktree
        .file_name()
        .ok_or_else(|| Error::new("cannot switch a filesystem root"))?
        .to_string_lossy()
        .into_owned();
    let kept_at = repo
        .worktree
        .with_file_name(format!("{name}.multibox-original"));
    if kept_at.exists() {
        return Err(Error::new(format!("{} already exists", kept_at.display())));
    }

    let filter = CopyFilter::new(&repo, exclude);
    let synced = sync(&repo.worktree, &destination, &filter, None)?;
    if let Some(failure) = synced.failures.first() {
        return Err(Error::new(format!(
            "{} file(s) could not be updated, first: {}: {}; nothing was switched",
            synced.failures.len(),
            failure.path,
            failure.error
        )));
    }

    refresh_copied_index(&repo, &destination)?;

    let linked: Vec<PathBuf> = git::worktrees(&repo.worktree)?
        .into_iter()
        .skip(1)
        .map(|info| info.path)
        .collect();
    let source_worktrees = repo.common_dir.join("worktrees");
    if source_worktrees.is_dir() {
        copy_registrations(
            &source_worktrees,
            &destination.join(".git").join("worktrees"),
        )?;
    }

    fs::rename(&repo.worktree, &kept_at).map_err(|err| {
        Error::new(format!(
            "cannot rename {}: {err}. Close anything using it, such as an IDE or a terminal in that folder, and run this again from outside it; nothing was switched",
            repo.worktree.display()
        ))
    })?;
    if let Err(err) = make_directory_link(&repo.worktree, &destination) {
        fs::rename(&kept_at, &repo.worktree)?;
        return Err(err);
    }

    let mut args = vec!["worktree".to_owned(), "repair".to_owned()];
    args.extend(linked.iter().map(|path| path.display().to_string()));
    git::quiet(&destination, &args)?;
    Ok(SwitchOutcome {
        kept_at,
        updated: synced.updated + synced.removed,
    })
}

/// Copies the registrations the destination does not have yet, so that a switch
/// stopped by a failed rename can simply be run again.
fn copy_registrations(from: &Path, to: &Path) -> Result<()> {
    let everything = |_: &str, _: bool| Decision::Include;
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let source = entry?.path();
        let Some(name) = source.file_name() else {
            continue;
        };
        let target = to.join(name);
        if target.exists() {
            continue;
        }
        let manifest = scan(
            &source,
            &ScanRequest {
                files: Vec::new(),
                links: Vec::new(),
                trees: vec![String::new()],
                filter: &everything,
                threads: None,
            },
        )?;
        fs::create_dir_all(&target)?;
        let report = execute(&manifest, &source, &target, &Options::default())?;
        if let Some(failure) = report.failures.first() {
            return Err(Error::new(format!(
                "cannot copy worktree registration {}: {}",
                failure.path, failure.error
            )));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn make_directory_link(link: &Path, target: &Path) -> Result<()> {
    // Junctions need no privileges, unlike directory symlinks. cmd reads a
    // forward slash as a switch, and git reports paths with forward slashes.
    let native = |path: &Path| {
        let text = path.to_string_lossy();
        text.strip_prefix(r"\\?\")
            .unwrap_or(&text)
            .replace('/', "\\")
    };
    let out = std::process::Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(native(link))
        .arg(native(target))
        .output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(Error::new(format!(
            "cannot create junction: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

#[cfg(unix)]
fn make_directory_link(link: &Path, target: &Path) -> Result<()> {
    Ok(std::os::unix::fs::symlink(target, link)?)
}

#[derive(Default)]
pub struct InitOptions {
    /// Where the project should live; defaults to a folder on the Dev Drive.
    pub destination: Option<PathBuf>,
    pub exclude: Vec<String>,
    pub setup: SetupOptions,
    pub dry_run: bool,
}

#[derive(Debug)]
pub enum InitOutcome {
    /// The project already sits on a volume with reflinks.
    Ready { filesystem: String },
    /// This filesystem has no reflinks and the platform offers no way to add
    /// them. Worktrees keep working, as full copies.
    CopyModeOnly { filesystem: String },
    /// What a real run would do.
    Planned {
        destination: PathBuf,
        creates_area: bool,
        files: u64,
        bytes: u64,
    },
    /// The copy is in place, but an editor is using the original. Running init
    /// again after closing it finishes the job.
    WaitingForEditor {
        destination: PathBuf,
        reason: String,
    },
    Switched {
        destination: PathBuf,
        kept_at: PathBuf,
        updated: u64,
    },
}

fn existing_area() -> Option<PathBuf> {
    volumes()
        .into_iter()
        .filter(|volume| volume.fixed && volume.reflink)
        .max_by_key(|volume| volume.free)
        .map(|volume| PathBuf::from(volume.root))
}

/// ReFS spreads its writes across the whole volume and a dynamic VHDX never gives
/// a touched block back, so the maximum size is the real cost on the host disk.
/// It should fit the project rather than be generous: half as much again leaves
/// room for worktrees, which share data, and for the project to grow.
fn area_size_gib(project_bytes: u64) -> u64 {
    const MINIMUM_GIB: u64 = 100;
    let project_gib = project_bytes.div_ceil(1 << 30);
    (project_gib * 3 / 2).max(MINIMUM_GIB).next_multiple_of(50)
}

/// The root of a reflink-capable area, created if the machine has none.
fn ensure_area(
    options: &InitOptions,
    project_bytes: u64,
    observer: &dyn Observer,
) -> Result<PathBuf> {
    if let Some(root) = existing_area() {
        return Ok(root);
    }
    let mut setup = options.setup.clone();
    setup
        .size_gib
        .get_or_insert_with(|| area_size_gib(project_bytes));
    let plan = plan_setup(&setup)?;
    observer.note(&format!(
        "creating a Dev Drive of {} GiB in {}; no existing volume is touched",
        plan.size_gib,
        plan.vhdx.display()
    ));
    run_setup(&plan)?;
    Ok(match plan.mount {
        Mount::Letter(letter) => PathBuf::from(format!("{letter}:\\")),
        Mount::Folder(path) => path,
    })
}

/// Does whatever is left to give this project fast, space-sharing worktrees.
/// Safe to run repeatedly: each run picks up where the last one stopped.
pub fn init(cwd: &Path, options: &InitOptions, observer: &dyn Observer) -> Result<InitOutcome> {
    let repo = main_worktree(cwd)?;
    let capability = probe(&repo.worktree, &repo.worktree)?;
    let moved_already = options
        .destination
        .as_deref()
        .is_some_and(|path| same_directory(path, &repo.worktree));
    if capability.reflink && (options.destination.is_none() || moved_already) {
        return Ok(InitOutcome::Ready {
            filesystem: capability.filesystem,
        });
    }
    if moved_already || (options.destination.is_none() && !cfg!(windows)) {
        return Ok(InitOutcome::CopyModeOnly {
            filesystem: capability.filesystem,
        });
    }

    let name = repo
        .worktree
        .file_name()
        .ok_or_else(|| Error::new("cannot migrate a filesystem root"))?
        .to_owned();
    let copy = |destination: &Path, dry_run: bool| {
        migrate(
            cwd,
            &MigrateOptions {
                destination: destination.to_path_buf(),
                exclude: options.exclude.clone(),
                dry_run,
                threads: None,
            },
            observer,
        )
    };

    if options.dry_run {
        let root = existing_area();
        let creates_area = options.destination.is_none() && root.is_none();
        let destination = match &options.destination {
            Some(path) => path.clone(),
            None if creates_area => plan_setup(&options.setup).map(|plan| match plan.mount {
                Mount::Letter(letter) => PathBuf::from(format!("{letter}:\\")).join(&name),
                Mount::Folder(path) => path.join(&name),
            })?,
            None => root.unwrap_or_default().join(&name),
        };
        let planned = selection(&repo, &options.exclude, None)?;
        return Ok(InitOutcome::Planned {
            destination,
            creates_area,
            files: planned.files.len() as u64,
            bytes: planned.bytes,
        });
    }

    let destination = match &options.destination {
        Some(path) => path.clone(),
        None => {
            let bytes = selection(&repo, &options.exclude, None)?.bytes;
            ensure_area(options, bytes, observer)?.join(&name)
        }
    };
    if !destination.exists() {
        copy(&destination, false)?;
    }
    if let Some(reason) = open_editor(&repo) {
        return Ok(InitOutcome::WaitingForEditor {
            destination,
            reason,
        });
    }
    let switched = switch(cwd, &destination, &options.exclude)?;
    Ok(InitOutcome::Switched {
        destination,
        kept_at: switched.kept_at,
        updated: switched.updated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    #[test]
    fn the_area_fits_the_project() {
        assert_eq!(area_size_gib(316 * GIB), 500);
        assert_eq!(area_size_gib(100 * GIB), 150);
    }

    #[test]
    fn the_area_has_a_floor() {
        assert_eq!(area_size_gib(GIB), 100);
    }
}
