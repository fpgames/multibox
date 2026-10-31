//! Measures raw clone throughput: `clonebench <src-dir> <dst-dir> <threads> [copy]`.
//! The destination must not exist. Useful for comparing filesystems and thread
//! counts without the rest of the worktree pipeline.

use multibox_clone::{Decision, Options, ScanRequest, Strategy, execute, scan};
use std::path::Path;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (src, dst) = (Path::new(&args[1]), Path::new(&args[2]));
    let threads: usize = args[3].parse().expect("threads");
    let strategy = match args.get(4).map(String::as_str) {
        Some("copy") => Strategy::Copy,
        _ => Strategy::Auto,
    };
    let everything = |_: &str, _: bool| Decision::Include;
    let started = Instant::now();
    let manifest = scan(
        src,
        &ScanRequest {
            files: Vec::new(),
            links: Vec::new(),
            trees: vec![String::new()],
            filter: &everything,
            threads: Some(threads),
        },
    )
    .expect("scan");
    let scanned = started.elapsed().as_secs_f64();
    std::fs::create_dir_all(dst).expect("create destination");
    let report = execute(
        &manifest,
        src,
        dst,
        &Options {
            strategy,
            threads: Some(threads),
            progress: None,
        },
    )
    .expect("execute");
    let seconds = report.elapsed.as_secs_f64();
    println!(
        "{} files {:.2} GiB scan {scanned:.1}s clone {seconds:.1}s {:.0} files/s reflinked {} copied {} failures {} threads {threads}",
        report.files,
        report.bytes as f64 / (1u64 << 30) as f64,
        report.files as f64 / seconds,
        report.reflinked,
        report.copied,
        report.failures.len(),
    );
}
