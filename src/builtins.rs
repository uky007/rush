//! ビルトインコマンドの実装。
//!
//! ビルトインはfork/execを経由せずプロセス内で直接実行されるため高速。
//! `try_exec()` が `Some(status)` を返せばビルトインとして処理済み、
//! `None` なら外部コマンドとしてexecutorに委ねる。
//!
//! ## 対応ビルトイン（23 種）
//!
//! - シェル制御: `exit`, `cd`（`cd -` / OLDPWD 対応）, `exec`
//! - 出力: `pwd`, `echo`（`-n` 対応）
//! - 環境変数: `export`, `unset`, `read`（`-p` プロンプト、IFS 分割、`REPLY`）
//! - ジョブコントロール: `jobs`, `fg`, `bg`, `wait`
//! - エイリアス: `alias`, `unalias`（`-a` 全削除）
//! - スクリプト: `source` / `.`（ファイル行単位実行）
//! - 情報: `type`
//! - 実行制御: `command`（`-v` パス表示、エイリアスバイパス）, `builtin`（ビルトイン限定実行）
//! - フロー制御: `true` / `:`（常に 0）, `false`（常に 1）, `return`（source からの早期脱出）
//! - 条件判定: `test` / `[`（文字列・整数・ファイル判定、`!` 否定）
//! - 出力: `printf`（`%s`, `%d`, `%x`, `%o`, 幅指定、ゼロパディング、エスケープ）
//! - 履歴: `history`（main.rs で特別扱い、`-c` クリア、`N` 件表示）

use std::env;
use std::io::Write;
use std::path::Path;

use crate::job::{self, JobStatus};
use crate::shell::Shell;
use crate::{executor, parser};

/// コマンド名がビルトインかどうかを判定する。
///
/// 以下の場面で使用される:
/// - [`executor`](crate::executor): ビルトイン判定 → fork なし高速パスの選択
/// - [`highlight`](crate::highlight): コマンドの有効性判定（緑/赤の着色）
/// - [`complete`](crate::complete): ビルトイン名のリストを補完候補に使用（`BUILTINS` 定数と同期）
pub fn is_builtin(name: &str) -> bool {
    matches!(name, "exit" | "cd" | "pwd" | "echo" | "export" | "unset"
                 | "jobs" | "fg" | "bg" | "type" | "source" | "."
                 | "alias" | "unalias" | "history"
                 | "command" | "builtin" | "read" | "exec" | "wait"
                 | "true" | "false" | ":" | "return"
                 | "test" | "[" | "printf")
}

/// ビルトインコマンドの実行を試みる。
///
/// 出力系ビルトイン (pwd, echo, export, jobs) はリダイレクト対応のため `stdout` writer に書き込む。
/// ジョブコントロールビルトイン (fg, bg) はターミナル制御を直接操作するため `stdout` を使わない。
///
/// 戻り値:
/// - `Some(status)` — ビルトインとして実行済み
/// - `None` — 該当するビルトインなし（外部コマンドとして実行すべき）
pub fn try_exec(shell: &mut Shell, args: &[&str], stdout: &mut dyn Write) -> Option<i32> {
    match args[0] {
        "exit" => Some(builtin_exit(shell, args)),
        "cd" => Some(builtin_cd(args, stdout)),
        "pwd" => Some(builtin_pwd(stdout)),
        "echo" => Some(builtin_echo(args, stdout)),
        "export" => Some(builtin_export(args, stdout)),
        "unset" => Some(builtin_unset(args)),
        "jobs" => Some(builtin_jobs(shell, stdout)),
        "fg" => Some(builtin_fg(shell, args)),
        "bg" => Some(builtin_bg(shell, args)),
        "type" => Some(builtin_type(args, stdout)),
        "source" | "." => Some(builtin_source(shell, args)),
        "alias" => Some(builtin_alias(shell, args, stdout)),
        "unalias" => Some(builtin_unalias(shell, args)),
        "command" => Some(builtin_command(shell, args, stdout)),
        "builtin" => Some(builtin_builtin(shell, args, stdout)),
        "read" => Some(builtin_read(args)),
        "exec" => Some(builtin_exec(args)),
        "wait" => Some(builtin_wait(shell, args)),
        "true" | ":" => Some(0),
        "false" => Some(1),
        "return" => Some(builtin_return(shell, args)),
        "test" | "[" => Some(builtin_test(args)),
        "printf" => Some(builtin_printf(args, stdout)),
        _ => None,
    }
}

/// `exit [N]` — シェルを終了する。Nが指定されればそのコードで、省略時は直前のステータスで終了。
fn builtin_exit(shell: &mut Shell, args: &[&str]) -> i32 {
    shell.should_exit = true;
    if args.len() > 1 {
        args[1].parse::<i32>().unwrap_or_else(|_| {
            eprintln!("rush: exit: {}: numeric argument required", args[1]);
            2
        })
    } else {
        shell.last_status
    }
}

