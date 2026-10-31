//! Engine adapters teach Multibox what a project's caches look like.
//!
//! An adapter only describes a project; it does no file work of its own. That
//! keeps the clone pipeline identical for every engine, so adding one means
//! answering a few questions: where are the projects, which ignored paths are
//! worth carrying, which must stay behind, which carried files record where the
//! project lives, and is an editor using this project?
//!
//! Every answer has to leave the new worktree at least as good as a native one.
//! A cache is carried only if it is cheaper to clone than to rebuild, and only
//! in a form that cannot reach back into the source.

mod cargo;
mod pnpm;
mod unity;

pub use cargo::Cargo;
pub use pnpm::Pnpm;
pub use unity::Unity;

use crate::Result;
use crate::git::{self, Repo};
use std::path::Path;

/// A project found inside the repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Project {
    pub engine: &'static str,
    /// Project root relative to the worktree root; empty for the root itself.
    pub root: String,
    pub version: Option<String>,
    /// Packages the project compiles from its own sources. Their build output
    /// records where the project lives, so it is never carried.
    pub local_packages: Vec<String>,
}

impl Project {
    pub fn path(&self, relative: &str) -> String {
        if self.root.is_empty() {
            relative.to_owned()
        } else {
            format!("{}/{relative}", self.root)
        }
    }
}

/// An editor process holding a project open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Editor {
    pub process_id: Option<u32>,
}

pub trait Adapter: Sync {
    /// Finds projects from the list of tracked paths.
    fn detect(&self, worktree: &Path, tracked: &[&str]) -> Vec<Project>;
    /// Gitignore-style patterns, relative to the worktree root.
    fn includes(&self, project: &Project) -> Vec<String>;
    /// Patterns that override [`Adapter::includes`].
    fn excludes(&self, project: &Project) -> Vec<String>;
    /// Large caches that are worth carrying only when cloning them is free.
    /// Left behind in copy mode, where each one costs real disk space.
    fn expensive(&self, _project: &Project) -> Vec<String> {
        Vec::new()
    }
    /// Caches whose own tool restores them faster than a copy could carry them.
    /// Unlike [`Adapter::expensive`] ones, they stay behind in copy mode even
    /// when `.worktreeinclude` asks for them.
    fn reflink_only(&self, _project: &Project) -> Vec<String> {
        Vec::new()
    }
    /// Carried files that record the project's absolute path. The source path
    /// in them is rewritten to the new worktree after cloning.
    fn relocated(&self, _project: &Project) -> Vec<String> {
        Vec::new()
    }
    /// Databases an editor writes to while running. They are cloned before
    /// everything else and retried if they change, so keep this list short.
    fn volatile_files(&self, _project: &Project) -> Vec<String> {
        Vec::new()
    }
    fn running_editor(&self, _worktree: &Path, _project: &Project) -> Option<Editor> {
        None
    }
    /// Variables that keep engine tooling inside this worktree.
    fn environment(&self, _worktree: &Path, _project: &Project) -> Vec<(String, String)> {
        Vec::new()
    }
}

pub fn adapters() -> &'static [&'static dyn Adapter] {
    &[&Unity, &Cargo, &Pnpm]
}

fn adapter_for(project: &Project) -> Option<&'static dyn Adapter> {
    match project.engine {
        "unity" => Some(&Unity),
        "cargo" => Some(&Cargo),
        "pnpm" => Some(&Pnpm),
        _ => None,
    }
}

/// Every project in the repository, across all adapters.
pub fn detect(worktree: &Path, tracked: &[&str]) -> Vec<Project> {
    adapters()
        .iter()
        .flat_map(|adapter| adapter.detect(worktree, tracked))
        .collect()
}

pub fn includes(project: &Project) -> Vec<String> {
    adapter_for(project).map_or_else(Vec::new, |adapter| adapter.includes(project))
}

pub fn excludes(project: &Project) -> Vec<String> {
    adapter_for(project).map_or_else(Vec::new, |adapter| adapter.excludes(project))
}

pub fn expensive(project: &Project) -> Vec<String> {
    adapter_for(project).map_or_else(Vec::new, |adapter| adapter.expensive(project))
}

pub fn reflink_only(project: &Project) -> Vec<String> {
    adapter_for(project).map_or_else(Vec::new, |adapter| adapter.reflink_only(project))
}

pub fn relocated(project: &Project) -> Vec<String> {
    adapter_for(project).map_or_else(Vec::new, |adapter| adapter.relocated(project))
}

pub fn volatile_files(project: &Project) -> Vec<String> {
    adapter_for(project).map_or_else(Vec::new, |adapter| adapter.volatile_files(project))
}

pub fn running_editor(worktree: &Path, project: &Project) -> Option<Editor> {
    adapter_for(project).and_then(|adapter| adapter.running_editor(worktree, project))
}

/// Environment for tools running in `worktree`. A variable claimed by two
/// projects is dropped: silently picking one could send commands to the wrong
/// editor, which is exactly what these variables exist to prevent.
pub fn environment(worktree: &Path, projects: &[Project]) -> Vec<(String, String)> {
    let found: Vec<(String, String)> = projects
        .iter()
        .filter_map(|project| Some((adapter_for(project)?, project)))
        .flat_map(|(adapter, project)| adapter.environment(worktree, project))
        .collect();
    found
        .iter()
        .filter(|(key, _)| found.iter().filter(|(other, _)| other == key).count() == 1)
        .cloned()
        .collect()
}

/// The environment of the worktree containing `dir`.
pub fn worktree_environment(dir: &Path) -> Result<Vec<(String, String)>> {
    let repo = Repo::discover(dir)?;
    let listed = git::output(&repo.worktree, &["ls-files", "-z"])?;
    let tracked: Vec<&str> = listed.split('\0').filter(|path| !path.is_empty()).collect();
    Ok(environment(
        &repo.worktree,
        &detect(&repo.worktree, &tracked),
    ))
}

#[cfg(windows)]
pub(crate) fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(handle, &mut code);
        CloseHandle(handle);
        ok != 0 && code == STILL_ACTIVE as u32
    }
}

#[cfg(unix)]
pub(crate) fn process_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}
