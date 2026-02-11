//! コマンド実行: ビルトイン判定、リダイレクト適用、パイプライン接続。
//!
//! - 単一コマンド: ビルトイン → 外部コマンドの順に試行（[`execute_single`]）
//!   - ビルトインには `&mut dyn Write` 経由で stdout リダイレクトを適用（[`open_builtin_stdout`]）
//! - パイプライン（2段以上）: `libc::pipe()` + `Stdio::from_raw_fd()` で接続（[`execute_pipeline`]）
//!   - パイプライン内のビルトインは外部コマンドとして実行（`/bin/echo` 等にフォールバック）
//!
//! fd 所有権: `from_raw_fd` で所有権移転し二重 close を防止。
//! 消費済み fd は -1 にマークし、エラー時に未消費分のみ手動 close する。

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::io::FromRawFd;
use std::process::{Command, Stdio};

use crate::builtins;
use crate::parser::{self, Pipeline, RedirectKind};
use crate::shell::Shell;

/// パイプライン全体を実行し、終了ステータスを返す。
///
/// 単一コマンドならビルトイン判定を含む [`execute_single`] へ、
/// 2段以上なら [`execute_pipeline`] へディスパッチする。
pub fn execute(shell: &mut Shell, pipeline: &Pipeline<'_>) -> i32 {
    if pipeline.commands.len() == 1 {
        execute_single(shell, &pipeline.commands[0])
    } else {
        execute_pipeline(pipeline)
    }
}

// ── 単一コマンド実行 ─────────────────────────────────────────────────

/// 単一コマンドを実行する。ビルトイン → 外部コマンドの順に試行。
///
/// ビルトインの場合はリダイレクトに応じた stdout writer を準備してから実行する。
fn execute_single(shell: &mut Shell, cmd: &parser::Command<'_>) -> i32 {
    let args: Vec<&str> = cmd.args.iter().map(|a| a.as_ref()).collect();

    // ビルトインを先にチェック（fork不要の高速パス）
    if builtins::is_builtin(args[0]) {
        // stdout リダイレクトがあればファイルを開く
        match open_builtin_stdout(&cmd.redirects) {
            Ok(Some(mut file)) => {
                return builtins::try_exec(shell, &args, &mut file).unwrap();
            }
            Ok(None) => {
                return builtins::try_exec(shell, &args, &mut io::stdout()).unwrap();
            }
            Err(status) => return status,
        }
    }

    // 外部コマンド: リダイレクト適用 → spawn → wait
    let mut command = Command::new(args[0]);
    command.args(&args[1..]);

    if let Err(status) = apply_redirects(&mut command, &cmd.redirects) {
        return status;
    }

    spawn_and_wait(&mut command, args[0])
}