/// `cd [dir]` — カレントディレクトリを変更する。引数省略時は `$HOME` に移動。
/// `cd -` で OLDPWD に移動し、新ディレクトリを stdout に表示する。
/// 成功時は `OLDPWD` 環境変数を更新する。
fn builtin_cd(args: &[&str], stdout: &mut dyn Write) -> i32 {
    let oldpwd = env::current_dir().ok().map(|p| p.to_string_lossy().to_string());

    let (target, print_dir) = if args.len() > 1 && args[1] == "-" {
        match env::var("OLDPWD") {
            Ok(old) => (old, true),
            Err(_) => {
                eprintln!("rush: cd: OLDPWD not set");
                return 1;
            }
        }
    } else if args.len() > 1 {
        (args[1].to_string(), false)
    } else {
        match env::var("HOME") {
            Ok(home) => (home, false),
            Err(_) => {
                eprintln!("rush: cd: HOME not set");
                return 1;
            }
        }
    };

    if let Err(e) = env::set_current_dir(Path::new(&target)) {
        eprintln!("rush: cd: {}: {}", target, e);
        1
    } else {
        if let Some(old) = oldpwd {
            env::set_var("OLDPWD", &old);
        }
        if print_dir {
            if let Ok(cwd) = env::current_dir() {
                let _ = writeln!(stdout, "{}", cwd.display());
            }
        }
        0
    }
}

/// `pwd` — カレントディレクトリを出力する。
fn builtin_pwd(stdout: &mut dyn Write) -> i32 {
    match env::current_dir() {
        Ok(path) => {
            let _ = writeln!(stdout, "{}", path.display());
            0
        }
        Err(e) => {
            eprintln!("rush: pwd: {}", e);
            1
        }
    }
}

/// `echo [-n] args...` — 引数をスペース区切りで出力する。`-n` で改行抑制。
fn builtin_echo(args: &[&str], stdout: &mut dyn Write) -> i32 {
    let (no_newline, words) = if args.len() > 1 && args[1] == "-n" {
        (true, &args[2..])
    } else {
        (false, &args[1..])
    };

    for (i, word) in words.iter().enumerate() {
        if i > 0 {
            let _ = write!(stdout, " ");
        }
        let _ = write!(stdout, "{}", word);
    }

    if !no_newline {
        let _ = writeln!(stdout);
    }

    0
}

/// `export [VAR=val...]` — 環境変数を設定する。引数なしなら全変数をソート済みで一覧表示。
fn builtin_export(args: &[&str], stdout: &mut dyn Write) -> i32 {
    if args.len() <= 1 {
        // 全変数を一覧表示（ソート済み）
        let mut vars: Vec<(String, String)> = env::vars().collect();
        vars.sort_by(|a, b| a.0.cmp(&b.0));
        for (key, value) in &vars {
            let _ = writeln!(stdout, "declare -x {}=\"{}\"", key, value);
        }
        return 0;
    }

    for arg in &args[1..] {
        if let Some(eq_pos) = arg.find('=') {
            let key = &arg[..eq_pos];
            let value = &arg[eq_pos + 1..];
            env::set_var(key, value);
        } else {
            // 引数に `=` がない場合は無視（bash互換: export VAR は既存変数をexportする）
        }
    }

    0
}

/// `unset VAR...` — 環境変数を削除する。
fn builtin_unset(args: &[&str]) -> i32 {
    for arg in &args[1..] {
        env::remove_var(arg);
    }
    0
}

// ── type ビルトイン ──────────────────────────────────────────────────

/// `type name [name ...]` — コマンドの所在を表示する。ビルトインか外部コマンドかを判定。
fn builtin_type(args: &[&str], stdout: &mut dyn Write) -> i32 {
    if args.len() <= 1 {
        let _ = writeln!(stdout, "type: usage: type name [name ...]");
        return 1;
    }
    let mut status = 0;
    for &name in &args[1..] {
        if is_builtin(name) {
            let _ = writeln!(stdout, "{} is a shell builtin", name);
        } else if let Some(path) = find_in_path(name) {
            let _ = writeln!(stdout, "{} is {}", name, path);
        } else {
            let _ = writeln!(stdout, "rush: type: {}: not found", name);
            status = 1;
        }
    }
    status
}

/// `$PATH` 内でコマンド名を検索し、最初に見つかった実行可能ファイルのフルパスを返す。
fn find_in_path(name: &str) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let path_var = env::var("PATH").ok()?;
    for dir in path_var.split(':') {
        let full = format!("{}/{}", dir, name);
        let p = Path::new(&full);
        if let Ok(meta) = p.metadata() {
            if meta.is_file() && meta.permissions().mode() & 0o111 != 0 {
                return Some(full);
            }
        }
    }
    None
}

// ── ジョブコントロールビルトイン ─────────────────────────────────────

/// `fg` / `bg` の引数を解析してジョブ ID を返す。
///
/// - `%N` → ジョブ番号 N
/// - 数値のみ → ジョブ番号として解釈
/// - 省略時 → [`JobTable::current_job_id`](crate::job::JobTable::current_job_id) で最新の非 Done ジョブを選択
///
/// 該当ジョブが見つからない場合はエラーメッセージを出力して `Err(1)` を返す。
fn parse_job_arg(shell: &Shell, args: &[&str]) -> Result<usize, i32> {
    if args.len() > 1 {
        let arg = args[1];
        let num_str = arg.strip_prefix('%').unwrap_or(arg);
        num_str.parse::<usize>().map_err(|_| {
            eprintln!("rush: {}: {}: no such job", args[0], arg);
            1
        })
    } else {
        shell.jobs.current_job_id().ok_or_else(|| {
            eprintln!("rush: {}: no current job", args[0]);
            1
        })
    }
}

/// `jobs` — 全ジョブを `[N]   Running/Stopped/Done   command` 形式で一覧表示する。
fn builtin_jobs(shell: &Shell, stdout: &mut dyn Write) -> i32 {
    for job in shell.jobs.iter() {
        let status_str = match job.status() {
            JobStatus::Running => "Running",
            JobStatus::Stopped => "Stopped",
            JobStatus::Done(_) => "Done",
        };
        let _ = writeln!(stdout, "[{}]   {}   {}", job.id, status_str, job.command);
    }
    0
}

