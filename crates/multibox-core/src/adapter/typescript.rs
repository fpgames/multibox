use super::{Adapter, Project};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::Path;

const BUILD_INFO: &str = ".tsbuildinfo";

/// Incremental TypeScript keeps what it learned in a `.tsbuildinfo` file. A
/// check-only run (`tsc --noEmit --incremental`) on a large project drops from
/// minutes to seconds once that file exists, and a new worktree can reuse the
/// source's: files are recorded relative to it, and tsc compares their
/// contents, so anything that differs in the worktree is checked again.
///
/// Only build info that emitted nothing is carried. tsc trusts its record of
/// emitted files, so a clone without those outputs would silently lack them.
/// Every file is read during detection; one that cannot be verified stays
/// behind, as it would with a native worktree.
pub struct TypeScript;

impl Adapter for TypeScript {
    fn detect(&self, worktree: &Path, tracked: &[&str]) -> Vec<Project> {
        let mut found: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for config in tracked.iter().filter(|path| is_tsconfig(path)) {
            let dir = config.rsplit_once('/').map_or("", |(dir, _)| dir);
            let text = fs::read_to_string(worktree.join(config)).unwrap_or_default();
            let mut candidates = build_info_in(worktree, dir);
            if let Some(file) = string_option(&text, "tsBuildInfoFile").and_then(|v| join(dir, &v))
            {
                candidates.push(file);
            }
            if let Some(out) = string_option(&text, "outDir").and_then(|v| join(dir, &v)) {
                candidates.extend(build_info_in(worktree, &out));
            }
            for file in candidates {
                let carried = fs::read_to_string(worktree.join(&file))
                    .is_ok_and(|info| carriable(&info, worktree));
                if carried {
                    found.entry(dir.to_owned()).or_default().insert(file);
                }
            }
        }
        found
            .into_iter()
            .map(|(root, files)| Project {
                engine: "typescript",
                root,
                version: None,
                local_packages: Vec::new(),
                verified_files: files.into_iter().collect(),
            })
            .collect()
    }

    fn includes(&self, project: &Project) -> Vec<String> {
        project
            .verified_files
            .iter()
            .map(|file| format!("/{file}"))
            .collect()
    }

    fn excludes(&self, _project: &Project) -> Vec<String> {
        Vec::new()
    }
}

fn is_tsconfig(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.starts_with("tsconfig") && name.ends_with(".json")
}

/// Build info files directly inside `dir`, relative to the worktree root.
fn build_info_in(worktree: &Path, dir: &str) -> Vec<String> {
    let Ok(entries) = fs::read_dir(worktree.join(dir)) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().into_string().ok()?;
            (name.ends_with(BUILD_INFO) && entry.file_type().ok()?.is_file())
                .then(|| join(dir, &name))
                .flatten()
        })
        .collect()
}

/// The string value of `key` in a tsconfig. JSON with comments is not worth
/// parsing in full: a stray match only adds a candidate that is verified anyway.
fn string_option(text: &str, key: &str) -> Option<String> {
    let after = &text[text.find(&format!("\"{key}\""))? + key.len() + 2..];
    let value = after
        .trim_start()
        .strip_prefix(':')?
        .trim_start()
        .strip_prefix('"')?;
    Some(value[..value.find('"')?].to_owned())
}

