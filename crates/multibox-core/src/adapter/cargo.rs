use super::{Adapter, Project};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;

const MANIFEST: &str = "Cargo.toml";
const LOCKFILE: &str = "Cargo.lock";

/// Cargo's default target directory.
const DEFAULT_TARGET: &str = "target";

/// Profiles live in `<target>/<profile>/`, or in `<target>/<triple>/<profile>/`
/// when cross-compiling.
const PROFILES: [&str; 2] = ["*", "*/*"];

/// What makes a compiled dependency reusable: its outputs, its build script's
/// results, and the fingerprints Cargo checks to decide it is fresh.
const CACHES: [&str; 3] = ["deps", "build", ".fingerprint"];

/// The metadata hash Cargo appends to every unit's directory name.
const UNIT_HASH: &str = "[0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]";

/// Cargo keeps compiled dependencies in `target/`. Those from the registry or
/// git are built from sources outside the project, so they stay valid wherever
/// the project lives and are carried.
///
/// The project's own packages are rebuilt. `env!("CARGO_MANIFEST_DIR")` and
/// `file!()` bake the source path into them, yet Cargo hashes their location
/// relative to the workspace and would call the copies fresh: tests in the new
/// worktree would then read and write files in the original one. Leaving their
/// fingerprints behind makes Cargo rebuild them, and everything depending on
/// them, as it would in a native worktree.
///
/// Besides the default `target/`, any directory up to two levels below the
/// workspace that Cargo has built into is recognised, wherever
/// `CARGO_TARGET_DIR` or a build script pointed it: Cargo writes
/// `.rustc_info.json` at the root of every target directory it uses.
pub struct Cargo;

/// The project's target directories, the default one first.
fn targets(project: &Project) -> Vec<&str> {
    if project.cache_dirs.is_empty() {
        vec![DEFAULT_TARGET]
    } else {
        project.cache_dirs.iter().map(String::as_str).collect()
    }
}

/// Whether Cargo has used `dir` as a target directory.
fn is_target_dir(dir: &Path) -> bool {
    dir.join(".rustc_info.json").is_file()
        || fs::read_to_string(dir.join("CACHEDIR.TAG"))
            .is_ok_and(|tag| tag.contains("created by cargo"))
}

/// Target directories other than the default one, relative to `root`. A
/// directory holding a `Cargo.toml` is source, possibly another workspace with
/// its own packages, and is not searched.
fn other_target_dirs(root: &Path) -> Vec<String> {
    let subdirs = |dir: &Path| -> Vec<(String, std::path::PathBuf)> {
        let Ok(entries) = fs::read_dir(dir) else {
            return Vec::new();
        };
        entries
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .filter_map(|entry| Some((entry.file_name().into_string().ok()?, entry.path())))
            .filter(|(name, path)| {
                ![".git", "node_modules"].contains(&name.as_str()) && !path.join(MANIFEST).exists()
            })
            .collect()
    };
    let mut found = Vec::new();
    for (name, path) in subdirs(root) {
        if name == DEFAULT_TARGET {
            continue;
        }
        if is_target_dir(&path) {
            found.push(name);
            continue;
        }
        for (inner, inner_path) in subdirs(&path) {
            if is_target_dir(&inner_path) {
                found.push(format!("{name}/{inner}"));
            }
        }
    }
    found.sort();
    found
}

impl Adapter for Cargo {
    fn detect(&self, worktree: &Path, tracked: &[&str]) -> Vec<Project> {
        tracked
            .iter()
            .filter_map(|path| path.strip_suffix(MANIFEST))
            .filter(|prefix| prefix.is_empty() || prefix.ends_with('/'))
            .filter_map(|prefix| {
                // A workspace root has the lockfile; its members do not.
                let lock = fs::read_to_string(worktree.join(prefix).join(LOCKFILE)).ok()?;
                let root = worktree.join(prefix);
                let mut targets = vec![DEFAULT_TARGET.to_owned()];
                targets.extend(other_target_dirs(&root));
                Some(Project {
                    engine: "cargo",
                    root: prefix.trim_end_matches('/').to_owned(),
                    version: None,
                    local_packages: local_packages(&lock),
                    verified_files: Vec::new(),
                    left_behind: own_units(worktree, &root, &targets),
                    cache_dirs: targets,
                })
            })
            .collect()
    }

