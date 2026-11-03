//! End-to-end tests against real git repositories in temporary directories.

use multibox_core::{
    AddOptions, FinishOptions, FinishOutcome, Mode, Observer, RemoveOptions, add, finish, git,
    purge_trash, remove,
};
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
    write(
        &repo,
        ".gitignore",
        "client/Library/\nclient/Temp/\nclient/Logs/\nclient/*.csproj\nclient/Assets/Generated/\n*.log\nscratch/\n",
    );
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
    write(
        &repo,
        "client/Library/EditorInstance.json",
        "{\"process_id\" : 1}\n",
    );
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
    assert!(!wt.join("client/Library/ilpp.pid").exists());
    assert!(!wt.join("client/Library/Pipeline").exists());
    assert!(!wt.join("client/Library/Search/fts.index-lock").exists());
    assert!(!wt.join("client/Library/ArtifactDB-lock").exists());
    assert!(!wt.join("client/Temp").exists());
    assert!(!wt.join("client/Logs").exists());
    assert!(!wt.join("scratch").exists());
}

#[test]
fn reports_the_environment_that_pins_tools_to_the_worktree() {
    let (_temp, repo) = unity_repo();
    let outcome = add(&repo, &options("../wt", &["--detach"]), &Silent).unwrap();
    let (key, value) = &outcome.environment[0];
    assert_eq!(key, "UNITY_PROJECT_PATH");
    let reported = fs::canonicalize(value).unwrap();
    let expected = fs::canonicalize(repo.parent().unwrap().join("wt/client")).unwrap();
    assert_eq!(reported, expected);

    let wt = repo.parent().unwrap().join("wt");
    assert_eq!(
        multibox_core::adapter::worktree_environment(&wt).unwrap(),
        outcome.environment
    );
}

#[test]
fn databases_arrive_intact_and_keep_their_timestamps() {
    let (_temp, repo) = unity_repo();
    write(&repo, "client/Library/SourceAssetDB", "source asset db\n");
    let old = SystemTime::now() - Duration::from_secs(3_600);
    let database = repo.join("client/Library/ArtifactDB");
    fs::OpenOptions::new()
        .write(true)
        .open(&database)
        .unwrap()
        .set_modified(old)
        .unwrap();

    let outcome = add(&repo, &options("../wt", &["--detach"]), &Silent).unwrap();
    assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);

    let wt = repo.parent().unwrap().join("wt");
    assert_eq!(
        fs::read(wt.join("client/Library/ArtifactDB")).unwrap(),
        fs::read(&database).unwrap()
    );
    assert_eq!(
        fs::read_to_string(wt.join("client/Library/SourceAssetDB")).unwrap(),
        "source asset db\n"
    );
    assert_eq!(
        fs::metadata(wt.join("client/Library/ArtifactDB"))
            .unwrap()
            .modified()
            .unwrap(),
        fs::metadata(&database).unwrap().modified().unwrap()
    );
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
fn a_same_size_edit_in_the_source_does_not_leak() {
    let (_temp, repo) = unity_repo();
    let edited = repo.join("client/Assets/a.txt");
    // Older than the index, so git's protection for racy files stays out of it.
    write(&repo, "client/Assets/a.txt", "COMMITTED A\n");
    fs::OpenOptions::new()
        .write(true)
        .open(&edited)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(3_600))
        .unwrap();
    assert_eq!(status(&repo).trim(), "M client/Assets/a.txt");

    add(&repo, &options("../wt", &["--detach"]), &Silent).unwrap();
    let wt = repo.parent().unwrap().join("wt");
    assert_eq!(
        fs::read_to_string(wt.join("client/Assets/a.txt")).unwrap(),
        "committed a\n"
    );
    assert_eq!(status(&wt), "");
}

