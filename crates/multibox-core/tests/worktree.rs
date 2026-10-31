//! End-to-end tests against real git repositories in temporary directories.

use multibox_core::{AddOptions, Mode, Observer, RemoveOptions, add, git, purge_trash, remove};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tempfile::TempDir;

struct Silent;
impl Observer for Silent {}

fn sh(dir: &Path, args: &[&str]) -> String {
    git::output(dir, args).unwrap_or_else(|err| panic!("git {args:?}: {err}"))
}

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// A repository holding a Unity project in `client/`, committed and clean.
fn unity_repo() -> (TempDir, PathBuf) {
    let temp = TempDir::new().unwrap();
    let repo = temp.path().join("main");
    fs::create_dir(&repo).unwrap();
    sh(&repo, &["init", "-q", "-b", "master"]);
    sh(&repo, &["config", "user.email", "test@example.com"]);
    sh(&repo, &["config", "user.name", "test"]);
    sh(&repo, &["config", "core.autocrlf", "false"]);
    write(&repo, ".gitignore", "client/Library/\nclient/Temp/\nclient/Logs/\nclient/*.csproj\nclient/Assets/Generated/\n*.log\nscratch/\n");
    write(
        &repo,
        "client/ProjectSettings/ProjectVersion.txt",
        "m_EditorVersion: 6000.4.10f1\n",
    );
    write(&repo, "client/Assets/a.txt", "committed a\n");
    write(&repo, "client/Assets/deep/er/b.txt", "committed b\n");
    write(&repo, "README.md", "readme\n");
    sh(&repo, &["add", "."]);
    sh(&repo, &["commit", "-q", "-m", "init"]);

    write(&repo, "client/Library/ArtifactDB", "artifact db\n");
    write(&repo, "client/Library/ArtifactDB-lock", "lock\n");
    write(&repo, "client/Library/EditorInstance.json", "{\"process_id\" : 1}\n");
    write(&repo, "client/Library/Artifacts/ab/abcd", "imported\n");
    write(&repo, "client/Temp/scratch", "temp\n");
    write(&repo, "client/Logs/Editor.log", "log\n");
    write(&repo, "client/Game.csproj", "<Project/>\n");
    write(&repo, "client/Assets/Generated/config.json", "{}\n");
    write(&repo, "scratch/notes.txt", "not selected\n");
    (temp, repo)
}

fn options(path: &str, extra: &[&str]) -> AddOptions {
    let mut git_args: Vec<String> = extra.iter().map(|arg| (*arg).to_owned()).collect();
    git_args.insert(0, path.to_owned());
    AddOptions {
        git_args,
        path: path.to_owned(),
        from: None,
        mode: Mode::Auto,
        carry: true,
        dry_run: false,
        threads: Some(2),
    }
}

fn status(dir: &Path) -> String {
    sh(dir, &["status", "--porcelain"])
}

#[test]
fn new_worktree_is_a_clean_checkout_with_caches() {
    let (_temp, repo) = unity_repo();
    let outcome = add(&repo, &options("../wt", &["-b", "feature"]), &Silent).unwrap();
    assert!(outcome.reused_index);
    let wt = repo.parent().unwrap().join("wt");

    assert_eq!(status(&wt), "");
    assert_eq!(sh(&wt, &["branch", "--show-current"]).trim(), "feature");
    assert_eq!(
        fs::read_to_string(wt.join("client/Assets/deep/er/b.txt")).unwrap(),
        "committed b\n"
    );

    // Carried: everything the engine's cache depends on.
    assert!(wt.join("client/Library/ArtifactDB").exists());
    assert!(wt.join("client/Library/Artifacts/ab/abcd").exists());
    assert!(wt.join("client/Game.csproj").exists());
    assert!(wt.join("client/Assets/Generated/config.json").exists());

    // Left behind: per-instance state and anything nobody asked for.
    assert!(!wt.join("client/Library/EditorInstance.json").exists());
    assert!(!wt.join("client/Library/ArtifactDB-lock").exists());
    assert!(!wt.join("client/Temp").exists());
    assert!(!wt.join("client/Logs").exists());
    assert!(!wt.join("scratch").exists());
}