    fn includes(&self, project: &Project) -> Vec<String> {
        let mut patterns = Vec::new();
        for target in targets(project) {
            for profile in PROFILES {
                for cache in CACHES {
                    patterns.push(format!(
                        "/{}/",
                        project.path(&format!("{target}/{profile}/{cache}"))
                    ));
                }
            }
        }
        patterns
    }

    fn excludes(&self, project: &Project) -> Vec<String> {
        let mut patterns = Vec::new();
        for target in targets(project) {
            // Only local packages compile incrementally, and the state is
            // tied to their sources. Listed in case all of target/ is asked for.
            patterns.push(format!("{target}/**/incremental/"));
            // Scratch space of the source's integration tests.
            patterns.push(format!("{target}/tmp/"));
            for name in &project.local_packages {
                for cache in ["build", ".fingerprint"] {
                    patterns.push(format!("{target}/**/{cache}/{name}-{UNIT_HASH}/"));
                }
                // Their outputs are compiled again anyway, under the same names.
                let krate = name.replace('-', "_");
                patterns.push(format!("{target}/**/deps/{krate}-{UNIT_HASH}.*"));
                patterns.push(format!("{target}/**/deps/lib{krate}-{UNIT_HASH}.*"));
            }
        }
        patterns.extend(project.left_behind.iter().cloned());
        patterns
            .iter()
            .map(|path| format!("/{}", project.path(path)))
            .collect()
    }

    /// A copy of `target/` can be as large as the project several times over.
    /// `.worktreeinclude` can still ask for it with `target/`.
    fn expensive(&self, project: &Project) -> Vec<String> {
        self.includes(project)
    }
}

/// Directories of a target directory that hold compiled units: `debug`, or
/// `x86_64-pc-windows-msvc/debug` when cross-compiling.
fn profile_dirs(target: &Path) -> Vec<String> {
    let children = |dir: &Path| -> Vec<String> {
        fs::read_dir(dir)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect()
    };
    let mut found = Vec::new();
    for name in children(target) {
        if target.join(&name).join("deps").is_dir() {
            found.push(name.clone());
        }
        for inner in children(&target.join(&name)) {
            if target.join(&name).join(&inner).join("deps").is_dir() {
                found.push(format!("{name}/{inner}"));
            }
        }
    }
    found.sort();
    found
}

/// The paths a Makefile rule depends on, as rustc writes them: separated by
/// spaces, with a space inside a path escaped by a backslash.
fn rule_sources(line: &str) -> Vec<String> {
    let Some((_, sources)) = line.trim_end().split_once(": ") else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    let mut current = String::new();
    let mut chars = sources.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&' ') => {
                current.push(' ');
                chars.next();
            }
            ' ' | '\t' => {
                if !current.is_empty() {
                    paths.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(c),
        }
    }
    if !current.is_empty() {
        paths.push(current);
    }
    paths
}

/// A path in a form that compares reliably: on Windows with forward slashes,
/// without a verbatim prefix and in lower case.
fn comparable(path: &str) -> String {
    if cfg!(windows) {
        let path = path.strip_prefix(r"\\?\").unwrap_or(path);
        path.replace('\\', "/").trim_end_matches('/').to_lowercase()
    } else {
        path.trim_end_matches('/').to_owned()
    }
}

/// The metadata hash at the end of a unit's name, as in `serde-0123456789abcdef`.
fn unit_hash(unit: &str) -> Option<&str> {
    let (_, hash) = unit.rsplit_once('-')?;
    (hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit())).then_some(hash)
}

