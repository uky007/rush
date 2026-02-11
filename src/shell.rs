//! シェルのグローバル状態を保持するモジュール。
//!
//! 将来の拡張で環境変数テーブル、ジョブリスト等を追加する予定。

/// シェルの実行状態。REPLループ全体で共有される。
pub struct Shell {
    /// 直前のコマンドの終了ステータス。プロンプト表示や `exit` のデフォルト値に使う。
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