/// ビルトイン用の stdout リダイレクト先ファイルを開く。
///
/// `>` / `>>` があればファイルを開いて `Ok(Some(File))` を返す。
/// stdout リダイレクトがなければ `Ok(None)` を返す（呼び出し側で `io::stdout()` を使う）。
/// ファイルオープン失敗時は `Err(1)` を返す。
fn open_builtin_stdout(redirects: &[parser::Redirect<'_>]) -> Result<Option<File>, i32> {
    // 最後の stdout リダイレクトを適用（bash互換: 複数指定時は最後が有効）
    for r in redirects.iter().rev() {
        match r.kind {
            RedirectKind::Output => {
                let f = File::create(r.target.as_ref()).map_err(|e| {
                    eprintln!("rush: {}: {}", r.target, e);
                    1
                })?;
                return Ok(Some(f));
            }
            RedirectKind::Append => {
                let f = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(r.target.as_ref())
                    .map_err(|e| {
                        eprintln!("rush: {}: {}", r.target, e);
                        1
                    })?;
                return Ok(Some(f));
            }
            _ => continue,
        }
    }
    Ok(None)
}

// ── パイプライン実行 ─────────────────────────────────────────────────

/// 2段以上のパイプラインを実行する。
///
/// 1. N-1 個の `libc::pipe()` で fd ペアを作成
/// 2. 各コマンドの stdin/stdout をパイプ fd で接続し spawn
/// 3. 全子プロセスを wait し、最終コマンドのステータスを返す
///
/// ビルトインはパイプラインに参加しない（全て外部コマンドとして実行）。
fn execute_pipeline(pipeline: &Pipeline<'_>) -> i32 {
    let n = pipeline.commands.len();

    // N-1 個のパイプを作成
    let mut pipes: Vec<[i32; 2]> = Vec::with_capacity(n - 1);
    for _ in 0..n - 1 {
        let mut fds = [0i32; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            eprintln!("rush: pipe: {}", std::io::Error::last_os_error());
            for p in &pipes {
                unsafe {
                    libc::close(p[0]);
                    libc::close(p[1]);
                }
            }
            return 1;
        }
        pipes.push(fds);
    }

    let mut children: Vec<std::process::Child> = Vec::with_capacity(n);
    let mut spawn_error = false;
    let mut error_status = 1i32;

    for i in 0..n {
        let cmd = &pipeline.commands[i];
        let args: Vec<&str> = cmd.args.iter().map(|a| a.as_ref()).collect();
        let mut command = Command::new(args[0]);
        command.args(&args[1..]);

        // 前段パイプの read_fd を stdin に接続
        if i > 0 {
            let fd = pipes[i - 1][0];
            pipes[i - 1][0] = -1; // consumed
            command.stdin(unsafe { Stdio::from_raw_fd(fd) });
        }

        // 次段パイプの write_fd を stdout に接続
        if i < n - 1 {
            let fd = pipes[i][1];
            pipes[i][1] = -1; // consumed
            command.stdout(unsafe { Stdio::from_raw_fd(fd) });
        }

        // コマンド個別のリダイレクトで上書き可能
        if let Err(status) = apply_redirects(&mut command, &cmd.redirects) {
            error_status = status;
            spawn_error = true;
            break;
        }

        match command.spawn() {
            Ok(child) => children.push(child),
            Err(e) => {
                error_status = spawn_error_status(args[0], &e);
                spawn_error = true;
                break;
            }
        }
    }

    // 未消費の fd を close（エラー時に残る分）
    for fds in &pipes {
        if fds[0] >= 0 {
            unsafe { libc::close(fds[0]) };
        }
        if fds[1] >= 0 {
            unsafe { libc::close(fds[1]) };
        }
    }

    // 全子プロセスを wait。最後のコマンドのステータスを返す。
    let spawned_all = !spawn_error;
    let mut last_status = error_status;
    for mut child in children {
        match child.wait() {
            Ok(status) => {
                if spawned_all {
                    last_status = status.code().unwrap_or(128);
                }
            }
            Err(e) => {
                eprintln!("rush: wait: {}", e);
                if spawned_all {
                    last_status = 1;
                }
            }
        }
    }

    last_status
}

// ── リダイレクト適用 ─────────────────────────────────────────────────

/// リダイレクトを `Command` に適用する。
///
/// ファイルオープンに失敗した場合はエラーメッセージを出力し `Err(1)` を返す。
/// 成功時、開いた `File` の所有権は `Command` に移転する。
fn apply_redirects(command: &mut Command, redirects: &[parser::Redirect<'_>]) -> Result<(), i32> {
    for r in redirects {
        let target = r.target.as_ref();
        match r.kind {
            RedirectKind::Output => {
                let f = File::create(target).map_err(|e| {
                    eprintln!("rush: {}: {}", target, e);
                    1
                })?;
                command.stdout(f);
            }
            RedirectKind::Append => {
                let f = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(target)
                    .map_err(|e| {
                        eprintln!("rush: {}: {}", target, e);
                        1
                    })?;
                command.stdout(f);
            }
            RedirectKind::Input => {
                let f = File::open(target).map_err(|e| {
                    eprintln!("rush: {}: {}", target, e);
                    1
                })?;
                command.stdin(f);
            }
            RedirectKind::Stderr => {
                let f = File::create(target).map_err(|e| {
                    eprintln!("rush: {}: {}", target, e);
                    1
                })?;
                command.stderr(f);
            }
        }
    }
    Ok(())
}

// ── ヘルパー ─────────────────────────────────────────────────────────

/// 外部コマンドを spawn し、完了を待って終了ステータスを返す。
fn spawn_and_wait(command: &mut Command, name: &str) -> i32 {
    match command.spawn() {
        Ok(mut child) => match child.wait() {
            Ok(status) => status.code().unwrap_or(128),
            Err(e) => {
                eprintln!("rush: {}: {}", name, e);
                1
            }
        },
        Err(e) => spawn_error_status(name, &e),
    }
}

/// spawn 失敗時のエラーメッセージ出力と終了コード決定。
///
/// 127 = command not found, 126 = permission denied, 1 = その他。
fn spawn_error_status(name: &str, e: &std::io::Error) -> i32 {
    if e.kind() == std::io::ErrorKind::NotFound {
        eprintln!("rush: {}: command not found", name);
        127
    } else if e.kind() == std::io::ErrorKind::PermissionDenied {
        eprintln!("rush: {}: permission denied", name);
        126
    } else {
        eprintln!("rush: {}: {}", name, e);
        1
    }
}
