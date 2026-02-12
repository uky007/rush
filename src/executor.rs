//! コマンド実行: コマンドリスト条件付き実行、ビルトイン判定、リダイレクト適用、
//! パイプライン接続、展開パイプライン（コマンド置換 → チルダ → glob）、ジョブ制御。
//!
//! - [`execute`]: コマンドリスト（`&&`/`||`/`;`）全体を条件付きで実行
//! - 単一ビルトイン（非 background、非 FdDup）: fork なしの高速パス（[`execute_builtin`]）
//! - それ以外: 統一 spawn パス（[`execute_job`]）
//!   - 各コマンドの引数を `expand_args_full` で統一展開（コマンド置換 → チルダ → glob）
//!   - `posix_spawnp` でプロセスグループ設定 + シグナル `SIG_DFL` リセット
//!   - fd 複製（`2>&1` 等）は `extra_dup2s` で spawn に渡す
//!   - foreground: `tcsetpgrp` でターミナル制御を渡し、`waitpid(WUNTRACED)` で待機
//!   - background: ジョブテーブルに登録して即座に返る

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::io::IntoRawFd;

use crate::builtins;
use crate::glob;
use crate::job;
use crate::parser::{self, CommandList, Connector, Pipeline, RedirectKind};
use crate::shell::Shell;
use crate::spawn;

/// コマンド置換 + チルダ展開 + glob 展開を統一的に適用する。
fn expand_args_full(args: &[std::borrow::Cow<'_, str>], shell: &mut Shell) -> Vec<String> {
    let mut result = Vec::new();
    for arg in args {
        // 1. コマンド置換
        let sub_expanded = if arg.contains("$(") || arg.contains('`') {
            std::borrow::Cow::Owned(expand_command_subs(arg, shell))
        } else {
            arg.clone()
        };
        // 2. チルダ展開
        let tilde_expanded = parser::expand_tilde(&sub_expanded);
        // 3. glob 展開
        if glob::has_glob_chars(&tilde_expanded) {
            result.extend(glob::expand(&tilde_expanded));
        } else {
            result.push(tilde_expanded.into_owned());
        }
    }
    result
}

/// コマンド文字列を実行して stdout の出力を取得する（コマンド置換用）。
fn execute_capture(cmd_str: &str, shell: &mut Shell) -> String {
    let mut pipefd = [0i32; 2];
    if unsafe { libc::pipe(pipefd.as_mut_ptr()) } != 0 {
        return String::new();
    }

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        unsafe { libc::close(pipefd[0]); libc::close(pipefd[1]); }
        return String::new();
    }

    if pid == 0 {
        // 子プロセス: stdout をパイプに接続
        unsafe {
            libc::close(pipefd[0]);
            libc::dup2(pipefd[1], libc::STDOUT_FILENO);
            libc::close(pipefd[1]);
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGTSTP, libc::SIG_DFL);
        }
        match parser::parse(cmd_str, shell.last_status) {
            Ok(Some(list)) => {
                let status = execute(shell, &list, cmd_str);
                std::process::exit(status);
            }
            _ => std::process::exit(1),
        }
    }

    // 親プロセス: パイプから出力を読み取り
    unsafe { libc::close(pipefd[1]); }
    let mut output = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = unsafe {
            libc::read(pipefd[0], buf.as_mut_ptr() as *mut libc::c_void, buf.len())
        };
        if n <= 0 { break; }
        output.extend_from_slice(&buf[..n as usize]);
    }
    unsafe { libc::close(pipefd[0]); }
    let mut status = 0i32;
    unsafe { libc::waitpid(pid, &mut status, 0); }

    String::from_utf8_lossy(&output).trim_end_matches('\n').to_string()
}