#[test]
fn registers_like_a_native_worktree() {
    let (_temp, repo) = unity_repo();
    add(&repo, &options("../wt", &["--detach"]), &Silent).unwrap();
    let list = git::worktrees(&repo).unwrap();
    assert_eq!(list.len(), 2);
    assert!(list[1].detached);
    let wt = repo.parent().unwrap().join("wt");
    write(&wt, "client/Assets/a.txt", "changed in worktree\n");
    assert_eq!(status(&wt).trim(), "M client/Assets/a.txt");
    assert_eq!(status(&repo), "");
}

#[test]
fn source_changes_do_not_leak_into_the_new_worktree() {
    let (_temp, repo) = unity_repo();
    write(&repo, "client/Assets/a.txt", "uncommitted edit\n");
    write(&repo, "staged.txt", "staged but never committed\n");
    sh(&repo, &["add", "staged.txt"]);
    fs::remove_file(repo.join("README.md")).unwrap();
    write(&repo, "untracked.txt", "untracked\n");
    let before = status(&repo);

    add(&repo, &options("../wt", &["--detach"]), &Silent).unwrap();
    let wt = repo.parent().unwrap().join("wt");

    assert_eq!(status(&wt), "");
    assert_eq!(
        fs::read_to_string(wt.join("client/Assets/a.txt")).unwrap(),
        "committed a\n"
    );
    assert!(wt.join("README.md").exists());
    assert!(!wt.join("staged.txt").exists());
    assert!(!wt.join("untracked.txt").exists());
    assert_eq!(status(&repo), before, "the source must be left untouched");
}

#[test]
fn checks_out_a_different_commit() {
    let (_temp, repo) = unity_repo();
    let first = sh(&repo, &["rev-parse", "HEAD"]);
    write(&repo, "client/Assets/a.txt", "second version\n");
    write(&repo, "client/Assets/new.txt", "added later\n");
    fs::remove_file(repo.join("README.md")).unwrap();
    sh(&repo, &["add", "-A"]);
    sh(&repo, &["commit", "-q", "-m", "second"]);

    add(&repo, &options("../wt", &[first.trim()]), &Silent).unwrap();
    let wt = repo.parent().unwrap().join("wt");

    assert_eq!(status(&wt), "");
    assert_eq!(
        fs::read_to_string(wt.join("client/Assets/a.txt")).unwrap(),
        "committed a\n"
    );
    assert!(!wt.join("client/Assets/new.txt").exists());
    assert!(wt.join("README.md").exists());
}

#[test]
fn keeps_timestamps_of_tracked_and_cached_files() {
    let (_temp, repo) = unity_repo();
    let old = SystemTime::now() - Duration::from_secs(86_400 * 30);
    for rel in ["client/Assets/a.txt", "client/Library/ArtifactDB"] {
        let file = fs::OpenOptions::new()
            .write(true)
            .open(repo.join(rel))
            .unwrap();
        file.set_modified(old).unwrap();
    }
    sh(&repo, &["status"]);

    add(&repo, &options("../wt", &["--detach"]), &Silent).unwrap();
    let wt = repo.parent().unwrap().join("wt");
    for rel in ["client/Assets/a.txt", "client/Library/ArtifactDB"] {
        let source = fs::metadata(repo.join(rel)).unwrap();
        let clone = fs::metadata(wt.join(rel)).unwrap();
        assert_eq!(source.modified().unwrap(), clone.modified().unwrap(), "{rel}");
        #[cfg(windows)]
        assert_eq!(source.created().unwrap(), clone.created().unwrap(), "{rel}");
    }
}

#[test]
fn still_carries_caches_when_the_index_cannot_be_reused() {
    let (_temp, repo) = unity_repo();
    sh(&repo, &["update-index", "--split-index"]);

    let outcome = add(&repo, &options("../wt", &["--detach"]), &Silent).unwrap();
    assert!(!outcome.reused_index);
    assert!(outcome.warnings[0].contains("split index"));

    let wt = repo.parent().unwrap().join("wt");
    assert_eq!(status(&wt), "");
    assert!(wt.join("client/Library/ArtifactDB").exists());
    assert!(!wt.join("client/Library/EditorInstance.json").exists());
    assert_eq!(
        fs::read_to_string(wt.join("client/Assets/a.txt")).unwrap(),
        "committed a\n"
    );
}