/// `fg [%N]` — ジョブをフォアグラウンドに復帰させる。
///
/// 1. ジョブ ID を解決（省略時は最新ジョブ）
/// 2. コマンド名を stderr に表示
/// 3. `give_terminal_to` でターミナル制御を渡す
/// 4. `SIGCONT` でプロセスグループを再開
/// 5. `wait_for_fg` で完了または停止まで待機
/// 6. `take_terminal_back` でターミナルをシェルに戻す
fn builtin_fg(shell: &mut Shell, args: &[&str]) -> i32 {
    let job_id = match parse_job_arg(shell, args) {
        Ok(id) => id,
        Err(status) => return status,
    };

    let (pgid, command) = match shell.jobs.get(job_id) {
        Some(job) => (job.pgid, job.command.clone()),
        None => {
            eprintln!("rush: fg: %{}: no such job", job_id);
            return 1;
        }
    };

    eprintln!("{}", command);

    // ターミナル制御を渡す
    job::give_terminal_to(shell.terminal_fd, pgid);

    // SIGCONT で再開
    unsafe {
        libc::kill(-pgid, libc::SIGCONT);
    }

    // 停止フラグをリセット
    if let Some(job) = shell.jobs.get_mut(job_id) {
        for proc in &mut job.processes {
            proc.stopped = false;
        }
    }

    // フォアグラウンドで待機
    let (status, stopped) = job::wait_for_fg(&mut shell.jobs, pgid);

    // ターミナルをシェルに戻す
    job::take_terminal_back(shell.terminal_fd, shell.shell_pgid);

    if stopped {
        // 再度停止された場合
        if let Some(job) = shell.jobs.get_mut(job_id) {
            for proc in &mut job.processes {
                proc.stopped = true;
            }
        }
        if let Some(job) = shell.jobs.get(job_id) {
            eprintln!("\n[{}]+  Stopped   {}", job.id, job.command);
        }
    } else {
        // 完了した場合はジョブテーブルから削除
        if let Some(job) = shell.jobs.get_mut(job_id) {
            job.notified = true;
        }
        shell.jobs.remove_done();
    }

    status
}

/// `bg [%N]` — 停止中のジョブをバックグラウンドで再開する。
///
/// Stopped でないジョブを指定した場合はエラー。
/// `SIGCONT` でプロセスグループを再開し、`[N]+ command &` を表示する。
fn builtin_bg(shell: &mut Shell, args: &[&str]) -> i32 {
    let job_id = match parse_job_arg(shell, args) {
        Ok(id) => id,
        Err(status) => return status,
    };

    let (pgid, command, is_stopped) = match shell.jobs.get(job_id) {
        Some(job) => (job.pgid, job.command.clone(), matches!(job.status(), JobStatus::Stopped)),
        None => {
            eprintln!("rush: bg: %{}: no such job", job_id);
            return 1;
        }
    };

    if !is_stopped {
        eprintln!("rush: bg: job {} already in background", job_id);
        return 1;
    }

    // SIGCONT で再開
    unsafe {
        libc::kill(-pgid, libc::SIGCONT);
    }

    // 停止フラグをリセット
    if let Some(job) = shell.jobs.get_mut(job_id) {
        for proc in &mut job.processes {
            proc.stopped = false;
        }
    }

    eprintln!("[{}]+ {} &", job_id, command);
    0
}

// ── wait ────────────────────────────────────────────────────────────

/// `wait [%N]` — バックグラウンドジョブの完了を待機する。
/// 引数なしなら全バックグラウンドジョブを待つ。`%N` で特定ジョブを待つ。
fn builtin_wait(shell: &mut Shell, args: &[&str]) -> i32 {
    if args.len() > 1 {
        // 特定ジョブを待機
        let job_id = match parse_job_arg(shell, args) {
            Ok(id) => id,
            Err(status) => return status,
        };
        let pgid = match shell.jobs.get(job_id) {
            Some(job) => job.pgid,
            None => {
                eprintln!("rush: wait: %{}: no such job", job_id);
                return 127;
            }
        };
        // waitpid で完了まで待機
        loop {
            let mut raw_status: i32 = 0;
            let pid = unsafe { libc::waitpid(-pgid, &mut raw_status, libc::WUNTRACED) };
            if pid <= 0 {
                break;
            }
            shell.jobs.mark_pid(pid, raw_status);
            if let Some(job) = shell.jobs.get(job_id) {
                match job.status() {
                    JobStatus::Done(code) => {
                        if let Some(job) = shell.jobs.get_mut(job_id) {
                            job.notified = true;
                        }
                        shell.jobs.remove_done();
                        return code;
                    }
                    JobStatus::Stopped => return 148,
                    JobStatus::Running => continue,
                }
            } else {
                break;
            }
        }
        0
    } else {
        // 全バックグラウンドジョブを待機
        loop {
            let mut raw_status: i32 = 0;
            let pid = unsafe { libc::waitpid(-1, &mut raw_status, libc::WUNTRACED) };
            if pid <= 0 {
                break;
            }
            shell.jobs.mark_pid(pid, raw_status);
        }
        // 完了済みジョブを通知・削除
        job::notify_and_clean(&mut shell.jobs);
        0
    }
}

// ── exec ────────────────────────────────────────────────────────────

