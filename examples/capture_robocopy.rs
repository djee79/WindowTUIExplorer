//! Captures real robocopy output for the parser's fixtures (CLAUDE.md, M4):
//! `cargo run --example capture_robocopy [-- <scenario>...]`
//!
//! Each scenario builds a small tree under `target\robocopy-capture`, runs robocopy with
//! sentier's arguments, and writes `tests\fixtures\robocopy\<name>.out` (stdout, raw bytes)
//! and `<name>.exit` (the exit code). The capture folder's path is replaced by
//! `C:\capture` so no user or machine names end up in the repo.
//!
//! Scenarios: success, nothing, mirror_extra, list, mt, locked, denied, missing_source,
//! unreachable (takes ~30 s: a non-routable host).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

const BASE: &[&str] = &["/Z", "/R:1", "/W:1", "/BYTES", "/FP", "/TEE"];

fn main() {
    let only: Vec<String> = std::env::args().skip(1).collect();
    let root = std::env::current_dir()
        .expect("cwd")
        .join("target")
        .join("robocopy-capture");
    let out_dir = PathBuf::from("tests").join("fixtures").join("robocopy");
    fs::create_dir_all(&out_dir).expect("fixture dir");
    let want = |n: &str| only.is_empty() || only.iter().any(|o| o == n);

    if want("success") {
        let (src, dst) = fresh(&root, "success");
        tree(&src);
        run("success", &root, &out_dir, &src, &dst, &["/E"], None);
        // The same again: nothing to copy (exit 0).
        if want("nothing") || only.is_empty() {
            run("nothing", &root, &out_dir, &src, &dst, &["/E"], None);
        }
    }
    if want("mirror_extra") {
        let (src, dst) = fresh(&root, "mirror_extra");
        tree(&src);
        fs::create_dir_all(dst.join("old")).expect("dst");
        fs::write(dst.join("old").join("stale.txt"), "stale").expect("stale");
        fs::write(dst.join("extra.txt"), "extra").expect("extra");
        run("mirror_extra", &root, &out_dir, &src, &dst, &["/MIR"], None);
    }
    if want("list") {
        let (src, dst) = fresh(&root, "list");
        tree(&src);
        fs::create_dir_all(&dst).expect("dst");
        fs::write(dst.join("extra.txt"), "extra").expect("extra");
        fs::write(dst.join("a.txt"), "older and different").expect("a");
        run("list", &root, &out_dir, &src, &dst, &["/MIR", "/L"], None);
    }
    if want("mt") {
        let (src, dst) = fresh(&root, "mt");
        tree(&src);
        run("mt", &root, &out_dir, &src, &dst, &["/E", "/MT:4"], None);
    }
    if want("locked") {
        let (src, dst) = fresh(&root, "locked");
        tree(&src);
        // Hold one file open with no sharing: ERROR 32 on every attempt.
        let held = hold(&src.join("sub").join("b.txt"));
        run("locked", &root, &out_dir, &src, &dst, &["/E"], None);
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
            run("denied", &root, &out_dir, &src, &dst, &["/E"], None);
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
            &["/E"],
            None,
        );
    }
    if want("unreachable") {
        let (src, _) = fresh(&root, "unreachable");
        tree(&src);
        let dst = PathBuf::from(r"\\10.255.255.1\x\dest");
        run(
            "unreachable",
            &root,
            &out_dir,
            &src,
            &dst,
            &["/E", "/R:0", "/W:0"],
            None,
        );
    }
}

fn fresh(root: &Path, name: &str) -> (PathBuf, PathBuf) {
    let dir = root.join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("scenario dir");
    (dir.join("src"), dir.join("dst"))
}

/// Two small files, one in a subfolder, and a 40 MB one (so % progress shows).
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

fn run(
    name: &str,
    root: &Path,
    out_dir: &Path,
    src: &Path,
    dst: &Path,
    extra: &[&str],
    _note: Option<&str>,
) {
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
