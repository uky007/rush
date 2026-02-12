//! コマンド実行: コマンドリスト条件付き実行、ビルトイン判定、リダイレクト適用、
//! パイプライン接続、展開パイプライン（コマンド置換 → チルダ → ブレース → glob）、ジョブ制御、
//! `if`/`then`/`elif`/`else`/`fi` 複合コマンド、
//! `for`/`while`/`until`/`do`/`done` ループ。
//!
//! ## パイプライン実行
//!
//! - [`execute`]: コマンドリスト（`&&`/`||`/`;`）全体を条件付きで実行
//! - 単一ビルトイン（非 background、非 FdDup）: fork なしの高速パス（[`execute_builtin`]）
//! - それ以外: 統一 spawn パス（[`execute_job`]）
//!   - 各コマンドの引数を `expand_args_full` で統一展開（コマンド置換 → チルダ → ブレース → glob）
//!   - `posix_spawnp` でプロセスグループ設定 + シグナル `SIG_DFL` リセット
//!   - fd 複製（`2>&1` 等）は `extra_dup2s` で spawn に渡す
//!   - foreground: `tcsetpgrp` でターミナル制御を渡し、`waitpid(WUNTRACED)` で待機
//!   - background: ジョブテーブルに登録して即座に返る
//!
//! ## 複合コマンド (`if`/`then`/`elif`/`else`/`fi`)
//!
//! テキストベースのアプローチで実装。既存の AST を変更せず、
//! if ブロック全体を文字列として収集してから専用関数で解釈・実行する。
//!
//! - [`execute_if_block`]: if ブロックのエントリポイント。テキストをセクション分割し条件評価
//! - [`collect_if_block`]: 行配列から `if`〜`fi` の範囲を収集（ネスト深さ追跡）
//! - [`starts_with_if`]: 行が `if` キーワードで始まるかの判定
//!
//! 対応構文:
//! ```sh
//! if command; then body; fi
//! if command; then body; else body; fi
//! if command; then body; elif command; then body; else body; fi
//! # ネスト・複数行にも対応
//! ```
//!
//! ## ループ (`for`/`while`/`until`/`do`/`done`)
//!
//! if と同じテキストベースアプローチで実装。
//!
//! - [`execute_for_block`]: `for VAR in WORDS; do BODY; done` を実行
//! - [`execute_while_block`]: `while COND; do BODY; done` / `until COND; do BODY; done` を実行
//! - [`collect_loop_block`]: 行配列から `for`/`while`/`until`〜`done` の範囲を収集
//! - [`starts_with_for`], [`starts_with_while`], [`starts_with_until`]: キーワード判定
//!
//! 対応構文:
//! ```sh
//! for var in a b c; do echo $var; done
//! while command; do body; done
//! until command; do body; done
//! # ネスト・break・continue 対応
//! ```

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::io::IntoRawFd;

use crate::builtins;
use crate::glob;
use crate::job;
use crate::parser::{self, CommandList, Connector, Pipeline, RedirectKind};
use crate::shell::Shell;
use crate::spawn;

/// コマンド置換 + チルダ展開 + ブレース展開 + glob 展開を統一的に適用する。
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
        // 3. ブレース展開
        let brace_expanded = expand_braces(&tilde_expanded);
        // 4. glob 展開
        for word in &brace_expanded {
            if glob::has_glob_chars(word) {
                result.extend(glob::expand(word));
            } else {
                result.push(word.clone());
            }
        }
    }
    result
}

/// ブレース展開: `{a,b,c}` → カンマ区切り、`{1..5}` → 数値レンジ、`{a..z}` → 文字レンジ。
/// ネスト対応（再帰展開）。
fn expand_braces(word: &str) -> Vec<String> {
    let bytes = word.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        if bytes[i] == b'{' {
            // 対応する `}` を探す
            let mut depth = 1;
            let mut j = i + 1;
            while j < len {
                match bytes[j] {
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 { break; }
                    }
                    _ => {}
                }
                j += 1;
            }

            if depth == 0 {
                let prefix = &word[..i];
                let inner = &word[i + 1..j];
                let suffix = &word[j + 1..];

                // レンジ展開を試す
                if let Some(items) = try_expand_range(inner) {
                    let mut results = Vec::new();
                    for item in items {
                        let combined = format!("{}{}{}", prefix, item, suffix);
                        results.extend(expand_braces(&combined));
                    }
                    return results;
                }

                // カンマ区切り展開を試す
                let parts = split_brace_commas(inner);
                if parts.len() >= 2 {
                    let mut results = Vec::new();
                    for part in parts {
                        let combined = format!("{}{}{}", prefix, part, suffix);
                        results.extend(expand_braces(&combined));
                    }
                    return results;
                }
            }
        }
        i += 1;
    }

    vec![word.to_string()]
}