/// `exec cmd [args...]` — シェルプロセスを `execvp` で置換する。引数なしなら no-op。
fn builtin_exec(args: &[&str]) -> i32 {
    if args.len() < 2 {
        return 0; // 引数なし → no-op
    }
    // シグナルハンドラを SIG_DFL に復元
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_DFL);
        libc::signal(libc::SIGTSTP, libc::SIG_DFL);
        libc::signal(libc::SIGTTOU, libc::SIG_DFL);
        libc::signal(libc::SIGTTIN, libc::SIG_DFL);
    }
    let c_args: Vec<std::ffi::CString> = args[1..]
        .iter()
        .map(|s| std::ffi::CString::new(*s).unwrap_or_default())
        .collect();
    let c_ptrs: Vec<*const libc::c_char> = c_args
        .iter()
        .map(|s| s.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();
    unsafe {
        libc::execvp(c_ptrs[0], c_ptrs.as_ptr());
    }
    // execvp が返った場合はエラー
    eprintln!("rush: exec: {}: {}", args[1], std::io::Error::last_os_error());
    126
}

// ── read ────────────────────────────────────────────────────────────

/// `read [-p prompt] VAR [VAR2 ...]` — stdin から 1 行読んで変数に代入する。
/// 複数変数時は IFS で分割。変数省略時は REPLY に代入。
fn builtin_read(args: &[&str]) -> i32 {
    let mut vars: Vec<&str> = Vec::new();
    let mut prompt_str: Option<&str> = None;
    let mut i = 1;
    while i < args.len() {
        if args[i] == "-p" && i + 1 < args.len() {
            prompt_str = Some(args[i + 1]);
            i += 2;
        } else {
            vars.push(args[i]);
            i += 1;
        }
    }

    // プロンプト表示
    if let Some(p) = prompt_str {
        eprint!("{}", p);
    }

    // stdin から 1 行読み取り
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) => return 1, // EOF
        Ok(_) => {}
        Err(_) => return 1,
    }
    let line = line.trim_end_matches('\n').trim_end_matches('\r');

    if vars.is_empty() {
        // 変数省略時は REPLY に代入
        env::set_var("REPLY", line);
    } else if vars.len() == 1 {
        env::set_var(vars[0], line);
    } else {
        // IFS で分割（デフォルト: スペース/タブ/改行）
        let parts: Vec<&str> = line.splitn(vars.len(), |c: char| c == ' ' || c == '\t').collect();
        for (j, var) in vars.iter().enumerate() {
            if j < parts.len() {
                env::set_var(var, parts[j]);
            } else {
                env::set_var(var, "");
            }
        }
    }
    0
}

// ── command / builtin ────────────────────────────────────────────────

/// `command [-v] name [args...]` — エイリアスをバイパスしてコマンドを実行する。
/// `command -v name` はコマンドのパスまたは "builtin" を表示する。
fn builtin_command(shell: &mut Shell, args: &[&str], stdout: &mut dyn Write) -> i32 {
    if args.len() < 2 {
        return 0;
    }
    if args[1] == "-v" {
        // command -v: コマンドの所在を表示
        if args.len() < 3 {
            return 1;
        }
        let name = args[2];
        if is_builtin(name) {
            let _ = writeln!(stdout, "{}", name);
            0
        } else if let Some(path) = find_in_path(name) {
            let _ = writeln!(stdout, "{}", path);
            0
        } else {
            1
        }
    } else {
        // command name args: ビルトインとして試行し、なければ外部コマンドとして実行
        // executor 側で alias をスキップして実行するため、ここではビルトインのみ試行
        let sub_args = &args[1..];
        if let Some(status) = try_exec(shell, sub_args, stdout) {
            status
        } else {
            // 外部コマンドとして実行 — executor に委ねるため 127 を返す
            // （実際には executor がこれを処理する）
            eprintln!("rush: {}: command not found", sub_args[0]);
            127
        }
    }
}

/// `builtin name [args...]` — ビルトインコマンドのみ実行する。外部コマンドならエラー。
fn builtin_builtin(shell: &mut Shell, args: &[&str], stdout: &mut dyn Write) -> i32 {
    if args.len() < 2 {
        return 0;
    }
    let sub_args = &args[1..];
    if let Some(status) = try_exec(shell, sub_args, stdout) {
        status
    } else {
        eprintln!("rush: builtin: {}: not a shell builtin", sub_args[0]);
        1
    }
}

// ── alias / unalias ─────────────────────────────────────────────────

/// `alias [name=value ...]` — エイリアスを定義・一覧表示する。
/// 引数なしなら全エイリアスをソート済みで表示。
fn builtin_alias(shell: &mut Shell, args: &[&str], stdout: &mut dyn Write) -> i32 {
    if args.len() <= 1 {
        let mut aliases: Vec<_> = shell.aliases.iter().collect();
        aliases.sort_by(|a, b| a.0.cmp(b.0));
        for (name, value) in aliases {
            let _ = writeln!(stdout, "alias {}='{}'", name, value);
        }
        return 0;
    }
    for arg in &args[1..] {
        if let Some(eq_pos) = arg.find('=') {
            let name = &arg[..eq_pos];
            let value = &arg[eq_pos + 1..];
            shell.aliases.insert(name.to_string(), value.to_string());
        } else {
            // alias name → 特定エイリアスを表示
            if let Some(value) = shell.aliases.get(*arg) {
                let _ = writeln!(stdout, "alias {}='{}'", arg, value);
            } else {
                eprintln!("rush: alias: {}: not found", arg);
                return 1;
            }
        }
    }
    0
}

