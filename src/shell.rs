//! シェルのグローバル状態を保持するモジュール。
//!
//! 環境変数は `std::env` を直接使用し、子プロセスへの自動継承を活用する。
//! ジョブテーブル（[`JobTable`]）、プロセスグループ/ターミナル制御、
//! `$PATH` キャッシュ（[`PathCache`]）、ユーザー定義関数マップ、
//! 位置パラメータ（`$1`〜`$9`）を保持する。
//!
//! [`PathCache`] は [`editor`](crate::editor) とは別インスタンスで管理される。
//! エディタの PathCache はハイライト・補完用で `read_line` 呼び出し毎にリフレッシュされ、
//! Shell の PathCache は executor での将来的な PATH 検索最適化用に保持される。

use std::collections::HashMap;

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
    /// `$PATH` 内コマンドのキャッシュ。executor での PATH 検索最適化用（将来使用予定）。
    #[allow(dead_code)]
    pub path_cache: PathCache,
    /// エイリアスマップ。`alias name=value` で定義される。
    pub aliases: HashMap<String, String>,
    /// 直前のバックグラウンドプロセスの PID（`$!` 展開用）。
    pub last_bg_pid: i32,
    /// `source` / 関数実行中のネスト深さ。`return` ビルトインの有効性判定に使用。
    pub source_depth: usize,
    /// `return` が呼ばれたら true にセットし、`source` のループを中断する。
    pub should_return: bool,
    /// ディレクトリスタック（`pushd`/`popd` 用）。スタックトップが最新。
    pub dir_stack: Vec<String>,
    /// トラップハンドラ（シグナル番号 → コマンド文字列）。`trap 'cmd' SIGNAL` で設定。
    pub traps: HashMap<i32, String>,
    /// `break` 要求の残り深さ。0 なら break 要求なし。
    /// `break N` で N が設定され、ループ脱出ごとにデクリメントする。
    pub break_level: usize,
    /// `continue` 要求の残り深さ。0 なら continue 要求なし。
    pub continue_level: usize,
    /// 現在のループネスト深さ。`break`/`continue` の有効性判定に使用。
    pub loop_depth: usize,
    /// ユーザー定義関数マップ。`name() { body }` で定義される。
    pub functions: HashMap<String, String>,
    /// 位置パラメータ（`$1`〜`$N`）。関数呼び出し時に設定される。
    pub positional_args: Vec<String>,
    /// `set -e` (errexit): コマンド失敗時にシェルを終了する。
    pub set_errexit: bool,
    /// `set -u` (nounset): 未定義変数の参照をエラーにする。
    pub set_nounset: bool,
    /// `set -o pipefail`: パイプライン中の最初の非ゼロ終了コードを返す。
    pub set_pipefail: bool,
    /// if/while/until 条件文脈の深さ。0 = 通常、>0 = 条件評価中（errexit 免除）。
    pub in_condition: usize,
    /// errexit 発動フラグ。run_command_string の早期リターンに使用。
    pub errexit_pending: bool,
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
            aliases: HashMap::new(),
            last_bg_pid: 0,
            source_depth: 0,
            should_return: false,
            dir_stack: Vec::new(),
            traps: HashMap::new(),
            break_level: 0,
            continue_level: 0,
            loop_depth: 0,
            functions: HashMap::new(),
            positional_args: Vec::new(),
            set_errexit: false,
            set_nounset: false,
            set_pipefail: false,
            in_condition: 0,
            errexit_pending: false,
        }
    }
}
