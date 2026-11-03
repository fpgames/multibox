//! `git multibox`: the command-line front end.
//!
//! Arguments mirror `git worktree`. `add` and `remove` are accelerated, and
//! every other subcommand is handed to git unchanged.

use multibox_core::{
    AddOptions, CONFIG_FILE, Config, FinishOptions, FinishOutcome, InitOptions, InitOutcome, Level,
    MigrateOptions, Mode, Mount, Observer, Progress, RemoveOptions, SetupOptions, adapter, add,
    diagnose, finish, git, init, migrate, plan_setup, purge_trash, remove, run_setup,
    spawn_detached, sweep_trash, switch,
};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const USAGE: &str = "\
usage: git multibox add [<options>] <path> [<commit-ish>]
       git multibox remove [-f] <worktree>
       git multibox finish [--keep-branch] [--wait] [--json] <worktree> [-- <merge options>]
       git multibox <list|lock|move|prune|repair|unlock> ...
       git multibox init [--project <path>] [--to <path>] [--exclude <path>]... [--dry-run]
       git multibox doctor [--json]
       git multibox env [--json]
       git multibox setup [--vhdx <file>] [--size <GiB>] [--letter <X> | --folder <dir>] [--dry-run]
       git multibox migrate <destination> [--exclude <path>]... [--dry-run]
       git multibox migrate --switch <destination>

Multibox-specific options for add:
    --from <worktree>   clone from this worktree instead of the current one
    --copy              always copy, even where reflinks are available
    --reflink           fail unless reflinks are available
    --no-cache          leave ignored caches behind
    --threads <n>       number of worker threads
    --dry-run           show what would be cloned and stop
    --json              print the result as JSON on stdout

Options for remove:
    --wait              delete in the foreground instead of in the background
    --json              print the result as JSON on stdout

finish merges the worktree's branch into the branch checked out here, then
removes the worktree and the merged branch. It stops before changing anything
if work is uncommitted or an editor has the worktree open, and leaves a merge
that hits conflicts in progress, as git does. Run it again once committed.
    --keep-branch       keep the branch after merging it
    --wait              delete in the foreground instead of in the background
    --json              print the result as JSON on stdout

Every option of `git worktree add` is accepted and passed through.

Nothing needs configuring. A .multibox.toml at the root of the repository can
carry more or fewer paths, turn engines off, set finish's merge options or
turn Multibox off for the repository; `git multibox doctor` shows what it does.
";

fn gib(bytes: u64) -> f64 {
    bytes as f64 / (1u64 << 30) as f64
}

#[derive(Default)]
struct Terminal {
    stop: Arc<AtomicBool>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Observer for Terminal {
    fn note(&self, message: &str) {
        eprintln!("multibox: {message}");
    }

    fn warn(&self, message: &str) {
        eprintln!("multibox: warning: {message}");
    }

    fn clone_started(&self, files: u64, bytes: u64, progress: Arc<Progress>) {
        eprintln!("multibox: cloning {files} files ({:.1} GiB)", gib(bytes));
        let stop = self.stop.clone();
        let started = Instant::now();
        let worker = std::thread::spawn(move || {
            let mut tick = 0u32;
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(250));
                tick += 1;
                if tick % 40 != 0 {
                    continue;
                }
                let done = progress.files.load(Ordering::Relaxed);
                let copied = progress.bytes.load(Ordering::Relaxed);
                eprintln!(
                    "multibox: {:>5.1}%  {done}/{files} files  {:.1}/{:.1} GiB  {:.0}s",
                    done as f64 * 100.0 / files.max(1) as f64,
                    gib(copied),
                    gib(bytes),
                    started.elapsed().as_secs_f32()
                );
            }
        });
        *self.worker.lock().unwrap() = Some(worker);
    }

    fn clone_finished(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.lock().unwrap().take() {
            let _ = worker.join();
        }
    }
}

fn environment_object(variables: &[(String, String)]) -> serde_json::Value {
    variables
        .iter()
        .map(|(key, value)| (key.clone(), serde_json::Value::from(value.as_str())))
        .collect::<serde_json::Map<_, _>>()
        .into()
}

fn take_value(args: &mut std::vec::IntoIter<String>, flag: &str) -> Result<String, String> {
    args.next().ok_or_else(|| format!("{flag} needs a value"))
}

