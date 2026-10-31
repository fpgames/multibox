//! `git multibox migrate`: moves a project onto a reflink-capable volume.
//!
//! Migration is two deliberate steps. `migrate` makes an independent copy and
//! leaves the source untouched, so the copy can be tried for as long as needed.
//! `migrate --switch` then redirects the old path to the new location. Neither
//! step deletes anything: `--switch` keeps the original under a new name.

use crate::adapter;
use crate::add::Observer;
use crate::git::{self, Repo};
use crate::index::Index;
use crate::{Error, Result};
use multibox_clone::{
    Decision, Options, Progress, Report, ScanRequest, Strategy, TreeFilter, execute, free_space,
    scan,
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

fn refuse_if_editor_open(repo: &Repo, action: &str) -> Result<()> {
    for project in projects(repo) {
        if adapter::running_editor(&repo.worktree, &project).is_some() {
            return Err(Error::new(format!(
                "close the {} editor before {action}: it is writing to {}",
                project.engine,
                repo.worktree.join(&project.root).display()
            )));
        }
    }
    Ok(())
}

/// Copies the main worktree, history included, to `destination`. The source is
/// only read.
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

    let mut skipped = vec![".git/worktrees".to_owned()];
    skipped.extend(
        options
            .exclude
            .iter()
            .map(|path| path.trim_matches('/').to_owned()),
    );
    for project in projects(&repo) {
        skipped.push(project.path("Temp"));
        skipped.push(project.path("Logs"));
    }
    let filter = CopyFilter { skipped };
    let manifest = scan(
        &repo.worktree,
        &ScanRequest {
            files: Vec::new(),
            links: Vec::new(),
            trees: vec![String::new()],
            filter: &filter,
            threads: options.threads,
        },
    )?;
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
            strategy: Strategy::Copy,
            threads: options.threads,
            progress: Some(&progress),
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
}

/// Points the old project path at a migrated copy, so Unity Hub, IDE history and
/// scripts keep working. The original is renamed, not deleted.
///
/// Linked worktrees are not moved. Their registrations are copied across and
/// each one is repointed at the migrated repository.
pub fn switch(cwd: &Path, destination: &Path) -> Result<SwitchOutcome> {
    let repo = main_worktree(cwd)?;
    let destination = fs::canonicalize(destination)?;
    if !destination.join(".git").is_dir() {
        return Err(Error::new(format!(
            "{} is not a migrated repository",
            destination.display()
        )));
    }
    let source_head = git::output(&repo.worktree, &["rev-parse", "HEAD"])?;
    let target_head = git::output(&destination, &["rev-parse", "HEAD"])?;
    if source_head != target_head {
        return Err(Error::new(format!(
            "{} and {} are on different commits; migrate again first",
            repo.worktree.display(),
            destination.display()
        )));
    }
    refuse_if_editor_open(&repo, "switching")?;

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
    fs::rename(&repo.worktree, &kept_at)?;
    if let Err(err) = make_directory_link(&repo.worktree, &destination) {
        fs::rename(&kept_at, &repo.worktree)?;
        return Err(err);
    }

    let mut args = vec!["worktree".to_owned(), "repair".to_owned()];
    args.extend(linked.iter().map(|path| path.display().to_string()));
    git::quiet(&destination, &args)?;
    Ok(SwitchOutcome { kept_at })
}

fn copy_registrations(from: &Path, to: &Path) -> Result<()> {
    let everything = |_: &str, _: bool| Decision::Include;
    let manifest = scan(
        from,
        &ScanRequest {
            files: Vec::new(),
            links: Vec::new(),
            trees: vec![String::new()],
            filter: &everything,
            threads: None,
        },
    )?;
    fs::create_dir_all(to)?;
    let report = execute(&manifest, from, to, &Options::default())?;
    match report.failures.first() {
        Some(failure) => Err(Error::new(format!(
            "cannot copy worktree registration {}: {}",
            failure.path, failure.error
        ))),
        None => Ok(()),
    }
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
