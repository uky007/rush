//! ビルトインコマンドの実装。
//!
//! ビルトインはfork/execを経由せずプロセス内で直接実行されるため高速。
//! `try_exec()` が `Some(status)` を返せばビルトインとして処理済み、
//! `None` なら外部コマンドとしてexecutorに委ねる。

use std::env;
use std::io::Write;
use std::path::Path;

use crate::shell::Shell;

/// コマンド名がビルトインかどうかを判定する。
///
/// executor がビルトイン判定 → リダイレクト準備 → 実行、の順で処理するために使用。
pub fn is_builtin(name: &str) -> bool {
    matches!(name, "exit" | "cd" | "pwd" | "echo" | "export" | "unset")
}

/// ビルトインコマンドの実行を試みる。
///
/// 出力系ビルトイン (pwd, echo, export) はリダイレクト対応のため `stdout` writer に書き込む。
///
/// 戻り値:
/// - `Some(status)` — ビルトインとして実行済み
/// - `None` — 該当するビルトインなし（外部コマンドとして実行すべき）
pub fn try_exec(shell: &mut Shell, args: &[&str], stdout: &mut dyn Write) -> Option<i32> {
    match args[0] {
        "exit" => Some(builtin_exit(shell, args)),
        "cd" => Some(builtin_cd(args)),
        "pwd" => Some(builtin_pwd(stdout)),
        "echo" => Some(builtin_echo(args, stdout)),
        "export" => Some(builtin_export(args, stdout)),
        "unset" => Some(builtin_unset(args)),
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
fn builtin_cd(args: &[&str]) -> i32 {
    let target = if args.len() > 1 {
        args[1].to_string()
    } else {
        match env::var("HOME") {
            Ok(home) => home,
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
        assert!(!is_builtin("ls"));
        assert!(!is_builtin("grep"));
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
}
