//! rush ライブラリ — ベンチマーク・テスト用にモジュールを公開する。
//!
//! バイナリ本体は `main.rs` の REPL ループ。
//! この `lib.rs` は `benches/bench_main.rs` 等の外部クレートから
//! パーサー・ビルトイン・スポーン機能に直接アクセスするために存在する。
//!
//! ## モジュール構成
//!
//! | モジュール | 役割 |
//! |-----------|------|
//! | [`editor`] | 行エディタ（raw モード、キー入力、バッファ操作、表示更新） |
//! | [`history`] | コマンド履歴（`~/.rush_history` 永続化、↑↓ ナビゲーション） |
//! | [`complete`] | Tab 補完（コマンド名、ファイル名、`&&`/`||`/`;` 後のコマンド位置認識） |
//! | [`highlight`] | シンタックスハイライト（ANSI カラー、PATH キャッシュ、`&&`/`||`/`;`/`${VAR}` 対応） |
//! | [`parser`] | 構文解析（コマンドリスト `&&`/`||`/`;`、パイプライン、リダイレクト、クォート、エスケープ、変数展開 `$VAR`/`${VAR}`/`$?`、`&`） |
//! | [`executor`] | コマンド実行（コマンドリスト条件付き実行、パイプライン接続、glob 展開、プロセスグループ管理） |
//! | [`builtins`] | ビルトイン（`exit`, `cd`, `pwd`, `echo`, `export`, `unset`, `jobs`, `fg`, `bg`） |
//! | [`glob`] | パス名展開（`*`, `?` によるファイル名マッチング） |
//! | [`job`] | ジョブコントロール（バックグラウンド実行、Ctrl+Z サスペンド、`fg`/`bg` 復帰） |
//! | [`shell`] | シェルのグローバル状態（終了ステータス、ジョブテーブル、プロセスグループ） |
//! | [`spawn`] | `posix_spawnp` ラッパー（外部コマンド起動の高速化） |

pub mod builtins;
pub mod complete;
pub mod editor;
pub mod executor;
pub mod glob;
pub mod highlight;
pub mod history;
pub mod job;
pub mod parser;
pub mod shell;
pub mod spawn;
