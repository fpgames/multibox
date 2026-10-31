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
    let mut request = options("../wt", &["--detach"]);
    request.mode = Mode::Copy;
    add(&repo, &request, &Silent).unwrap();
    let wt = repo.parent().unwrap().join("wt");
    assert!(wt.join("client/Library/ArtifactDB").exists());
    assert!(!wt.join("client/Library/BuildCache").exists());
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