/// 文字列内の $(...) と `...` を展開する。
fn expand_command_subs(s: &str, shell: &mut Shell) -> String {
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut result = String::new();
    let mut pos = 0;

    while pos < len {
        if bytes[pos] == b'$' && pos + 1 < len && bytes[pos + 1] == b'(' {
            pos += 2;
            let start = pos;
            let mut depth = 1;
            while pos < len && depth > 0 {
                match bytes[pos] {
                    b'(' => depth += 1,
                    b')' => {
                        depth -= 1;
                        if depth == 0 { break; }
                    }
                    b'\'' => { pos += 1; while pos < len && bytes[pos] != b'\'' { pos += 1; } }
                    b'"' => { pos += 1; while pos < len && bytes[pos] != b'"' {
                        if bytes[pos] == b'\\' { pos += 1; }
                        pos += 1;
                    }}
                    _ => {}
                }
                pos += 1;
            }
            let inner = &s[start..pos];
            if pos < len { pos += 1; } // skip ')'
            result.push_str(&execute_capture(inner, shell));
        } else if bytes[pos] == b'`' {
            pos += 1;
            let start = pos;
            while pos < len && bytes[pos] != b'`' { pos += 1; }
            let inner = &s[start..pos];
            if pos < len { pos += 1; }
            result.push_str(&execute_capture(inner, shell));
        } else {
            result.push(bytes[pos] as char);
            pos += 1;
        }
    }
    result
}

/// コマンドリスト全体を実行し、終了ステータスを返す。
///
/// `cmd_text` は元のコマンド文字列で、ジョブテーブルの表示用に使用される。
///
/// 各パイプラインを接続子（`&&`, `||`, `;`）に基づいて条件付きで実行する。
pub fn execute(shell: &mut Shell, list: &CommandList<'_>, cmd_text: &str) -> i32 {
    // バックグラウンドジョブを reap
    job::reap_jobs(&mut shell.jobs);

    let mut last_status = 0;

    for (i, item) in list.items.iter().enumerate() {
        // 前の接続子に基づく条件判定
        if i > 0 {
            match list.items[i - 1].connector {
                Connector::And if last_status != 0 => continue,
                Connector::Or if last_status == 0 => continue,
                _ => {}
            }
        }

        last_status = execute_pipeline(shell, &item.pipeline, cmd_text);
    }

    last_status
}

/// 単一パイプラインを実行し、終了ステータスを返す。
///
/// ディスパッチ:
/// 1. 単一ビルトイン（非 background） → [`execute_builtin`]（fork なし高速パス）
/// 2. それ以外（外部コマンド、パイプライン、ビルトイン + `&`） → [`execute_job`]
fn execute_pipeline(shell: &mut Shell, pipeline: &Pipeline<'_>, cmd_text: &str) -> i32 {
    // 単一ビルトイン（非 background）→ fork なしの高速パス
    if pipeline.commands.len() == 1 && !pipeline.background {
        let cmd = &pipeline.commands[0];
        // FdDup があれば spawn パスにフォールバック
        let has_fd_dup = cmd.redirects.iter().any(|r| matches!(r.kind, RedirectKind::FdDup { .. }));
        if !has_fd_dup {
            let expanded = expand_args_full(&cmd.args, shell);
            let args: Vec<&str> = expanded.iter().map(|s| s.as_str()).collect();
            if !args.is_empty() && builtins::is_builtin(args[0]) {
                return execute_builtin(shell, cmd, &expanded);
            }
        }
    }

    execute_job(shell, pipeline, cmd_text)
}

// ── ビルトイン高速パス ──────────────────────────────────────────────

