//! シェルのグローバル状態を保持するモジュール。
//!
//! 環境変数は `std::env` を直接使用し、子プロセスへの自動継承を活用する。
//! 将来の拡張でジョブリスト等を追加する予定。

/// シェルの実行状態。REPLループ全体で共有される。
pub struct Shell {
    /// 直前のコマンドの終了ステータス。プロンプト表示、`exit` のデフォルト値、`$?` 展開に使う。
    pub last_status: i32,
    /// `exit` ビルトインで true にセットされ、REPLループを終了させる。
    pub should_exit: bool,
}

impl Shell {
    pub fn new() -> Self {
        Self {
            last_status: 0,
            should_exit: false,
        }
    }
}
