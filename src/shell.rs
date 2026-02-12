//! シェルのグローバル状態を保持するモジュール。
//!
//! 環境変数は `std::env` を直接使用し、子プロセスへの自動継承を活用する。
//! ジョブテーブル（[`JobTable`]）、プロセスグループ/ターミナル制御、
//! `$PATH` キャッシュ（[`PathCache`]）を保持する。
//!
//! [`PathCache`] は [`editor`](crate::editor) とは別インスタンスで管理される。
//! エディタの PathCache はハイライト・補完用で `read_line` 呼び出し毎にリフレッシュされ、
//! Shell の PathCache は executor での将来的な PATH 検索最適化用に保持される。

use libc::pid_t;

use crate::highlight::PathCache;
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
    /// `$PATH` 内コマンドのキャッシュ。executor での PATH 検索最適化用。
    pub path_cache: PathCache,
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
            path_cache: PathCache::new(),
        }
    }
}