/// ブレース内のカンマ区切り分割（ネストされた `{...}` 内のカンマは無視）。
fn split_brace_commas(content: &str) -> Vec<&str> {
    let bytes = content.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0;
    let mut depth = 0;

    for i in 0..bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => { if depth > 0 { depth -= 1; } }
            b',' if depth == 0 => {
                parts.push(&content[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&content[start..]);
    parts
}

/// レンジ展開を試みる。`a..z` → 文字レンジ、`1..5` → 数値レンジ。
/// 有効なレンジでなければ `None` を返す。
fn try_expand_range(inner: &str) -> Option<Vec<String>> {
    let sep = inner.find("..")?;
    let start_s = &inner[..sep];
    let end_s = &inner[sep + 2..];

    // 二重 `..` が含まれる場合はレンジとして扱わない
    if end_s.contains("..") {
        return None;
    }

    // 文字レンジ: 単一文字 .. 単一文字
    if start_s.len() == 1 && end_s.len() == 1 {
        let s = start_s.as_bytes()[0];
        let e = end_s.as_bytes()[0];
        if s.is_ascii_alphabetic() && e.is_ascii_alphabetic() {
            let mut results = Vec::new();
            if s <= e {
                for c in s..=e {
                    results.push((c as char).to_string());
                }
            } else {
                for c in (e..=s).rev() {
                    results.push((c as char).to_string());
                }
            }
            return Some(results);
        }
    }

    // 数値レンジ
    let s_val = start_s.parse::<i64>().ok()?;
    let e_val = end_s.parse::<i64>().ok()?;

    // ゼロパディング検出
    let pad = if (start_s.starts_with('0') && start_s.len() > 1)
        || (end_s.starts_with('0') && end_s.len() > 1) {
        start_s.len().max(end_s.len())
    } else {
        0
    };

    let mut results = Vec::new();
    if s_val <= e_val {
        for n in s_val..=e_val {
            if pad > 0 {
                results.push(format!("{:0>width$}", n, width = pad));
            } else {
                results.push(n.to_string());
            }
        }
    } else {
        for n in (e_val..=s_val).rev() {
            if pad > 0 {
                results.push(format!("{:0>width$}", n, width = pad));
            } else {
                results.push(n.to_string());
            }
        }
    }

    Some(results)
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

        // 代入のみ（コマンドなし）→ シェル環境に設定
        if cmd.args.is_empty() && !cmd.assignments.is_empty() {
            for (name, value) in &cmd.assignments {
                std::env::set_var(name, value);
            }
            return 0;
        }

        // FdDup があれば spawn パスにフォールバック
        let has_fd_dup = cmd.redirects.iter().any(|r| matches!(r.kind, RedirectKind::FdDup { .. }));
        if !has_fd_dup {
            let expanded = expand_args_full(&cmd.args, shell);
            let args: Vec<&str> = expanded.iter().map(|s| s.as_str()).collect();
            if !args.is_empty() && builtins::is_builtin(args[0]) {
                // ビルトイン: 代入を一時的にシェル環境に設定し、実行後に復元
                let saved: Vec<(String, Option<String>)> = cmd.assignments.iter()
                    .map(|(k, v)| {
                        let old = std::env::var(k).ok();
                        std::env::set_var(k, v);
                        (k.clone(), old)
                    })
                    .collect();
                let status = execute_builtin(shell, cmd, &expanded);
                for (k, old) in saved {
                    match old {
                        Some(v) => std::env::set_var(&k, &v),
                        None => std::env::remove_var(&k),
                    }
                }
                return status;
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
            RedirectKind::StderrAppend => {
                if let Some(old) = fds.stderr_fd {
                    unsafe { libc::close(old); }
                }
                let f = OpenOptions::new().create(true).append(true).open(target).map_err(|e| {
                    eprintln!("rush: {}: {}", target, e);
                    1
                })?;
                fds.stderr_fd = Some(f.into_raw_fd());
            }
            RedirectKind::FdDup { src_fd, dst_fd } => {
                fds.dup_actions.push((src_fd, dst_fd));
            }
            RedirectKind::HereDoc => {
                // <<DELIM — target にはデリミタ文字列が入っている
                // REPL の継続行入力で本体が蓄積されているはずだが、
                // 非インタラクティブ実行時は target に本体テキストが入る
                if let Some(old) = fds.stdin_fd {
                    unsafe { libc::close(old); }
                }
                let fd = create_pipe_from_string(target);
                fds.stdin_fd = Some(fd);
            }
            RedirectKind::HereString => {
                // <<<word — word + 改行を stdin に供給
                if let Some(old) = fds.stdin_fd {
                    unsafe { libc::close(old); }
                }
                let content = format!("{}\n", target);
                let fd = create_pipe_from_string(&content);
                fds.stdin_fd = Some(fd);
            }
        }
    }

    Ok(fds)
}

/// 文字列をパイプの書き込み側に書き込み、読み取り側の fd を返す。
/// ヒアドキュメント・ヒアストリング用。
fn create_pipe_from_string(content: &str) -> i32 {
    let mut pipe_fds: [i32; 2] = [0; 2];
    unsafe { libc::pipe(pipe_fds.as_mut_ptr()); }
    let read_fd = pipe_fds[0];
    let write_fd = pipe_fds[1];
    let bytes = content.as_bytes();
    unsafe {
        libc::write(write_fd, bytes.as_ptr() as *const libc::c_void, bytes.len());
        libc::close(write_fd);
    }
    read_fd
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

        // インライン代入を環境変数に設定（子プロセスに継承される）
        let saved_env: Vec<(String, Option<String>)> = cmd.assignments.iter()
            .map(|(k, v)| {
                let old = std::env::var(k).ok();
                std::env::set_var(k, v);
                (k.clone(), old)
            })
            .collect();

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

        // インライン代入を復元（子プロセスにのみ影響させるため）
        for (k, old) in saved_env {
            match old {
                Some(v) => std::env::set_var(&k, &v),
                None => std::env::remove_var(&k),
            }
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
        shell.last_bg_pid = pgid;
        std::env::set_var("RUSH_LAST_BG_PID", pgid.to_string());
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

// ── if/then/elif/else/fi 複合コマンド ────────────────────────────────

/// テキストベースで if ブロック全体を解釈・実行する。
///
/// 処理フロー:
/// 1. トークン列を `if` / `then` / `elif` / `else` / `fi` で分割
/// 2. 条件部分をパース＆実行し、ステータス 0 なら本体を実行
/// 3. elif チェーン、else 部分を順に評価
/// 4. 最終ステータスを返す（どの分岐も実行されなければ 0）
pub fn execute_if_block(shell: &mut Shell, block: &str) -> i32 {
    // if ブロックをセクションに分割
    let sections = match parse_if_sections(block) {
        Ok(s) => s,
        Err(msg) => {
            eprintln!("rush: {}", msg);
            return 2;
        }
    };

    // if 条件を評価
    let cond_status = run_command_string(shell, &sections.condition);
    if cond_status == 0 {
        return run_command_string(shell, &sections.then_body);
    }

    // elif チェーン
    for (elif_cond, elif_body) in &sections.elif_parts {
        let s = run_command_string(shell, elif_cond);
        if s == 0 {
            return run_command_string(shell, elif_body);
        }
    }

    // else
    if let Some(ref else_body) = sections.else_body {
        return run_command_string(shell, else_body);
    }

    0
}

// ── for/while/until ループ ─────────────────────────────────────────

/// `for VAR in WORDS...; do BODY; done` ブロックを解釈・実行する。
///
/// 処理フロー:
/// 1. `for VAR [in WORDS...]` と `do ... done` の間を分割
/// 2. `in` がなければ `"$@"` 相当（現在は空リスト）
/// 3. WORDS を展開し、各要素で VAR に代入して BODY を実行
/// 4. `break`/`continue` を適切にハンドリング
pub fn execute_for_block(shell: &mut Shell, block: &str) -> i32 {
    let tokens = tokenize_block(block);

    // for VAR in words... ; do body ; done を解析
    let mut var_name = String::new();
    let mut word_tokens: Vec<String> = Vec::new();
    let mut body_tokens: Vec<String> = Vec::new();
    let mut depth = 0i32;

    #[derive(PartialEq)]
    enum State { BeforeFor, InHeader, InBody }
    let mut state = State::BeforeFor;

    for token in &tokens {
        let trimmed = token.trim();
        if trimmed.is_empty() {
            continue;
        }
        let kw = extract_keyword(trimmed);

        match state {
            State::BeforeFor => {
                if let Some("for") = kw {
                    state = State::InHeader;
                    let after = trimmed.strip_prefix("for").unwrap().trim();
                    if !after.is_empty() {
                        // "for VAR in a b c" or "for VAR"
                        let parts: Vec<&str> = after.splitn(2, char::is_whitespace).collect();
                        var_name = parts[0].to_string();
                        if parts.len() > 1 {
                            let rest = parts[1].trim();
                            if let Some(after_in) = rest.strip_prefix("in") {
                                let words_str = after_in.trim();
                                if !words_str.is_empty() {
                                    for w in words_str.split_whitespace() {
                                        word_tokens.push(w.to_string());
                                    }
                                }
                            }
                        }
                    }
                }
            }
            State::InHeader => {
                if let Some("do") = kw {
                    state = State::InBody;
                    let after_do = trimmed.strip_prefix("do").unwrap().trim();
                    if !after_do.is_empty() {
                        body_tokens.push(after_do.to_string());
                    }
                } else {
                    // 変数名 or in words
                    if var_name.is_empty() {
                        let parts: Vec<&str> = trimmed.splitn(2, char::is_whitespace).collect();
                        var_name = parts[0].to_string();
                        if parts.len() > 1 {
                            let rest = parts[1].trim();
                            if let Some(after_in) = rest.strip_prefix("in") {
                                for w in after_in.trim().split_whitespace() {
                                    word_tokens.push(w.to_string());
                                }
                            }
                        }
                    } else if let Some(after_in) = trimmed.strip_prefix("in") {
                        for w in after_in.trim().split_whitespace() {
                            word_tokens.push(w.to_string());
                        }
                    } else {
                        for w in trimmed.split_whitespace() {
                            word_tokens.push(w.to_string());
                        }
                    }
                }
            }
            State::InBody => {
                match kw {
                    Some("for") | Some("while") | Some("until") => {
                        depth += 1;
                        body_tokens.push(trimmed.to_string());
                    }
                    Some("done") if depth > 0 => {
                        depth -= 1;
                        body_tokens.push(trimmed.to_string());
                    }
                    Some("done") => {
                        break;
                    }
                    _ => {
                        body_tokens.push(trimmed.to_string());
                    }
                }
            }
        }
    }

    if var_name.is_empty() {
        eprintln!("rush: syntax error: missing variable name in `for`");
        return 2;
    }

    let body = body_tokens.join("\n");

    // word_tokens を展開（コマンド置換、チルダ、ブレース、glob）
    let expanded_words: Vec<String> = if word_tokens.is_empty() {
        Vec::new()
    } else {
        let cow_words: Vec<std::borrow::Cow<'_, str>> = word_tokens.iter()
            .map(|s| std::borrow::Cow::Owned(s.clone()))
            .collect();
        expand_args_full(&cow_words, shell)
    };

    let mut last_status = 0;
    shell.loop_depth += 1;

    for word in &expanded_words {
        std::env::set_var(&var_name, word);
        last_status = run_command_string(shell, &body);
        shell.last_status = last_status;

        // break チェック
        if shell.break_level > 0 {
            shell.break_level -= 1;
            break;
        }
        // continue チェック
        if shell.continue_level > 0 {
            shell.continue_level -= 1;
            if shell.continue_level > 0 {
                // 外側ループの continue
                break;
            }
            continue;
        }
        if shell.should_return || shell.should_exit {
            break;
        }
    }

    shell.loop_depth -= 1;
    last_status
}

/// `while COND; do BODY; done` / `until COND; do BODY; done` ブロックを解釈・実行する。
///
/// `is_until=true` のとき until ループ（条件が偽の間ループ継続）。
/// `is_until=false` のとき while ループ（条件が真の間ループ継続）。
pub fn execute_while_block(shell: &mut Shell, block: &str, is_until: bool) -> i32 {
    let tokens = tokenize_block(block);

    // while/until COND; do BODY; done を解析
    let mut cond_tokens: Vec<String> = Vec::new();
    let mut body_tokens: Vec<String> = Vec::new();
    let mut depth = 0i32;

    #[derive(PartialEq)]
    enum State { BeforeKw, InCond, InBody }
    let mut state = State::BeforeKw;

    for token in &tokens {
        let trimmed = token.trim();
        if trimmed.is_empty() {
            continue;
        }
        let kw = extract_keyword(trimmed);

        match state {
            State::BeforeKw => {
                if let Some("while") | Some("until") = kw {
                    state = State::InCond;
                    let prefix = if trimmed.starts_with("while") { "while" } else { "until" };
                    let after = trimmed.strip_prefix(prefix).unwrap().trim();
                    if !after.is_empty() {
                        cond_tokens.push(after.to_string());
                    }
                }
            }
            State::InCond => {
                if let Some("do") = kw {
                    state = State::InBody;
                    let after_do = trimmed.strip_prefix("do").unwrap().trim();
                    if !after_do.is_empty() {
                        body_tokens.push(after_do.to_string());
                    }
                } else {
                    cond_tokens.push(trimmed.to_string());
                }
            }
            State::InBody => {
                match kw {
                    Some("for") | Some("while") | Some("until") => {
                        depth += 1;
                        body_tokens.push(trimmed.to_string());
                    }
                    Some("done") if depth > 0 => {
                        depth -= 1;
                        body_tokens.push(trimmed.to_string());
                    }
                    Some("done") => {
                        break;
                    }
                    _ => {
                        body_tokens.push(trimmed.to_string());
                    }
                }
            }
        }
    }

    let cond = cond_tokens.join("\n");
    let body = body_tokens.join("\n");

    if cond.is_empty() {
        eprintln!("rush: syntax error: missing condition in `{}`",
            if is_until { "until" } else { "while" });
        return 2;
    }

    let mut last_status = 0;
    shell.loop_depth += 1;

    loop {
        let cond_status = run_command_string(shell, &cond);
        let should_run = if is_until { cond_status != 0 } else { cond_status == 0 };
        if !should_run {
            break;
        }

        last_status = run_command_string(shell, &body);
        shell.last_status = last_status;

        if shell.break_level > 0 {
            shell.break_level -= 1;
            break;
        }
        if shell.continue_level > 0 {
            shell.continue_level -= 1;
            if shell.continue_level > 0 {
                break;
            }
            continue;
        }
        if shell.should_return || shell.should_exit {
            break;
        }
    }

    shell.loop_depth -= 1;
    last_status
}

/// if ブロックの各セクションを保持する構造体。
///
/// [`parse_if_sections`] が if ブロックテキストを解析した結果を格納する。
/// 各フィールドは実行可能なコマンド文字列（改行区切り）。
struct IfSections {
    /// `if` と `then` の間の条件コマンド。
    condition: String,
    /// `then` 以降（`elif`/`else`/`fi` の前）の本体コマンド。
    then_body: String,
    /// `elif` 部分のリスト。各要素は `(条件, 本体)` のペア。
    elif_parts: Vec<(String, String)>,
    /// `else` 部分。存在しない場合は `None`。
    else_body: Option<String>,
}

/// if ブロックのテキストを解析し、各セクション（condition, then, elif, else）に分割する。
///
/// 入力は `if ... fi` の完全なブロック。ネストした if/fi も正しく追跡する。
fn parse_if_sections(block: &str) -> Result<IfSections, String> {
    let tokens = tokenize_block(block);

    let mut depth = 0i32;
    let mut state = IfParseState::BeforeIf;
    let mut condition = String::new();
    let mut then_body = String::new();
    let mut elif_parts: Vec<(String, String)> = Vec::new();
    let mut else_body: Option<String> = None;
    let mut current_elif_cond = String::new();

    for token in &tokens {
        let trimmed = token.trim();
        if trimmed.is_empty() {
            continue;
        }

        let keyword = extract_keyword(trimmed);

        match keyword {
            Some("if") if matches!(state, IfParseState::BeforeIf) => {
                state = IfParseState::InCondition;
                let after_if = trimmed.strip_prefix("if").unwrap().trim();
                if !after_if.is_empty() {
                    append_to_section(&mut state, after_if, &mut condition, &mut then_body,
                        &mut elif_parts, &mut else_body, &mut current_elif_cond);
                }
            }
            Some("if") => {
                // ネストした if
                depth += 1;
                append_to_section(&mut state, trimmed, &mut condition, &mut then_body,
                    &mut elif_parts, &mut else_body, &mut current_elif_cond);
            }
            Some("fi") if depth > 0 => {
                depth -= 1;
                append_to_section(&mut state, trimmed, &mut condition, &mut then_body,
                    &mut elif_parts, &mut else_body, &mut current_elif_cond);
            }
            Some("fi") => {
                // ブロック終了
                break;
            }
            Some("then") if depth == 0 => {
                match state {
                    IfParseState::InCondition => {
                        state = IfParseState::InThenBody;
                        let after_then = trimmed.strip_prefix("then").unwrap().trim();
                        if !after_then.is_empty() {
                            append_to_section(&mut state, after_then, &mut condition, &mut then_body,
                                &mut elif_parts, &mut else_body, &mut current_elif_cond);
                        }
                    }
                    IfParseState::InElifCond => {
                        state = IfParseState::InElifBody;
                        elif_parts.push((current_elif_cond.clone(), String::new()));
                        current_elif_cond.clear();
                        let after_then = trimmed.strip_prefix("then").unwrap().trim();
                        if !after_then.is_empty() {
                            append_to_section(&mut state, after_then, &mut condition, &mut then_body,
                                &mut elif_parts, &mut else_body, &mut current_elif_cond);
                        }
                    }
                    _ => {
                        append_to_section(&mut state, trimmed, &mut condition, &mut then_body,
                            &mut elif_parts, &mut else_body, &mut current_elif_cond);
                    }
                }
            }
            Some("elif") if depth == 0 => {
                state = IfParseState::InElifCond;
                let after_elif = trimmed.strip_prefix("elif").unwrap().trim();
                if !after_elif.is_empty() {
                    append_to_section(&mut state, after_elif, &mut condition, &mut then_body,
                        &mut elif_parts, &mut else_body, &mut current_elif_cond);
                }
            }
            Some("else") if depth == 0 => {
                state = IfParseState::InElseBody;
                else_body = Some(String::new());
                let after_else = trimmed.strip_prefix("else").unwrap().trim();
                if !after_else.is_empty() {
                    append_to_section(&mut state, after_else, &mut condition, &mut then_body,
                        &mut elif_parts, &mut else_body, &mut current_elif_cond);
                }
            }
            _ => {
                append_to_section(&mut state, trimmed, &mut condition, &mut then_body,
                    &mut elif_parts, &mut else_body, &mut current_elif_cond);
            }
        }
    }

    if condition.is_empty() {
        return Err("syntax error: missing condition after `if`".to_string());
    }
    if matches!(state, IfParseState::InCondition | IfParseState::BeforeIf) {
        return Err("syntax error: missing `then` keyword".to_string());
    }

    Ok(IfSections {
        condition,
        then_body,
        elif_parts,
        else_body,
    })
}

/// [`parse_if_sections`] の状態遷移マシン。
///
/// トークンを走査しながら以下の順で遷移する:
/// `BeforeIf` → `InCondition` → `InThenBody` → (`InElifCond` → `InElifBody`)* → `InElseBody`?
enum IfParseState {
    /// `if` キーワード出現前の初期状態。
    BeforeIf,
    /// `if` と `then` の間（条件部分を蓄積中）。
    InCondition,
    /// `then` 以降の本体を蓄積中。
    InThenBody,
    /// `elif` と次の `then` の間（elif 条件を蓄積中）。
    InElifCond,
    /// `elif ... then` 以降の本体を蓄積中。
    InElifBody,
    /// `else` 以降の本体を蓄積中。
    InElseBody,
}

/// 現在の解析状態に応じて、適切なセクション文字列の末尾にテキストを追加する。
///
/// 複数行のセクション内容は `\n` で連結される。
fn append_to_section(
    state: &mut IfParseState,
    text: &str,
    condition: &mut String,
    then_body: &mut String,
    elif_parts: &mut Vec<(String, String)>,
    else_body: &mut Option<String>,
    current_elif_cond: &mut String,
) {
    let target = match state {
        IfParseState::BeforeIf => return,
        IfParseState::InCondition => condition,
        IfParseState::InThenBody => then_body,
        IfParseState::InElifCond => current_elif_cond,
        IfParseState::InElifBody => {
            if let Some((_, ref mut body)) = elif_parts.last_mut() {
                body
            } else {
                return;
            }
        }
        IfParseState::InElseBody => {
            if let Some(ref mut body) = else_body {
                body
            } else {
                return;
            }
        }
    };
    if !target.is_empty() {
        target.push('\n');
    }
    target.push_str(text);
}

/// if ブロックテキストをステートメント単位のトークン列に分割する。
///
/// `;` と改行 (`\n`) を区切りとして使い、各トークンは 1 つのステートメント
/// （キーワード付き or コマンド）に対応する。
/// シングル/ダブルクォート内の `;` や改行は区切りとして扱わない。
///
/// 例: `"if true; then echo yes; fi"` → `["if true", "then echo yes", "fi"]`
fn tokenize_block(block: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let bytes = block.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        match bytes[i] {
            b'\'' => {
                current.push('\'');
                i += 1;
                while i < len && bytes[i] != b'\'' {
                    current.push(bytes[i] as char);
                    i += 1;
                }
                if i < len {
                    current.push('\'');
                    i += 1;
                }
            }
            b'"' => {
                current.push('"');
                i += 1;
                while i < len && bytes[i] != b'"' {
                    if bytes[i] == b'\\' && i + 1 < len {
                        current.push(bytes[i] as char);
                        i += 1;
                        current.push(bytes[i] as char);
                        i += 1;
                    } else {
                        current.push(bytes[i] as char);
                        i += 1;
                    }
                }
                if i < len {
                    current.push('"');
                    i += 1;
                }
            }
            b'\n' | b';' => {
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() {
                    tokens.push(trimmed);
                }
                current.clear();
                i += 1;
            }
            _ => {
                current.push(bytes[i] as char);
                i += 1;
            }
        }
    }

    let trimmed = current.trim().to_string();
    if !trimmed.is_empty() {
        tokens.push(trimmed);
    }

    tokens
}

/// トークンの先頭がシェルキーワード (`if`, `then`, `elif`, `else`, `fi`) かを判定する。
///
/// キーワードがトークンと完全一致するか、キーワードの直後に空白または `;` が
/// 続く場合にキーワードとして認識する。`ifdef` や `finally` のような
/// キーワードを含む非キーワードは認識しない。
fn extract_keyword(token: &str) -> Option<&'static str> {
    for kw in &["if", "then", "elif", "else", "fi", "for", "while", "until", "do", "done", "in"] {
        if token == *kw {
            return Some(kw);
        }
        if token.starts_with(kw) {
            let rest = &token[kw.len()..];
            if rest.starts_with(char::is_whitespace) || rest.starts_with(';') {
                return Some(kw);
            }
        }
    }
    None
}

/// コマンド文字列を行単位でパース・実行する内部ヘルパー。
///
/// if ブロックの条件部分・本体部分を実行するために使用する。
/// 複数行入力・ネストした if ブロックにも対応し、各行を
/// [`parser::parse`] → [`execute`] で順次実行する。
/// 最後に実行されたコマンドの終了ステータスを返す。
fn run_command_string(shell: &mut Shell, input: &str) -> i32 {
    let lines: Vec<&str> = input.lines().collect();
    let mut last_status = 0;
    let mut i = 0;

    while i < lines.len() {
        let trimmed = lines[i].trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            i += 1;
            continue;
        }

        // ネストした if ブロックを検出
        if starts_with_if(trimmed) {
            let (block, next_i) = collect_if_block(&lines, i);
            last_status = execute_if_block(shell, &block);
            shell.last_status = last_status;
            i = next_i;
            if shell.should_return || shell.should_exit
                || shell.break_level > 0 || shell.continue_level > 0
            {
                return last_status;
            }
            continue;
        }

        // ネストした for/while/until ブロックを検出
        if starts_with_for(trimmed) || starts_with_while(trimmed) || starts_with_until(trimmed) {
            let (block, next_i) = collect_loop_block(&lines, i);
            if starts_with_for(trimmed) {
                last_status = execute_for_block(shell, &block);
            } else {
                last_status = execute_while_block(shell, &block, starts_with_until(trimmed));
            }
            shell.last_status = last_status;
            i = next_i;
            if shell.should_return || shell.should_exit
                || shell.break_level > 0 || shell.continue_level > 0
            {
                return last_status;
            }
            continue;
        }

        match parser::parse(trimmed, shell.last_status) {
            Ok(Some(list)) => {
                let cmd_text = trimmed.to_string();
                last_status = execute(shell, &list, &cmd_text);
                shell.last_status = last_status;
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("rush: {}", e);
                return 2;
            }
        }
        if shell.should_return || shell.should_exit
            || shell.break_level > 0 || shell.continue_level > 0
        {
            return last_status;
        }

        i += 1;
    }
    last_status
}

/// 行が `if` キーワードで始まるかどうかを判定する。
///
/// 先頭空白を除去した後、`if` の直後に空白・タブがあるか、
/// `if` のみの行であれば `true` を返す。
/// `ifdef` や `ifconfig` のような `if` を含む非キーワードは `false`。
pub fn starts_with_if(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed == "if" || trimmed.starts_with("if ") || trimmed.starts_with("if\t")
}

/// 行配列から if ブロック全体（`if`〜`fi`）を収集する。
///
/// `lines[start]` は最初の `if` キーワードを含む行。
/// ネストした `if`/`fi` を深さカウントで追跡し、対応する `fi` が見つかるまで
/// 行を `\n` で連結する。
///
/// 戻り値: `(収集したブロック文字列, 次に処理すべき行インデックス)`。
/// `fi` が見つからなかった場合は入力末尾まで収集する。
pub fn collect_if_block(lines: &[&str], start: usize) -> (String, usize) {
    let mut depth = 0i32;
    let mut block = String::new();
    let mut i = start;

    while i < lines.len() {
        let line = lines[i];
        if !block.is_empty() {
            block.push('\n');
        }
        block.push_str(line);

        // キーワードの出現をカウント（クォート内は無視）
        for token in shell_tokens(line.trim()) {
            match token {
                "if" => depth += 1,
                "fi" => {
                    depth -= 1;
                    if depth == 0 {
                        return (block, i + 1);
                    }
                }
                _ => {}
            }
        }

        i += 1;
    }

    (block, i)
}

/// 行が `for` キーワードで始まるかどうかを判定する。
pub fn starts_with_for(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed == "for" || trimmed.starts_with("for ") || trimmed.starts_with("for\t")
}

/// 行が `while` キーワードで始まるかどうかを判定する。
pub fn starts_with_while(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed == "while" || trimmed.starts_with("while ") || trimmed.starts_with("while\t")
}

/// 行が `until` キーワードで始まるかどうかを判定する。
pub fn starts_with_until(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed == "until" || trimmed.starts_with("until ") || trimmed.starts_with("until\t")
}

/// 行配列から for/while/until ブロック全体（`for`/`while`/`until`〜`done`）を収集する。
///
/// `lines[start]` は最初のキーワードを含む行。
/// ネストした for/while/until/done と if/fi を深さカウントで追跡し、
/// 対応する `done` が見つかるまで行を `\n` で連結する。
///
/// 戻り値: `(収集したブロック文字列, 次に処理すべき行インデックス)`。
pub fn collect_loop_block(lines: &[&str], start: usize) -> (String, usize) {
    let mut depth = 0i32;
    let mut block = String::new();
    let mut i = start;

    while i < lines.len() {
        let line = lines[i];
        if !block.is_empty() {
            block.push('\n');
        }
        block.push_str(line);

        for token in shell_tokens(line.trim()) {
            match token {
                "for" | "while" | "until" => depth += 1,
                "done" => {
                    depth -= 1;
                    if depth == 0 {
                        return (block, i + 1);
                    }
                }
                // if/fi もカウントしないとネストした if がずれる可能性があるが、
                // if/fi は do/done のカウントに影響しないため不要
                _ => {}
            }
        }

        i += 1;
    }

    (block, i)
}

/// 行をシェルトークンに分割する（公開版）。
///
/// REPL の if ブロック収集で `if`/`fi` キーワードの出現をカウントするために使用。
/// 内部の [`shell_tokens`] を呼び出す薄いラッパー。
pub fn shell_tokens_pub(line: &str) -> Vec<&str> {
    shell_tokens(line)
}

/// 行をシェルの単語単位に分割する。
///
/// キーワード（`if`, `fi` 等）の検出と if/fi のネスト深さ追跡に使用。
/// 空白とタブで単語を区切り、`;` はセパレータとして消費する。
/// シングル/ダブルクォート内のテキストは 1 トークンとしてスキップし、
/// クォート内の空白・`;` は区切りとして扱わない。
fn shell_tokens(line: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        // 空白スキップ
        while i < len && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        if i >= len {
            break;
        }

        match bytes[i] {
            b';' => {
                i += 1;
            }
            b'\'' => {
                let start = i;
                i += 1;
                while i < len && bytes[i] != b'\'' {
                    i += 1;
                }
                if i < len {
                    i += 1;
                }
                tokens.push(&line[start..i]);
            }
            b'"' => {
                let start = i;
                i += 1;
                while i < len && bytes[i] != b'"' {
                    if bytes[i] == b'\\' && i + 1 < len {
                        i += 1;
                    }
                    i += 1;
                }
                if i < len {
                    i += 1;
                }
                tokens.push(&line[start..i]);
            }
            _ => {
                let start = i;
                while i < len && bytes[i] != b' ' && bytes[i] != b'\t' && bytes[i] != b';' {
                    if bytes[i] == b'\'' {
                        i += 1;
                        while i < len && bytes[i] != b'\'' {
                            i += 1;
                        }
                        if i < len {
                            i += 1;
                        }
                    } else if bytes[i] == b'"' {
                        i += 1;
                        while i < len && bytes[i] != b'"' {
                            if bytes[i] == b'\\' && i + 1 < len {
                                i += 1;
                            }
                            i += 1;
                        }
                        if i < len {
                            i += 1;
                        }
                    } else {
                        i += 1;
                    }
                }
                tokens.push(&line[start..i]);
            }
        }
    }

    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brace_comma() {
        assert_eq!(expand_braces("file.{rs,toml}"), vec!["file.rs", "file.toml"]);
    }

    #[test]
    fn brace_three() {
        assert_eq!(expand_braces("{a,b,c}"), vec!["a", "b", "c"]);
    }

    #[test]
    fn brace_prefix_suffix() {
        assert_eq!(expand_braces("pre{x,y}suf"), vec!["prexsuf", "preysuf"]);
    }

    #[test]
    fn brace_nested() {
        assert_eq!(expand_braces("{a,{b,c}}"), vec!["a", "b", "c"]);
    }

    #[test]
    fn brace_multi() {
        assert_eq!(
            expand_braces("{a,b}{1,2}"),
            vec!["a1", "a2", "b1", "b2"],
        );
    }

    #[test]
    fn brace_numeric_range() {
        assert_eq!(expand_braces("{1..5}"), vec!["1", "2", "3", "4", "5"]);
    }

    #[test]
    fn brace_reverse_range() {
        assert_eq!(expand_braces("{5..1}"), vec!["5", "4", "3", "2", "1"]);
    }

    #[test]
    fn brace_char_range() {
        assert_eq!(expand_braces("{a..e}"), vec!["a", "b", "c", "d", "e"]);
    }

    #[test]
    fn brace_zero_pad() {
        assert_eq!(
            expand_braces("{01..03}"),
            vec!["01", "02", "03"],
        );
    }

    #[test]
    fn brace_no_expansion() {
        assert_eq!(expand_braces("hello"), vec!["hello"]);
        assert_eq!(expand_braces("{single}"), vec!["{single}"]);
    }

    #[test]
    fn brace_range_with_prefix() {
        assert_eq!(
            expand_braces("file{1..3}.txt"),
            vec!["file1.txt", "file2.txt", "file3.txt"],
        );
    }

    // ── if/then/fi テスト ────────────────────────────────────────────

    #[test]
    fn starts_with_if_basic() {
        assert!(starts_with_if("if true; then echo yes; fi"));
        assert!(starts_with_if("if false; then echo no; fi"));
        assert!(starts_with_if("  if true; then echo yes; fi"));
        assert!(!starts_with_if("echo if"));
        assert!(!starts_with_if("ifdef"));
        assert!(!starts_with_if("ifconfig"));
    }

    #[test]
    fn shell_tokens_basic() {
        assert_eq!(shell_tokens("if true; then echo yes; fi"),
            vec!["if", "true", "then", "echo", "yes", "fi"]);
    }

    #[test]
    fn shell_tokens_quoted() {
        assert_eq!(shell_tokens("echo 'hello world'"),
            vec!["echo", "'hello world'"]);
        assert_eq!(shell_tokens("echo \"hello world\""),
            vec!["echo", "\"hello world\""]);
    }

    #[test]
    fn collect_if_block_oneliner() {
        let lines = vec!["if true; then echo yes; fi"];
        let (block, next) = collect_if_block(&lines, 0);
        assert_eq!(block, "if true; then echo yes; fi");
        assert_eq!(next, 1);
    }

    #[test]
    fn collect_if_block_multiline() {
        let lines = vec!["if true", "then", "echo yes", "fi", "echo after"];
        let (block, next) = collect_if_block(&lines, 0);
        assert_eq!(block, "if true\nthen\necho yes\nfi");
        assert_eq!(next, 4);
    }

    #[test]
    fn collect_if_block_nested() {
        let lines = vec![
            "if true; then",
            "  if false; then",
            "    echo inner",
            "  fi",
            "fi",
            "echo after",
        ];
        let (block, next) = collect_if_block(&lines, 0);
        assert!(block.contains("fi\nfi"));
        assert_eq!(next, 5);
    }

    #[test]
    fn parse_if_sections_basic() {
        let sections = parse_if_sections("if true; then echo yes; fi").unwrap();
        assert_eq!(sections.condition.trim(), "true");
        assert_eq!(sections.then_body.trim(), "echo yes");
        assert!(sections.elif_parts.is_empty());
        assert!(sections.else_body.is_none());
    }

    #[test]
    fn parse_if_sections_else() {
        let sections = parse_if_sections("if false; then echo no; else echo yes; fi").unwrap();
        assert_eq!(sections.condition.trim(), "false");
        assert_eq!(sections.then_body.trim(), "echo no");
        assert!(sections.elif_parts.is_empty());
        assert_eq!(sections.else_body.as_deref().unwrap().trim(), "echo yes");
    }

    #[test]
    fn parse_if_sections_elif() {
        let sections = parse_if_sections(
            "if false; then echo first; elif true; then echo second; else echo third; fi"
        ).unwrap();
        assert_eq!(sections.condition.trim(), "false");
        assert_eq!(sections.then_body.trim(), "echo first");
        assert_eq!(sections.elif_parts.len(), 1);
        assert_eq!(sections.elif_parts[0].0.trim(), "true");
        assert_eq!(sections.elif_parts[0].1.trim(), "echo second");
        assert_eq!(sections.else_body.as_deref().unwrap().trim(), "echo third");
    }

    #[test]
    fn parse_if_sections_multiline() {
        let block = "if true\nthen\necho hello\necho world\nfi";
        let sections = parse_if_sections(block).unwrap();
        assert_eq!(sections.condition.trim(), "true");
        assert!(sections.then_body.contains("echo hello"));
        assert!(sections.then_body.contains("echo world"));
    }

    #[test]
    fn execute_if_block_true() {
        let mut shell = Shell::new();
        let status = execute_if_block(&mut shell, "if true; then echo yes; fi");
        assert_eq!(status, 0);
    }

    #[test]
    fn execute_if_block_false_with_else() {
        let mut shell = Shell::new();
        let status = execute_if_block(&mut shell, "if false; then echo no; else true; fi");
        assert_eq!(status, 0);
    }

    #[test]
    fn execute_if_block_false_no_else() {
        let mut shell = Shell::new();
        let status = execute_if_block(&mut shell, "if false; then echo no; fi");
        assert_eq!(status, 0); // no branch executed → 0
    }

    #[test]
    fn execute_if_block_elif() {
        let mut shell = Shell::new();
        let status = execute_if_block(&mut shell,
            "if false; then false; elif true; then true; fi");
        assert_eq!(status, 0);
    }

    #[test]
    fn tokenize_block_basic() {
        let tokens = tokenize_block("if true; then echo yes; fi");
        assert_eq!(tokens, vec!["if true", "then echo yes", "fi"]);
    }

    #[test]
    fn tokenize_block_multiline() {
        let tokens = tokenize_block("if true\nthen\necho yes\nfi");
        assert_eq!(tokens, vec!["if true", "then", "echo yes", "fi"]);
    }

    #[test]
    fn tokenize_block_quoted_semicolons() {
        let tokens = tokenize_block("echo 'hello;world'; echo done");
        assert_eq!(tokens, vec!["echo 'hello;world'", "echo done"]);
    }

    #[test]
    fn extract_keyword_basic() {
        assert_eq!(extract_keyword("if"), Some("if"));
        assert_eq!(extract_keyword("if true"), Some("if"));
        assert_eq!(extract_keyword("then"), Some("then"));
        assert_eq!(extract_keyword("then echo"), Some("then"));
        assert_eq!(extract_keyword("elif"), Some("elif"));
        assert_eq!(extract_keyword("else"), Some("else"));
        assert_eq!(extract_keyword("fi"), Some("fi"));
        assert_eq!(extract_keyword("echo"), None);
        assert_eq!(extract_keyword("ifdef"), None);
        assert_eq!(extract_keyword("finally"), None);
        assert_eq!(extract_keyword("for"), Some("for"));
        assert_eq!(extract_keyword("for x"), Some("for"));
        assert_eq!(extract_keyword("while"), Some("while"));
        assert_eq!(extract_keyword("until"), Some("until"));
        assert_eq!(extract_keyword("do"), Some("do"));
        assert_eq!(extract_keyword("done"), Some("done"));
        assert_eq!(extract_keyword("in"), Some("in"));
        assert_eq!(extract_keyword("foreach"), None);
        assert_eq!(extract_keyword("donut"), None);
    }

    // ── for/while/until ループテスト ──────────────────────────────────

    #[test]
    fn starts_with_for_basic() {
        assert!(starts_with_for("for x in a b c; do echo $x; done"));
        assert!(starts_with_for("  for i in 1 2 3"));
        assert!(!starts_with_for("echo for"));
        assert!(!starts_with_for("foreach"));
        assert!(!starts_with_for("fortune"));
    }

    #[test]
    fn starts_with_while_basic() {
        assert!(starts_with_while("while true; do echo hi; done"));
        assert!(starts_with_while("  while [ 1 ]"));
        assert!(!starts_with_while("echo while"));
        assert!(!starts_with_while("whileloop"));
    }

    #[test]
    fn starts_with_until_basic() {
        assert!(starts_with_until("until false; do echo hi; done"));
        assert!(!starts_with_until("echo until"));
        assert!(!starts_with_until("untilnow"));
    }

    #[test]
    fn collect_loop_block_oneliner() {
        let lines = vec!["for x in a b; do echo $x; done"];
        let (block, next) = collect_loop_block(&lines, 0);
        assert_eq!(block, "for x in a b; do echo $x; done");
        assert_eq!(next, 1);
    }

    #[test]
    fn collect_loop_block_multiline() {
        let lines = vec!["for x in a b", "do", "echo $x", "done", "echo after"];
        let (block, next) = collect_loop_block(&lines, 0);
        assert_eq!(block, "for x in a b\ndo\necho $x\ndone");
        assert_eq!(next, 4);
    }

    #[test]
    fn collect_loop_block_nested() {
        let lines = vec![
            "for i in a b; do",
            "  for j in 1 2; do",
            "    echo $i$j",
            "  done",
            "done",
            "echo after",
        ];
        let (block, next) = collect_loop_block(&lines, 0);
        assert!(block.contains("done\ndone"));
        assert_eq!(next, 5);
    }

    #[test]
    fn execute_for_block_basic() {
        let mut shell = Shell::new();
        let status = execute_for_block(&mut shell, "for x in a b c; do true; done");
        assert_eq!(status, 0);
        // x should be set to the last value
        assert_eq!(std::env::var("x").unwrap_or_default(), "c");
    }

    #[test]
    fn execute_for_block_empty_list() {
        let mut shell = Shell::new();
        let status = execute_for_block(&mut shell, "for x in; do echo $x; done");
        assert_eq!(status, 0); // no iterations
    }

    #[test]
    fn execute_while_block_basic() {
        let mut shell = Shell::new();
        std::env::set_var("RUSH_WHILE_TEST", "3");
        let block = "while [ $RUSH_WHILE_TEST -gt 0 ]; do\nexport RUSH_WHILE_TEST=$(( RUSH_WHILE_TEST - 1 ))\ndone";
        let status = execute_while_block(&mut shell, block, false);
        assert_eq!(status, 0);
        assert_eq!(std::env::var("RUSH_WHILE_TEST").unwrap(), "0");
        std::env::remove_var("RUSH_WHILE_TEST");
    }

    #[test]
    fn execute_until_block_basic() {
        let mut shell = Shell::new();
        std::env::set_var("RUSH_UNTIL_TEST", "0");
        let block = "until [ $RUSH_UNTIL_TEST -eq 3 ]; do\nexport RUSH_UNTIL_TEST=$(( RUSH_UNTIL_TEST + 1 ))\ndone";
        let status = execute_while_block(&mut shell, block, true);
        assert_eq!(status, 0);
        assert_eq!(std::env::var("RUSH_UNTIL_TEST").unwrap(), "3");
        std::env::remove_var("RUSH_UNTIL_TEST");
    }

    #[test]
    fn execute_for_block_with_break() {
        let mut shell = Shell::new();
        // break after first iteration
        let block = "for x in a b c; do\nbreak\ndone";
        let status = execute_for_block(&mut shell, block);
        assert_eq!(status, 0);
        assert_eq!(std::env::var("x").unwrap_or_default(), "a");
    }
}
