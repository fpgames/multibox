//! Thin wrappers around the `git` executable.

use crate::{Error, Result};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn command(dir: &Path) -> Command {
    let mut command = Command::new("git");
    command.current_dir(dir);
    command
}

/// Runs git in `dir` and returns its stdout.
pub fn output<S: AsRef<OsStr>>(dir: &Path, args: &[S]) -> Result<String> {
    let result = command(dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|err| Error::new(format!("cannot run git: {err}")))?;
    if result.status.success() {
        Ok(String::from_utf8_lossy(&result.stdout).into_owned())
    } else {
        Err(Error::new(
            String::from_utf8_lossy(&result.stderr).trim().to_owned(),
        ))
    }
}

/// Runs git in `dir` with the caller's terminal attached.
pub fn run<S: AsRef<OsStr>>(dir: &Path, args: &[S]) -> Result<()> {
    let status = command(dir)
        .args(args)
        .stdin(Stdio::null())
        .status()
        .map_err(|err| Error::new(format!("cannot run git: {err}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::new(format!("git exited with {status}")))
    }
}

/// Runs git in `dir` with all output discarded except for errors.
pub fn quiet<S: AsRef<OsStr>>(dir: &Path, args: &[S]) -> Result<()> {
    output(dir, args).map(|_| ())
}

/// Runs git with inherited stdio and reports its exit code; used to pass
/// subcommands straight through to `git worktree`.
pub fn passthrough<S: AsRef<OsStr>>(dir: &Path, args: &[S]) -> Result<i32> {
    let status = command(dir)
        .args(args)
        .status()
        .map_err(|err| Error::new(format!("cannot run git: {err}")))?;
    Ok(status.code().unwrap_or(1))
}

#[derive(Clone, Debug)]
pub struct Repo {
    /// Root of the worktree `discover` was pointed at.
    pub worktree: PathBuf,
    /// Per-worktree git directory, for example `.git/worktrees/feat`.
    pub git_dir: PathBuf,
    /// Shared git directory.
    pub common_dir: PathBuf,
    pub hash_len: usize,
}

impl Repo {
    pub fn discover(dir: &Path) -> Result<Repo> {
        let out = output(
            dir,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--show-toplevel",
                "--absolute-git-dir",
                "--git-common-dir",
                "--show-object-format",
            ],
        )?;
        let mut lines = out.lines();
        let mut next = || {
            lines
                .next()
                .map(str::to_owned)
                .ok_or_else(|| Error::new("unexpected output from git rev-parse"))
        };
        let worktree = PathBuf::from(next()?);
        let git_dir = PathBuf::from(next()?);
        let common_dir = PathBuf::from(next()?);
        let hash_len = if next()? == "sha256" { 32 } else { 20 };
        Ok(Repo {
            worktree,
            git_dir,
            common_dir,
            hash_len,
        })
    }

    pub fn index_path(&self) -> PathBuf {
        self.git_dir.join("index")
    }
}

/// One record from `git worktree list --porcelain`.
#[derive(Clone, Debug, Default)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub bare: bool,
    pub detached: bool,
    pub locked: bool,
    pub prunable: bool,
}

pub fn worktrees(dir: &Path) -> Result<Vec<WorktreeInfo>> {
    let out = output(dir, &["worktree", "list", "--porcelain"])?;
    let mut list = Vec::new();
    let mut current: Option<WorktreeInfo> = None;
    for line in out.lines() {
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        match key {
            "worktree" => {
                list.extend(current.take());
                current = Some(WorktreeInfo {
                    path: PathBuf::from(value),
                    ..WorktreeInfo::default()
                });
            }
            _ => {
                if let Some(info) = current.as_mut() {
                    match key {
                        "HEAD" => info.head = Some(value.to_owned()),
                        "branch" => info.branch = Some(value.to_owned()),
                        "bare" => info.bare = true,
                        "detached" => info.detached = true,
                        "locked" => info.locked = true,
                        "prunable" => info.prunable = true,
                        _ => {}
                    }
                }
            }
        }
    }
    list.extend(current);
    Ok(list)
}
