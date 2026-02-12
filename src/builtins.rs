//! ビルトインコマンドの実装。
//!
//! ビルトインはfork/execを経由せずプロセス内で直接実行されるため高速。
//! `try_exec()` が `Some(status)` を返せばビルトインとして処理済み、
//! `None` なら外部コマンドとしてexecutorに委ねる。
//!
//! ## 対応ビルトイン
//!
//! - シェル制御: `exit`, `cd`
//! - 出力: `pwd`, `echo`
//! - 環境変数: `export`, `unset`
//! - ジョブコントロール: `jobs`, `fg`, `bg`
//! - 情報: `type`

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
                 | "jobs" | "fg" | "bg" | "type" | "source" | ".")
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

// ── source / . ──────────────────────────────────────────────────────

/// `source file` / `. file` — ファイルを現在のシェルコンテキストで行単位実行する。
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
    }
    shell.last_status
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
}
