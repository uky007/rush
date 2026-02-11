# rush - Rust Shell

動作速度を重視した学習目的のRust製コンソールシェル。

## Goals

### Primary: 体感速度の追求

「サクサク動く」シェルを実現する。具体的な指標：

| 指標 | 目標値 | 参考 (bash) |
|------|--------|-------------|
| 起動時間 | < 5ms | ~4ms |
| 単純コマンド実行 (echo等) | < 1ms | ~2ms |
| 外部コマンド起動オーバーヘッド | < 0.5ms | ~1ms |
| プロンプト表示遅延 | < 10ms | ~5ms (素) |

### Secondary: 学習と理解

- Rustの所有権・ライフタイムシステムを実践的に学ぶ
- OSのプロセス管理・シグナル・ファイルディスクリプタを深く理解する
- シェルの内部構造を一から構築して把握する

## Design Principles

### 1. Speed First — 速度最優先

全設計判断において速度を最優先する。

- **ビルトインコマンドはin-process実行** — fork/execを回避し、cd/echo/export等を直接実行
- **`posix_spawn()` 優先** — 外部コマンドはfork+execではなくposix_spawnで生成（macOSで2-5倍高速）
- **ゼロコピーパース** — 入力文字列への参照でトークン化、String割り当てを最小化
- **アリーナアロケーション** — コマンドライン毎にbump allocatorで一括確保・一括解放
- **遅延初期化** — プロンプトを即座に表示、設定・履歴・補完はバックグラウンドで読み込み

### 2. Simple and Minimal — シンプルに保つ

- POSIX互換を目指さない（必要最低限の実用的サブセットのみ）
- 構造化データエンジンは作らない（Nushellとは異なるアプローチ）
- 機能追加より既存機能の速度改善を優先

### 3. Learn by Building — 作って学ぶ

- 外部クレートに頼りすぎず、コア部分は自作して理解を深める
- パーサー、プロセス管理、ジョブコントロールは自前実装
- 行編集は既存クレート（reedline等）の利用も検討

## Architecture Overview

```
┌─────────────────────────────────────────────┐
│                   main loop                  │
│  prompt → read_line → parse → execute → loop │
└─────┬───────┬──────────┬──────────┬─────────┘
      │       │          │          │
      v       v          v          v
  ┌───────┐ ┌──────┐ ┌───────┐ ┌──────────┐
  │Prompt │ │Line  │ │Parser │ │Executor  │
  │Render │ │Editor│ │(zero- │ │          │
  │(async)│ │      │ │ copy) │ │ builtin  │
  └───────┘ └──────┘ └───┬───┘ │ external │
                         │     │ pipeline │
                         v     └──────────┘
                    ┌─────────┐
                    │   AST   │
                    │ (arena  │
                    │  alloc) │
                    └─────────┘
```

### Core Modules

| モジュール | 責務 | 高速化手法 |
|-----------|------|-----------|
| `parser` | 入力をAST（コマンド列）に変換 | ゼロコピー、アリーナ割当 |
| `executor` | コマンドの実行・プロセス生成 | ビルトインin-process、posix_spawn |
| `builtins` | cd, echo, export等の組込コマンド | fork不要、直接実行 |
| `pipeline` | パイプライン接続・リダイレクト | pipe(2)直接操作 |
| `job` | ジョブコントロール (bg/fg) | シグナル管理 |
| `line_editor` | 行編集・履歴・補完 | 非同期補完、差分描画 |
| `prompt` | プロンプト文字列の生成 | バックグラウンド計算 |
| `env` | 環境変数・PATHキャッシュ | ハッシュマップキャッシュ |

## Implementation Phases

### Phase 1: Minimal REPL
- プロンプト表示 → 入力読み取り → 外部コマンド実行
- 最低限の動くシェル

### Phase 2: Parser
- パイプライン (`cmd1 | cmd2 | cmd3`)
- リダイレクト (`>`, `<`, `>>`, `2>`)
- クォート処理 (`"..."`, `'...'`)

### Phase 3: Builtins
- cd, pwd, echo, export, unset, exit
- 環境変数展開 (`$HOME`, `$PATH`)

### Phase 4: Job Control
- バックグラウンド実行 (`&`)
- Ctrl+Z (SIGTSTP) / fg / bg
- ジョブ一覧 (jobs)

### Phase 5: Line Editing
- カーソル移動、履歴 (↑↓)
- Tab補完
- シンタックスハイライト

### Phase 6: Performance Tuning
- posix_spawn() 導入
- PATHキャッシュ
- アリーナアロケーション最適化
- ベンチマーク整備・計測

## Speed Comparison Targets

サーベイに基づく既存シェルとの比較目標：

| シェル | 起動時間 | メモリ(RSS) | 特徴 |
|--------|---------|------------|------|
| dash | ~1ms | ~1.5MB | 最速、行編集なし |
| bash | ~4ms | ~3-4MB | 標準 |
| zsh | ~5ms | ~4-6MB | プラグインで激重化 |
| fish | ~7ms | ~8-12MB | リッチUI |
| nushell | ~20-40ms | ~25-40MB | 構造化データ |
| **rush** | **< 5ms** | **< 5MB** | **速度特化** |

rushはbash並の起動速度を維持しつつ、コマンド実行のオーバーヘッドを最小化することを目指す。

## Building

```bash
cargo build --release
```

## License

MIT