#[test]
fn relative_from_is_resolved_against_cwd() {
    let (_temp, repo) = unity_repo();
    add(&repo, &options("../first", &["--detach"]), &Silent).unwrap();
    let mut request = options("../second", &["--detach"]);
    request.from = Some(PathBuf::from("../first"));
    add(&repo, &request, &Silent).unwrap();
    assert!(repo.parent().unwrap().join("second/client/Library").exists());
}

#[test]
fn honours_worktreeinclude() {
    let (_temp, repo) = unity_repo();
    write(
        &repo,
        ".worktreeinclude",
        "# extra files\nscratch/\n!scratch/secret.txt\n!/client/Library/Artifacts/\n",
    );
    write(&repo, "scratch/secret.txt", "keep out\n");
    sh(&repo, &["add", ".worktreeinclude"]);
    sh(&repo, &["commit", "-q", "-m", "rules"]);

    add(&repo, &options("../wt", &["--detach"]), &Silent).unwrap();
    let wt = repo.parent().unwrap().join("wt");
    assert!(wt.join("scratch/notes.txt").exists());
    assert!(!wt.join("scratch/secret.txt").exists());
    assert!(!wt.join("client/Library/Artifacts").exists());
    assert!(wt.join("client/Library/ArtifactDB").exists());
}

#[test]
fn no_cache_leaves_ignored_files_behind() {
    let (_temp, repo) = unity_repo();
    let mut request = options("../wt", &["--detach"]);
    request.carry = false;
    add(&repo, &request, &Silent).unwrap();
    let wt = repo.parent().unwrap().join("wt");
    assert_eq!(status(&wt), "");
    assert!(!wt.join("client/Library").exists());
}

#[test]
fn dry_run_touches_nothing() {
    let (_temp, repo) = unity_repo();
    let mut request = options("../wt", &["--detach"]);
    request.dry_run = true;
    let outcome = add(&repo, &request, &Silent).unwrap();
    assert!(outcome.report.is_none());
    assert_eq!(outcome.summary.tracked_files, 5);
    assert!(outcome.summary.cached_files >= 4);
    assert!(!repo.parent().unwrap().join("wt").exists());
    assert_eq!(git::worktrees(&repo).unwrap().len(), 1);
}

#[test]
fn refuses_to_overwrite_an_existing_directory() {
    let (_temp, repo) = unity_repo();
    let occupied = repo.parent().unwrap().join("wt");
    write(&occupied, "precious.txt", "do not touch\n");
    assert!(add(&repo, &options("../wt", &["--detach"]), &Silent).is_err());
    assert_eq!(
        fs::read_to_string(occupied.join("precious.txt")).unwrap(),
        "do not touch\n"
    );
}

#[test]
fn clones_from_another_worktree() {
    let (_temp, repo) = unity_repo();
    let base = repo.parent().unwrap();
    add(&repo, &options("../first", &["--detach"]), &Silent).unwrap();
    write(&base.join("first"), "client/Library/only-here", "x\n");

    let mut request = options("../second", &["--detach"]);
    request.from = Some(base.join("first"));
    add(&repo, &request, &Silent).unwrap();
    assert!(base.join("second/client/Library/only-here").exists());
}

fn removal(path: &str, force: u8) -> RemoveOptions {
    RemoveOptions {
        path: path.to_owned(),
        force,
    }
}

#[test]
fn remove_unregisters_and_purges() {
    let (_temp, repo) = unity_repo();
    add(&repo, &options("../wt", &["--detach"]), &Silent).unwrap();
    let wt = repo.parent().unwrap().join("wt");

    let outcome = remove(&repo, &removal("../wt", 0)).unwrap();
    assert!(!wt.exists());
    assert_eq!(git::worktrees(&repo).unwrap().len(), 1);

    let trash = outcome.trash.expect("removal should be deferred");
    assert!(trash.exists());
    let report = purge_trash(&trash).unwrap();
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert!(!trash.exists());
}

#[test]
fn remove_refuses_the_main_worktree_and_strangers() {
    let (_temp, repo) = unity_repo();
    assert!(remove(&repo, &removal(".", 2)).is_err());
    assert!(repo.join("README.md").exists());

    let stranger = repo.parent().unwrap().join("stranger");
    write(&stranger, "file.txt", "not a worktree\n");
    assert!(remove(&repo, &removal("../stranger", 2)).is_err());
    assert!(stranger.join("file.txt").exists());
}

