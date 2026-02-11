//! rush — 速度重視のRust製シェル
//!
//! REPLループ: プロンプト表示 → 入力読み取り → コマンド実行 → ループ
//!
//! 現在のPhase 1実装:
//! - `split_whitespace()` による簡易パース（Phase 2で本格パーサーに置換）
//! - `std::process::Command` による外部コマンド実行（Phase 6で posix_spawn に置換）
//! - SIGINT無視による最小限のシグナル対応（Phase 4で正式実装）

mod builtins;
mod executor;
mod shell;

use std::io::{self, BufRead, Write};

use shell::Shell;

fn main() {
    // SIGINTを無視し、Ctrl+Cでシェル自体が終了しないようにする。
    // 子プロセスは独自のシグナルハンドラを持つため、Ctrl+Cで正常に停止する。
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
    }

    let mut shell = Shell::new();

    // stdin/stdoutのロックを保持し、毎回のmutexロックオーバーヘッドを回避
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut stdin = stdin.lock();
    let mut stdout = stdout.lock();
    let mut line = String::new();

    loop {
        // プロンプト表示: 失敗時は終了ステータスを接頭辞に付ける
        if shell.last_status == 0 {
            let _ = write!(stdout, "rush$ ");
        } else {
            let _ = write!(stdout, "[{}] rush$ ", shell.last_status);
        }
        let _ = stdout.flush();

        // バッファを再利用して読み取り（アロケーション回避）
        line.clear();
        match stdin.read_line(&mut line) {
            Ok(0) => {
                // EOF (Ctrl+D): 改行を出力して正常終了
                let _ = writeln!(stdout);
                break;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("rush: read error: {}", e);
                break;
            }
        }

        // 簡易パース: Phase 2で本格的なパーサーに置換予定
        let args: Vec<&str> = line.split_whitespace().collect();
        if args.is_empty() {
            continue;
        }

        shell.last_status = executor::execute(&mut shell, &args);

        if shell.should_exit {
            break;
        }
    }

    std::process::exit(shell.last_status);
}
