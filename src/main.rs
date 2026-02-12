//! rush — 速度重視のRust製シェル
//!
//! REPLループ: プロンプト表示 → 行エディタで入力読み取り → パース → 実行 → ループ
//!
//! ## 主な機能
//!
//! - エイリアス展開（再帰ガード付き）
//! - `history` ビルトイン（editor 所有の履歴への直接アクセス）
//! - 継続行入力（末尾 `\`・未完了パイプ/演算子・未閉クォートで `> ` プロンプト）
//! - `~/.rushrc` 読み込み
//! - 非インタラクティブモード（`rush -c 'cmd'`、`rush script.sh`）
//!
//! ## モジュール構成
//!
//! | モジュール | 役割 |
//! |-----------|------|
//! | [`editor`] | 行エディタ（raw モード、キー入力、Ctrl+R 逆方向検索、Tab 補完、ハイライト） |
//! | [`history`] | コマンド履歴（`~/.rush_history` 永続化、↑↓ ナビゲーション、逆方向検索） |
//! | [`complete`] | Tab 補完（コマンド名、ファイル名、`&&`/`||`/`;` 後のコマンド位置認識） |
//! | [`highlight`] | シンタックスハイライト（ANSI カラー、PATH キャッシュ、`$(cmd)`/`2>&1` 対応） |
//! | [`parser`] | 構文解析（パイプライン、リダイレクト、クォート、変数展開、パラメータ展開、算術展開、継続行検出） |
//! | [`executor`] | コマンド実行（条件付き実行、展開パイプライン: コマンド置換 → チルダ → ブレース → glob） |
//! | [`builtins`] | ビルトイン（`cd`, `echo`, `export`, `alias`, `source`, `read`, `exec`, `wait` 等 20 種） |
//! | [`glob`] | パス名展開（`*`, `?` によるファイル名マッチング、パラメータ展開のパターン共有） |
//! | [`job`] | ジョブコントロール（バックグラウンド実行、Ctrl+Z サスペンド、`fg`/`bg` 復帰） |
//! | [`shell`] | シェルのグローバル状態（終了ステータス、ジョブテーブル、エイリアスマップ） |
//! | [`spawn`] | `posix_spawnp` ラッパー（外部コマンド起動の高速化） |

mod builtins;
mod complete;
mod editor;
mod executor;
mod glob;
mod highlight;
mod history;
mod job;
mod parser;
mod shell;
mod spawn;

use std::collections::HashMap;

use shell::Shell;

/// `~/.rushrc` を読み込んで各行を実行する。ファイルが存在しなければサイレントスキップ。
fn load_rc(shell: &mut Shell) {
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => return,
    };
    let rc_path = format!("{}/.rushrc", home);
    let content = match std::fs::read_to_string(&rc_path) {
        Ok(c) => c,
        Err(_) => return, // ファイルなし → サイレントスキップ
    };
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let expanded = expand_alias(trimmed, &shell.aliases);
        match parser::parse(&expanded, shell.last_status) {
            Ok(Some(list)) => {
                let cmd_text = expanded.trim().to_string();
                shell.last_status = executor::execute(shell, &list, &cmd_text);
            }
            Ok(None) => {}
            Err(e) => eprintln!("rush: ~/.rushrc: {}", e),
        }
    }
}

/// `history` / `history N` / `history -c` を処理する。
/// editor が履歴を所有しているため main.rs で特別扱いする。
fn handle_history(editor: &mut editor::LineEditor, cmd: &str) -> i32 {
    let args: Vec<&str> = cmd.split_whitespace().collect();
    match args.get(1).copied() {
        Some("-c") => {
            editor.history_mut().clear();
            0
        }
        Some(n_str) => match n_str.parse::<usize>() {
            Ok(n) => {
                let history = editor.history();
                let entries = history.entries();
                let start = entries.len().saturating_sub(n);
                for (i, entry) in entries[start..].iter().enumerate() {
                    println!("{:5}  {}", start + i + 1, entry);
                }
                0
            }
            Err(_) => {
                eprintln!("rush: history: {}: numeric argument required", n_str);
                2
            }
        },
        None => {
            let history = editor.history();
            let entries = history.entries();
            for (i, entry) in entries.iter().enumerate() {
                println!("{:5}  {}", i + 1, entry);
            }
            0
        }
    }
}

/// エイリアス展開: 行の最初のワードがエイリアスならその値に置換する。
/// 再帰ガード付き（同じエイリアスは 1 回のみ展開）。
fn expand_alias(line: &str, aliases: &HashMap<String, String>) -> String {
    let trimmed = line.trim_start();
    if trimmed.is_empty() {
        return line.to_string();
    }
    let word_end = trimmed
        .find(|c: char| c.is_whitespace())
        .unwrap_or(trimmed.len());
    let first_word = &trimmed[..word_end];
    let rest = &trimmed[word_end..];

    if let Some(value) = aliases.get(first_word) {
        // 再帰ガード: 展開結果の最初の単語が同じエイリアスなら停止
        let expanded_first = value
            .split_whitespace()
            .next()
            .unwrap_or("");
        if expanded_first == first_word {
            return line.to_string();
        }
        let new_line = format!("{}{}", value, rest);
        // 再帰展開（別のエイリアスが先頭に来る場合）
        expand_alias(&new_line, aliases)
    } else {
        line.to_string()
    }
}