fn parse_add(args: Vec<String>) -> Result<(AddOptions, bool), String> {
    const WITH_VALUE: [&str; 4] = ["-b", "-B", "--reason", "--orphan"];
    let mut options = AddOptions {
        git_args: Vec::new(),
        path: String::new(),
        from: None,
        mode: Mode::Auto,
        carry: true,
        dry_run: false,
        threads: None,
    };
    let mut positional = 0;
    let mut json = false;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--from" => options.from = Some(PathBuf::from(take_value(&mut args, "--from")?)),
            "--copy" => options.mode = Mode::Copy,
            "--reflink" => options.mode = Mode::Reflink,
            "--no-cache" => options.carry = false,
            "--dry-run" => options.dry_run = true,
            "--threads" => {
                let value = take_value(&mut args, "--threads")?;
                options.threads = Some(
                    value
                        .parse()
                        .map_err(|_| format!("bad --threads: {value}"))?,
                );
            }
            flag if WITH_VALUE.contains(&flag) => {
                let value = take_value(&mut args, flag)?;
                options.git_args.push(arg);
                options.git_args.push(value);
            }
            flag if flag.starts_with('-') && flag.len() > 1 => options.git_args.push(arg),
            _ => {
                if positional == 0 {
                    options.path = arg.clone();
                }
                positional += 1;
                options.git_args.push(arg);
            }
        }
    }
    if options.path.is_empty() {
        return Err("add needs a <path>".to_owned());
    }
    if json {
        // git prints its own summary on stdout, which must stay parseable.
        options.git_args.insert(0, "--quiet".to_owned());
    }
    Ok((options, json))
}

/// These forms have nothing to accelerate: they produce no checkout, or no
/// history to clone from.
fn is_native_add(args: &[String]) -> bool {
    args.iter()
        .any(|arg| arg == "--no-checkout" || arg == "--orphan" || arg.starts_with("--orphan="))
}

fn run_add(cwd: &Path, args: Vec<String>) -> Result<ExitCode, String> {
    if is_native_add(&args) {
        return passthrough(cwd, "add", args);
    }
    let (options, json) = parse_add(args)?;
    let terminal = Terminal::default();
    let started = Instant::now();
    let outcome = add(cwd, &options, &terminal).map_err(|err| err.to_string())?;
    for warning in &outcome.warnings {
        terminal.warn(warning);
    }
    let summary = outcome.summary;
    let method = if outcome.native {
        "git"
    } else if outcome.capability.reflink {
        "reflink"
    } else {
        "copy"
    };
    if json {
        let report = outcome.report.as_ref();
        let value = serde_json::json!({
            "path": outcome.path,
            "dryRun": options.dry_run,
            "method": method,
            "filesystem": outcome.capability.filesystem,
            "reusedIndex": outcome.reused_index,
            "tracked": { "files": summary.tracked_files, "bytes": summary.tracked_bytes },
            "cached": { "files": summary.cached_files, "bytes": summary.cached_bytes },
            "shared": { "files": summary.shared_files, "bytes": summary.shared_bytes },
            "reflinked": report.map(|report| report.reflinked),
            "copied": report.map(|report| report.copied),
            "hardLinked": report.map(|report| report.hard_linked),
            "failures": report.map_or(0, |report| report.failures.len()),
            "seconds": started.elapsed().as_secs_f64(),
            "warnings": outcome.warnings,
            "environment": environment_object(&outcome.environment),
        });
        println!("{value}");
        return Ok(ExitCode::SUCCESS);
    }
    if outcome.native {
        if !options.dry_run {
            println!("{}: plain git worktree", outcome.path.display());
        }
        return Ok(ExitCode::SUCCESS);
    }
    // Shared files are counted among the cached ones, not added to them.
    let sharing = if summary.shared_files > 0 {
        format!(
            ", {} of them ({:.1} GiB) shared with the source through hard links",
            summary.shared_files,
            gib(summary.shared_bytes)
        )
    } else {
        String::new()
    };
    if options.dry_run {
        println!(
            "would clone {} tracked files ({:.1} GiB) and {} cached files ({:.1} GiB) by {method} onto {}{sharing}",
            summary.tracked_files,
            gib(summary.tracked_bytes),
            summary.cached_files,
            gib(summary.cached_bytes),
            outcome.capability.filesystem,
        );
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "{}: {} files ({:.1} GiB) by {method} on {} in {:.1}s{sharing}",
        outcome.path.display(),
        summary.tracked_files + summary.cached_files,
        gib(summary.tracked_bytes + summary.cached_bytes),
        outcome.capability.filesystem,
        started.elapsed().as_secs_f32()
    );
    for (key, value) in &outcome.environment {
        terminal.note(&format!(
            "run tools in this worktree with {key}={value}, or use `git multibox env`"
        ));
    }
    // Across volumes the advice is where to put the worktree, given above.
    if !outcome.capability.reflink && outcome.capability.same_volume && options.mode == Mode::Auto {
        terminal.note(&format!(
            "copy mode used {:.1} GiB. `git multibox setup` enables reflinks: new worktrees then take seconds and almost no space.",
            gib(summary.tracked_bytes + summary.cached_bytes - summary.shared_bytes)
        ));
    }
    Ok(ExitCode::SUCCESS)
}

