//! コマンド実行: ビルトイン判定、リダイレクト適用、パイプライン接続、ジョブ制御。
//!
//! - 単一ビルトイン（非 background）: fork なしの高速パス（[`execute_builtin`]）
//! - それ以外: 統一 spawn パス（[`execute_job`]）
//!   - `pre_exec` でプロセスグループ設定 + シグナルリセット
//!   - `std::mem::forget(child)` で Rust の Child 管理を放棄し、`waitpid` で手動 reap
//!   - foreground: `tcsetpgrp` でターミナル制御を渡し、`waitpid(WUNTRACED)` で待機
//!   - background: ジョブテーブルに登録して即座に返る

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::io::FromRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use crate::builtins;
use crate::job;
use crate::parser::{self, Pipeline, RedirectKind};
use crate::shell::Shell;

/// パイプライン全体を実行し、終了ステータスを返す。
///
/// `cmd_text` は元のコマンド文字列で、ジョブテーブルの表示用に使用される。
///
/// ディスパッチ:
/// 1. 単一ビルトイン（非 background） → [`execute_builtin`]（fork なし高速パス）
/// 2. それ以外（外部コマンド、パイプライン、ビルトイン + `&`） → [`execute_job`]
pub fn execute(shell: &mut Shell, pipeline: &Pipeline<'_>, cmd_text: &str) -> i32 {
    // バックグラウンドジョブを reap
    job::reap_jobs(&mut shell.jobs);

    // 単一ビルトイン（非 background）→ fork なしの高速パス
    if pipeline.commands.len() == 1 && !pipeline.background {
        let args: Vec<&str> = pipeline.commands[0].args.iter().map(|a| a.as_ref()).collect();
        if builtins::is_builtin(args[0]) {
            return execute_builtin(shell, &pipeline.commands[0]);
        }
    }

    execute_job(shell, pipeline, cmd_text)
}

// ── ビルトイン高速パス ──────────────────────────────────────────────

/// 単一ビルトインを fork なしで実行する。
///
/// stdout リダイレクトがあればファイルを開いてから実行する。
/// `&` 付きビルトインはこのパスを通らず [`execute_job`] で外部コマンドとして spawn される。
fn execute_builtin(shell: &mut Shell, cmd: &parser::Command<'_>) -> i32 {
    let args: Vec<&str> = cmd.args.iter().map(|a| a.as_ref()).collect();
    match open_builtin_stdout(&cmd.redirects) {
        Ok(Some(mut file)) => builtins::try_exec(shell, &args, &mut file).unwrap(),
        Ok(None) => builtins::try_exec(shell, &args, &mut io::stdout()).unwrap(),
        Err(status) => status,
    }
}

/// ビルトイン用の stdout リダイレクト先ファイルを開く。
///
/// `>` / `>>` があればファイルを開いて `Ok(Some(File))` を返す。
/// stdout リダイレクトがなければ `Ok(None)` を返す（呼び出し側で `io::stdout()` を使う）。
/// ファイルオープン失敗時は `Err(1)` を返す。
/// 複数指定時は bash 互換で最後の指定が有効。
fn open_builtin_stdout(redirects: &[parser::Redirect<'_>]) -> Result<Option<File>, i32> {
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

// ── 統一 spawn パス ─────────────────────────────────────────────────

/// パイプライン（単一 or 複数コマンド）を子プロセスとして実行する。
///
/// 処理の流れ:
/// 1. N-1 個のパイプを作成
/// 2. 各コマンドを spawn（`pre_exec` でプロセスグループ参加 + シグナル `SIG_DFL` リセット）
/// 3. 親側でも `setpgid` を呼び、レースコンディションを防止
/// 4. `std::mem::forget(child)` で Rust の `Child` 管理を放棄（`waitpid` で手動 reap するため）
/// 5. background → ジョブテーブルに追加し `[N] pgid` を表示
///    foreground → `tcsetpgrp` でターミナルを渡し、`wait_for_fg` で待機。
///    停止検出時はジョブテーブルに Stopped として登録。
fn execute_job(shell: &mut Shell, pipeline: &Pipeline<'_>, cmd_text: &str) -> i32 {
    let n = pipeline.commands.len();

    // N-1 個のパイプを作成
    let mut pipes: Vec<[i32; 2]> = Vec::with_capacity(n.saturating_sub(1));
    for _ in 0..n.saturating_sub(1) {
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

    let mut pids: Vec<libc::pid_t> = Vec::with_capacity(n);
    let mut pgid: libc::pid_t = 0; // 最初の子の PID がグループリーダーになる
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

        // コマンド個別のリダイレクト
        if let Err(status) = apply_redirects(&mut command, &cmd.redirects) {
            error_status = status;
            spawn_error = true;
            break;
        }

        // pre_exec: プロセスグループ設定 + シグナルリセット
        let pgid_for_child = pgid;
        unsafe {
            command.pre_exec(move || {
                let target = if pgid_for_child == 0 {
                    libc::getpid()
                } else {
                    pgid_for_child
                };
                libc::setpgid(0, target);
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                libc::signal(libc::SIGTSTP, libc::SIG_DFL);
                libc::signal(libc::SIGTTOU, libc::SIG_DFL);
                libc::signal(libc::SIGTTIN, libc::SIG_DFL);
                Ok(())
            });
        }

        match command.spawn() {
            Ok(child) => {
                let child_pid = child.id() as libc::pid_t;

                // 親側でもプロセスグループを設定（レースコンディション防止）
                if pgid == 0 {
                    pgid = child_pid;
                }
                unsafe {
                    libc::setpgid(child_pid, pgid);
                }

                pids.push(child_pid);

                // Rust の Child を forget し、waitpid で手動 reap する
                std::mem::forget(child);
            }
            Err(e) => {
                error_status = spawn_error_status(args[0], &e);
                spawn_error = true;
                break;
            }
        }
    }

    // 未消費の fd を close
    for fds in &pipes {
        if fds[0] >= 0 {
            unsafe { libc::close(fds[0]) };
        }
        if fds[1] >= 0 {
            unsafe { libc::close(fds[1]) };
        }
    }

    if spawn_error {
        // エラー時: 既に spawn したプロセスを待機してクリーンアップ
        for &pid in &pids {
            unsafe {
                libc::waitpid(pid, std::ptr::null_mut(), 0);
            }
        }
        return error_status;
    }

    // コマンドテキストから末尾の & を除去した表示用文字列
    let display_cmd = cmd_text.strip_suffix('&').unwrap_or(cmd_text).trim();

    if pipeline.background {
        // バックグラウンド: ジョブテーブルに追加
        let job_id = shell.jobs.insert(pgid, display_cmd.to_string(), pids);
        eprintln!("[{}] {}", job_id, pgid);
        0
    } else {
        // フォアグラウンド: ターミナル制御を渡して待機
        job::give_terminal_to(shell.terminal_fd, pgid);

        let (status, stopped) = job::wait_for_fg(&mut shell.jobs, pgid);

        // ターミナルをシェルに戻す
        job::take_terminal_back(shell.terminal_fd, shell.shell_pgid);

        if stopped {
            // Ctrl+Z で停止: ジョブテーブルに追加
            let job_id = shell.jobs.insert(pgid, display_cmd.to_string(), pids);
            // 停止状態をマーク（insert 後のプロセスは stopped=false なので更新する）
            if let Some(job) = shell.jobs.get_mut(job_id) {
                for proc in &mut job.processes {
                    proc.stopped = true;
                }
            }
            eprintln!("\n[{}]+  Stopped   {}", job_id, display_cmd);
        }

        status
    }
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