#[test]
fn remove_refuses_dirty_worktrees_without_force() {
    let (_temp, repo) = unity_repo();
    add(&repo, &options("../wt", &["--detach"]), &Silent).unwrap();
    let wt = repo.parent().unwrap().join("wt");
    write(&wt, "client/Assets/a.txt", "work in progress\n");

    assert!(remove(&repo, &removal("../wt", 0)).is_err());
    assert_eq!(
        fs::read_to_string(wt.join("client/Assets/a.txt")).unwrap(),
        "work in progress\n"
    );

    let outcome = remove(&repo, &removal("../wt", 1)).unwrap();
    assert!(!wt.exists());
    purge_trash(&outcome.trash.unwrap()).unwrap();
}

#[test]
fn remove_respects_locks() {
    let (_temp, repo) = unity_repo();
    add(&repo, &options("../wt", &["--detach", "--lock"]), &Silent).unwrap();
    assert!(remove(&repo, &removal("../wt", 1)).is_err());
    let outcome = remove(&repo, &removal("../wt", 2)).unwrap();
    purge_trash(&outcome.trash.unwrap()).unwrap();
}

#[test]
fn purge_refuses_anything_that_is_not_trash() {
    let (_temp, repo) = unity_repo();
    assert!(purge_trash(&repo).is_err());
    assert!(repo.join("README.md").exists());
}

#[cfg(windows)]
#[test]
fn junctions_into_the_source_follow_the_clone() {
    let (_temp, repo) = unity_repo();
    write(&repo, ".worktreeinclude", "/client/Library/\n");
    let source_assets = repo.join("client").join("Assets");
    let link = repo.join("client").join("Library").join("mirror");
    let native = |path: &Path| path.display().to_string().replace('/', "\\");
    let made = std::process::Command::new("cmd")
        .args(["/c", "mklink", "/J", &native(&link), &native(&source_assets)])
        .output()
        .unwrap();
    assert!(made.status.success());

    let outcome = add(&repo, &options("../wt", &["--detach"]), &Silent).unwrap();
    assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
    let wt = repo.parent().unwrap().join("wt");

    write(&wt, "client/Library/mirror/through-link.txt", "written in the clone\n");
    assert!(wt.join("client/Assets/through-link.txt").exists());
    assert!(!source_assets.join("through-link.txt").exists());
}

#[test]
fn unity_virtual_players_stay_behind() {
    let (_temp, repo) = unity_repo();
    write(&repo, "client/Library/VP/mppm1/Library/data", "virtual player\n");
    add(&repo, &options("../wt", &["--detach"]), &Silent).unwrap();
    let wt = repo.parent().unwrap().join("wt");
    assert!(wt.join("client/Library/ArtifactDB").exists());
    assert!(!wt.join("client/Library/VP").exists());
}

#[cfg(windows)]
#[test]
fn purge_does_not_follow_junctions() {
    let temp = TempDir::new().unwrap();
    let outside = temp.path().join("outside");
    write(&outside, "keep.txt", "survives\n");
    let trash = temp.path().join(".multibox-trash-test");
    fs::create_dir(&trash).unwrap();
    let link = trash.join("link");
    let status = std::process::Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(&link)
        .arg(&outside)
        .output()
        .unwrap();
    assert!(status.status.success());

    purge_trash(&trash).unwrap();
    assert!(!trash.exists());
    assert_eq!(
        fs::read_to_string(outside.join("keep.txt")).unwrap(),
        "survives\n"
    );
}