/// `unalias [-a] name ...` — エイリアスを削除する。`-a` で全削除。
fn builtin_unalias(shell: &mut Shell, args: &[&str]) -> i32 {
    if args.len() <= 1 {
        eprintln!("rush: unalias: usage: unalias [-a] name [name ...]");
        return 2;
    }
    if args[1] == "-a" {
        shell.aliases.clear();
        return 0;
    }
    for arg in &args[1..] {
        if shell.aliases.remove(*arg).is_none() {
            eprintln!("rush: unalias: {}: not found", arg);
        }
    }
    0
}

// ── return ──────────────────────────────────────────────────────────

/// `return [N]` — `source` で実行中のスクリプトから早期脱出する。
/// `source` の外で呼ばれた場合はエラー。
fn builtin_return(shell: &mut Shell, args: &[&str]) -> i32 {
    if shell.source_depth == 0 {
        eprintln!("rush: return: can only `return' from a function or sourced script");
        return 1;
    }
    let code = if args.len() > 1 {
        args[1].parse::<i32>().unwrap_or_else(|_| {
            eprintln!("rush: return: {}: numeric argument required", args[1]);
            2
        })
    } else {
        shell.last_status
    };
    shell.should_return = true;
    code
}

// ── source / . ──────────────────────────────────────────────────────

/// `source file` / `. file` — ファイルを現在のシェルコンテキストで行単位実行する。
/// `return` による早期脱出をサポート。
fn builtin_source(shell: &mut Shell, args: &[&str]) -> i32 {
    if args.len() < 2 {
        eprintln!("rush: {}: filename argument required", args[0]);
        return 2;
    }
    let path = args[1];
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("rush: {}: {}: {}", args[0], path, e);
            return 1;
        }
    };
    shell.source_depth += 1;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        match parser::parse(trimmed, shell.last_status) {
            Ok(Some(list)) => {
                let cmd_text = trimmed.to_string();
                shell.last_status = executor::execute(shell, &list, &cmd_text);
            }
            Ok(None) => {}
            Err(e) => eprintln!("rush: {}: {}", path, e),
        }
        if shell.should_return {
            shell.should_return = false;
            break;
        }
    }
    shell.source_depth -= 1;
    shell.last_status
}

// ── printf ──────────────────────────────────────────────────────────

/// `printf format [args...]` — フォーマット文字列に従って出力する。
///
/// 対応フォーマット指定子: `%s`（文字列）, `%d`（整数）, `%x`（16進数）, `%o`（8進数）
/// エスケープ: `\n`, `\t`, `\\`, `\0NNN`（8進数）
fn builtin_printf(args: &[&str], stdout: &mut dyn Write) -> i32 {
    if args.len() < 2 {
        eprintln!("rush: printf: usage: printf format [arguments]");
        return 1;
    }
    let format = args[1];
    let arguments = &args[2..];
    let mut arg_idx = 0;

    let bytes = format.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            // エスケープシーケンス
            match bytes[i + 1] {
                b'n' => { let _ = write!(stdout, "\n"); i += 2; }
                b't' => { let _ = write!(stdout, "\t"); i += 2; }
                b'r' => { let _ = write!(stdout, "\r"); i += 2; }
                b'\\' => { let _ = write!(stdout, "\\"); i += 2; }
                b'0' => {
                    // \0NNN — 8進数文字
                    let mut val: u8 = 0;
                    let mut j = i + 2;
                    let end = (j + 3).min(bytes.len());
                    while j < end && bytes[j] >= b'0' && bytes[j] <= b'7' {
                        val = val * 8 + (bytes[j] - b'0');
                        j += 1;
                    }
                    let _ = stdout.write_all(&[val]);
                    i = j;
                }
                _ => {
                    let _ = write!(stdout, "\\");
                    i += 1;
                }
            }
        } else if bytes[i] == b'%' && i + 1 < bytes.len() {
            // フォーマット指定子
            i += 1;
            // 幅とフラグを解析
            let mut width: Option<usize> = None;
            let mut zero_pad = false;
            let mut left_align = false;

            if i < bytes.len() && bytes[i] == b'-' {
                left_align = true;
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'0' {
                zero_pad = true;
                i += 1;
            }
            let width_start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i > width_start {
                width = std::str::from_utf8(&bytes[width_start..i]).ok()
                    .and_then(|s| s.parse().ok());
            }

            if i >= bytes.len() { break; }

            let arg_val = if arg_idx < arguments.len() {
                arguments[arg_idx]
            } else {
                ""
            };

            match bytes[i] {
                b's' => {
                    if let Some(w) = width {
                        if left_align {
                            let _ = write!(stdout, "{:<width$}", arg_val, width = w);
                        } else {
                            let _ = write!(stdout, "{:>width$}", arg_val, width = w);
                        }
                    } else {
                        let _ = write!(stdout, "{}", arg_val);
                    }
                    arg_idx += 1;
                }
                b'd' => {
                    let n: i64 = arg_val.parse().unwrap_or(0);
                    if let Some(w) = width {
                        if zero_pad {
                            let _ = write!(stdout, "{:0>width$}", n, width = w);
                        } else if left_align {
                            let _ = write!(stdout, "{:<width$}", n, width = w);
                        } else {
                            let _ = write!(stdout, "{:>width$}", n, width = w);
                        }
                    } else {
                        let _ = write!(stdout, "{}", n);
                    }
                    arg_idx += 1;
                }
                b'x' => {
                    let n: u64 = arg_val.parse().unwrap_or(0);
                    if let Some(w) = width {
                        if zero_pad {
                            let _ = write!(stdout, "{:0>width$x}", n, width = w);
                        } else {
                            let _ = write!(stdout, "{:width$x}", n, width = w);
                        }
                    } else {
                        let _ = write!(stdout, "{:x}", n);
                    }
                    arg_idx += 1;
                }
                b'o' => {
                    let n: u64 = arg_val.parse().unwrap_or(0);
                    if let Some(w) = width {
                        if zero_pad {
                            let _ = write!(stdout, "{:0>width$o}", n, width = w);
                        } else {
                            let _ = write!(stdout, "{:width$o}", n, width = w);
                        }
                    } else {
                        let _ = write!(stdout, "{:o}", n);
                    }
                    arg_idx += 1;
                }
                b'%' => {
                    let _ = write!(stdout, "%");
                }
                _ => {
                    let _ = write!(stdout, "%");
                    let _ = stdout.write_all(&[bytes[i]]);
                }
            }
            i += 1;
        } else {
            let _ = stdout.write_all(&[bytes[i]]);
            i += 1;
        }
    }

    0
}

