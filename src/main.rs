//! rush — 速度重視のRust製シェル
//!
//! REPLループ: プロンプト表示 → 入力読み取り → パース → 実行 → ループ
//!
//! 現在の機能:
//! - 構文解析: パイプライン、リダイレクト、クォート、変数展開、`&`（[`parser`]）
//! - コマンド実行: パイプライン接続、プロセスグループ管理、ジョブ制御（[`executor`]）
//! - ビルトイン: `exit`, `cd`, `pwd`, `echo`, `export`, `unset`, `jobs`, `fg`, `bg`（[`builtins`]）
//! - ジョブコントロール: バックグラウンド実行 (`&`)、Ctrl+Z サスペンド、`fg`/`bg` 復帰（[`job`]）

mod builtins;
mod executor;
mod job;
mod parser;
mod shell;

use std::io::{self, BufRead, Write};

use shell::Shell;

fn main() {
    // シグナル設定: シェル自体は SIGINT/SIGTSTP/SIGTTOU/SIGTTIN を無視する。
    // 子プロセスは pre_exec で SIG_DFL にリセットされる。
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGTSTP, libc::SIG_IGN);
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
        libc::signal(libc::SIGTTIN, libc::SIG_IGN);
    }

    // シェルを自身のプロセスグループリーダーにし、ターミナルを掌握する。
    unsafe {
        let shell_pid = libc::getpid();
        libc::setpgid(shell_pid, shell_pid);
        libc::tcsetpgrp(libc::STDIN_FILENO, shell_pid);
    }

    let mut shell = Shell::new();

    // stdin/stdoutのロックを保持し、毎回のmutexロックオーバーヘッドを回避
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut stdin = stdin.lock();
    let mut stdout = stdout.lock();
    let mut line = String::new();

    loop {
        // プロンプト前にバックグラウンドジョブを reap し、完了通知を出力
        job::reap_jobs(&mut shell.jobs);
        job::notify_and_clean(&mut shell.jobs);

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
        // cmd_text はジョブテーブルの表示用コマンド文字列として execute に渡す
        let cmd_text = line.trim().to_string();
        match parser::parse(&line, shell.last_status) {
            Ok(Some(pipeline)) => {
                shell.last_status = executor::execute(&mut shell, &pipeline, &cmd_text);
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
