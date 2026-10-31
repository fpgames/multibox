//! Measures the system copy call: `copyfilebench <src-dir> <dst-dir> <threads>`.
//! On recent Windows builds CopyFile block-clones on ReFS by itself.

use multibox_clone::{Decision, ScanRequest, scan};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (src, dst) = (Path::new(&args[1]), Path::new(&args[2]));
    let threads: usize = args[3].parse().expect("threads");
    let everything = |_: &str, _: bool| Decision::Include;
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
    fs::create_dir_all(dst).unwrap();
    let mut dirs: Vec<&str> = manifest.dirs.iter().map(|dir| dir.rel.as_str()).collect();
    dirs.sort_by_key(|rel| rel.len());
    for rel in dirs {
        fs::create_dir_all(dst.join(rel)).unwrap();
    }
    let started = Instant::now();
    let cursor = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let at = cursor.fetch_add(1, Ordering::Relaxed);
                    let Some(file) = manifest.files.get(at) else {
                        return;
                    };
                    fs::copy(src.join(&file.rel), dst.join(&file.rel)).unwrap();
                }
            });
        }
    });
    let seconds = started.elapsed().as_secs_f64();
    println!(
        "{} files in {seconds:.1}s, {:.0} files/s, threads {threads}",
        manifest.files.len(),
        manifest.files.len() as f64 / seconds
    );
}