// ── test / [ ────────────────────────────────────────────────────────

/// `test expr` / `[ expr ]` — 条件式を評価する。
///
/// 対応演算子:
/// - 文字列: `-n STR`, `-z STR`, `STR = STR`, `STR != STR`
/// - 整数: `-eq`, `-ne`, `-lt`, `-le`, `-gt`, `-ge`
/// - ファイル: `-e`, `-f`, `-d`, `-r`, `-w`, `-x`, `-s`
/// - 論理: `!`（否定）
fn builtin_test(args: &[&str]) -> i32 {
    let is_bracket = args[0] == "[";
    let test_args = if is_bracket {
        // `[` の場合、末尾の `]` を除去
        if args.last() != Some(&"]") {
            eprintln!("rush: [: missing `]'");
            return 2;
        }
        &args[1..args.len() - 1]
    } else {
        &args[1..]
    };

    if eval_test(test_args) { 0 } else { 1 }
}

/// test の条件式を再帰的に評価する。
fn eval_test(args: &[&str]) -> bool {
    match args.len() {
        0 => false,
        1 => !args[0].is_empty(),
        2 => eval_unary(args[0], args[1]),
        3 => {
            if args[0] == "!" {
                return !eval_test(&args[1..]);
            }
            eval_binary(args[0], args[1], args[2])
        }
        4 => {
            if args[0] == "!" {
                !eval_test(&args[1..])
            } else {
                false
            }
        }
        _ => false,
    }
}

/// 単項演算子: `-n`, `-z`, `-e`, `-f`, `-d`, `-r`, `-w`, `-x`, `-s`
fn eval_unary(op: &str, operand: &str) -> bool {
    match op {
        "-n" => !operand.is_empty(),
        "-z" => operand.is_empty(),
        "-e" => Path::new(operand).exists(),
        "-f" => Path::new(operand).is_file(),
        "-d" => Path::new(operand).is_dir(),
        "-r" => check_access(operand, libc::R_OK),
        "-w" => check_access(operand, libc::W_OK),
        "-x" => check_access(operand, libc::X_OK),
        "-s" => std::fs::metadata(operand).map(|m| m.len() > 0).unwrap_or(false),
        "!" => operand.is_empty(), // `! STR` → true if STR is empty
        _ => false,
    }
}

/// `access(2)` でファイルアクセス権をチェックする。
fn check_access(path: &str, mode: i32) -> bool {
    let c_path = match std::ffi::CString::new(path) {
        Ok(p) => p,
        Err(_) => return false,
    };
    unsafe { libc::access(c_path.as_ptr(), mode) == 0 }
}

