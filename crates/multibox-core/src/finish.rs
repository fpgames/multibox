use crate::adapter;
use crate::config::Config;
use crate::git::{self, Repo};
use crate::index::Index;
use crate::remove::{RemoveOptions, RemoveOutcome, refuse_if_editor_open, remove};
use crate::{Error, Result};
use std::path::Path;

pub struct FinishOptions {
    /// The worktree whose branch comes back, removed once it has.
    pub path: String,
    /// Keep the branch after it is merged, as `.multibox.toml` can also ask.
    pub keep_branch: bool,
    /// Passed to `git merge` last, after the adapters' own and those of
    /// `.multibox.toml`, such as `--no-ff`.
    pub merge_args: Vec<String>,
}

#[derive(Debug)]
pub enum FinishOutcome {
    /// The branch is part of the receiving branch and the worktree is gone.
    Finished {
        branch: String,
        into: String,
        /// False when the branch had been merged before, by hand or by a
        /// previous `finish` that stopped on conflicts.
        merged_now: bool,
        merge_output: String,
        removal: RemoveOutcome,
        branch_deleted: bool,
    },
    /// The merge stopped on conflicts. It is left in progress, as `git merge`
    /// leaves it, and nothing was removed.
    Conflicts {
        branch: String,
        into: String,
        files: Vec<String>,
    },
}

fn current_branch(dir: &Path) -> Option<String> {
    git::output(dir, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .ok()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
}

/// The engines' merge options, then those of `.multibox.toml`.
fn merge_options(repo: &Repo, config: &Config) -> Vec<String> {
    let mut options = match Index::read(&repo.index_path(), repo.hash_len) {
        Ok(index) => {
            let tracked: Vec<&str> = index.entries.iter().map(|e| e.path.as_str()).collect();
            adapter::merge_options(&adapter::detect_enabled(&repo.worktree, &tracked, config))
        }
        Err(_) => Vec::new(),
    };
    options.extend(config.merge_options.iter().cloned());
    options
}

/// Merges the branch of the worktree at `options.path` into the branch checked
/// out at `cwd`, then removes the worktree and the merged branch.
///
/// Every check runs before anything changes: the work must be committed, the
/// receiving worktree clean, and no editor may hold the finished one open. A
/// merge that stops on conflicts is left in progress, as `git merge` leaves
/// it; once it is committed, running `finish` again only cleans up.
pub fn finish(cwd: &Path, options: &FinishOptions) -> Result<FinishOutcome> {
    let target = Repo::discover(cwd)?;
    let task = Repo::discover(&cwd.join(&options.path))?;
    if task.git_dir == target.git_dir {
        return Err(Error::new(
            "run finish from the worktree that should receive the branch",
        ));
    }
    let branch = current_branch(&task.worktree).ok_or_else(|| {
        Error::new(format!(
            "'{}' has no branch checked out; create one with `git switch -c <name>` there first",
            options.path
        ))
    })?;
    let into = current_branch(&target.worktree).ok_or_else(|| {
        Error::new(format!(
            "{} has no branch checked out to merge into",
            target.worktree.display()
        ))
    })?;
    if target.git_dir.join("MERGE_HEAD").exists() {
        return Err(Error::new(format!(
            "a merge is in progress in {}; commit it or run `git merge --abort` first",
            target.worktree.display()
        )));
    }
    let uncommitted = git::output(
        &task.worktree,
        &["status", "--porcelain", "--ignore-submodules=none"],
    )?;
    if !uncommitted.is_empty() {
        return Err(Error::new(format!(
            "'{}' has uncommitted changes; commit them on '{branch}' first",
            options.path
        )));
    }
    let receiving = git::output(
        &target.worktree,
        &["status", "--porcelain", "--untracked-files=no"],
    )?;
    if !receiving.is_empty() {
        return Err(Error::new(format!(
            "{} has uncommitted changes to tracked files; commit or stash them first",
            target.worktree.display()
        )));
    }
    refuse_if_editor_open(&task)?;
    let config = Config::load(&target.worktree);

    let merged_before = git::quiet(
        &target.worktree,
        &["merge-base", "--is-ancestor", branch.as_str(), "HEAD"],
    )
    .is_ok();
    let mut merge_output = String::new();
    if !merged_before {
        let mut args = vec!["merge".to_owned(), "--no-edit".to_owned()];
        args.extend(merge_options(&target, &config));
        args.extend(options.merge_args.iter().cloned());
        args.push(branch.clone());
        match git::output(&target.worktree, &args) {
            Ok(output) => merge_output = output,
            Err(err) => {
                let files: Vec<String> = git::output(
                    &target.worktree,
                    &["diff", "--name-only", "--diff-filter=U"],
                )?
                .lines()
                .map(str::to_owned)
                .collect();
                if files.is_empty() && !target.git_dir.join("MERGE_HEAD").exists() {
                    return Err(Error::new(format!(
                        "git merge failed and nothing was removed: {err}"
                    )));
                }
                return Ok(FinishOutcome::Conflicts {
                    branch,
                    into,
                    files,
                });
            }
        }
    }

    let removal = remove(
        cwd,
        &RemoveOptions {
            path: options.path.clone(),
            force: 0,
        },
    )?;
    let keep_branch = options.keep_branch || config.keep_branch;
    let branch_deleted =
        !keep_branch && git::quiet(&target.worktree, &["branch", "-d", branch.as_str()]).is_ok();
    Ok(FinishOutcome::Finished {
        branch,
        into,
        merged_now: !merged_before,
        merge_output,
        removal,
        branch_deleted,
    })
}
