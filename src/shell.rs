//! シェルのグローバル状態を保持するモジュール。
//!
//! 環境変数は `std::env` を直接使用し、子プロセスへの自動継承を活用する。
//! ジョブテーブル（[`JobTable`]）とプロセスグループ/ターミナル制御に必要な情報を保持する。

use libc::pid_t;

use crate::job::JobTable;

/// シェルの実行状態。REPLループ全体で共有される。
pub struct Shell {
    /// 直前のコマンドの終了ステータス。プロンプト表示、`exit` のデフォルト値、`$?` 展開に使う。
    pub last_status: i32,
    /// `exit` ビルトインで true にセットされ、REPLループを終了させる。
    pub should_exit: bool,
    /// ジョブテーブル。バックグラウンド/停止ジョブを管理する。
    pub jobs: JobTable,
    /// シェル自身のプロセスグループ ID。
    pub shell_pgid: pid_t,
    /// ターミナルのファイルディスクリプタ（通常 STDIN_FILENO）。
    pub terminal_fd: i32,
}

impl Shell {
    pub fn new() -> Self {
        let shell_pgid = unsafe { libc::getpgrp() };
        Self {
            last_status: 0,
            should_exit: false,
            jobs: JobTable::new(),
            shell_pgid,
            terminal_fd: libc::STDIN_FILENO,
        }
    }
}