/// Units compiled from the project's own sources, found in the dep-info file
/// rustc writes beside each unit's outputs in `deps/`. Cargo hands rustc the
/// sources of workspace members relative to the workspace, and those of
/// everything else as absolute paths, so a relative source, or an absolute one
/// inside the repository other than code generated into a target directory,
/// makes a unit the project's own. This also catches tests, binaries and
/// libraries named differently from their package.
///
/// Returns patterns for the unit's outputs and fingerprint, relative to `root`.
fn own_units(worktree: &Path, root: &Path, targets: &[String]) -> Vec<String> {
    let repository = comparable(&worktree.to_string_lossy());
    let target_roots: Vec<String> = targets
        .iter()
        .map(|target| comparable(&root.join(target).to_string_lossy()))
        .collect();
    let inside = |path: &str, dir: &str| {
        path.strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
    };
    let is_own = |source: &str| {
        if !Path::new(source).is_absolute() {
            return true;
        }
        let source = comparable(source);
        inside(&source, &repository) && !target_roots.iter().any(|dir| inside(&source, dir))
    };

    let mut patterns = Vec::new();
    for target in targets {
        for profile in profile_dirs(&root.join(target)) {
            let deps = root.join(target).join(&profile).join("deps");
            for entry in fs::read_dir(&deps)
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
            {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some(unit) = name.strip_suffix(".d") else {
                    continue;
                };
                let Some(hash) = unit_hash(unit) else {
                    continue;
                };
                // Unit names end up inside glob patterns.
                if !unit
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                {
                    continue;
                }
                let Ok(file) = fs::File::open(entry.path()) else {
                    continue;
                };
                let mut rule = String::new();
                if BufReader::new(file).read_line(&mut rule).is_err() {
                    continue;
                }
                if rule_sources(&rule).iter().any(|source| is_own(source)) {
                    let dir = format!("{target}/{profile}");
                    patterns.push(format!("{dir}/deps/{unit}.*"));
                    patterns.push(format!("{dir}/deps/lib{unit}.*"));
                    patterns.push(format!("{dir}/.fingerprint/*-{hash}/"));
                }
            }
        }
    }
    patterns.sort();
    patterns.dedup();
    patterns
}

