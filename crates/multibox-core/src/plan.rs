use crate::adapter::{self, Project};
use crate::git::{self, Repo};
use crate::index::Index;
use crate::rules::Rules;
use crate::{Error, Result};
use multibox_clone::{Manifest, ScanRequest, scan};
use std::collections::HashSet;
use std::fs;
use std::path::Path;

/// What a new worktree will receive, before anything is written.
pub struct Plan {
    pub repo: Repo,
    /// The source index, when it can be reused.
    pub index: Option<Index>,
    /// Why the source index cannot be reused.
    pub index_problem: Option<String>,
    pub projects: Vec<Project>,
    pub manifest: Manifest,
    pub summary: PlanSummary,
    /// Ignored directories the rules asked for that an adapter refused to
    /// carry in this mode.
    pub refused: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PlanSummary {
    pub tracked_files: u64,
    pub tracked_bytes: u64,
    pub cached_files: u64,
    pub cached_bytes: u64,
}

/// Top-level ignored entries of `worktree`. `--directory` stops git at the
/// outermost ignored directory, so huge caches are named but never walked here.
fn ignored_entries(worktree: &Path) -> Result<Vec<String>> {
    let out = git::output(
        worktree,
        &[
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
        ],
    )?;
    Ok(out
        .split('\0')
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect())
}

fn read_index(repo: &Repo) -> (Option<Index>, Option<String>) {
    match Index::read(&repo.index_path(), repo.hash_len) {
        Ok(index) => (Some(index), None),
        Err(err) if err.is_unsupported() => (None, Some(err.to_string())),
        Err(err) => (None, Some(format!("unreadable index: {err}"))),
    }
}

/// Lists every file a new worktree should get from `source`, without writing.
/// With `carry` off, only tracked files are selected. `frugal` skips caches
/// that are only worth carrying when cloning is free.
pub fn build_plan(
    source: &Path,
    carry: bool,
    frugal: bool,
    threads: Option<usize>,
) -> Result<Plan> {
    let repo = Repo::discover(source)?;
    let (index, index_problem) = read_index(&repo);

    // Project detection must not depend on the index being reusable: caches
    // are still worth carrying when git has to do the checkout itself.
    let listed;
    let tracked: Vec<&str> = match &index {
        Some(index) => index
            .entries
            .iter()
            .filter(|entry| entry.stage == 0 || entry.stage == 2)
            .map(|entry| entry.path.as_str())
            .collect(),
        None => {
            listed = git::output(&repo.worktree, &["ls-files", "-z"])?;
            listed.split('\0').filter(|path| !path.is_empty()).collect()
        }
    };
    let projects = adapter::detect(&repo.worktree, &tracked);
    let rules = Rules::load(&repo.worktree, &projects, frugal)?;

    let mut files = Vec::new();
    let mut links = Vec::new();
    let mut seen = HashSet::new();
    if let Some(index) = &index {
        for entry in &index.entries {
            if entry.skip_worktree || entry.is_gitlink() || !seen.insert(entry.path.as_str()) {
                continue;
            }
            if entry.is_symlink() {
                links.push(entry.path.clone());
            } else {
                files.push((entry.path.clone(), u64::from(entry.size)));
            }
        }
    }
    let tracked_files = files.len() as u64;
    let tracked_bytes: u64 = files.iter().map(|(_, size)| size).sum();

    let mut trees = Vec::new();
    let mut refused = Vec::new();
    if carry {
        for entry in ignored_entries(&repo.worktree)? {
            if let Some(dir) = entry.strip_suffix('/') {
                if rules.refuses(dir, true) {
                    refused.push(dir.to_owned());
                } else if multibox_clone::TreeFilter::check(&rules, dir, true)
                    != multibox_clone::Decision::Exclude
                {
                    trees.push(dir.to_owned());
                }
            } else if rules.includes(&entry, false) {
                let size = fs::metadata(repo.worktree.join(&entry)).map_or(0, |meta| meta.len());
                files.push((entry, size));
            }
        }
    }

    // Outermost first, so a report names the one a user would recognise.
    refused.sort_by_key(|dir| (dir.matches('/').count(), dir.clone()));

    let manifest = scan(
        &repo.worktree,
        &ScanRequest {
            files,
            links,
            trees,
            filter: &rules,
            threads,
        },
    )
    .map_err(|err| Error::new(format!("cannot scan {}: {err}", repo.worktree.display())))?;

    let cached_files = (manifest.files.len() as u64).saturating_sub(tracked_files);
    let summary = PlanSummary {
        tracked_files,
        tracked_bytes,
        cached_files,
        cached_bytes: manifest.bytes.saturating_sub(tracked_bytes),
    };
    Ok(Plan {
        repo,
        index,
        index_problem,
        projects,
        manifest,
        summary,
        refused,
    })
}
