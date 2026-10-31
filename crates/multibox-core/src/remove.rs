use crate::adapter;
use crate::git::{self, Repo, worktrees};
use crate::index::Index;
use crate::{Error, Result};
use std::fs;
use std::path::{Path, PathBuf};

const TRASH_PREFIX: &str = ".multibox-trash-";

pub struct RemoveOptions {
    pub path: String,
    /// Number of `--force` flags, with the same meaning as in `git worktree remove`.
    pub force: u8,
}

#[derive(Debug)]
pub struct RemoveOutcome {
    pub path: PathBuf,
    /// Directory still being deleted, when removal was deferred.
    pub trash: Option<PathBuf>,
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn refuse_if_editor_open(repo: &Repo) -> Result<()> {
    let Ok(index) = Index::read(&repo.index_path(), repo.hash_len) else {
        return Ok(());
    };
    let tracked: Vec<&str> = index.entries.iter().map(|e| e.path.as_str()).collect();
    for project in adapter::detect(&repo.worktree, &tracked) {
        if adapter::running_editor(&repo.worktree, &project).is_some() {
            return Err(Error::new(format!(
                "a {} editor has {} open; close it or use --force",
                project.engine,
                repo.worktree.join(&project.root).display()
            )));
        }
    }
    Ok(())
}

fn trash_path(target: &Path) -> Option<PathBuf> {
    let parent = target.parent()?;
    let name = target.file_name()?.to_string_lossy();
    Some(parent.join(format!("{TRASH_PREFIX}{name}-{}", std::process::id())))
}

/// Moves the worktree aside and leaves a stub for `git worktree remove`, so git
/// still performs its own bookkeeping while the expensive delete happens later.
fn detach_contents(target: &Path, trash: &Path) -> std::io::Result<()> {
    fs::rename(target, trash)?;
    let stub =
        fs::create_dir(target).and_then(|()| fs::copy(trash.join(".git"), target.join(".git")));
    if let Err(err) = stub {
        let _ = fs::remove_dir_all(target);
        fs::rename(trash, target)?;
        return Err(err);
    }
    Ok(())
}

/// Removes a linked worktree with the semantics of `git worktree remove`:
/// a dirty or locked worktree needs `--force`, the main worktree is refused.
///
/// Returns right after detaching the contents. The caller decides how to run
/// [`purge_trash`] on [`RemoveOutcome::trash`], typically in a background process.
pub fn remove(cwd: &Path, options: &RemoveOptions) -> Result<RemoveOutcome> {
    let target = cwd.join(&options.path);
    let list = worktrees(cwd)?;
    let position = list
        .iter()
        .position(|info| same_path(&info.path, &target))
        .ok_or_else(|| Error::new(format!("'{}' is not a working tree", options.path)))?;
    if position == 0 {
        return Err(Error::new(format!(
            "'{}' is a main working tree",
            options.path
        )));
    }
    let info = &list[position];
    let target = info.path.clone();
    let force_required = if info.locked { 2 } else { 0 };
    if options.force < force_required {
        return Err(Error::new(format!(
            "'{}' is locked; use 'remove -f -f' to override",
            options.path
        )));
    }

    let repo = Repo::discover(&target)?;
    if options.force == 0 {
        refuse_if_editor_open(&repo)?;
        let dirty = git::output(
            &target,
            &["status", "--porcelain", "--ignore-submodules=none"],
        )?;
        if !dirty.is_empty() {
            return Err(Error::new(format!(
                "'{}' contains modified or untracked files, use --force to delete it",
                options.path
            )));
        }
    }

    let flags: Vec<&str> =
        std::iter::repeat_n("--force", usize::from(options.force.max(1))).collect();
    let target_text = target.to_string_lossy().into_owned();
    let mut args = vec!["worktree", "remove"];
    args.extend(&flags);
    args.push(&target_text);

    // Submodules need git's own checks, and the fast path cannot help anyway.
    let has_submodules = repo.git_dir.join("modules").exists();
    let trash = trash_path(&target).filter(|_| !has_submodules);
    if let Some(trash) = &trash {
        if detach_contents(&target, trash).is_ok() {
            if let Err(err) = git::quiet(cwd, &args) {
                let _ = fs::remove_dir_all(&target);
                fs::rename(trash, &target)?;
                return Err(err);
            }
            return Ok(RemoveOutcome {
                path: target,
                trash: Some(trash.clone()),
            });
        }
    }

    // Something inside is held open, or the fast path does not apply.
    git::quiet(cwd, &args)?;
    Ok(RemoveOutcome {
        path: target,
        trash: None,
    })
}

/// Deletes a directory created by [`remove`]. Any other path is refused, so a
/// bad argument can never turn into deleting something else.
pub fn purge_trash(path: &Path) -> Result<multibox_clone::PurgeReport> {
    let is_trash = path
        .file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with(TRASH_PREFIX));
    if !is_trash {
        return Err(Error::new(format!(
            "refusing to purge {}: not a multibox trash directory",
            path.display()
        )));
    }
    Ok(multibox_clone::purge(path, None)?)
}

/// Finds trash left next to the worktrees of this repository by an interrupted
/// purge, so it can be resumed.
pub fn sweep_trash(cwd: &Path) -> Vec<PathBuf> {
    let Ok(list) = worktrees(cwd) else {
        return Vec::new();
    };
    let mut parents: Vec<PathBuf> = list
        .iter()
        .filter_map(|info| info.path.parent().map(Path::to_path_buf))
        .collect();
    parents.sort();
    parents.dedup();
    let mut found = Vec::new();
    for parent in parents {
        let Ok(entries) = fs::read_dir(&parent) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(TRASH_PREFIX)
            {
                found.push(entry.path());
            }
        }
    }
    found
}