/// Starts a detached copy of this program that outlives the current command.
fn spawn_purge(trash: &Path) -> std::io::Result<()> {
    let mut command = Command::new(std::env::current_exe()?);
    command.arg("__purge").arg(trash);
    spawn_detached(command)
}

fn run_remove(cwd: &Path, args: Vec<String>) -> Result<ExitCode, String> {
    let mut options = RemoveOptions {
        path: String::new(),
        force: 0,
    };
    let (mut wait, mut json) = (false, false);
    for arg in args {
        match arg.as_str() {
            "-f" | "--force" => options.force += 1,
            "--wait" => wait = true,
            "--json" => json = true,
            flag if flag.starts_with('-') => return Err(format!("unknown option {flag}")),
            _ if options.path.is_empty() => options.path = arg,
            _ => return Err("remove takes one <worktree>".to_owned()),
        }
    }
    if options.path.is_empty() {
        return Err("remove needs a <worktree>".to_owned());
    }
    let outcome = remove(cwd, &options).map_err(|err| err.to_string())?;
    if json {
        let value = serde_json::json!({
            "path": outcome.path,
            "deferred": outcome.trash.is_some() && !wait,
        });
        println!("{value}");
    }
    purge_removed(cwd, outcome.trash, wait)?;
    Ok(ExitCode::SUCCESS)
}

/// Deletes what a removal moved aside, with any earlier leftovers: in the
/// foreground with `wait`, otherwise in a detached process.
fn purge_removed(cwd: &Path, trash: Option<PathBuf>, wait: bool) -> Result<(), String> {
    let mut leftovers = sweep_trash(cwd);
    if let Some(trash) = trash {
        leftovers.retain(|path| path != &trash);
        leftovers.push(trash);
    }
    for trash in leftovers {
        if wait {
            let report = purge_trash(&trash).map_err(|err| err.to_string())?;
            for failure in report.failures.iter().take(5) {
                eprintln!("multibox: warning: could not delete {failure}");
            }
        } else if let Err(err) = spawn_purge(&trash) {
            eprintln!(
                "multibox: warning: {} needs deleting by hand: {err}",
                trash.display()
            );
        }
    }
    Ok(())
}

