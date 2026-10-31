use super::{Adapter, Project};
use std::path::Path;

const LOCKFILE: &str = "pnpm-lock.yaml";

/// pnpm links every package from a store shared by all projects on the volume,
/// so a fresh `pnpm install` costs little time and almost no space. Copying
/// `node_modules/` was slower than that; cloning it was no faster on a Dev
/// Drive and only slightly faster on APFS. It is therefore never carried in
/// copy mode, and with reflinks only when `.worktreeinclude` asks for it.
///
/// When it is carried, two state files still name the source: pnpm compares
/// the virtual store path in `.modules.yaml` with its own and offers to delete
/// `node_modules` when they differ. Both are rewritten for the new worktree.
/// Junctions and symlinks into the source are repointed like any other link.
pub struct Pnpm;

impl Adapter for Pnpm {
    fn detect(&self, _worktree: &Path, tracked: &[&str]) -> Vec<Project> {
        tracked
            .iter()
            .filter_map(|path| path.strip_suffix(LOCKFILE))
            .filter(|prefix| prefix.is_empty() || prefix.ends_with('/'))
            .map(|prefix| Project {
                engine: "pnpm",
                root: prefix.trim_end_matches('/').to_owned(),
                version: None,
                local_packages: Vec::new(),
            })
            .collect()
    }

    fn includes(&self, _project: &Project) -> Vec<String> {
        Vec::new()
    }

    fn excludes(&self, _project: &Project) -> Vec<String> {
        Vec::new()
    }

    fn reflink_only(&self, project: &Project) -> Vec<String> {
        vec![format!("/{}", project.path("**/node_modules/"))]
    }

    fn relocated(&self, project: &Project) -> Vec<String> {
        [
            "**/node_modules/.modules.yaml",
            "**/node_modules/.pnpm-workspace-state*.json",
        ]
        .iter()
        .map(|path| format!("/{}", project.path(path)))
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::Rules;

    #[test]
    fn finds_projects_by_their_lockfile() {
        let tracked = [
            "pnpm-lock.yaml",
            "web/pnpm-lock.yaml",
            "web/apnpm-lock.yaml",
        ];
        let roots: Vec<String> = Pnpm
            .detect(Path::new("."), &tracked)
            .into_iter()
            .map(|project| project.root)
            .collect();
        assert_eq!(roots, ["", "web"]);
    }

    #[test]
    fn relocates_the_state_files_of_every_node_modules() {
        let project = Pnpm
            .detect(Path::new("."), &["web/pnpm-lock.yaml"])
            .remove(0);
        let rules = Rules::new(&Pnpm.relocated(&project)).unwrap();
        assert!(rules.includes("web/node_modules/.modules.yaml", false));
        assert!(rules.includes("web/apps/site/node_modules/.modules.yaml", false));
        assert!(rules.includes("web/node_modules/.pnpm-workspace-state-v1.json", false));
        assert!(!rules.includes("web/node_modules/left-pad/package.json", false));
        assert!(!rules.includes("node_modules/.modules.yaml", false));
    }
}
