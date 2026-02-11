//! rush — 速度重視のRust製シェル
//!
//! REPLループ: プロンプト表示 → 入力読み取り → パース → 実行 → ループ
//!
//! 現在の機能:
//! - 手書きトークナイザ + パーサーによる構文解析（[`parser`]）
//! - パイプライン接続（`libc::pipe` + `Stdio::from_raw_fd`）（[`executor`]）
//! - ファイルリダイレクト（`>`, `>>`, `<`, `2>`）（[`executor`]）
//! - シングルクォート / ダブルクォート（[`parser`]）
//! - ビルトイン: `cd`, `exit`（[`builtins`]）

mod builtins;
mod executor;
mod parser;
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

        // パース: Pipeline<'_> は line を借用 → execute 後に drop → line.clear() は安全
        match parser::parse(&line) {
            Ok(Some(pipeline)) => {
                shell.last_status = executor::execute(&mut shell, &pipeline);
            }
            Ok(None) => continue,
            Err(e) => {
                eprintln!("rush: {}", e);
                shell.last_status = 2;
                continue;
            }
        }

        if shell.should_exit {
            break;
        }
    }

    std::process::exit(shell.last_status);
}
