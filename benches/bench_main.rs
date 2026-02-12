//! rush ベンチマーク: パーサー、ビルトイン、spawn、フルパイプライン、glob の計測。
//!
//! `std::time::Instant` による手動計測（外部クレート不要）。
//!
//! 実行: `cargo bench`

use std::time::{Duration, Instant};

// ── ベンチマークインフラ ──────────────────────────────────────────

struct BenchResult {
    category: &'static str,
    name: &'static str,
    avg: Duration,
    iters: u64,
}

impl BenchResult {
    fn print(&self) {
        let avg_us = self.avg.as_nanos() as f64 / 1000.0;
        println!(
            "[{:<8}] {:<40}: avg {:>10.2}µs  ({} iters)",
            self.category, self.name, avg_us, self.iters,
        );
    }
}

fn bench<F: FnMut()>(category: &'static str, name: &'static str, iters: u64, mut f: F) -> BenchResult {
    // ウォームアップ
    for _ in 0..iters.min(100) {
        f();
    }

    let start = Instant::now();
    for _ in 0..iters {
        f();
    }
    let elapsed = start.elapsed();

    BenchResult {
        category,
        name,
        avg: elapsed / iters as u32,
        iters,
    }
}

// ── メイン ────────────────────────────────────────────────────────

fn main() {
    println!("rush benchmark suite");
    println!("{}", "=".repeat(80));

    let mut results = Vec::new();

    // ── パーサーベンチマーク ──
    println!("\n--- Parser ---");

    results.push(bench("parser", "echo hello", 10_000, || {
        let _ = rush::parser::parse("echo hello", 0);
    }));

    results.push(bench(
        "parser",
        "echo \"hello $HOME world\"",
        10_000,
        || {
            let _ = rush::parser::parse("echo \"hello $HOME world\"", 0);
        },
    ));

    results.push(bench("parser", "ls | grep Cargo | head -1", 10_000, || {
        let _ = rush::parser::parse("ls | grep Cargo | head -1", 0);
    }));

    results.push(bench(
        "parser",
        "cat < /dev/null > /dev/null 2> /dev/null",
        10_000,
        || {
            let _ = rush::parser::parse("cat < /dev/null > /dev/null 2> /dev/null", 0);
        },
    ));

    results.push(bench("parser", "sleep 1 &", 10_000, || {
        let _ = rush::parser::parse("sleep 1 &", 0);
    }));

    results.push(bench("parser", "echo hello && echo world", 10_000, || {
        let _ = rush::parser::parse("echo hello && echo world", 0);
    }));

    results.push(bench("parser", "a || b ; c && d", 10_000, || {
        let _ = rush::parser::parse("a || b ; c && d", 0);
    }));

    for r in &results {
        r.print();
    }
    results.clear();

    // ── ビルトインベンチマーク ──
    println!("\n--- Builtins ---");

    let mut shell = rush::shell::Shell::new();

    results.push(bench("builtin", "echo hello", 10_000, || {
        let mut buf = Vec::new();
        rush::builtins::try_exec(&mut shell, &["echo", "hello"], &mut buf);
    }));

    results.push(bench("builtin", "pwd", 10_000, || {
        let mut buf = Vec::new();
        rush::builtins::try_exec(&mut shell, &["pwd"], &mut buf);
    }));

    for r in &results {
        r.print();
    }
    results.clear();

    // ── spawn ベンチマーク ──
    println!("\n--- Spawn (posix_spawnp) ---");

    results.push(bench("spawn", "/bin/true (posix_spawnp)", 1_000, || {
        match rush::spawn::spawn(&["/bin/true"], 0, None, None, None, &[], &[]) {
            Ok(pid) => {
                let mut status = 0i32;
                unsafe { libc::waitpid(pid, &mut status, 0); }
            }
            Err(_) => {}
        }
    }));

    for r in &results {
        r.print();
    }
    results.clear();

    // ── フルパイプライン (parse → execute) ──
    println!("\n--- Full pipeline (parse + spawn + wait) ---");

    results.push(bench("full", "/bin/echo hello > /dev/null", 1_000, || {
        if let Ok(Some(list)) = rush::parser::parse("/bin/echo hello > /dev/null", 0) {
            rush::executor::execute(&mut shell, &list, "/bin/echo hello > /dev/null");
        }
    }));

    for r in &results {
        r.print();
    }

    // ── glob ベンチマーク ──
    println!("\n--- Glob ---");

    results.clear();

    results.push(bench("glob", "expand *.rs", 1_000, || {
        let _ = rush::glob::expand("*.rs");
    }));

    for r in &results {
        r.print();
    }

    // ── チルダ展開ベンチマーク ──
    println!("\n--- Tilde expansion ---");

    results.clear();

    results.push(bench("tilde", "expand_tilde(\"~\")", 10_000, || {
        let _ = rush::parser::expand_tilde("~");
    }));

    results.push(bench("tilde", "expand_tilde(\"~/Documents\")", 10_000, || {
        let _ = rush::parser::expand_tilde("~/Documents");
    }));

    results.push(bench("tilde", "expand_tilde(\"hello\") (no-op)", 10_000, || {
        let _ = rush::parser::expand_tilde("hello");
    }));

    for r in &results {
        r.print();
    }

    println!("\n{}", "=".repeat(80));
    println!("done.");
}