/// 文字列を 1 行ずつ（または単一コマンドとして）パースして実行する。
fn run_string(shell: &mut Shell, input: &str) {
    let lines: Vec<&str> = input.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        i += 1;
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let expanded = expand_alias(trimmed, &shell.aliases);
        match parser::parse(&expanded, shell.last_status) {
            Ok(Some(mut list)) => {
                // ヒアドキュメントの本文を収集
                let delims = parser::heredoc_delimiters(&list);
                if !delims.is_empty() {
                    let mut bodies = Vec::new();
                    for delim in &delims {
                        let mut body = String::new();
                        while i < lines.len() {
                            let line = lines[i];
                            i += 1;
                            if line.trim() == delim.as_str() {
                                break;
                            }
                            if !body.is_empty() {
                                body.push('\n');
                            }
                            body.push_str(line);
                        }
                        bodies.push(body);
                    }
                    parser::fill_heredoc_bodies(&mut list, &bodies);
                }
                let cmd_text = expanded.trim().to_string();
                shell.last_status = executor::execute(shell, &list, &cmd_text);
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("rush: {}", e);
                shell.last_status = 2;
            }
        }
        if shell.should_exit {
            break;
        }
    }
}

/// スクリプトファイルを行単位で実行する。
fn run_file(shell: &mut Shell, path: &str) {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("rush: {}: {}", path, e);
            shell.last_status = 127;
            return;
        }
    };
    run_string(shell, &content);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // 非インタラクティブモード: rush -c 'command' または rush script.sh
    if args.len() > 1 {
        let mut shell = Shell::new();
        if args[1] == "-c" {
            if args.len() < 3 {
                eprintln!("rush: -c: option requires an argument");
                std::process::exit(2);
            }
            run_string(&mut shell, &args[2]);
        } else {
            run_file(&mut shell, &args[1]);
        }
        std::process::exit(shell.last_status);
    }

    // シグナル設定: シェル自体は SIGINT/SIGTSTP/SIGTTOU/SIGTTIN を無視する。
    // 子プロセスは posix_spawnattr の POSIX_SPAWN_SETSIGDEF で SIG_DFL にリセットされる。
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
    load_rc(&mut shell);
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
                // エイリアス展開（コマンド位置の最初の単語のみ、再帰ガード付き）
                let mut accumulated = expand_alias(&line, &shell.aliases);

                // 継続行入力ループ: 末尾 `\`、未完了パイプ/演算子、未閉クォートに対応
                loop {
                    // 末尾 `\` → バックスラッシュを除去して次行を連結
                    let trimmed_end = accumulated.trim_end();
                    if trimmed_end.ends_with('\\') {
                        accumulated = trimmed_end[..trimmed_end.len() - 1].to_string();
                        match editor.read_line("> ") {
                            Some(next) => {
                                accumulated.push_str(&next);
                                continue;
                            }
                            None => break,
                        }
                    }

                    // history ビルトイン: editor へのアクセスが必要なため main.rs で特別扱い
                    let cmd_trimmed = accumulated.trim();
                    if cmd_trimmed == "history" || cmd_trimmed.starts_with("history ") {
                        shell.last_status = handle_history(&mut editor, cmd_trimmed);
                        break;
                    }

                    // パース: 不完全入力なら `> ` プロンプトで継続行を読み取る
                    match parser::parse(&accumulated, shell.last_status) {
                        Ok(Some(mut list)) => {
                            // ヒアドキュメントの本文を対話的に収集
                            let delims = parser::heredoc_delimiters(&list);
                            if !delims.is_empty() {
                                let mut bodies = Vec::new();
                                for delim in &delims {
                                    let mut body = String::new();
                                    loop {
                                        match editor.read_line("> ") {
                                            Some(line) => {
                                                if line.trim() == delim.as_str() {
                                                    break;
                                                }
                                                if !body.is_empty() {
                                                    body.push('\n');
                                                }
                                                body.push_str(&line);
                                            }
                                            None => break,
                                        }
                                    }
                                    bodies.push(body);
                                }
                                parser::fill_heredoc_bodies(&mut list, &bodies);
                            }
                            let cmd_text = accumulated.trim().to_string();
                            shell.last_status = executor::execute(&mut shell, &list, &cmd_text);
                            break;
                        }
                        Ok(None) => break,
                        Err(parser::ParseError::IncompleteInput)
                        | Err(parser::ParseError::UnterminatedQuote(_)) => {
                            match editor.read_line("> ") {
                                Some(next) => {
                                    accumulated.push('\n');
                                    accumulated.push_str(&next);
                                }
                                None => {
                                    println!();
                                    break;
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("rush: {}", e);
                            shell.last_status = 2;
                            break;
                        }
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
