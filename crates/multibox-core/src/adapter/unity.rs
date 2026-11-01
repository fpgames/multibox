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
                    local_packages: Vec::new(),
                    verified_files: Vec::new(),
                    cache_dirs: Vec::new(),
                    left_behind: Vec::new(),
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
            // PIDs of helper processes. A new editor kills the process named in
            // ilpp.pid as a leftover, which would take down the source editor's
            // IL post-processing runner.
            "Library/*.pid",
            // The discovery file of the source editor's control server: its PID,
            // port and an access token. Carried over, tools running in the new
            // worktree would drive the source editor instead.
            "Library/Pipeline/",
            "Library/Search/*-lock",
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

    fn expensive(&self, project: &Project, _sharing: bool) -> Vec<String> {
        [
            // The build pipeline's cache: only player and bundle builds read it.
            "Library/BuildCache/",
            // The search index, 2.8 GiB in a project of 100,000 assets. The
            // editor rebuilds it in the background; nothing waits for it.
            "Library/Search/",
        ]
        .iter()
        .map(|path| format!("/{}", project.path(path)))
        .collect()
    }

    /// Import results are named after their content and moved into place from
    /// `Library/TempArtifacts/`; a new result is a new file. In a project of
    /// 93,000 artifacts written over its whole life, none was modified more
    /// than 0.2 seconds after it was created.
    fn write_once(&self, project: &Project) -> Vec<String> {
        vec![format!("/{}", project.path("Library/Artifacts/"))]
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
        lockfile_held(&root.join("Temp/UnityLockfile")).then_some(Editor { process_id: None })
    }

    /// A bare `unity command` run in a project with no editor of its own falls
    /// back to whichever editor is open, so a worktree without a running editor
    /// would drive the main one. This variable makes the CLI fail instead.
    fn environment(&self, worktree: &Path, project: &Project) -> Vec<(String, String)> {
        let path = if project.root.is_empty() {
            worktree.to_path_buf()
        } else {
            worktree.join(&project.root)
        };
        let path = path.display().to_string();
        let path = if cfg!(windows) {
            path.replace('/', "\\")
        } else {
            path
        };
        vec![("UNITY_PROJECT_PATH".to_owned(), path)]
    }

    /// `.meta` files are small and alike, so git's default 50% similarity
    /// pairs unrelated ones as renames. Replaying a real project's history,
    /// that produced a fifth of all conflicts and once replaced a GUID both
    /// branches agreed on; 90% removed them without changing any clean merge.
    fn merge_options(&self, _project: &Project) -> Vec<String> {
        vec!["-X".to_owned(), "find-renames=90%".to_owned()]
    }
}

/// An editor that crashed or failed to start leaves its lockfile behind, so only a
/// lock some process still holds counts as an open editor.
#[cfg(windows)]
fn lockfile_held(path: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    let opened = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(path);
    matches!(opened, Err(err) if err.raw_os_error() == Some(ERROR_SHARING_VIOLATION))
}

/// An editor that crashed or failed to start leaves its lockfile behind, so only a
/// lock some process still holds counts as an open editor.
#[cfg(unix)]
fn lockfile_held(path: &Path) -> bool {
    use std::os::fd::AsRawFd;
    let Ok(file) = fs::File::open(path) else {
        return false;
    };
    let free = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    !free
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
    fn a_leftover_lockfile_is_not_an_open_editor() {
        let temp = tempfile::tempdir().unwrap();
        let lockfile = temp.path().join("UnityLockfile");
        fs::write(&lockfile, "").unwrap();
        assert!(!lockfile_held(&lockfile));
        assert!(!lockfile_held(&temp.path().join("missing")));
    }

    #[cfg(windows)]
    #[test]
    fn a_lockfile_held_open_is_an_open_editor() {
        use std::os::windows::fs::OpenOptionsExt;
        let temp = tempfile::tempdir().unwrap();
        let lockfile = temp.path().join("UnityLockfile");
        fs::write(&lockfile, "").unwrap();
        let _held = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&lockfile)
            .unwrap();
        assert!(lockfile_held(&lockfile));
    }

    #[test]
    fn scopes_patterns_to_the_project() {
        let project = Project {
            engine: "unity",
            root: "client".to_owned(),
            version: None,
            local_packages: Vec::new(),
            verified_files: Vec::new(),
            cache_dirs: Vec::new(),
            left_behind: Vec::new(),
        };
        assert!(
            Unity
                .includes(&project)
                .contains(&"/client/Library/".to_owned())
        );
        let root = Project {
            engine: "unity",
            root: String::new(),
            version: None,
            local_packages: Vec::new(),
            verified_files: Vec::new(),
            cache_dirs: Vec::new(),
            left_behind: Vec::new(),
        };
        assert!(Unity.includes(&root).contains(&"/Library/".to_owned()));
    }
}
