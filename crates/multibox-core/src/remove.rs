use crate::adapter;
use crate::git::{self, Repo, worktrees};
use crate::index::Index;
use crate::{Error, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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

pub(crate) fn same_path(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// The repository of a directory git lists as a worktree.
///
/// `git rev-parse` walks up from a directory without a `.git`, so a worktree
/// whose `.git` is gone, typically while another tool is deleting it, reads as
/// whatever repository contains it. For a worktree kept inside the main one,
/// such as `.cindy-worktrees/feat`, that is the main worktree, whose editor
/// and changes would then be reported as this worktree's.
pub(crate) fn discover_worktree(dir: &Path, shown: &str) -> Result<Repo> {
    let repo = Repo::discover(dir)?;
    if !same_path(&repo.worktree, dir) {
        return Err(Error::new(format!(
            "'{shown}' has lost its .git, so git no longer sees a working tree there; \
             if no other tool is deleting it, check what is left in it, \
             then delete it and run `git worktree prune`"
        )));
    }
    Ok(repo)
}

pub(crate) fn refuse_if_unity_in_use(repo: &Repo, force_editor: bool) -> Result<()> {
    let Ok(index) = Index::read(&repo.index_path(), repo.hash_len) else {
        return Ok(());
    };
    let tracked: Vec<&str> = index.entries.iter().map(|e| e.path.as_str()).collect();
    for project in adapter::detect(&repo.worktree, &tracked) {
        if !force_editor && adapter::running_editor(&repo.worktree, &project).is_some() {
            return Err(Error::new(format!(
                "a {} editor has {} open; close it or use --force",
                project.engine,
                repo.worktree.join(&project.root).display()
            )));
        }
        if adapter::unity_datastore_in_use(&repo.worktree, &project) {
            return Err(Error::new(format!(
                "Unity DataStore still has its database open in {}; close Unity and retry after DataStore exits",
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
/// A Cindy runtime lease always prevents removal, including with `--force`.
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

    let repo = discover_worktree(&target, &options.path)?;
    if options.force == 0 {
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
    // Re-read runtime evidence immediately before changing the directory. This
    // also applies to --force: it only authorizes discarding local changes.
    crate::cindy::refuse_if_in_use(&target)?;
    refuse_if_unity_in_use(&repo, options.force > 0)?;
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
    crate::cindy::refuse_if_in_use(&target)?;
    refuse_if_unity_in_use(&repo, options.force > 0)?;
    git::quiet(cwd, &args)?;
    Ok(RemoveOutcome {
        path: target,
        trash: None,
    })
}

/// Deletes a directory created by [`remove`]. Any other path is refused, so a
/// bad argument can never turn into deleting something else.
pub fn purge_trash(path: &Path) -> Result<multibox_clone::PurgeReport> {
    let Some(name) = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| name.starts_with(TRASH_PREFIX))
    else {
        return Err(Error::new(format!(
            "refusing to purge {}: not a multibox trash directory",
            path.display()
        )));
    };
    let Some(claimed) = claim(path, &name)? else {
        return Ok(multibox_clone::PurgeReport::default());
    };
    Ok(multibox_clone::purge(&claimed, None)?)
}

const CLAIM_MARK: &str = "~purging-";

/// Renames trash to mark this process as the one deleting it. Several removals
/// running at once each sweep up leftover trash, and two of them deleting the
/// same tree trip over each other. A rename succeeds for exactly one.
///
/// Returns `None` when there is nothing to do: the trash is gone, or another
/// live process has claimed it. A claim whose process has exited is taken over.
fn claim(path: &Path, name: &str) -> Result<Option<PathBuf>> {
    let own = std::process::id();
    let base = match name.split_once(CLAIM_MARK) {
        Some((base, owner)) => match owner.parse::<u32>() {
            Ok(pid) if pid == own => return Ok(Some(path.to_path_buf())),
            Ok(pid) if adapter::process_alive(pid) => return Ok(None),
            _ => base,
        },
        None => name,
    };
    let claimed = path.with_file_name(format!("{base}{CLAIM_MARK}{own}"));
    // On Windows a rename that loses the race to another one does not always
    // find the trash gone: while the winner's rename is under way it is denied
    // access, as it is while an indexer or a virus scanner holds the directory.
    // Once the trash is gone it was claimed; until then, try again shortly.
    let mut delay = std::time::Duration::from_millis(20);
    for attempt in 0.. {
        match fs::rename(path, &claimed) {
            Ok(()) => return Ok(Some(claimed)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) if fs::symlink_metadata(path).is_err() => return Ok(None),
            Err(err) if attempt == 6 => return Err(err.into()),
            Err(_) => {
                std::thread::sleep(delay);
                delay *= 2;
            }
        }
    }
    unreachable!("the loop returns by its seventh attempt")
}

/// A pipe held by the caller stays open for as long as any process has a copy of
/// it, so a child that inherits one keeps the caller waiting for EOF. Rust's
/// spawn passes on every inheritable handle, even with stdio redirected.
#[cfg(windows)]
fn disown_standard_handles() {
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        unsafe {
            SetHandleInformation(GetStdHandle(id), HANDLE_FLAG_INHERIT, 0);
        }
    }
}

/// Runs `command` as a background process that outlives this one and does not
/// hold on to the caller's terminal or pipes.
pub fn spawn_detached(mut command: Command) -> std::io::Result<()> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
        disown_standard_handles();
    }
    command.spawn().map(|_| ())
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