/// Resolves `relative` against `dir`, both `/`-separated and inside the
/// worktree. Absolute paths and paths leaving the worktree are refused.
fn join(dir: &str, relative: &str) -> Option<String> {
    let relative = relative.replace('\\', "/");
    if is_absolute(&relative) {
        return None;
    }
    let mut parts: Vec<&str> = dir.split('/').filter(|part| !part.is_empty()).collect();
    for part in relative.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn is_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    path.starts_with('/')
        || path.starts_with('\\')
        || (bytes.len() > 1 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
}

/// Whether build info text can be carried: every path in it is relative, it
/// does not name the source worktree, and nothing was emitted from it, so
/// every emittable root is still pending emit.
fn carriable(text: &str, worktree: &Path) -> bool {
    if mentions(text, worktree) {
        return false;
    }
    let Ok(info) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    if !info.get("version").is_some_and(Value::is_string) {
        return false;
    }
    let Some(names) = info.get("fileNames").and_then(Value::as_array) else {
        return false;
    };
    let Some(names) = names
        .iter()
        .map(Value::as_str)
        .collect::<Option<Vec<&str>>>()
    else {
        return false;
    };
    if names.iter().any(|name| is_absolute(name)) {
        return false;
    }
    let Some(roots) = info
        .get("root")
        .and_then(Value::as_array)
        .and_then(|r| ids(r, true))
    else {
        return false;
    };
    let pending = match info.get("affectedFilesPendingEmit") {
        None => HashSet::new(),
        Some(list) => match list.as_array().and_then(|l| ids(l, false)) {
            Some(ids) => ids.into_iter().collect(),
            None => return false,
        },
    };
    roots.into_iter().all(|id| {
        let Some(name) = id.checked_sub(1).and_then(|at| names.get(at as usize)) else {
            return false;
        };
        let declaration = [".d.ts", ".d.mts", ".d.cts"]
            .iter()
            .any(|suffix| name.ends_with(suffix));
        declaration || pending.contains(&id)
    })
}

/// File ids in a build info list: numbers, `[id, kind]` pairs, or, for roots,
/// `[first, last]` ranges. Anything else means a format this code does not know.
fn ids(list: &[Value], ranges: bool) -> Option<Vec<u64>> {
    let mut out = Vec::new();
    for item in list {
        match item {
            Value::Number(id) => out.push(id.as_u64()?),
            Value::Array(pair) if ranges && pair.len() == 2 => {
                let (first, last) = (pair[0].as_u64()?, pair[1].as_u64()?);
                if last < first || last - first > 1_000_000 {
                    return None;
                }
                out.extend(first..=last);
            }
            Value::Array(pair) if !ranges && !pair.is_empty() => out.push(pair[0].as_u64()?),
            _ => return None,
        }
    }
    Some(out)
}

/// Whether `text` names the worktree, in any of the spellings JSON may use.
fn mentions(text: &str, worktree: &Path) -> bool {
    let root = worktree.display().to_string();
    let forward = root.replace('\\', "/").to_ascii_lowercase();
    let escaped = forward.replace('/', r"\\");
    let text = text.to_ascii_lowercase();
    text.contains(&forward) || text.contains(&escaped)
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNTOUCHED: &str = r#"{"fileNames":["../../node_modules/typescript/lib/lib.d.ts","./src/a.ts","./src/b.ts","./src/g.d.ts"],"root":[[2,4]],"options":{"strict":true},"affectedFilesPendingEmit":[[2,17],[3,17]],"version":"5.9.3"}"#;

    fn write(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn check_only_build_info_is_carriable() {
        assert!(carriable(UNTOUCHED, Path::new("/nowhere")));
    }

    #[test]
    fn build_info_that_emitted_anything_is_not() {
        let partly = UNTOUCHED.replace(r#"[[2,17],[3,17]]"#, "[3]");
        assert!(!carriable(&partly, Path::new("/nowhere")));
        let emitted = UNTOUCHED.replace(r#","affectedFilesPendingEmit":[[2,17],[3,17]]"#, "");
        assert!(!carriable(&emitted, Path::new("/nowhere")));
    }

    #[test]
    fn absolute_paths_and_unknown_formats_are_refused() {
        let absolute = UNTOUCHED.replace("./src/a.ts", "C:/game/src/a.ts");
        assert!(!carriable(&absolute, Path::new("/nowhere")));
        let naming_source =
            UNTOUCHED.replace(r#""strict":true"#, r#""rootDir":"E:\\Game\\Main\\src""#);
        assert!(!carriable(&naming_source, Path::new("E:/game/main")));
        assert!(!carriable(r#"{"program":{}}"#, Path::new("/nowhere")));
        assert!(!carriable("not json", Path::new("/nowhere")));
    }

    #[test]
    fn resolves_paths_inside_the_worktree_only() {
        assert_eq!(
            join("apps/web", "./.cache/tsc.tsbuildinfo").as_deref(),
            Some("apps/web/.cache/tsc.tsbuildinfo")
        );
        assert_eq!(join("apps/web", "../../out").as_deref(), Some("out"));
        assert_eq!(join("apps/web", "../../../out"), None);
        assert_eq!(join("", "/tmp/x"), None);
        assert_eq!(join("", "C:\\x"), None);
    }

    #[test]
    fn reads_options_from_a_tsconfig_with_comments() {
        let text = "{\n  // check only\n  \"compilerOptions\": { \"outDir\" : \"dist\", \"tsBuildInfoFile\": \"./.cache/app.tsbuildinfo\" }\n}";
        assert_eq!(string_option(text, "outDir").as_deref(), Some("dist"));
        assert_eq!(
            string_option(text, "tsBuildInfoFile").as_deref(),
            Some("./.cache/app.tsbuildinfo")
        );
        assert_eq!(string_option(text, "rootDir"), None);
    }

    #[test]
    fn finds_build_info_next_to_the_config_in_its_out_dir_and_where_configured() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(
            root,
            "apps/web/tsconfig.json",
            "{ \"compilerOptions\": { \"outDir\": \"dist\", \"tsBuildInfoFile\": \".cache/check.tsbuildinfo\" } }",
        );
        write(root, "apps/web/tsconfig.tsbuildinfo", UNTOUCHED);
        write(root, "apps/web/dist/tsconfig.tsbuildinfo", UNTOUCHED);
        write(root, "apps/web/.cache/check.tsbuildinfo", UNTOUCHED);
        write(
            root,
            "apps/web/emitted.tsbuildinfo",
            &UNTOUCHED.replace(r#"[[2,17],[3,17]]"#, "[]"),
        );
        write(root, "tsconfig.base.json", "{}");
        let found = TypeScript.detect(
            root,
            &[
                "apps/web/tsconfig.json",
                "tsconfig.base.json",
                "apps/web/src/a.ts",
            ],
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].root, "apps/web");
        assert_eq!(
            found[0].verified_files,
            [
                "apps/web/.cache/check.tsbuildinfo",
                "apps/web/dist/tsconfig.tsbuildinfo",
                "apps/web/tsconfig.tsbuildinfo",
            ]
        );
        assert_eq!(
            TypeScript.includes(&found[0])[0],
            "/apps/web/.cache/check.tsbuildinfo"
        );
    }
}