fn run_finish(cwd: &Path, args: Vec<String>) -> Result<ExitCode, String> {
    let mut options = FinishOptions {
        path: String::new(),
        keep_branch: false,
        merge_args: Vec::new(),
    };
    let (mut wait, mut json) = (false, false);
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--keep-branch" => options.keep_branch = true,
            "--wait" => wait = true,
            "--json" => json = true,
            "--" => options.merge_args.extend(args.by_ref()),
            flag if flag.starts_with('-') => return Err(format!("unknown option {flag}")),
            _ if options.path.is_empty() => options.path = arg,
            _ => return Err("finish takes one <worktree>".to_owned()),
        }
    }
    if options.path.is_empty() {
        return Err("finish needs a <worktree>".to_owned());
    }
    if let Ok(repo) = git::Repo::discover(cwd) {
        let terminal = Terminal::default();
        for problem in Config::load(&repo.worktree).problems {
            terminal.warn(&format!("{CONFIG_FILE}: {problem}"));
        }
    }
    match finish(cwd, &options).map_err(|err| err.to_string())? {
        FinishOutcome::Conflicts {
            branch,
            into,
            files,
        } => {
            if json {
                let value = serde_json::json!({
                    "branch": branch,
                    "into": into,
                    "finished": false,
                    "conflicts": files,
                });
                println!("{value}");
            } else {
                eprintln!("multibox: merging '{branch}' into '{into}' stopped on conflicts:");
                for file in &files {
                    eprintln!("  {file}");
                }
                eprintln!(
                    "Resolve them and commit, then run `git multibox finish {}` again to clean up. `git merge --abort` undoes the merge.",
                    options.path
                );
            }
            Ok(ExitCode::from(1))
        }
        FinishOutcome::Finished {
            branch,
            into,
            merged_now,
            merge_output,
            removal,
            branch_deleted,
        } => {
            if json {
                let value = serde_json::json!({
                    "branch": branch,
                    "into": into,
                    "finished": true,
                    "merged_now": merged_now,
                    "removed": removal.path,
                    "branch_deleted": branch_deleted,
                });
                println!("{value}");
            } else {
                print!("{merge_output}");
                let how = if merged_now {
                    "merged"
                } else {
                    "was already merged"
                };
                let deleted = if branch_deleted {
                    ", branch deleted"
                } else {
                    ""
                };
                eprintln!(
                    "multibox: '{branch}' {how} into '{into}'; removed {}{deleted}",
                    removal.path.display()
                );
            }
            purge_removed(cwd, removal.trash, wait)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn level_name(level: Level) -> &'static str {
    match level {
        Level::Ok => "ok",
        Level::Info => "info",
        Level::Warn => "warn",
    }
}

fn run_doctor(cwd: &Path, args: &[String]) -> Result<ExitCode, String> {
    let report = diagnose(cwd);
    if args.iter().any(|arg| arg == "--json") {
        let checks: Vec<_> = report
            .checks
            .iter()
            .map(|check| {
                serde_json::json!({
                    "name": check.name,
                    "level": level_name(check.level),
                    "detail": check.detail,
                })
            })
            .collect();
        let volumes: Vec<_> = report
            .volumes
            .iter()
            .map(|volume| {
                serde_json::json!({
                    "root": volume.root,
                    "filesystem": volume.filesystem,
                    "reflink": volume.reflink,
                    "fixed": volume.fixed,
                    "freeBytes": volume.free,
                    "totalBytes": volume.total,
                })
            })
            .collect();
        let value = serde_json::json!({
            "reflink": report.capability.as_ref().map(|capability| capability.reflink),
            "devDriveSupported": report.dev_drive_supported,
            "checks": checks,
            "volumes": volumes,
        });
        println!("{value}");
        return Ok(ExitCode::SUCCESS);
    }
    for check in &report.checks {
        println!(
            "{:<4}  {:<11} {}",
            level_name(check.level),
            check.name,
            check.detail
        );
    }
    if !report.volumes.is_empty() {
        println!();
        for volume in &report.volumes {
            println!(
                "      {:<4} {:<6} {:>7.0} GiB free of {:>7.0} GiB{}",
                volume.root,
                volume.filesystem,
                gib(volume.free),
                gib(volume.total),
                if volume.reflink { "  reflinks" } else { "" }
            );
        }
    }
    let reflink = report.capability.as_ref().is_some_and(|c| c.reflink);
    if !reflink && report.dev_drive_supported == Some(true) {
        println!(
            "\nNext: `git multibox setup` creates a Dev Drive; `git multibox migrate` then moves this project onto it."
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn run_env(cwd: &Path, args: &[String]) -> Result<ExitCode, String> {
    let variables = adapter::worktree_environment(cwd).map_err(|err| err.to_string())?;
    if args.iter().any(|arg| arg == "--json") {
        println!("{}", environment_object(&variables));
    } else {
        for (key, value) in &variables {
            println!("export {key}='{}'", value.replace('\'', "'\\''"));
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn run_setup_command(args: Vec<String>) -> Result<ExitCode, String> {
    let mut options = SetupOptions::default();
    let mut dry_run = false;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--vhdx" => options.vhdx = Some(PathBuf::from(take_value(&mut args, "--vhdx")?)),
            "--size" => {
                let value = take_value(&mut args, "--size")?;
                options.size_gib = Some(value.parse().map_err(|_| format!("bad --size: {value}"))?);
            }
            "--letter" => {
                let value = take_value(&mut args, "--letter")?;
                let letter = value
                    .chars()
                    .next()
                    .filter(char::is_ascii_alphabetic)
                    .ok_or_else(|| format!("bad --letter: {value}"))?;
                options.mount = Some(Mount::Letter(letter.to_ascii_uppercase()));
            }
            "--folder" => {
                options.mount = Some(Mount::Folder(PathBuf::from(take_value(
                    &mut args, "--folder",
                )?)));
            }
            "--dry-run" => dry_run = true,
            other => return Err(format!("unknown option {other}")),
        }
    }
    let plan = plan_setup(&options).map_err(|err| err.to_string())?;
    let mount = match &plan.mount {
        Mount::Letter(letter) => format!("{letter}:"),
        Mount::Folder(path) => path.display().to_string(),
    };
    println!(
        "Multibox will create a Dev Drive of {} GiB:\n  file   {} (fills up to that size as it is used and does not shrink; no existing volume is touched)\n  mount  {mount}",
        plan.size_gib,
        plan.vhdx.display()
    );
    if dry_run {
        return Ok(ExitCode::SUCCESS);
    }
    println!("Windows may ask for administrator approval.");
    run_setup(&plan).map_err(|err| err.to_string())?;
    println!("Dev Drive ready at {mount}. Move a project there with `git multibox init`.");
    Ok(ExitCode::SUCCESS)
}

fn run_init(cwd: &Path, args: Vec<String>) -> Result<ExitCode, String> {
    let mut options = InitOptions::default();
    let mut project = cwd.to_path_buf();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--project" => {
                project = real_directory(cwd.join(take_value(&mut args, "--project")?));
            }
            "--to" => options.destination = Some(cwd.join(take_value(&mut args, "--to")?)),
            "--exclude" => options.exclude.push(take_value(&mut args, "--exclude")?),
            "--dry-run" => options.dry_run = true,
            other => return Err(format!("unknown option {other}")),
        }
    }
    // The final step renames the project folder, which Windows refuses while any
    // process has it as its working directory. That includes this one.
    if let Some(parent) = project.parent() {
        let _ = std::env::set_current_dir(parent);
    }
    let outcome = init(&project, &options, &Terminal::default()).map_err(|err| err.to_string())?;
    match outcome {
        InitOutcome::Ready { filesystem } => {
            println!("Ready: {filesystem} has reflinks, so new worktrees share data.");
        }
        InitOutcome::CopyModeOnly { filesystem } => {
            println!("{filesystem} has no reflinks; worktrees work, as full copies.");
        }
        InitOutcome::Planned {
            destination,
            creates_area,
            files,
            bytes,
        } => {
            if creates_area {
                println!("Would create a Dev Drive.");
            }
            println!(
                "Would copy {files} files ({:.1} GiB) to {}, then redirect this project there.",
                gib(bytes),
                destination.display()
            );
        }
        InitOutcome::WaitingForEditor {
            destination,
            reason,
        } => {
            println!(
                "Copied to {}, but {reason}.\nSave your work and close it, then finish from outside the project folder, so nothing holds it open:\n  git multibox init --project \"{}\"\nChanges made until then are brought across.",
                destination.display(),
                project.display()
            );
        }
        InitOutcome::Switched {
            destination,
            kept_at,
            updated,
        } => {
            println!(
                "Done: the project now lives in {} and the old path still works ({updated} late changes brought across).\nThe original is kept at {}; delete it yourself once you are happy.",
                destination.display(),
                kept_at.display()
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn run_migrate(cwd: &Path, args: Vec<String>) -> Result<ExitCode, String> {
    let mut destination = None;
    let mut exclude = Vec::new();
    let (mut dry_run, mut do_switch) = (false, false);
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dry-run" => dry_run = true,
            "--switch" => do_switch = true,
            "--exclude" => exclude.push(take_value(&mut args, "--exclude")?),
            flag if flag.starts_with('-') => return Err(format!("unknown option {flag}")),
            _ => destination = Some(PathBuf::from(arg)),
        }
    }
    let destination = cwd.join(destination.ok_or("migrate needs a <destination>")?);
    if do_switch {
        let outcome = switch(cwd, &destination, &exclude).map_err(|err| err.to_string())?;
        println!(
            "Switched. The original is kept at {}; delete it yourself once you are happy.",
            outcome.kept_at.display()
        );
        return Ok(ExitCode::SUCCESS);
    }
    let outcome = migrate(
        cwd,
        &MigrateOptions {
            destination,
            exclude,
            dry_run,
            threads: None,
        },
        &Terminal::default(),
    )
    .map_err(|err| err.to_string())?;
    if dry_run {
        println!(
            "would copy {} files ({:.1} GiB) to {}",
            outcome.files,
            gib(outcome.bytes),
            outcome.destination.display()
        );
    } else {
        println!(
            "Copied {} files ({:.1} GiB) to {}. The source was not changed.\nTry it out, then run `git multibox migrate --switch <destination>`.",
            outcome.files,
            gib(outcome.bytes),
            outcome.destination.display()
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn passthrough(cwd: &Path, subcommand: &str, args: Vec<String>) -> Result<ExitCode, String> {
    let mut full = vec!["worktree".to_owned(), subcommand.to_owned()];
    full.extend(args);
    let code = git::passthrough(cwd, &full).map_err(|err| err.to_string())?;
    Ok(ExitCode::from(code.clamp(0, 255) as u8))
}

/// Resolves links in the working directory. After `init` the old project path is
/// a link into the Dev Drive, and `../feature` has to land next to the real
/// project: reflinks only work within one volume.
fn real_directory(cwd: PathBuf) -> PathBuf {
    let Ok(real) = std::fs::canonicalize(&cwd) else {
        return cwd;
    };
    match real.to_str().and_then(|text| text.strip_prefix(r"\\?\")) {
        // git cannot run from a verbatim path. UNC shares keep their original form.
        Some(plain) if !plain.starts_with("UNC") => PathBuf::from(plain),
        Some(_) => cwd,
        None => real,
    }
}

/// Commands this program implements itself; git answers `-h` for the rest.
const OWN_COMMANDS: [&str; 8] = [
    "add", "remove", "finish", "doctor", "env", "setup", "init", "migrate",
];

/// `-h` or `--help` among a command's own options, before any `--`.
fn asks_for_help(args: &[String]) -> bool {
    args.iter()
        .take_while(|arg| *arg != "--")
        .any(|arg| arg == "-h" || arg == "--help")
}

fn run() -> Result<ExitCode, String> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprint!("{USAGE}");
        return Ok(ExitCode::from(129));
    }
    let subcommand = args.remove(0);
    if OWN_COMMANDS.contains(&subcommand.as_str()) && asks_for_help(&args) {
        print!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    let cwd = real_directory(std::env::current_dir().map_err(|err| err.to_string())?);
    match subcommand.as_str() {
        "add" => run_add(&cwd, args),
        "remove" => run_remove(&cwd, args),
        "finish" => run_finish(&cwd, args),
        "doctor" => run_doctor(&cwd, &args),
        "env" => run_env(&cwd, &args),
        "setup" => run_setup_command(args),
        "init" => run_init(&cwd, args),
        "migrate" => run_migrate(&cwd, args),
        "__purge" => {
            let trash = args.first().ok_or("__purge needs a path")?;
            purge_trash(Path::new(trash)).map_err(|err| err.to_string())?;
            Ok(ExitCode::SUCCESS)
        }
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            Ok(ExitCode::SUCCESS)
        }
        "-V" | "--version" => {
            println!("git-multibox {}", env!("CARGO_PKG_VERSION"));
            Ok(ExitCode::SUCCESS)
        }
        other => passthrough(&cwd, other, args),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(message) => {
            eprintln!("fatal: {message}");
            ExitCode::from(128)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::asks_for_help;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn help_is_found_among_options_but_not_after_the_separator() {
        assert!(asks_for_help(&args(&["--help"])));
        assert!(asks_for_help(&args(&["../wt", "-h"])));
        assert!(!asks_for_help(&args(&["../wt", "-b", "task"])));
        assert!(!asks_for_help(&args(&["../wt", "--", "-h"])));
    }
}