#[test]
fn init_is_a_no_op_once_the_project_has_moved() {
    let (_temp, repo) = unity_repo();
    let destination = repo.parent().unwrap().join("moved");
    let options = multibox_core::InitOptions {
        destination: Some(destination.clone()),
        ..Default::default()
    };
    fs::remove_file(repo.join("client/Library/EditorInstance.json")).unwrap();
    multibox_core::init(&repo, &options, &Silent).unwrap();
    let again = multibox_core::init(&destination, &options, &Silent).unwrap();
    assert!(
        !matches!(again, multibox_core::InitOutcome::Switched { .. }),
        "a second run must not move the project onto itself: {again:?}"
    );
    assert!(destination.join("README.md").exists());
    assert!(multibox_core::switch(&destination, &destination, &[]).is_err());
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
        assert_eq!(
            source.modified().unwrap(),
            clone.modified().unwrap(),
            "{rel}"
        );
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
    assert!(
        repo.parent()
            .unwrap()
            .join("second/client/Library")
            .exists()
    );
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
fn remove_ignores_a_lockfile_left_by_a_crashed_editor() {
    let (_temp, repo) = unity_repo();
    add(&repo, &options("../wt", &["--detach"]), &Silent).unwrap();
    let wt = repo.parent().unwrap().join("wt");
    write(&wt, "client/Temp/UnityLockfile", "");
    let outcome = remove(&repo, &removal("../wt", 0)).unwrap();
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
        .args([
            "/c",
            "mklink",
            "/J",
            &native(&link),
            &native(&source_assets),
        ])
        .output()
        .unwrap();
    assert!(made.status.success());

    let outcome = add(&repo, &options("../wt", &["--detach"]), &Silent).unwrap();
    assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
    let wt = repo.parent().unwrap().join("wt");

    write(
        &wt,
        "client/Library/mirror/through-link.txt",
        "written in the clone\n",
    );
    assert!(wt.join("client/Assets/through-link.txt").exists());
    assert!(!source_assets.join("through-link.txt").exists());
}

#[test]
fn unity_virtual_players_stay_behind() {
    let (_temp, repo) = unity_repo();
    write(
        &repo,
        "client/Library/VP/mppm1/Library/data",
        "virtual player\n",
    );
    add(&repo, &options("../wt", &["--detach"]), &Silent).unwrap();
    let wt = repo.parent().unwrap().join("wt");
    assert!(wt.join("client/Library/ArtifactDB").exists());
    assert!(!wt.join("client/Library/VP").exists());
}

#[cfg(windows)]
#[test]
fn switch_can_be_retried_after_something_held_the_project_open() {
    use std::os::windows::fs::OpenOptionsExt;

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

    let held = fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(repo.join("README.md"))
        .unwrap();
    let error = multibox_core::switch(&repo, &destination, &[]).unwrap_err();
    assert!(
        error.to_string().contains("nothing was switched"),
        "{error}"
    );
    assert!(
        repo.join(".git").is_dir(),
        "the original must stay in place"
    );
    drop(held);

    multibox_core::switch(&repo, &destination, &[]).unwrap();
    assert!(repo.join("README.md").exists());
    assert_eq!(git::worktrees(&destination).unwrap().len(), 2);
    assert_eq!(status(&base.join("linked")), "");
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
    assert!(!destination.join("client/Library/ilpp.pid").exists());
    assert!(!destination.join("client/Library/Pipeline").exists());
    assert!(!destination.join("client/Temp").exists());
}

#[test]
fn a_moved_project_needs_no_rehash() {
    let (_temp, repo) = unity_repo();
    let destination = repo.parent().unwrap().join("moved");
    // The index must be newer than the files for their stat data to count.
    std::thread::sleep(Duration::from_millis(50));
    sh(&repo, &["update-index", "--really-refresh"]);
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
    // diff-files trusts the index as it is, so a stale entry shows up here.
    assert_eq!(sh(&destination, &["diff-files", "--name-only"]), "");

    multibox_core::switch(&repo, &destination, &[]).unwrap();
    assert_eq!(sh(&destination, &["diff-files", "--name-only"]), "");
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

    let outcome = multibox_core::switch(&repo, &destination, &[]).unwrap();

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
fn switch_brings_the_copy_up_to_date() {
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
    let head = {
        write(&repo, "committed-later.txt", "new commit\n");
        sh(&repo, &["add", "committed-later.txt"]);
        sh(&repo, &["commit", "-q", "-m", "later"]);
        sh(&repo, &["rev-parse", "HEAD"])
    };
    write(&repo, "client/Assets/a.txt", "uncommitted work\n");
    fs::remove_file(repo.join("README.md")).unwrap();

    let outcome = multibox_core::switch(&repo, &destination, &[]).unwrap();

    assert!(outcome.updated > 0);
    assert_eq!(sh(&destination, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        fs::read_to_string(destination.join("client/Assets/a.txt")).unwrap(),
        "uncommitted work\n"
    );
    assert!(!destination.join("README.md").exists());
    assert_eq!(
        status(&destination).trim(),
        "D README.md\n M client/Assets/a.txt"
    );
}

#[test]
fn init_copies_while_the_editor_is_open_and_finishes_once_it_closes() {
    let (_temp, repo) = unity_repo();
    let destination = repo.parent().unwrap().join("area").join("game");
    // A live process recorded as the editor, like a real open project.
    write(
        &repo,
        "client/Library/EditorInstance.json",
        &format!("{{\"process_id\" : {}}}\n", std::process::id()),
    );
    let options = multibox_core::InitOptions {
        destination: Some(destination.clone()),
        ..Default::default()
    };

    match multibox_core::init(&repo, &options, &Silent).unwrap() {
        multibox_core::InitOutcome::WaitingForEditor { reason, .. } => {
            assert!(reason.contains("unity editor"));
        }
        other => panic!("expected to wait for the editor, got {other:?}"),
    }
    assert!(repo.join("README.md").is_file(), "source must be untouched");
    assert!(destination.join("client/Library/ArtifactDB").exists());

    write(&repo, "client/Assets/a.txt", "edited during the copy\n");
    fs::remove_file(repo.join("client/Library/EditorInstance.json")).unwrap();

    match multibox_core::init(&repo, &options, &Silent).unwrap() {
        multibox_core::InitOutcome::Switched {
            kept_at, updated, ..
        } => {
            assert!(updated >= 1);
            assert!(kept_at.join("README.md").exists(), "original kept");
        }
        other => panic!("expected the switch, got {other:?}"),
    }
    assert_eq!(
        fs::read_to_string(repo.join("client/Assets/a.txt")).unwrap(),
        "edited during the copy\n"
    );
    assert_eq!(status(&repo).trim(), "M client/Assets/a.txt");
}

#[test]
fn copy_mode_leaves_expensive_caches_behind() {
    let (_temp, repo) = unity_repo();
    write(&repo, "client/Library/BuildCache/huge.bin", "build cache\n");
    write(
        &repo,
        "client/Library/Search/assets.index",
        "search index\n",
    );
    let mut request = options("../wt", &["--detach"]);
    request.mode = Mode::Copy;
    add(&repo, &request, &Silent).unwrap();
    let wt = repo.parent().unwrap().join("wt");
    assert!(wt.join("client/Library/ArtifactDB").exists());
    assert!(!wt.join("client/Library/BuildCache").exists());
    assert!(!wt.join("client/Library/Search").exists());
}

#[test]
fn worktreeinclude_can_bring_expensive_caches_back() {
    let (_temp, repo) = unity_repo();
    write(&repo, "client/Library/BuildCache/huge.bin", "build cache\n");
    write(&repo, ".worktreeinclude", "/client/Library/BuildCache/\n");
    sh(&repo, &["add", ".worktreeinclude"]);
    sh(&repo, &["commit", "-q", "-m", "rules"]);
    let mut request = options("../wt", &["--detach"]);
    request.mode = Mode::Copy;
    add(&repo, &request, &Silent).unwrap();
    assert!(
        repo.parent()
            .unwrap()
            .join("wt/client/Library/BuildCache/huge.bin")
            .exists()
    );
}

/// A repository holding a Cargo workspace with one local package, built once.
fn cargo_repo() -> (TempDir, PathBuf) {
    let temp = TempDir::new().unwrap();
    let repo = temp.path().join("main");
    fs::create_dir(&repo).unwrap();
    sh(&repo, &["init", "-q", "-b", "master"]);
    sh(&repo, &["config", "user.email", "test@example.com"]);
    sh(&repo, &["config", "user.name", "test"]);
    sh(&repo, &["config", "core.autocrlf", "false"]);
    write(&repo, ".gitignore", "/target/\n");
    write(&repo, "Cargo.toml", "[workspace]\nmembers = [\"app\"]\n");
    write(&repo, "app/Cargo.toml", "[package]\nname = \"app\"\n");
    write(
        &repo,
        "Cargo.lock",
        "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"serde\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n",
    );
    sh(&repo, &["add", "."]);
    sh(&repo, &["commit", "-q", "-m", "init"]);

    for rel in [
        "target/debug/deps/libserde-0123456789abcdef.rlib",
        "target/debug/.fingerprint/serde-0123456789abcdef/lib-serde",
        "target/debug/build/serde-fedcba9876543210/output",
        "target/debug/deps/libapp-1111111111111111.rlib",
        "target/debug/.fingerprint/app-1111111111111111/lib-app",
        "target/debug/build/app-2222222222222222/out/generated.rs",
        "target/debug/incremental/app-3x4y/s-abc/query-cache.bin",
        "target/debug/app.exe",
        "target/debug/deps/retry_http-3333333333333333.exe",
    ] {
        write(&repo, rel, "built\n");
    }

    // Dep-info as rustc writes it: workspace sources relative to the
    // workspace, everything else absolute, spaces escaped.
    let rule = |unit: &str, sources: &[PathBuf]| {
        let target = repo.join(format!("target/debug/deps/{unit}.d"));
        let sources: Vec<String> = sources
            .iter()
            .map(|path| path.display().to_string().replace(' ', "\\ "))
            .collect();
        format!("{}: {}\n", target.display(), sources.join(" "))
    };
    let registry = temp.path().join("registry/serde-1.0.0/src/lib.rs");
    let generated = repo.join("target/debug/build/serde-fedcba9876543210/out/generated.rs");
    write(
        &repo,
        "target/debug/deps/serde-0123456789abcdef.d",
        &rule("serde-0123456789abcdef", &[registry, generated]),
    );
    write(
        &repo,
        "target/debug/deps/app-1111111111111111.d",
        &rule("app-1111111111111111", &[PathBuf::from("app/src/lib.rs")]),
    );
    // An integration test of `app`, named after its file.
    write(
        &repo,
        "target/debug/deps/retry_http-3333333333333333.d",
        &rule(
            "retry_http-3333333333333333",
            &[PathBuf::from("app/tests/retry_http.rs")],
        ),
    );
    (temp, repo)
}

fn copy_mode(path: &str) -> AddOptions {
    let mut request = options(path, &["--detach"]);
    request.mode = Mode::Copy;
    request
}

#[test]
fn copy_mode_leaves_target_behind_like_a_native_worktree() {
    let (_temp, repo) = cargo_repo();
    add(&repo, &copy_mode("../wt"), &Silent).unwrap();
    let wt = repo.parent().unwrap().join("wt");
    assert!(wt.join("app/Cargo.toml").exists());
    assert!(!wt.join("target").exists());
}

#[test]
fn a_carried_target_never_includes_local_packages() {
    let (_temp, repo) = cargo_repo();
    write(&repo, ".worktreeinclude", "target/\n");
    add(&repo, &copy_mode("../wt"), &Silent).unwrap();
    let wt = repo.parent().unwrap().join("wt");

    // Built from the registry: valid wherever the project lives.
    assert!(
        wt.join("target/debug/deps/libserde-0123456789abcdef.rlib")
            .exists()
    );
    assert!(
        wt.join("target/debug/.fingerprint/serde-0123456789abcdef/lib-serde")
            .exists()
    );
    assert!(
        wt.join("target/debug/build/serde-fedcba9876543210/output")
            .exists()
    );
    // Its dep-info names code generated into the target directory, which
    // does not make it the project's own.
    assert!(
        wt.join("target/debug/deps/serde-0123456789abcdef.d")
            .exists()
    );

    // Built from the project: without a fingerprint, Cargo rebuilds it here,
    // and its outputs would only be overwritten.
    assert!(
        !wt.join("target/debug/.fingerprint/app-1111111111111111")
            .exists()
    );
    assert!(!wt.join("target/debug/build/app-2222222222222222").exists());
    assert!(!wt.join("target/debug/incremental").exists());
    assert!(
        !wt.join("target/debug/deps/libapp-1111111111111111.rlib")
            .exists()
    );
    assert!(!wt.join("target/debug/deps/app-1111111111111111.d").exists());
    // A test named after its file is found by its dep-info alone.
    assert!(
        !wt.join("target/debug/deps/retry_http-3333333333333333.exe")
            .exists()
    );
}

#[test]
fn a_target_directory_full_of_stale_local_units_still_works() {
    let (_temp, repo) = cargo_repo();
    write(&repo, ".worktreeinclude", "target/\n");
    // Every rebuild of a changed test leaves its old unit behind. A workspace
    // used for months had 3,400, and one glob each made the rules too large
    // to compile.
    for n in 0..4000u64 {
        let unit = format!("retry_http-{n:016x}");
        write(
            &repo,
            &format!("target/debug/deps/{unit}.d"),
            &format!("{unit}: app/tests/retry_http.rs\n"),
        );
    }
    add(&repo, &copy_mode("../wt"), &Silent).unwrap();
    let wt = repo.parent().unwrap().join("wt");
    assert!(
        wt.join("target/debug/deps/libserde-0123456789abcdef.rlib")
            .exists()
    );
    assert!(
        !wt.join("target/debug/deps/retry_http-0000000000000fff.d")
            .exists()
    );
    assert!(
        !wt.join("target/debug/deps/retry_http-3333333333333333.exe")
            .exists()
    );
}

/// A repository holding a pnpm workspace, installed once.
fn pnpm_repo() -> (TempDir, PathBuf) {
    let temp = TempDir::new().unwrap();
    let repo = temp.path().join("main");
    fs::create_dir(&repo).unwrap();
    sh(&repo, &["init", "-q", "-b", "master"]);
    sh(&repo, &["config", "user.email", "test@example.com"]);
    sh(&repo, &["config", "user.name", "test"]);
    sh(&repo, &["config", "core.autocrlf", "false"]);
    write(&repo, ".gitignore", "node_modules/\n");
    write(&repo, ".worktreeinclude", "node_modules/\n");
    write(&repo, "pnpm-lock.yaml", "lockfileVersion: '9.0'\n");
    write(&repo, "package.json", "{}\n");
    write(&repo, "apps/site/package.json", "{}\n");
    sh(&repo, &["add", "."]);
    sh(&repo, &["commit", "-q", "-m", "init"]);

    let root = fs::canonicalize(&repo).unwrap().display().to_string();
    let root = root.strip_prefix(r"\\?\").unwrap_or(&root).to_owned();
    let json = root.replace('\\', r"\\");
    write(
        &repo,
        "node_modules/.modules.yaml",
        &format!(
            "{{\n  \"virtualStoreDir\": \"{json}{sep}node_modules{sep}.pnpm\"\n}}\n",
            sep = if cfg!(windows) { r"\\" } else { "/" }
        ),
    );
    write(
        &repo,
        "node_modules/left-pad/index.js",
        "module.exports = 1;\n",
    );
    write(&repo, "apps/site/node_modules/.bin/tool", "#!/bin/sh\n");
    (temp, repo)
}

#[test]
fn copy_mode_refuses_pnpm_node_modules_and_says_so() {
    let (_temp, repo) = pnpm_repo();
    let outcome = add(&repo, &copy_mode("../wt"), &Silent).unwrap();
    let wt = repo.parent().unwrap().join("wt");
    assert!(wt.join("apps/site/package.json").exists());
    assert!(!wt.join("node_modules").exists());
    assert!(!wt.join("apps/site/node_modules").exists());
    assert!(
        outcome
            .warnings
            .iter()
            .any(|warning| warning.starts_with("node_modules and 1 more left behind")),
        "{:?}",
        outcome.warnings
    );
}

#[test]
fn carried_pnpm_state_names_the_new_worktree() {
    let (_temp, repo) = pnpm_repo();
    let outcome = add(&repo, &options("../wt", &["--detach"]), &Silent).unwrap();
    let wt = repo.parent().unwrap().join("wt");
    if !outcome.capability.reflink {
        // Copy mode: refused, as above.
        assert!(!wt.join("node_modules").exists());
        return;
    }
    let state = fs::read_to_string(wt.join("node_modules/.modules.yaml")).unwrap();
    let new_root = fs::canonicalize(&wt).unwrap().display().to_string();
    let new_root = new_root
        .strip_prefix(r"\\?\")
        .unwrap_or(&new_root)
        .replace('\\', r"\\");
    assert!(state.contains(&new_root), "{state}");
    let old_root = fs::canonicalize(&repo).unwrap().display().to_string();
    let old_root = old_root
        .strip_prefix(r"\\?\")
        .unwrap_or(&old_root)
        .replace('\\', r"\\");
    assert!(
        !state.contains(&format!(
            "{old_root}{}",
            if cfg!(windows) { r"\\" } else { "/" }
        )),
        "{state}"
    );
    assert!(wt.join("node_modules/left-pad/index.js").exists());
}

#[test]
fn check_only_typescript_build_info_is_carried_even_in_copy_mode() {
    let temp = TempDir::new().unwrap();
    let repo = temp.path().join("main");
    fs::create_dir(&repo).unwrap();
    sh(&repo, &["init", "-q", "-b", "master"]);
    sh(&repo, &["config", "user.email", "test@example.com"]);
    sh(&repo, &["config", "user.name", "test"]);
    sh(&repo, &["config", "core.autocrlf", "false"]);
    write(&repo, ".gitignore", "*.tsbuildinfo\n");
    write(&repo, "web/tsconfig.json", "{ \"include\": [\"src\"] }\n");
    write(&repo, "web/src/a.ts", "export const a = 1;\n");
    sh(&repo, &["add", "."]);
    sh(&repo, &["commit", "-q", "-m", "init"]);

    let info = |pending: &str| {
        format!(
            "{{\"fileNames\":[\"./src/a.ts\"],\"root\":[1],\"options\":{{}},\"affectedFilesPendingEmit\":{pending},\"version\":\"5.9.3\"}}"
        )
    };
    write(&repo, "web/tsconfig.tsbuildinfo", &info("[[1,17]]"));
    write(&repo, "web/emitted.tsbuildinfo", &info("[]"));

    add(&repo, &copy_mode("../wt"), &Silent).unwrap();
    let wt = repo.parent().unwrap().join("wt");
    assert_eq!(
        fs::read_to_string(wt.join("web/tsconfig.tsbuildinfo")).unwrap(),
        info("[[1,17]]")
    );
    assert!(!wt.join("web/emitted.tsbuildinfo").exists());
}

#[test]
fn verified_build_info_inside_node_modules_survives_the_copy_mode_refusal() {
    let (_temp, repo) = pnpm_repo();
    // The layout create-vite generates for TypeScript projects.
    write(
        &repo,
        "tsconfig.app.json",
        "{ \"compilerOptions\": { \"tsBuildInfoFile\": \"./node_modules/.tmp/tsconfig.app.tsbuildinfo\", \"noEmit\": true } }\n",
    );
    sh(&repo, &["add", "tsconfig.app.json"]);
    sh(&repo, &["commit", "-q", "-m", "typescript"]);
    let info = "{\"fileNames\":[\"../../src/a.ts\"],\"root\":[1],\"options\":{},\"affectedFilesPendingEmit\":[[1,17]],\"version\":\"5.9.3\"}";
    write(&repo, "node_modules/.tmp/tsconfig.app.tsbuildinfo", info);

    add(&repo, &copy_mode("../wt"), &Silent).unwrap();
    let wt = repo.parent().unwrap().join("wt");
    assert_eq!(
        fs::read_to_string(wt.join("node_modules/.tmp/tsconfig.app.tsbuildinfo")).unwrap(),
        info
    );
    assert!(!wt.join("node_modules/left-pad").exists());
    assert!(!wt.join("node_modules/.modules.yaml").exists());
    assert!(!wt.join("apps/site/node_modules").exists());
}

fn finish_options(path: &str) -> FinishOptions {
    FinishOptions {
        path: path.to_owned(),
        keep_branch: false,
        merge_args: Vec::new(),
    }
}

fn commit_all(dir: &Path, message: &str) {
    sh(dir, &["add", "-A"]);
    sh(dir, &["commit", "-q", "-m", message]);
}

fn finished(outcome: FinishOutcome) -> (bool, bool) {
    match outcome {
        FinishOutcome::Finished {
            merged_now,
            removal,
            branch_deleted,
            ..
        } => {
            if let Some(trash) = removal.trash {
                purge_trash(&trash).unwrap();
            }
            (merged_now, branch_deleted)
        }
        other => panic!("not finished: {other:?}"),
    }
}

#[test]
fn finish_merges_the_branch_and_cleans_up() {
    let (_temp, repo) = unity_repo();
    add(&repo, &options("../wt", &["-b", "task"]), &Silent).unwrap();
    let wt = repo.parent().unwrap().join("wt");
    write(&wt, "client/Assets/new.txt", "from the task\n");
    commit_all(&wt, "task work");

    let (merged_now, branch_deleted) = finished(finish(&repo, &finish_options("../wt")).unwrap());
    assert!(merged_now && branch_deleted);
    assert_eq!(
        fs::read_to_string(repo.join("client/Assets/new.txt")).unwrap(),
        "from the task\n"
    );
    assert!(!wt.exists());
    assert_eq!(sh(&repo, &["branch", "--list", "task"]), "");
    assert_eq!(sh(&repo, &["worktree", "list"]).lines().count(), 1);
}

#[test]
fn finish_refuses_uncommitted_work_and_changes_nothing() {
    let (_temp, repo) = unity_repo();
    add(&repo, &options("../wt", &["-b", "task"]), &Silent).unwrap();
    let wt = repo.parent().unwrap().join("wt");
    write(&wt, "client/Assets/a.txt", "not committed\n");

    let err = finish(&repo, &finish_options("../wt")).unwrap_err();
    assert!(err.to_string().contains("uncommitted"), "{err}");
    assert_eq!(status(&wt), " M client/Assets/a.txt\n");
    // `git branch --list` marks a branch checked out elsewhere with "+".
    assert_eq!(
        sh(
            &repo,
            &["branch", "--list", "--format=%(refname:short)", "task"]
        )
        .trim(),
        "task"
    );
}

#[test]
fn finish_stops_on_conflicts_and_cleans_up_once_they_are_resolved() {
    let (_temp, repo) = unity_repo();
    add(&repo, &options("../wt", &["-b", "task"]), &Silent).unwrap();
    let wt = repo.parent().unwrap().join("wt");
    write(&wt, "README.md", "task side\n");
    commit_all(&wt, "task edits the readme");
    write(&repo, "README.md", "main side\n");
    commit_all(&repo, "main edits the readme");

    match finish(&repo, &finish_options("../wt")).unwrap() {
        FinishOutcome::Conflicts { files, .. } => assert_eq!(files, ["README.md"]),
        other => panic!("expected conflicts: {other:?}"),
    }
    assert!(wt.join("README.md").exists());

    write(&repo, "README.md", "both sides\n");
    sh(&repo, &["add", "README.md"]);
    sh(&repo, &["commit", "-q", "--no-edit"]);
    let (merged_now, branch_deleted) = finished(finish(&repo, &finish_options("../wt")).unwrap());
    assert!(!merged_now && branch_deleted);
    assert!(!wt.exists());
}

#[test]
fn finish_does_not_pair_lookalike_meta_files_as_renames() {
    let (_temp, repo) = unity_repo();
    let meta = |guid: &str| {
        format!(
            "fileFormatVersion: 2\nguid: {guid}\nNativeFormatImporter:\n  externalObjects: {{}}\n  mainObjectFileID: 2100000\n  userData: \n  assetBundleName: \n  assetBundleVariant: \n"
        )
    };
    write(
        &repo,
        "client/Assets/old.mat.meta",
        &meta("0c7d727a4b9bc754a878a3b92ed37d5a"),
    );
    commit_all(&repo, "a material");
    add(&repo, &options("../wt", &["-b", "task"]), &Silent).unwrap();
    let wt = repo.parent().unwrap().join("wt");
    // Each side drops the same file and adds an unrelated one that looks alike.
    fs::remove_file(wt.join("client/Assets/old.mat.meta")).unwrap();
    write(
        &wt,
        "client/Assets/Shader.meta",
        &meta("d45fd25396399a04480ddbca4f9e92ef"),
    );
    commit_all(&wt, "task adds a shader folder");
    fs::remove_file(repo.join("client/Assets/old.mat.meta")).unwrap();
    write(
        &repo,
        "client/Assets/Other.png.meta",
        &meta("e03bddd27456498dbd893df73e0c9f89"),
    );
    commit_all(&repo, "main adds a texture");

    // Plain git sees two renames of one file and stops.
    assert!(git::output(&repo, &["merge-tree", "--write-tree", "HEAD", "task"]).is_err());

    finished(finish(&repo, &finish_options("../wt")).unwrap());
    assert!(repo.join("client/Assets/Shader.meta").exists());
    assert!(repo.join("client/Assets/Other.png.meta").exists());
    assert!(!repo.join("client/Assets/old.mat.meta").exists());
}

#[test]
fn concurrent_removals_never_trip_over_each_others_trash() {
    let (_temp, repo) = unity_repo();
    let mut trash = Vec::new();
    for name in ["../a", "../b", "../c"] {
        add(&repo, &options(name, &["--detach"]), &Silent).unwrap();
        trash.push(remove(&repo, &removal(name, 0)).unwrap().trash.unwrap());
    }
    // Every removal sweeps up all trash it finds, so each one tries them all.
    let results: Vec<_> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..3)
            .map(|_| {
                let trash = &trash;
                scope.spawn(move || trash.iter().map(|t| purge_trash(t)).collect::<Vec<_>>())
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().unwrap())
            .collect()
    });
    for result in results {
        let report = result.unwrap();
        assert!(report.failures.is_empty(), "{:?}", report.failures);
    }
    let parent = repo.parent().unwrap();
    let left: Vec<_> = fs::read_dir(parent)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".multibox-trash-"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn a_missing_worktree_is_named_in_the_error() {
    let (_temp, repo) = unity_repo();
    let err = finish(&repo, &finish_options("../nope")).unwrap_err();
    assert_eq!(err.to_string(), "'../nope' is not a working tree");

    let mut request = options("../wt", &["--detach"]);
    request.from = Some(PathBuf::from("../nope"));
    let err = add(&repo, &request, &Silent).unwrap_err();
    assert_eq!(err.to_string(), "'../nope' is not a working tree");
    assert!(!repo.parent().unwrap().join("wt").exists());
}

#[test]
fn git_errors_do_not_repeat_fatal() {
    let outside = TempDir::new().unwrap();
    let err = git::Repo::discover(outside.path()).unwrap_err();
    assert!(err.to_string().starts_with("not a git repository"), "{err}");
}

#[test]
fn write_once_caches_are_hard_linked_where_files_cannot_be_cloned() {
    let (_temp, repo) = unity_repo();
    let outcome = add(&repo, &options("../wt", &["--detach"]), &Silent).unwrap();
    let report = outcome.report.as_ref().unwrap();
    let wt = repo.parent().unwrap().join("wt");
    let artifact = "client/Library/Artifacts/ab/abcd";
    assert_eq!(fs::read_to_string(wt.join(artifact)).unwrap(), "imported\n");
    if outcome.capability.reflink {
        // Clones are free and independent: nothing to share.
        assert_eq!(report.hard_linked, 0);
        assert_eq!(outcome.summary.shared_files, 0);
    } else {
        // Only the import result; tracked files and databases are copied.
        assert_eq!(outcome.summary.shared_files, 1);
        assert_eq!(report.hard_linked, 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(fs::metadata(wt.join(artifact)).unwrap().nlink(), 2);
        }
    }
    assert_eq!(status(&wt), "");

    // Removing the worktree removes its links, not the source's files.
    let removal = remove(&repo, &removal("../wt", 0)).unwrap();
    if let Some(trash) = removal.trash {
        purge_trash(&trash).unwrap();
    }
    assert_eq!(
        fs::read_to_string(repo.join(artifact)).unwrap(),
        "imported\n"
    );
}

#[test]
fn copy_mode_and_the_config_switch_turn_sharing_off() {
    let (_temp, repo) = unity_repo();
    let mut request = options("../wt", &["--detach"]);
    request.mode = Mode::Copy;
    let outcome = add(&repo, &request, &Silent).unwrap();
    assert_eq!(outcome.report.unwrap().hard_linked, 0);

    local_config(&repo, "[carry]\nshare-write-once = false\n");
    let outcome = add(&repo, &options("../wt2", &["--detach"]), &Silent).unwrap();
    assert_eq!(outcome.report.unwrap().hard_linked, 0);
    assert!(
        repo.parent()
            .unwrap()
            .join("wt2/client/Library/Artifacts/ab/abcd")
            .exists()
    );
}

#[test]
fn a_shared_target_is_carried_by_default_even_where_files_are_copied() {
    let (_temp, repo) = cargo_repo();
    let outcome = add(&repo, &options("../wt", &["--detach"]), &Silent).unwrap();
    let wt = repo.parent().unwrap().join("wt");
    assert!(
        wt.join("target/debug/deps/libserde-0123456789abcdef.rlib")
            .exists()
    );
    assert!(
        wt.join("target/debug/.fingerprint/serde-0123456789abcdef/lib-serde")
            .exists()
    );
    // The project's own units still stay behind.
    assert!(
        !wt.join("target/debug/deps/libapp-1111111111111111.rlib")
            .exists()
    );
    if !outcome.capability.reflink {
        // The library is shared; its fingerprint and dep-info are copied.
        assert_eq!(outcome.report.unwrap().hard_linked, 1);
    }
}

/// Writes a `.multibox.toml` kept out of git, as a developer would for
/// settings of their own.
fn local_config(repo: &Path, text: &str) {
    write(repo, ".multibox.toml", text);
    let exclude = repo.join(".git/info/exclude");
    let mut rules = fs::read_to_string(&exclude).unwrap_or_default();
    rules.push_str("\n/.multibox.toml\n");
    write(repo, ".git/info/exclude", &rules);
}

#[test]
fn config_includes_and_excludes_have_the_last_word() {
    let (_temp, repo) = unity_repo();
    local_config(
        &repo,
        "[carry]\ninclude = [\"/scratch/\"]\nexclude = [\"/client/Library/Artifacts/\"]\n",
    );
    add(&repo, &options("../wt", &["--detach"]), &Silent).unwrap();
    let wt = repo.parent().unwrap().join("wt");
    assert!(wt.join("scratch/notes.txt").exists());
    assert!(wt.join("client/Library/ArtifactDB").exists());
    assert!(!wt.join("client/Library/Artifacts").exists());
    // Kept out of git, it still travels, so commands run there see it.
    assert!(wt.join(".multibox.toml").exists());
    assert_eq!(status(&wt), "");
}

#[test]
fn large_caches_always_copies_them_except_node_modules() {
    let (_temp, repo) = cargo_repo();
    local_config(&repo, "[carry]\nlarge-caches = \"always\"\n");
    add(&repo, &copy_mode("../wt"), &Silent).unwrap();
    let wt = repo.parent().unwrap().join("wt");
    assert!(
        wt.join("target/debug/deps/libserde-0123456789abcdef.rlib")
            .exists()
    );
    assert!(
        !wt.join("target/debug/.fingerprint/app-1111111111111111")
            .exists()
    );

    let (_temp, repo) = pnpm_repo();
    local_config(&repo, "[carry]\nlarge-caches = \"always\"\n");
    add(&repo, &copy_mode("../wt"), &Silent).unwrap();
    assert!(!repo.parent().unwrap().join("wt/node_modules").exists());
}

#[test]
fn large_caches_never_leaves_them_behind_unless_a_pattern_asks() {
    let (_temp, repo) = cargo_repo();
    local_config(&repo, "[carry]\nlarge-caches = \"never\"\n");
    add(&repo, &options("../wt", &["--detach"]), &Silent).unwrap();
    let wt = repo.parent().unwrap().join("wt");
    assert!(wt.join("app/Cargo.toml").exists());
    assert!(!wt.join("target/debug/deps").exists());

    // An explicit pattern is a decision for this repository, so it wins.
    write(&repo, ".worktreeinclude", "target/\n");
    add(&repo, &options("../wt2", &["--detach"]), &Silent).unwrap();
    assert!(
        repo.parent()
            .unwrap()
            .join("wt2/target/debug/deps/libserde-0123456789abcdef.rlib")
            .exists()
    );
}

#[test]
fn a_turned_off_engine_carries_nothing_of_its_own() {
    let (_temp, repo) = unity_repo();
    local_config(&repo, "[engines]\nunity = false\n");
    add(&repo, &options("../wt", &["--detach"]), &Silent).unwrap();
    let wt = repo.parent().unwrap().join("wt");
    assert!(wt.join("client/Assets/a.txt").exists());
    assert!(!wt.join("client/Library").exists());
}

#[test]
fn turned_off_multibox_is_a_plain_git_worktree() {
    let (_temp, repo) = unity_repo();
    local_config(&repo, "enabled = false\n");
    let outcome = add(&repo, &options("../wt", &["-b", "task"]), &Silent).unwrap();
    assert!(outcome.native);
    let wt = repo.parent().unwrap().join("wt");
    assert!(wt.join("client/Assets/a.txt").exists());
    assert!(!wt.join("client/Library").exists());
    assert_eq!(sh(&wt, &["branch", "--show-current"]).trim(), "task");
    assert_eq!(status(&wt), "");
}

#[test]
fn a_broken_config_never_stops_a_worktree() {
    let (_temp, repo) = unity_repo();
    local_config(&repo, "[carry\ninclude = [");
    let outcome = add(&repo, &options("../wt", &["--detach"]), &Silent).unwrap();
    let wt = repo.parent().unwrap().join("wt");
    assert!(wt.join("client/Library/ArtifactDB").exists());
    assert!(
        outcome
            .warnings
            .iter()
            .any(|warning| warning.starts_with(".multibox.toml: not valid TOML")),
        "{:?}",
        outcome.warnings
    );
}

#[test]
fn finish_takes_merge_options_and_branch_keeping_from_the_config() {
    let (_temp, repo) = unity_repo();
    write(
        &repo,
        ".multibox.toml",
        "[finish]\nmerge-options = [\"--no-ff\"]\nkeep-branch = true\n",
    );
    commit_all(&repo, "settings");
    add(&repo, &options("../wt", &["-b", "task"]), &Silent).unwrap();
    let wt = repo.parent().unwrap().join("wt");
    write(&wt, "client/Assets/new.txt", "from the task\n");
    commit_all(&wt, "task work");

    let (merged_now, branch_deleted) = finished(finish(&repo, &finish_options("../wt")).unwrap());
    assert!(merged_now && !branch_deleted);
    let parents = sh(&repo, &["rev-list", "--parents", "-n", "1", "HEAD"]);
    assert_eq!(parents.split_whitespace().count(), 3, "{parents}");
    assert_eq!(
        sh(
            &repo,
            &["branch", "--list", "--format=%(refname:short)", "task"]
        )
        .trim(),
        "task"
    );
}
