//! Captures real robocopy output for the parser's fixtures (CLAUDE.md, M4):
//! `cargo run --example capture_robocopy [-- <scenario>...]`
//!
//! Each scenario builds a small tree under `target\robocopy-capture`, runs robocopy with
//! sentier's arguments (`/J /R:0 /W:0 /BYTES /FP /NP /TEE`, plus `/MT:8` for copies, as
//! `jobs::robocopy::args` builds them), and writes `tests\fixtures\robocopy\<name>.out`
//! (stdout, raw bytes) and `<name>.exit` (the exit code). The capture folder's path is
//! replaced by `C:\capture` so no user or machine names end up in the repo.
//!
//! Scenarios: success, nothing, many, restartable, mirror_extra, list, locked, denied,
//! missing_source, unreachable (takes ~30 s: a non-routable host).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

/// What every sentier run passes (`/Z` only with `restartable = true`).
const BASE: &[&str] = &["/J", "/R:0", "/W:0", "/BYTES", "/FP", "/NP", "/TEE"];
/// Copies run multi-threaded (`jobs.robocopy_threads`, default 8); `/L` dry runs don't.
const MT: &str = "/MT:8";

fn main() {
    let only: Vec<String> = std::env::args().skip(1).collect();
    let root = std::env::current_dir()
        .expect("cwd")
        .join("target")
        .join("robocopy-capture");
    let out_dir = PathBuf::from("tests").join("fixtures").join("robocopy");
    fs::create_dir_all(&out_dir).expect("fixture dir");
    let want = |n: &str| only.is_empty() || only.iter().any(|o| o == n);

    if want("success") || want("nothing") {
        let (src, dst) = fresh(&root, "success");
        tree(&src);
        run("success", &root, &out_dir, &src, &dst, &["/E", MT]);
        // The same again: nothing to copy (exit 0).
        run("nothing", &root, &out_dir, &src, &dst, &["/E", MT]);
    }
    if want("many") {
        // Enough small files for the threads to interleave.
        let (src, dst) = fresh(&root, "many");
        for d in 0..6 {
            let dir = src.join(format!("d{d}"));
            fs::create_dir_all(&dir).expect("many");
            for f in 0..50 {
                fs::write(dir.join(format!("f{f:03}.txt")), "x".repeat(f + 1)).expect("f");
            }
        }
        run("many", &root, &out_dir, &src, &dst, &["/E", MT]);
    }
    if want("restartable") {
        let (src, dst) = fresh(&root, "restartable");
        tree(&src);
        run(
            "restartable",
            &root,
            &out_dir,
            &src,
            &dst,
            &["/E", "/Z", MT],
        );
    }
    if want("mirror_extra") {
        let (src, dst) = fresh(&root, "mirror_extra");
        tree(&src);
        fs::create_dir_all(dst.join("old")).expect("dst");
        fs::write(dst.join("old").join("stale.txt"), "stale").expect("stale");
        fs::write(dst.join("extra.txt"), "extra").expect("extra");
        run("mirror_extra", &root, &out_dir, &src, &dst, &["/MIR", MT]);
    }
    if want("list") {
        let (src, dst) = fresh(&root, "list");
        tree(&src);
        fs::create_dir_all(&dst).expect("dst");
        fs::write(dst.join("extra.txt"), "extra").expect("extra");
        fs::write(dst.join("a.txt"), "older and different").expect("a");
        run("list", &root, &out_dir, &src, &dst, &["/MIR", "/L"]);
    }
    if want("locked") {
        let (src, dst) = fresh(&root, "locked");
        tree(&src);
        // Hold one file open with no sharing: ERROR 32.
        let held = hold(&src.join("sub").join("b.txt"));
        run("locked", &root, &out_dir, &src, &dst, &["/E", MT]);
        drop(held);
    }
    if want("denied") {
        let (src, dst) = fresh(&root, "denied");
        tree(&src);
        let f = src.join("a.txt");
        let user = std::env::var("USERNAME").unwrap_or_default();
        let deny = format!("{user}:(R)");
        let ok = Command::new("icacls")
            .arg(&f)
            .args(["/deny", &deny])
            .status()
            .is_ok_and(|s| s.success());
        if ok {
            run("denied", &root, &out_dir, &src, &dst, &["/E", MT]);
            let _ = Command::new("icacls")
                .arg(&f)
                .args(["/remove:d", &user])
                .status();
        } else {
            eprintln!("denied: icacls failed, skipped");
        }
    }
    if want("missing_source") {
        let (src, dst) = fresh(&root, "missing_source");
        run(
            "missing_source",
            &root,
            &out_dir,
            &src.join("nope"),
            &dst,
            &["/E", MT],
        );
    }
    if want("unreachable") {
        let (src, _) = fresh(&root, "unreachable");
        tree(&src);
        let dst = PathBuf::from(r"\\10.255.255.1\x\dest");
        run("unreachable", &root, &out_dir, &src, &dst, &["/E", MT]);
    }
}

fn fresh(root: &Path, name: &str) -> (PathBuf, PathBuf) {
    let dir = root.join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("scenario dir");
    (dir.join("src"), dir.join("dst"))
}

/// Two small files, one in a subfolder, and a 40 MB one.
fn tree(src: &Path) {
    fs::create_dir_all(src.join("sub").join("deep")).expect("tree");
    fs::write(src.join("a.txt"), "hello\n").expect("a");
    fs::write(src.join("sub").join("b.txt"), "b\n").expect("b");
    fs::write(src.join("sub").join("deep").join("c.pmlmac"), "c\n").expect("c");
    let mut big = fs::File::create(src.join("sub").join("big.bin")).expect("big");
    let chunk: Vec<u8> = (0..1024 * 1024).map(|i| (i * 7 % 251) as u8).collect();
    for _ in 0..40 {
        big.write_all(&chunk).expect("write");
    }
}

fn hold(path: &Path) -> fs::File {
    use std::os::windows::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(path)
        .expect("hold")
}

fn run(name: &str, root: &Path, out_dir: &Path, src: &Path, dst: &Path, extra: &[&str]) {
    let log = root.join(format!("{name}.log"));
    let started = std::time::Instant::now();
    let out = Command::new("robocopy")
        .arg(src)
        .arg(dst)
        .args(extra)
        .args(BASE)
        .arg(format!("/UNILOG:{}", log.display()))
        .output()
        .expect("robocopy");
    let code = out.status.code().unwrap_or(-1);
    let scrub = |bytes: &[u8]| -> Vec<u8> {
        let text = String::from_utf8_lossy(bytes);
        let r = root.display().to_string();
        text.replace(&r, r"C:\capture").into_bytes()
    };
    fs::write(out_dir.join(format!("{name}.out")), scrub(&out.stdout)).expect("out");
    fs::write(out_dir.join(format!("{name}.exit")), format!("{code}\n")).expect("exit");
    println!(
        "{name:<16} exit {code:>2}  {:>6} bytes  {:.1} s",
        out.stdout.len(),
        started.elapsed().as_secs_f32()
    );
}