/// Packages the lockfile lists without a `source` are built from paths: the
/// workspace members and any path dependency, inside the project or not.
fn local_packages(lock: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut package: Option<(Option<&str>, bool)> = None;
    for line in lock.lines().map(str::trim).chain(["[end]"]) {
        if line.starts_with('[') {
            if let Some((Some(name), false)) = package {
                names.push(name.to_owned());
            }
            package = (line == "[[package]]").then_some((None, false));
        } else if let Some((name, sourced)) = package.as_mut() {
            if let Some(value) = line.strip_prefix("name = ") {
                *name = Some(value.trim_matches('"'));
            } else if line.starts_with("source = ") {
                *sourced = true;
            }
        }
    }
    // Package names are plain, but they end up inside glob patterns.
    names.retain(|name| {
        name.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    });
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::Rules;
    use multibox_clone::{Decision, TreeFilter};

    const LOCK: &str = r#"# This file is automatically @generated by Cargo.
version = 4

[[package]]
name = "app"
version = "0.1.0"
dependencies = [
 "serde",
 "shared",
]

[[package]]
name = "serde"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "00"

[[package]]
name = "shared"
version = "0.2.0"

[[package]]
name = "forked"
version = "0.3.0"
source = "git+https://example.com/forked#abc"
"#;

    fn project(root: &str) -> Project {
        Project {
            engine: "cargo",
            root: root.to_owned(),
            version: None,
            local_packages: local_packages(LOCK),
            verified_files: Vec::new(),
            cache_dirs: Vec::new(),
            left_behind: Vec::new(),
        }
    }

    fn rules(project: &Project) -> Rules {
        let mut lines = Cargo.includes(project);
        lines.extend(
            Cargo
                .excludes(project)
                .iter()
                .map(|line| format!("!{line}")),
        );
        Rules::new(&lines).unwrap()
    }

    #[test]
    fn local_packages_are_those_without_a_source() {
        assert_eq!(local_packages(LOCK), ["app", "shared"]);
    }

    #[test]
    fn reads_the_sources_of_a_dep_info_rule() {
        assert_eq!(
            rule_sources(
                "E:\\p\\.tmp/target\\debug\\deps\\core-0123456789abcdef.d: core\\src\\lib.rs core\\src\\a.rs\r\n"
            ),
            ["core\\src\\lib.rs", "core\\src\\a.rs"]
        );
        assert_eq!(
            rule_sources("/p/target/debug/deps/x.d: /home/me/My\\ Crates/x/src/lib.rs\n"),
            ["/home/me/My Crates/x/src/lib.rs"]
        );
        assert!(rule_sources("no rule here\n").is_empty());
    }

    #[test]
    fn unit_names_end_in_a_metadata_hash() {
        assert_eq!(
            unit_hash("serde_json-0123456789abcdef"),
            Some("0123456789abcdef")
        );
        assert_eq!(unit_hash("serde_json"), None);
        assert_eq!(unit_hash("build-script-build"), None);
    }

    #[test]
    fn finds_workspace_roots_by_their_lockfile() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join(LOCKFILE), LOCK).unwrap();
        fs::create_dir_all(temp.path().join("fuzz")).unwrap();
        fs::write(temp.path().join("fuzz").join(LOCKFILE), LOCK).unwrap();
        let tracked = [
            "Cargo.toml",
            "crates/app/Cargo.toml",
            "fuzz/Cargo.toml",
            "notCargo.toml",
        ];
        let roots: Vec<String> = Cargo
            .detect(temp.path(), &tracked)
            .into_iter()
            .map(|project| project.root)
            .collect();
        assert_eq!(roots, ["", "fuzz"]);
    }

    #[test]
    fn carries_dependencies_and_rebuilds_local_packages() {
        let rules = rules(&project(""));
        let carried = |rel: &str| rules.includes(rel, false);
        assert!(carried("target/debug/deps/libserde-0123456789abcdef.rlib"));
        assert!(carried(
            "target/debug/.fingerprint/serde-0123456789abcdef/lib-serde"
        ));
        assert!(carried("target/debug/build/forked-0123456789abcdef/output"));
        assert!(carried(
            "target/x86_64-pc-windows-msvc/release/.fingerprint/serde-0123456789abcdef/lib-serde"
        ));

        assert!(!carried(
            "target/debug/.fingerprint/app-0123456789abcdef/lib-app"
        ));
        assert!(!carried(
            "target/debug/build/shared-0123456789abcdef/out/gen.rs"
        ));
        assert!(!carried(
            "target/aarch64-apple-darwin/debug/.fingerprint/shared-0123456789abcdef/lib-shared"
        ));
        assert!(!carried(
            "target/debug/incremental/app-1x2y/s-abc/query-cache.bin"
        ));
        assert!(!carried("target/debug/app.exe"));
        assert!(!carried("target/debug/examples/demo.exe"));
        assert!(!carried("target/tmp/scratch"));
        assert!(!carried("target/doc/app/index.html"));
    }

    #[test]
    fn a_dependency_named_like_a_local_package_is_still_carried() {
        // "shared-macros" starts with the local "shared-", but is not it.
        let rules = rules(&project(""));
        assert!(rules.includes(
            "target/debug/.fingerprint/shared-macros-0123456789abcdef/lib-shared_macros",
            false
        ));
    }

    #[test]
    fn finds_target_directories_wherever_cargo_put_them() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::write(root.join(LOCKFILE), LOCK).unwrap();
        for dir in [".tmp/target", "build-out", "fuzz/target", "docs/notes"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        fs::write(root.join(".tmp/target/.rustc_info.json"), "{}").unwrap();
        fs::write(
            root.join("build-out/CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n# This file is a cache directory tag created by cargo.\n",
        )
        .unwrap();
        // Another workspace and its own target directory are not this one's.
        fs::write(root.join("fuzz/Cargo.toml"), "[package]\n").unwrap();
        fs::write(root.join("fuzz/target/.rustc_info.json"), "{}").unwrap();
        let found = Cargo.detect(root, &["Cargo.toml"]);
        assert_eq!(found[0].cache_dirs, ["target", ".tmp/target", "build-out"]);
    }

    #[test]
    fn a_custom_target_directory_gets_the_same_rules() {
        let mut project = project("");
        project.cache_dirs = vec!["target".to_owned(), ".tmp/target".to_owned()];
        let rules = rules(&project);
        assert!(rules.includes(
            ".tmp/target/debug/deps/libserde-0123456789abcdef.rlib",
            false
        ));
        assert!(!rules.includes(
            ".tmp/target/debug/.fingerprint/app-0123456789abcdef/lib-app",
            false
        ));
        assert!(!rules.includes(
            ".tmp/target/debug/incremental/app-1x2y/s-abc/dep-graph.bin",
            false
        ));
        assert!(!rules.includes(".tmp/target/debug/app.exe", false));
        assert!(!rules.includes(".tmp/scratch.log", false));
        assert!(rules.includes("target/debug/deps/libserde-0123456789abcdef.rlib", false));
    }

    #[test]
    fn patterns_are_scoped_to_the_project() {
        let rules = rules(&project("server"));
        assert!(rules.includes(
            "server/target/debug/deps/libserde-0123456789abcdef.rlib",
            false
        ));
        assert!(!rules.includes("target/debug/deps/libserde-0123456789abcdef.rlib", false));
        assert!(!rules.includes(
            "server/target/debug/.fingerprint/app-0123456789abcdef/lib-app",
            false
        ));
        assert_eq!(
            rules.check("server/target/debug/.fingerprint", true),
            Decision::Include
        );
    }
}
