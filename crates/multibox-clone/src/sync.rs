use crate::exec::{Failure, Options, execute};
use crate::scan::{DirEntry, FileEntry, Manifest, ScanRequest, TreeFilter, scan};
use crate::{join, thread_count};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::Path;

#[derive(Debug, Default)]
pub struct SyncReport {
    pub updated: u64,
    pub removed: u64,
    pub failures: Vec<Failure>,
}

fn remove_quietly(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Makes `dst_root` match `src_root` for every path `filter` selects, touching
/// only what differs. A file counts as unchanged when its size and modification
/// time match, which holds for anything [`execute`] cloned. Paths the filter
/// rejects are left alone on both sides.
pub fn sync(
    src_root: &Path,
    dst_root: &Path,
    filter: &dyn TreeFilter,
    threads: Option<usize>,
) -> io::Result<SyncReport> {
    let threads = thread_count(threads);
    let walk = |root: &Path| {
        scan(
            root,
            &ScanRequest {
                files: Vec::new(),
                links: Vec::new(),
                trees: vec![String::new()],
                filter,
                threads: Some(threads),
            },
        )
    };
    let source = walk(src_root)?;
    let target = walk(dst_root)?;
    let dst = fs::canonicalize(dst_root)?;

    let existing: HashMap<&str, &FileEntry> = target
        .files
        .iter()
        .map(|file| (file.rel.as_str(), file))
        .collect();
    let wanted: HashSet<&str> = source.files.iter().map(|file| file.rel.as_str()).collect();

    let changed: Vec<FileEntry> = source
        .files
        .iter()
        .filter(|file| {
            existing.get(file.rel.as_str()).is_none_or(|current| {
                current.size != file.size || current.modified != file.modified
            })
        })
        .cloned()
        .collect();

    let mut report = SyncReport {
        updated: changed.len() as u64,
        ..SyncReport::default()
    };
    for file in &changed {
        remove_quietly(fs::remove_file(join(&dst, &file.rel)))?;
    }
    for file in &target.files {
        if !wanted.contains(file.rel.as_str()) {
            remove_quietly(fs::remove_file(join(&dst, &file.rel)))?;
            report.removed += 1;
        }
    }

    let bytes = changed.iter().map(|file| file.size).sum();
    let dirs: Vec<DirEntry> = source.dirs.clone();
    let manifest = Manifest {
        dirs,
        files: changed,
        links: Vec::new(),
        bytes,
        skipped: Vec::new(),
    };
    let result = execute(
        &manifest,
        src_root,
        dst_root,
        &Options {
            threads: Some(threads),
            ..Options::default()
        },
    )?;
    report.failures = result.failures;

    let kept: HashSet<&str> = source.dirs.iter().map(|dir| dir.rel.as_str()).collect();
    let mut stale: Vec<&str> = target
        .dirs
        .iter()
        .map(|dir| dir.rel.as_str())
        .filter(|rel| !kept.contains(rel))
        .collect();
    stale.sort_unstable_by_key(|rel| std::cmp::Reverse(rel.len()));
    for rel in stale {
        // A directory the filter still has contents for simply stays.
        let _ = fs::remove_dir(join(&dst, rel));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::Decision;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    fn everything(_: &str, _: bool) -> Decision {
        Decision::Include
    }

    fn write(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn brings_the_target_in_line_and_touches_nothing_else() {
        let temp = tempfile::tempdir().unwrap();
        let (src, dst) = (temp.path().join("src"), temp.path().join("dst"));
        write(&src, "same.txt", "same");
        write(&src, "edit.txt", "new");
        write(&src, "deep/added.txt", "added");
        write(&dst, "edit.txt", "old");
        write(&dst, "gone/stale.txt", "stale");
        fs::copy(src.join("same.txt"), dst.join("same.txt")).unwrap();
        let set_time = |path: PathBuf, age: u64| {
            fs::OpenOptions::new()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::now() - Duration::from_secs(age))
                .unwrap();
        };
        let older = SystemTime::now() - Duration::from_secs(60);
        for root in [&src, &dst] {
            fs::OpenOptions::new()
                .write(true)
                .open(root.join("same.txt"))
                .unwrap()
                .set_modified(older)
                .unwrap();
        }
        set_time(dst.join("edit.txt"), 120);

        let report = sync(&src, &dst, &everything, Some(2)).unwrap();

        assert_eq!(report.updated, 2);
        assert_eq!(report.removed, 1);
        assert_eq!(fs::read_to_string(dst.join("edit.txt")).unwrap(), "new");
        assert_eq!(
            fs::read_to_string(dst.join("deep/added.txt")).unwrap(),
            "added"
        );
        assert!(!dst.join("gone").exists());
    }

    #[test]
    fn leaves_filtered_paths_alone() {
        let temp = tempfile::tempdir().unwrap();
        let (src, dst) = (temp.path().join("src"), temp.path().join("dst"));
        write(&src, "keep.txt", "x");
        write(&dst, "Temp/editor-state", "belongs to the target");
        let filter = |rel: &str, _: bool| {
            if rel == "Temp" {
                Decision::Exclude
            } else {
                Decision::Include
            }
        };
        sync(&src, &dst, &filter, Some(2)).unwrap();
        assert!(dst.join("Temp/editor-state").exists());
    }
}