/// 二項演算子: `=`, `!=`, `-eq`, `-ne`, `-lt`, `-le`, `-gt`, `-ge`
fn eval_binary(left: &str, op: &str, right: &str) -> bool {
    match op {
        "=" | "==" => left == right,
        "!=" => left != right,
        "-eq" | "-ne" | "-lt" | "-le" | "-gt" | "-ge" => {
            let l = left.parse::<i64>().unwrap_or(0);
            let r = right.parse::<i64>().unwrap_or(0);
            match op {
                "-eq" => l == r,
                "-ne" => l != r,
                "-lt" => l < r,
                "-le" => l <= r,
                "-gt" => l > r,
                "-ge" => l >= r,
                _ => unreachable!(),
            }
        }
        _ => false,
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::Shell;

    #[test]
    fn pwd_outputs_current_dir() {
        let mut buf = Vec::new();
        let status = builtin_pwd(&mut buf);
        assert_eq!(status, 0);
        let output = String::from_utf8(buf).unwrap();
        assert!(output.ends_with('\n'));
        assert!(!output.trim().is_empty());
    }

    #[test]
    fn echo_basic() {
        let mut buf = Vec::new();
        builtin_echo(&["echo", "hello", "world"], &mut buf);
        assert_eq!(String::from_utf8(buf).unwrap(), "hello world\n");
    }

    #[test]
    fn echo_no_args() {
        let mut buf = Vec::new();
        builtin_echo(&["echo"], &mut buf);
        assert_eq!(String::from_utf8(buf).unwrap(), "\n");
    }

    #[test]
    fn echo_dash_n() {
        let mut buf = Vec::new();
        builtin_echo(&["echo", "-n", "hello"], &mut buf);
        assert_eq!(String::from_utf8(buf).unwrap(), "hello");
    }

    #[test]
    fn echo_dash_n_no_args() {
        let mut buf = Vec::new();
        builtin_echo(&["echo", "-n"], &mut buf);
        assert_eq!(String::from_utf8(buf).unwrap(), "");
    }

    #[test]
    fn export_set_and_get() {
        let mut buf = Vec::new();
        builtin_export(&["export", "RUSH_TEST_EXPORT=hello123"], &mut buf);
        assert_eq!(env::var("RUSH_TEST_EXPORT").unwrap(), "hello123");
        env::remove_var("RUSH_TEST_EXPORT");
    }

    #[test]
    fn export_value_with_equals() {
        let mut buf = Vec::new();
        builtin_export(&["export", "RUSH_TEST_EQ=A=B=C"], &mut buf);
        assert_eq!(env::var("RUSH_TEST_EQ").unwrap(), "A=B=C");
        env::remove_var("RUSH_TEST_EQ");
    }

    #[test]
    fn export_list_sorted() {
        env::set_var("RUSH_TEST_Z", "z");
        env::set_var("RUSH_TEST_A", "a");
        let mut buf = Vec::new();
        builtin_export(&["export"], &mut buf);
        let output = String::from_utf8(buf).unwrap();
        let a_pos = output.find("RUSH_TEST_A").unwrap();
        let z_pos = output.find("RUSH_TEST_Z").unwrap();
        assert!(a_pos < z_pos, "export listing should be sorted");
        env::remove_var("RUSH_TEST_Z");
        env::remove_var("RUSH_TEST_A");
    }

    #[test]
    fn unset_removes_var() {
        env::set_var("RUSH_TEST_UNSET", "value");
        builtin_unset(&["unset", "RUSH_TEST_UNSET"]);
        assert!(env::var("RUSH_TEST_UNSET").is_err());
    }

    #[test]
    fn is_builtin_check() {
        assert!(is_builtin("exit"));
        assert!(is_builtin("cd"));
        assert!(is_builtin("pwd"));
        assert!(is_builtin("echo"));
        assert!(is_builtin("export"));
        assert!(is_builtin("unset"));
        assert!(is_builtin("jobs"));
        assert!(is_builtin("fg"));
        assert!(is_builtin("bg"));
        assert!(is_builtin("type"));
        assert!(!is_builtin("ls"));
        assert!(!is_builtin("grep"));
    }

    #[test]
    fn type_builtin_reports_builtin() {
        let mut shell = Shell::new();
        let mut buf = Vec::new();
        let status = try_exec(&mut shell, &["type", "echo"], &mut buf).unwrap();
        assert_eq!(status, 0);
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("shell builtin"));
    }

    #[test]
    fn type_builtin_reports_not_found() {
        let mut shell = Shell::new();
        let mut buf = Vec::new();
        let status = try_exec(&mut shell, &["type", "nonexistent_cmd_xyz"], &mut buf).unwrap();
        assert_eq!(status, 1);
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("not found"));
    }

    #[test]
    fn type_no_args() {
        let mut shell = Shell::new();
        let mut buf = Vec::new();
        let status = try_exec(&mut shell, &["type"], &mut buf).unwrap();
        assert_eq!(status, 1);
    }

    #[test]
    fn type_external_command() {
        let mut shell = Shell::new();
        let mut buf = Vec::new();
        // /bin/ls or /usr/bin/ls should exist on any Unix system
        let status = try_exec(&mut shell, &["type", "ls"], &mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();
        // Should either be found or not, but no crash
        if status == 0 {
            assert!(output.contains("ls is /"));
        }
    }

    #[test]
    fn try_exec_returns_none_for_external() {
        let mut shell = Shell::new();
        let mut buf = Vec::new();
        assert!(try_exec(&mut shell, &["ls"], &mut buf).is_none());
    }

    #[test]
    fn try_exec_echo() {
        let mut shell = Shell::new();
        let mut buf = Vec::new();
        let status = try_exec(&mut shell, &["echo", "test"], &mut buf).unwrap();
        assert_eq!(status, 0);
        assert_eq!(String::from_utf8(buf).unwrap(), "test\n");
    }

    #[test]
    fn cd_dash_returns_to_oldpwd() {
        let orig = env::current_dir().unwrap();
        let tmp = env::temp_dir();
        // First cd to tmp to set OLDPWD
        let mut buf = Vec::new();
        builtin_cd(&["cd", tmp.to_str().unwrap()], &mut buf);
        // Now cd - should return to orig
        let mut buf2 = Vec::new();
        let status = builtin_cd(&["cd", "-"], &mut buf2);
        assert_eq!(status, 0);
        let output = String::from_utf8(buf2).unwrap();
        assert!(!output.trim().is_empty()); // should print the directory
        // Restore
        let _ = env::set_current_dir(&orig);
    }

    #[test]
    fn cd_sets_oldpwd() {
        let orig = env::current_dir().unwrap();
        let tmp = env::temp_dir();
        let mut buf = Vec::new();
        builtin_cd(&["cd", tmp.to_str().unwrap()], &mut buf);
        let oldpwd = env::var("OLDPWD").unwrap();
        assert_eq!(oldpwd, orig.to_string_lossy());
        let _ = env::set_current_dir(&orig);
    }

    #[test]
    fn true_returns_zero() {
        let mut shell = Shell::new();
        let mut buf = Vec::new();
        assert_eq!(try_exec(&mut shell, &["true"], &mut buf), Some(0));
        assert_eq!(try_exec(&mut shell, &[":"], &mut buf), Some(0));
    }

    #[test]
    fn false_returns_one() {
        let mut shell = Shell::new();
        let mut buf = Vec::new();
        assert_eq!(try_exec(&mut shell, &["false"], &mut buf), Some(1));
    }

    #[test]
    fn return_outside_source_errors() {
        let mut shell = Shell::new();
        let mut buf = Vec::new();
        let status = try_exec(&mut shell, &["return"], &mut buf).unwrap();
        assert_eq!(status, 1); // error: not in source
    }

    #[test]
    fn return_inside_source_sets_flag() {
        let mut shell = Shell::new();
        let mut buf = Vec::new();
        shell.source_depth = 1; // simulate being inside source
        let status = try_exec(&mut shell, &["return", "42"], &mut buf).unwrap();
        assert_eq!(status, 42);
        assert!(shell.should_return);
    }

    #[test]
    fn test_string_nonempty() {
        assert_eq!(builtin_test(&["test", "hello"]), 0);
        assert_eq!(builtin_test(&["test", ""]), 1);
    }

    #[test]
    fn test_dash_n_z() {
        assert_eq!(builtin_test(&["test", "-n", "hello"]), 0);
        assert_eq!(builtin_test(&["test", "-n", ""]), 1);
        assert_eq!(builtin_test(&["test", "-z", ""]), 0);
        assert_eq!(builtin_test(&["test", "-z", "hello"]), 1);
    }

    #[test]
    fn test_string_eq_ne() {
        assert_eq!(builtin_test(&["test", "a", "=", "a"]), 0);
        assert_eq!(builtin_test(&["test", "a", "=", "b"]), 1);
        assert_eq!(builtin_test(&["test", "a", "!=", "b"]), 0);
        assert_eq!(builtin_test(&["test", "a", "!=", "a"]), 1);
    }

    #[test]
    fn test_integer_comparisons() {
        assert_eq!(builtin_test(&["test", "5", "-eq", "5"]), 0);
        assert_eq!(builtin_test(&["test", "5", "-ne", "3"]), 0);
        assert_eq!(builtin_test(&["test", "3", "-lt", "5"]), 0);
        assert_eq!(builtin_test(&["test", "5", "-gt", "3"]), 0);
        assert_eq!(builtin_test(&["test", "5", "-le", "5"]), 0);
        assert_eq!(builtin_test(&["test", "5", "-ge", "5"]), 0);
        assert_eq!(builtin_test(&["test", "5", "-lt", "3"]), 1);
    }

    #[test]
    fn test_file_exists() {
        assert_eq!(builtin_test(&["test", "-e", "Cargo.toml"]), 0);
        assert_eq!(builtin_test(&["test", "-e", "nonexistent_xyz"]), 1);
        assert_eq!(builtin_test(&["test", "-f", "Cargo.toml"]), 0);
        assert_eq!(builtin_test(&["test", "-d", "src"]), 0);
        assert_eq!(builtin_test(&["test", "-d", "Cargo.toml"]), 1);
    }

    #[test]
    fn test_negation() {
        assert_eq!(builtin_test(&["test", "!", "hello"]), 1);
        assert_eq!(builtin_test(&["test", "!", ""]), 0);
        assert_eq!(builtin_test(&["test", "!", "-f", "Cargo.toml"]), 1);
    }

    #[test]
    fn test_bracket_syntax() {
        assert_eq!(builtin_test(&["[", "hello", "]"]), 0);
        assert_eq!(builtin_test(&["[", "-f", "Cargo.toml", "]"]), 0);
        assert_eq!(builtin_test(&["[", "a", "=", "a", "]"]), 0);
    }

    #[test]
    fn test_bracket_missing_close() {
        assert_eq!(builtin_test(&["[", "hello"]), 2);
    }

    #[test]
    fn printf_basic_string() {
        let mut buf = Vec::new();
        builtin_printf(&["printf", "%s", "hello"], &mut buf);
        assert_eq!(String::from_utf8(buf).unwrap(), "hello");
    }

    #[test]
    fn printf_newline_escape() {
        let mut buf = Vec::new();
        builtin_printf(&["printf", "%s\\n", "hello"], &mut buf);
        assert_eq!(String::from_utf8(buf).unwrap(), "hello\n");
    }

    #[test]
    fn printf_integer() {
        let mut buf = Vec::new();
        builtin_printf(&["printf", "%d", "42"], &mut buf);
        assert_eq!(String::from_utf8(buf).unwrap(), "42");
    }

    #[test]
    fn printf_hex() {
        let mut buf = Vec::new();
        builtin_printf(&["printf", "%x", "255"], &mut buf);
        assert_eq!(String::from_utf8(buf).unwrap(), "ff");
    }

    #[test]
    fn printf_zero_padded() {
        let mut buf = Vec::new();
        builtin_printf(&["printf", "%03d", "5"], &mut buf);
        assert_eq!(String::from_utf8(buf).unwrap(), "005");
    }

    #[test]
    fn printf_multiple_args() {
        let mut buf = Vec::new();
        builtin_printf(&["printf", "Name: %s, Age: %d\\n", "Alice", "30"], &mut buf);
        assert_eq!(String::from_utf8(buf).unwrap(), "Name: Alice, Age: 30\n");
    }

    #[test]
    fn printf_percent_literal() {
        let mut buf = Vec::new();
        builtin_printf(&["printf", "100%%"], &mut buf);
        assert_eq!(String::from_utf8(buf).unwrap(), "100%");
    }
}