/// 単一ビルトインを fork なしで実行する。
///
/// stdout リダイレクトがあればファイルを開いてから実行する。
/// `&` 付きビルトインはこのパスを通らず [`execute_job`] で外部コマンドとして spawn される。
fn execute_builtin(shell: &mut Shell, cmd: &parser::Command<'_>, expanded_args: &[String]) -> i32 {
    let args: Vec<&str> = expanded_args.iter().map(|s| s.as_str()).collect();
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

/// リダイレクト先の fd 情報。`open_redirect_fds` が返す。
struct RedirectFds {
    stdin_fd: Option<i32>,
    stdout_fd: Option<i32>,
    stderr_fd: Option<i32>,
    dup_actions: Vec<(i32, i32)>, // (src_fd, dst_fd) — spawn で適用
}

/// リダイレクト先ファイルを開き、raw fd を返す。
///
/// 開いた fd は呼び出し側（spawn 後の親プロセス）で close する責任がある。
fn open_redirect_fds(redirects: &[parser::Redirect<'_>]) -> Result<RedirectFds, i32> {
    let mut fds = RedirectFds {
        stdin_fd: None,
        stdout_fd: None,
        stderr_fd: None,
        dup_actions: Vec::new(),
    };

    for r in redirects {
        let target = r.target.as_ref();
        match r.kind {
            RedirectKind::Output => {
                // 前の stdout_fd があれば close
                if let Some(old) = fds.stdout_fd {
                    unsafe { libc::close(old); }
                }
                let f = File::create(target).map_err(|e| {
                    eprintln!("rush: {}: {}", target, e);
                    1
                })?;
                fds.stdout_fd = Some(f.into_raw_fd());
            }
            RedirectKind::Append => {
                if let Some(old) = fds.stdout_fd {
                    unsafe { libc::close(old); }
                }
                let f = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(target)
                    .map_err(|e| {
                        eprintln!("rush: {}: {}", target, e);
                        1
                    })?;
                fds.stdout_fd = Some(f.into_raw_fd());
            }
            RedirectKind::Input => {
                if let Some(old) = fds.stdin_fd {
                    unsafe { libc::close(old); }
                }
                let f = File::open(target).map_err(|e| {
                    eprintln!("rush: {}: {}", target, e);
                    1
                })?;
                fds.stdin_fd = Some(f.into_raw_fd());
            }
            RedirectKind::Stderr => {
                if let Some(old) = fds.stderr_fd {
                    unsafe { libc::close(old); }
                }
                let f = File::create(target).map_err(|e| {
                    eprintln!("rush: {}: {}", target, e);
                    1
                })?;
                fds.stderr_fd = Some(f.into_raw_fd());
            }
            RedirectKind::FdDup { src_fd, dst_fd } => {
                fds.dup_actions.push((src_fd, dst_fd));
            }
        }
    }

    Ok(fds)
}

/// パイプライン（単一 or 複数コマンド）を子プロセスとして実行する。
///
/// 処理の流れ:
/// 1. N-1 個のパイプを作成（8 段以下はスタック配列）
/// 2. 各コマンドの fd を `posix_spawnp` で起動
/// 3. 親側でも `setpgid` を呼び、レースコンディションを防止
/// 4. background → ジョブテーブルに追加し `[N] pgid` を表示
///    foreground → `tcsetpgrp` でターミナルを渡し、`wait_for_fg` で待機。
///    停止検出時はジョブテーブルに Stopped として登録。
fn execute_job(shell: &mut Shell, pipeline: &Pipeline<'_>, cmd_text: &str) -> i32 {
    let n = pipeline.commands.len();

    // ── パイプ作成（8 段以下はスタック配列、超過時はヒープフォールバック）──
    let mut pipe_stack: [[i32; 2]; 7] = [[-1; 2]; 7];
    let pipe_count = n.saturating_sub(1);
    let mut pipe_heap: Vec<[i32; 2]> = Vec::new();

    let pipes: &mut [[i32; 2]] = if pipe_count <= 7 {
        &mut pipe_stack[..pipe_count]
    } else {
        pipe_heap.resize(pipe_count, [-1; 2]);
        &mut pipe_heap
    };

    for p in pipes.iter_mut() {
        if unsafe { libc::pipe(p.as_mut_ptr()) } != 0 {
            eprintln!("rush: pipe: {}", std::io::Error::last_os_error());
            // 既に作成済みのパイプを close
            for created in pipes.iter() {
                if created[0] >= 0 { unsafe { libc::close(created[0]); } }
                if created[1] >= 0 { unsafe { libc::close(created[1]); } }
            }
            return 1;
        }
    }

    // ── PID 配列（8 個以下はスタック）──
    let mut pid_stack: [libc::pid_t; 8] = [0; 8];
    let mut pid_heap: Vec<libc::pid_t> = Vec::new();
    let mut pid_count: usize = 0;

    let pids: &mut [libc::pid_t] = if n <= 8 {
        &mut pid_stack[..n]
    } else {
        pid_heap.resize(n, 0);
        &mut pid_heap
    };

    let mut pgid: libc::pid_t = 0;
    let mut spawn_error = false;
    let mut error_status = 1i32;

    // ── close 対象 fd 収集用スタック配列 ──
    let mut close_fds_buf: [i32; 16] = [-1; 16];

    for i in 0..n {
        let cmd = &pipeline.commands[i];

        // コマンド置換 + チルダ + glob 展開
        let expanded = expand_args_full(&cmd.args, shell);
        let args: Vec<&str> = expanded.iter().map(|s| s.as_str()).collect();

        // stdin/stdout の決定（パイプ接続）
        let mut stdin_fd: Option<i32> = None;
        let mut stdout_fd: Option<i32> = None;

        if i > 0 {
            stdin_fd = Some(pipes[i - 1][0]);
        }
        if i < n - 1 {
            stdout_fd = Some(pipes[i][1]);
        }

        // リダイレクトの fd を開く
        let redir_fds = match open_redirect_fds(&cmd.redirects) {
            Ok(fds) => fds,
            Err(status) => {
                error_status = status;
                spawn_error = true;
                break;
            }
        };

        // リダイレクトの fd でパイプの fd を上書き
        if redir_fds.stdin_fd.is_some() {
            stdin_fd = redir_fds.stdin_fd;
        }
        if redir_fds.stdout_fd.is_some() {
            stdout_fd = redir_fds.stdout_fd;
        }

        // 子プロセスで close すべき fd を収集
        let mut close_count = 0;
        for j in 0..pipe_count {
            // パイプの read end
            if pipes[j][0] >= 0 {
                let fd = pipes[j][0];
                // 今回 stdin として使う fd は close しない（dup2 後に close される）
                if stdin_fd != Some(fd) && close_count < close_fds_buf.len() {
                    close_fds_buf[close_count] = fd;
                    close_count += 1;
                }
            }
            // パイプの write end
            if pipes[j][1] >= 0 {
                let fd = pipes[j][1];
                if stdout_fd != Some(fd) && close_count < close_fds_buf.len() {
                    close_fds_buf[close_count] = fd;
                    close_count += 1;
                }
            }
        }

        match spawn::spawn(
            &args,
            pgid,
            stdin_fd,
            stdout_fd,
            redir_fds.stderr_fd,
            &close_fds_buf[..close_count],
            &redir_fds.dup_actions,
        ) {
            Ok(child_pid) => {
                // 親側でもプロセスグループを設定（レースコンディション防止）
                if pgid == 0 {
                    pgid = child_pid;
                }
                unsafe {
                    libc::setpgid(child_pid, pgid);
                }

                pids[pid_count] = child_pid;
                pid_count += 1;
            }
            Err(e) => {
                eprintln!("{}", e);
                error_status = e.exit_status();
                spawn_error = true;
                break;
            }
        }

        // 消費したパイプ fd を親側で close
        if i > 0 && pipes[i - 1][0] >= 0 {
            unsafe { libc::close(pipes[i - 1][0]); }
            pipes[i - 1][0] = -1;
        }
        if i < n - 1 && pipes[i][1] >= 0 {
            unsafe { libc::close(pipes[i][1]); }
            pipes[i][1] = -1;
        }

        // リダイレクト用に開いた fd を親側で close
        if let Some(fd) = redir_fds.stdin_fd {
            unsafe { libc::close(fd); }
        }
        if let Some(fd) = redir_fds.stdout_fd {
            unsafe { libc::close(fd); }
        }
        if let Some(fd) = redir_fds.stderr_fd {
            unsafe { libc::close(fd); }
        }
    }

    // 未消費のパイプ fd を close
    for p in pipes.iter() {
        if p[0] >= 0 { unsafe { libc::close(p[0]); } }
        if p[1] >= 0 { unsafe { libc::close(p[1]); } }
    }

    let active_pids = &pids[..pid_count];

    if spawn_error {
        // エラー時: 既に spawn したプロセスを待機してクリーンアップ
        for &pid in active_pids {
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
        let job_id = shell
            .jobs
            .insert(pgid, display_cmd.to_string(), active_pids.to_vec());
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
            let job_id = shell
                .jobs
                .insert(pgid, display_cmd.to_string(), active_pids.to_vec());
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