#[test]
fn migrate_copies_without_touching_the_source_or_its_worktrees() {
    let (_temp, repo) = unity_repo();
    let base = repo.parent().unwrap().to_path_buf();
    add(&repo, &options("../linked", &["--detach"]), &Silent).unwrap();
    write(&repo, "client/Assets/a.txt", "uncommitted edit\n");
    let before = status(&repo);

    let destination = base.join("moved");
    multibox_core::migrate(
        &repo,
        &multibox_core::MigrateOptions {
            destination: destination.clone(),
            exclude: Vec::new(),
            dry_run: false,
            threads: Some(2),
        },
        &Silent,
    )
    .unwrap();

    assert_eq!(status(&repo), before, "source must be unchanged");
    assert_eq!(git::worktrees(&repo).unwrap().len(), 2);
    assert_eq!(git::worktrees(&base.join("linked")).unwrap().len(), 2);

    // The copy is a working repository that does not claim the linked worktree.
    assert_eq!(git::worktrees(&destination).unwrap().len(), 1);
    assert_eq!(status(&destination), before);
    assert_eq!(
        fs::read_to_string(destination.join("client/Assets/a.txt")).unwrap(),
        "uncommitted edit\n"
    );
    assert!(destination.join("client/Library/ArtifactDB").exists());
    assert!(!destination.join("client/Library/ArtifactDB-lock").exists());
    assert!(!destination.join("client/Temp").exists());
}

#[test]
fn migrate_leaves_excluded_paths_behind() {
    let (_temp, repo) = unity_repo();
    write(&repo, "server/target/debug/huge.bin", "build output\n");
    let destination = repo.parent().unwrap().join("moved");
    multibox_core::migrate(
        &repo,
        &multibox_core::MigrateOptions {
            destination: destination.clone(),
            exclude: vec!["server/target/".to_owned()],
            dry_run: false,
            threads: Some(2),
        },
        &Silent,
    )
    .unwrap();
    assert!(!destination.join("server/target").exists());
    assert!(destination.join("client/Library/ArtifactDB").exists());
    assert!(repo.join("server/target/debug/huge.bin").exists());
}

#[test]
fn migrate_refuses_an_existing_destination() {
    let (_temp, repo) = unity_repo();
    let occupied = repo.parent().unwrap().join("moved");
    write(&occupied, "precious.txt", "do not touch\n");
    let result = multibox_core::migrate(
        &repo,
        &multibox_core::MigrateOptions {
            destination: occupied.clone(),
            exclude: Vec::new(),
            dry_run: false,
            threads: Some(2),
        },
        &Silent,
    );
    assert!(result.is_err());
    assert!(occupied.join("precious.txt").exists());
}

#[test]
fn switch_keeps_the_original_and_redirects_the_old_path() {
    let (_temp, repo) = unity_repo();
    let base = repo.parent().unwrap().to_path_buf();
    add(&repo, &options("../linked", &["--detach"]), &Silent).unwrap();
    let destination = base.join("moved");
    multibox_core::migrate(
        &repo,
        &multibox_core::MigrateOptions {
            destination: destination.clone(),
            exclude: Vec::new(),
            dry_run: false,
            threads: Some(2),
        },
        &Silent,
    )
    .unwrap();

    let outcome = multibox_core::switch(&repo, &destination).unwrap();

    assert!(outcome.kept_at.join("README.md").exists(), "original kept");
    assert!(repo.join("README.md").exists(), "old path still resolves");
    assert_eq!(status(&repo), "");
    assert_eq!(status(&destination), "");

    // Commits made through either path land in the same repository, and the
    // linked worktree keeps working.
    let linked = base.join("linked");
    assert_eq!(status(&linked), "");
    assert_eq!(git::worktrees(&destination).unwrap().len(), 2);
    write(&repo, "after.txt", "made through the old path\n");
    sh(&repo, &["add", "after.txt"]);
    sh(&repo, &["commit", "-q", "-m", "after"]);
    assert!(destination.join("after.txt").exists());
    assert_eq!(
        sh(&linked, &["rev-parse", "master"]),
        sh(&destination, &["rev-parse", "master"])
    );
}

#[test]
fn switch_refuses_a_stale_copy() {
    let (_temp, repo) = unity_repo();
    let destination = repo.parent().unwrap().join("moved");
    multibox_core::migrate(
        &repo,
        &multibox_core::MigrateOptions {
            destination: destination.clone(),
            exclude: Vec::new(),
            dry_run: false,
            threads: Some(2),
        },
        &Silent,
    )
    .unwrap();
    write(&repo, "later.txt", "new work\n");
    sh(&repo, &["add", "later.txt"]);
    sh(&repo, &["commit", "-q", "-m", "later"]);

    assert!(multibox_core::switch(&repo, &destination).is_err());
    assert!(repo.join("later.txt").exists());
}
