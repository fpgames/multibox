//! `git multibox`: the command-line front end.
//!
//! Arguments mirror `git worktree`. `add` and `remove` are accelerated, and
//! every other subcommand is handed to git unchanged.

use multibox_core::{
    AddOptions, Level, MigrateOptions, Mode, Mount, Observer, Progress, RemoveOptions,
    SetupOptions, add, diagnose, git, migrate, plan_setup, purge_trash, remove, run_setup,
    sweep_trash, switch,
};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const USAGE: &str = "\
usage: git multibox add [<options>] <path> [<commit-ish>]
       git multibox remove [-f] <worktree>
       git multibox <list|lock|move|prune|repair|unlock> ...
       git multibox doctor
       git multibox setup [--vhdx <file>] [--size <GiB>] [--letter <X> | --folder <dir>] [--dry-run]
       git multibox migrate <destination> [--dry-run]
       git multibox migrate --switch <destination>

Multibox-specific options for add:
    --from <worktree>   clone from this worktree instead of the current one
    --copy              always copy, even where reflinks are available
    --reflink           fail unless reflinks are available
    --no-cache          leave ignored caches behind
    --threads <n>       number of worker threads
    --dry-run           show what would be cloned and stop

Options for remove:
    --wait              delete in the foreground instead of in the background

Every option of `git worktree add` is accepted and passed through.
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

fn take_value(args: &mut std::vec::IntoIter<String>, flag: &str) -> Result<String, String> {
    args.next().ok_or_else(|| format!("{flag} needs a value"))
}

fn parse_add(args: Vec<String>) -> Result<AddOptions, String> {
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
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--from" => options.from = Some(PathBuf::from(take_value(&mut args, "--from")?)),
            "--copy" => options.mode = Mode::Copy,
            "--reflink" => options.mode = Mode::Reflink,
            "--no-cache" => options.carry = false,
            "--dry-run" => options.dry_run = true,
            "--threads" => {
                let value = take_value(&mut args, "--threads")?;
                options.threads =
                    Some(value.parse().map_err(|_| format!("bad --threads: {value}"))?);
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
    Ok(options)
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
    let options = parse_add(args)?;
    let terminal = Terminal::default();
    let started = Instant::now();
    let outcome = add(cwd, &options, &terminal).map_err(|err| err.to_string())?;
    for warning in &outcome.warnings {
        terminal.warn(warning);
    }
    let summary = outcome.summary;
    let method = if outcome.capability.reflink {
        "reflink"
    } else {
        "copy"
    };
    if options.dry_run {
        println!(
            "would clone {} tracked files ({:.1} GiB) and {} cached files ({:.1} GiB) by {method} onto {}",
            summary.tracked_files,
            gib(summary.tracked_bytes),
            summary.cached_files,
            gib(summary.cached_bytes),
            outcome.capability.filesystem,
        );
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "{}: {} files ({:.1} GiB) by {method} on {} in {:.1}s",
        outcome.path.display(),
        summary.tracked_files + summary.cached_files,
        gib(summary.tracked_bytes + summary.cached_bytes),
        outcome.capability.filesystem,
        started.elapsed().as_secs_f32()
    );
    if !outcome.capability.reflink && options.mode == Mode::Auto {
        terminal.note(&format!(
            "copy mode used {:.1} GiB. `git multibox setup` enables reflinks: new worktrees then take seconds and almost no space.",
            gib(summary.tracked_bytes + summary.cached_bytes)
        ));
    }
    Ok(ExitCode::SUCCESS)
}

/// Starts a detached copy of this program that outlives the current command.
fn spawn_purge(trash: &Path) -> std::io::Result<()> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("__purge")
        .arg(trash)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    command.spawn().map(|_| ())
}

fn run_remove(cwd: &Path, args: Vec<String>) -> Result<ExitCode, String> {
    let mut options = RemoveOptions {
        path: String::new(),
        force: 0,
    };
    let mut wait = false;
    for arg in args {
        match arg.as_str() {
            "-f" | "--force" => options.force += 1,
            "--wait" => wait = true,
            flag if flag.starts_with('-') => return Err(format!("unknown option {flag}")),
            _ if options.path.is_empty() => options.path = arg,
            _ => return Err("remove takes one <worktree>".to_owned()),
        }
    }
    if options.path.is_empty() {
        return Err("remove needs a <worktree>".to_owned());
    }
    let outcome = remove(cwd, &options).map_err(|err| err.to_string())?;
    let mut leftovers = sweep_trash(cwd);
    if let Some(trash) = outcome.trash {
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
    Ok(ExitCode::SUCCESS)
}

fn run_doctor(cwd: &Path) -> Result<ExitCode, String> {
    let report = diagnose(cwd);
    for check in &report.checks {
        let mark = match check.level {
            Level::Ok => "ok  ",
            Level::Info => "info",
            Level::Warn => "warn",
        };
        println!("{mark}  {:<11} {}", check.name, check.detail);
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
        println!("\nNext: `git multibox setup` creates a Dev Drive; `git multibox migrate` then moves this project onto it.");
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
        "Multibox will create a Dev Drive of up to {} GiB:\n  file   {} (grows on demand; no existing volume is touched)\n  mount  {mount}",
        plan.size_gib,
        plan.vhdx.display()
    );
    if dry_run {
        return Ok(ExitCode::SUCCESS);
    }
    println!("Windows will ask for administrator approval.");
    run_setup(&plan).map_err(|err| err.to_string())?;
    println!("Dev Drive ready at {mount}. Move a project there with `git multibox migrate`.");
    Ok(ExitCode::SUCCESS)
}

fn run_migrate(cwd: &Path, args: Vec<String>) -> Result<ExitCode, String> {
    let mut destination = None;
    let (mut dry_run, mut do_switch) = (false, false);
    for arg in args {
        match arg.as_str() {
            "--dry-run" => dry_run = true,
            "--switch" => do_switch = true,
            flag if flag.starts_with('-') => return Err(format!("unknown option {flag}")),
            _ => destination = Some(PathBuf::from(arg)),
        }
    }
    let destination = cwd.join(destination.ok_or("migrate needs a <destination>")?);
    if do_switch {
        let outcome = switch(cwd, &destination).map_err(|err| err.to_string())?;
        println!(
            "Switched. The original is kept at {}; delete it yourself once you are happy.",
            outcome.kept_at.display()
        );
        return Ok(ExitCode::SUCCESS);
    }
    let terminal = Terminal::default();
    let progress = Arc::new(Progress::default());
    if !dry_run {
        terminal.clone_started(0, 0, progress.clone());
    }
    let outcome = migrate(
        cwd,
        &MigrateOptions {
            destination,
            dry_run,
            threads: None,
        },
        Some(progress),
    );
    terminal.clone_finished();
    let outcome = outcome.map_err(|err| err.to_string())?;
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

fn run() -> Result<ExitCode, String> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprint!("{USAGE}");
        return Ok(ExitCode::from(129));
    }
    let subcommand = args.remove(0);
    let cwd = std::env::current_dir().map_err(|err| err.to_string())?;
    match subcommand.as_str() {
        "add" => run_add(&cwd, args),
        "remove" => run_remove(&cwd, args),
        "doctor" => run_doctor(&cwd),
        "setup" => run_setup_command(args),
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
