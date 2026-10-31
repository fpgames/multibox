use super::{Adapter, Editor, Project, process_alive};
use std::fs;
use std::path::Path;

const VERSION_FILE: &str = "ProjectSettings/ProjectVersion.txt";

/// Unity keeps its import results in `Library/`, keyed by the exact contents of
/// `Assets/`, `Packages/` and `ProjectSettings/`. A carried Library therefore
/// only stays valid if every file it was built from comes along, including
/// ignored generated assets. That is why those three trees are carried whole.
pub struct Unity;

impl Adapter for Unity {
    fn detect(&self, worktree: &Path, tracked: &[&str]) -> Vec<Project> {
        tracked
            .iter()
            .filter_map(|path| path.strip_suffix(VERSION_FILE))
            .filter(|prefix| prefix.is_empty() || prefix.ends_with('/'))
            .map(|prefix| {
                let root = prefix.trim_end_matches('/').to_owned();
                let version = fs::read_to_string(worktree.join(prefix).join(VERSION_FILE))
                    .ok()
                    .and_then(|text| parse_version(&text));
                Project {
                    engine: "unity",
                    root,
                    version,
                }
            })
            .collect()
    }

    fn includes(&self, project: &Project) -> Vec<String> {
        [
            "Library/",
            "UserSettings/",
            "Assets/",
            "Packages/",
            "ProjectSettings/",
            "*.csproj",
            "*.sln",
            "*.slnx",
        ]
        .iter()
        .map(|path| format!("/{}", project.path(path)))
        .collect()
    }

    fn excludes(&self, project: &Project) -> Vec<String> {
        [
            // Records the PID of the editor that owns the source project.
            "Library/EditorInstance.json",
            // LMDB lock files hold reader slots of live processes; Unity recreates them.
            "Library/*-lock",
            "Library/*.lock",
            "Library/Temp/",
            "Library/TempArtifacts/",
            // Multiplayer Play Mode's virtual players: rebuilt on demand, and they
            // hold junctions into the source project's Assets and ProjectSettings.
            "Library/VP/",
            "Assets/_Recovery/",
            "Assets/_Recovery.meta",
            "Temp/",
            "Logs/",
            "obj/",
        ]
        .iter()
        .map(|path| format!("/{}", project.path(path)))
        .collect()
    }

    fn volatile_files(&self, project: &Project) -> Vec<String> {
        ["Library/ArtifactDB", "Library/SourceAssetDB"]
            .iter()
            .map(|path| project.path(path))
            .collect()
    }

    fn running_editor(&self, worktree: &Path, project: &Project) -> Option<Editor> {
        let root = worktree.join(&project.root);
        let instance = fs::read_to_string(root.join("Library/EditorInstance.json")).ok();
        if let Some(pid) = instance.as_deref().and_then(parse_process_id) {
            if process_alive(pid) {
                return Some(Editor {
                    process_id: Some(pid),
                });
            }
            return None;
        }
        // Without a readable instance file the lockfile is the only evidence.
        root.join("Temp/UnityLockfile")
            .exists()
            .then_some(Editor { process_id: None })
    }
}

fn parse_version(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.strip_prefix("m_EditorVersion:"))
        .map(|version| version.trim().to_owned())
}

fn parse_process_id(json: &str) -> Option<u32> {
    let after_key = &json[json.find("\"process_id\"")? + "\"process_id\"".len()..];
    let value = after_key.split_once(':')?.1.trim_start();
    let digits: String = value.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_projects_in_subdirectories_only_on_path_boundaries() {
        let tracked = [
            "client/ProjectSettings/ProjectVersion.txt",
            "ProjectSettings/ProjectVersion.txt",
            "notclientProjectSettings/ProjectVersion.txt.bak",
        ];
        let found = Unity.detect(Path::new("."), &tracked);
        let roots: Vec<_> = found.iter().map(|project| project.root.as_str()).collect();
        assert_eq!(roots, ["client", ""]);
    }

    #[test]
    fn reads_editor_version() {
        let text = "m_EditorVersion: 6000.4.10f1\nm_EditorVersionWithRevision: x\n";
        assert_eq!(parse_version(text).as_deref(), Some("6000.4.10f1"));
    }

    #[test]
    fn reads_process_id() {
        let text = "{\n  \"process_id\" : 19180,\n  \"version\" : \"6000\"\n}";
        assert_eq!(parse_process_id(text), Some(19180));
    }

    #[test]
    fn scopes_patterns_to_the_project() {
        let project = Project {
            engine: "unity",
            root: "client".to_owned(),
            version: None,
        };
        assert!(Unity.includes(&project).contains(&"/client/Library/".to_owned()));
        let root = Project {
            engine: "unity",
            root: String::new(),
            version: None,
        };
        assert!(Unity.includes(&root).contains(&"/Library/".to_owned()));
    }
}
