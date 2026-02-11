//! コマンドディスパッチ: ビルトイン → 外部コマンドの順に実行を試みる。
//!
//! Phase 6で外部コマンド実行を `std::process::Command` から `posix_spawn` に置換予定。

use std::process::Command;

use crate::builtins;
use crate::shell::Shell;

/// コマンドを実行し、終了ステータスを返す。
///
/// 実行順序:
/// 1. ビルトインコマンドを検索（fork不要、高速パス）
/// 2. 該当なければ外部コマンドを `spawn` + `wait`
///
/// エラーコード: 127 = command not found, 126 = permission denied
pub fn execute(shell: &mut Shell, args: &[&str]) -> i32 {
    if args.is_empty() {
        return shell.last_status;
    }

    // ビルトインを先にチェック（fork不要の高速パス）
    if let Some(status) = builtins::try_exec(shell, args) {
        return status;
    }

    // 外部コマンド: spawn して完了を待つ
    match Command::new(args[0]).args(&args[1..]).spawn() {
        Ok(mut child) => match child.wait() {
            Ok(status) => status.code().unwrap_or(128),
            Err(e) => {
                eprintln!("rush: {}: {}", args[0], e);
                1
            }
        },
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                eprintln!("rush: {}: command not found", args[0]);
                127
            } else if e.kind() == std::io::ErrorKind::PermissionDenied {
                eprintln!("rush: {}: permission denied", args[0]);
                126
            } else {
                eprintln!("rush: {}: {}", args[0], e);
                1
            }
        }
    }
}
