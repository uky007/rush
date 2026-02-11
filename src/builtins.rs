//! ビルトインコマンドの実装。
//!
//! ビルトインはfork/execを経由せずプロセス内で直接実行されるため高速。
//! `try_exec()` が `Some(status)` を返せばビルトインとして処理済み、
//! `None` なら外部コマンドとしてexecutorに委ねる。

use std::env;
use std::path::Path;

use crate::shell::Shell;

/// ビルトインコマンドの実行を試みる。
///
/// 戻り値:
/// - `Some(status)` — ビルトインとして実行済み
/// - `None` — 該当するビルトインなし（外部コマンドとして実行すべき）
pub fn try_exec(shell: &mut Shell, args: &[&str]) -> Option<i32> {
    match args[0] {
        "exit" => Some(builtin_exit(shell, args)),
        "cd" => Some(builtin_cd(args)),
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
