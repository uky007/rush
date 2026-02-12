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
//! | [`complete`] | Tab 補完（コマンド名、ファイル名） |
//! | [`highlight`] | シンタックスハイライト（ANSI カラー、PATH キャッシュ） |
//! | [`parser`] | 構文解析（パイプライン、リダイレクト、クォート、変数展開、`&`） |
//! | [`executor`] | コマンド実行（パイプライン接続、プロセスグループ管理） |
//! | [`builtins`] | ビルトイン（`exit`, `cd`, `pwd`, `echo`, `export`, `unset`, `jobs`, `fg`, `bg`） |
//! | [`job`] | ジョブコントロール（バックグラウンド実行、Ctrl+Z サスペンド、`fg`/`bg` 復帰） |
//! | [`shell`] | シェルのグローバル状態（終了ステータス、ジョブテーブル、PATH キャッシュ） |
//! | [`spawn`] | `posix_spawnp` ラッパー（外部コマンド起動の高速化） |

pub mod builtins;
pub mod complete;
pub mod editor;
pub mod executor;
pub mod highlight;
pub mod history;
pub mod job;
pub mod parser;
pub mod shell;
pub mod spawn;
