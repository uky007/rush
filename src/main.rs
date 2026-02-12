//! rush — 速度重視のRust製シェル
//!
//! REPLループ: プロンプト表示 → 行エディタで入力読み取り → パース → 実行 → ループ
//!
//! ## モジュール構成
//!
//! | モジュール | 役割 |
//! |-----------|------|
//! | [`editor`] | 行エディタ（raw モード、キー入力、バッファ操作、表示更新） |
//! | [`history`] | コマンド履歴（`~/.rush_history` 永続化、↑↓ ナビゲーション） |
//! | [`complete`] | Tab 補完（コマンド名、ファイル名） |
//! | [`highlight`] | シンタックスハイライト（ANSI カラー、PATH キャッシュ） |
//! | [`parser`] | 構文解析（パイプライン、リダイレクト、クォート、変数展開、`&`） |
//! | [`executor`] | コマンド実行（パイプライン接続、プロセスグループ管理） |
//! | [`builtins`] | ビルトイン（`exit`, `cd`, `pwd`, `echo`, `export`, `unset`, `jobs`, `fg`, `bg`） |
//! | [`job`] | ジョブコントロール（バックグラウンド実行、Ctrl+Z サスペンド、`fg`/`bg` 復帰） |
//! | [`shell`] | シェルのグローバル状態（終了ステータス、ジョブテーブル、プロセスグループ） |

mod builtins;
mod complete;
mod editor;
mod executor;
mod highlight;
mod history;
mod job;
mod parser;
mod shell;

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
    // 行エディタ: raw モードによるキー入力、履歴、Tab 補完、シンタックスハイライトを統合。
    // raw モードは read_line() 内でのみ有効で、コマンド実行中は cooked モードに戻る。
    let mut editor = editor::LineEditor::new();

    loop {
        // プロンプト前にバックグラウンドジョブを reap し、完了通知を出力
        job::reap_jobs(&mut shell.jobs);
        job::notify_and_clean(&mut shell.jobs);

        // プロンプト構築: 終了ステータスが非ゼロなら接頭辞に付ける
        let prompt = if shell.last_status == 0 {
            "rush$ ".to_string()
        } else {
            format!("[{}] rush$ ", shell.last_status)
        };

        // 行エディタで 1 行読み取る（raw モード → Enter で確定 → cooked モードに復帰）
        match editor.read_line(&prompt) {
            Some(line) if !line.trim().is_empty() => {
                editor.add_history(&line);
                // パース: Pipeline<'_> は line を借用 → execute 後に drop
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
            }
            Some(_) => continue, // 空行
            None => {
                // EOF (Ctrl+D): 改行を出力して正常終了
                println!();
                break;
            }
        }

        if shell.should_exit {
            break;
        }
    }

    std::process::exit(shell.last_status);
}
