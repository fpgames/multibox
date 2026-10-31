use crate::adapter;
use crate::git::{self, Repo};
use crate::index::Index;
use multibox_clone::{Capability, Volume, probe, volumes};
use std::path::Path;
use std::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Ok,
    Info,
    Warn,
}

#[derive(Clone, Debug)]
pub struct Check {
    pub name: &'static str,
    pub level: Level,
    pub detail: String,
}

/// A read-only report of what Multibox can do from a given directory.
#[derive(Debug, Default)]
pub struct Diagnosis {
    pub checks: Vec<Check>,
    pub volumes: Vec<Volume>,
    pub capability: Option<Capability>,
    pub dev_drive_supported: Option<bool>,
}

impl Diagnosis {
    fn push(&mut self, name: &'static str, level: Level, detail: impl Into<String>) {
        self.checks.push(Check {
            name,
            level,
            detail: detail.into(),
        });
    }
}

/// Dev Drive shipped in Windows 11 22H2 build 22621.2338.
pub fn supports_dev_drive(build: u32, revision: u32) -> bool {
    build > 22621 || (build == 22621 && revision >= 2338)
}

pub fn windows_build() -> Option<(u32, u32)> {
    multibox_clone::os_build()
}

fn tool_version(tool: &str) -> Option<String> {
    let out = Command::new(tool).arg("--version").output().ok()?;
    out.status.success().then(|| {
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned()
    })
}

fn check_platform(report: &mut Diagnosis) {
    if let Some((build, revision)) = windows_build() {
        let supported = supports_dev_drive(build, revision);
        report.dev_drive_supported = Some(supported);
        let detail = format!("build {build}.{revision}");
        if supported {
            report.push(
                "Windows",
                Level::Ok,
                format!("{detail}, Dev Drive available"),
            );
        } else {
            report.push(
                "Windows",
                Level::Warn,
                format!("{detail}, no Dev Drive; worktrees use copy mode"),
            );
        }
    }
    match tool_version("git") {
        Some(version) => report.push("git", Level::Ok, version),
        None => report.push("git", Level::Warn, "not found on PATH"),
    }
    match tool_version("git-lfs") {
        Some(version) => report.push("git-lfs", Level::Ok, version),
        None => report.push("git-lfs", Level::Info, "not found on PATH"),
    }
}

fn check_repository(report: &mut Diagnosis, cwd: &Path) {
    let Ok(repo) = Repo::discover(cwd) else {
        report.push("repository", Level::Info, "not inside a git repository");
        return;
    };
    report.push("repository", Level::Ok, repo.worktree.display().to_string());

    match probe(&repo.worktree, &repo.worktree) {
        Ok(capability) => {
            let detail = format!(
                "{}, {}",
                capability.filesystem,
                if capability.reflink {
                    "reflinks available: new worktrees share data and take almost no space"
                } else {
                    "no reflinks: new worktrees are full copies"
                }
            );
            let level = if capability.reflink {
                Level::Ok
            } else {
                Level::Info
            };
            report.push("volume", level, detail);
            report.capability = Some(capability);
        }
        Err(err) => report.push("volume", Level::Warn, err.to_string()),
    }

    let Ok(index) = Index::read(&repo.index_path(), repo.hash_len) else {
        report.push(
            "index",
            Level::Warn,
            "cannot be reused; git will check out from scratch",
        );
        return;
    };
    report.push(
        "index",
        Level::Ok,
        format!(
            "{} entries, version {}",
            index.entries.len(),
            index.version()
        ),
    );
    if let Some(longest) = index.entries.iter().map(|entry| entry.path.len()).max() {
        let total = repo.worktree.as_os_str().len() + 1 + longest;
        let long_paths = git::output(cwd, &["config", "--type=bool", "core.longpaths"])
            .is_ok_and(|value| value.trim() == "true");
        if cfg!(windows) && total >= 260 && !long_paths {
            report.push(
                "paths",
                Level::Warn,
                format!("longest path is {total} characters; set core.longpaths=true"),
            );
        }
    }

    let tracked: Vec<&str> = index
        .entries
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    for project in adapter::detect(&repo.worktree, &tracked) {
        let name = format!(
            "{} {} at {}",
            project.engine,
            project.version.as_deref().unwrap_or("(unknown version)"),
            if project.root.is_empty() {
                "."
            } else {
                &project.root
            }
        );
        report.push("project", Level::Ok, name);
        if let Some(editor) = adapter::running_editor(&repo.worktree, &project) {
            let pid = editor
                .process_id
                .map_or_else(String::new, |pid| format!(" (pid {pid})"));
            report.push(
                "editor",
                Level::Info,
                format!("running{pid}; its cache may change while being cloned"),
            );
        }
    }
}

/// Inspects the machine and the repository at `cwd`. Nothing is modified.
pub fn diagnose(cwd: &Path) -> Diagnosis {
    let mut report = Diagnosis {
        volumes: volumes(),
        ..Diagnosis::default()
    };
    check_platform(&mut report);
    check_repository(&mut report, cwd);
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_drive_cutoff() {
        assert!(supports_dev_drive(26200, 1));
        assert!(supports_dev_drive(22631, 0));
        assert!(supports_dev_drive(22621, 2338));
        assert!(!supports_dev_drive(22621, 2337));
        assert!(!supports_dev_drive(19045, 5000));
    }
}
