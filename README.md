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
- **ゼロコピーパース** — 入力文字列への参照（`Cow::Borrowed`）でトークン化、String割り当てを最小化
- **スタック配列化** — パイプ/PID 配列を 8 段以下はスタックに確保し、ヒープ割り当てを排除
- **PATH キャッシュ** — `$PATH` 内コマンドを `HashSet` でキャッシュし、変更検出で自動再構築

### 2. Simple and Minimal — シンプルに保つ

- POSIX互換を目指さない（必要最低限の実用的サブセットのみ）
- 構造化データエンジンは作らない（Nushellとは異なるアプローチ）
- 機能追加より既存機能の速度改善を優先

### 3. Learn by Building — 作って学ぶ

- 外部クレートに頼りすぎず、コア部分は自作して理解を深める
- パーサー、プロセス管理、ジョブコントロールは自前実装
- 行編集も `libc` のみで自前実装（raw モード、termios 操作）

## Architecture Overview

```
┌──────────────────────────────────────────────┐
│                   main loop                   │
│  prompt → read_line → parse → execute → loop  │
└─────┬───────┬──────────┬──────────┬──────────┘
      │       │          │          │
      v       v          v          v
  ┌───────┐ ┌──────┐ ┌───────┐ ┌──────────┐
  │Shell  │ │Line  │ │Parser │ │Executor  │
  │(state)│ │Editor│ │(zero- │ │          │
  │       │ │      │ │ copy) │ │ builtin  │
  └───────┘ └──┬───┘ └───┬───┘ │ external │
               │         │     │ pipeline │
          ┌────┴────┐    v     └────┬─────┘
          │highlight│ ┌─────┐      │
          │complete │ │ AST │      v
          │history  │ │(Cow)│ ┌─────────┐
          └─────────┘ └─────┘ │ spawn   │
                              │(posix_  │
                              │ spawnp) │
                              └─────────┘
```

### Core Modules

| モジュール | 責務 | 高速化手法 |
|-----------|------|-----------|
| `parser` | 入力をAST（コマンド列）に変換 | ゼロコピー (`Cow::Borrowed`) |
| `executor` | コマンドの実行・パイプライン接続 | ビルトイン in-process、スタック配列 |
| `spawn` | 外部コマンドの起動 (`posix_spawnp`) | fork+exec 回避、RAII ラッパー |
| `builtins` | cd, echo, export 等の組込コマンド | fork 不要、直接実行 |
| `job` | ジョブコントロール (bg/fg/jobs) | waitpid 手動 reap |
| `editor` | 行編集 (raw モード、キー入力) | libc 直接操作、1 回の write(2) |
| `highlight` | シンタックスハイライト・PATH キャッシュ | HashSet キャッシュ、変更検出 |
| `complete` | Tab 補完（コマンド名、ファイル名） | PATH キャッシュ共有 |
| `history` | コマンド履歴 (~/.rush_history) | 追記モード永続化 |
| `shell` | シェルのグローバル状態 | PATH キャッシュ統合 |

## Implementation Phases

### Phase 1: Minimal REPL ✅
- REPLループ (`main.rs`): プロンプト表示 → 入力読み取り → コマンド実行 → ループ
- シェル状態管理 (`shell.rs`): 終了ステータスと終了フラグ
- ビルトイン (`builtins.rs`): `exit [N]`, `cd [dir]`
- コマンド実行 (`executor.rs`): ビルトイン優先 → 外部コマンド spawn
- SIGINTを無視してCtrl+Cからシェルを保護
- プロンプトに終了ステータスを表示 (`[N] rush$ `)

### Phase 2: Parser ✅
- パイプライン (`cmd1 | cmd2 | cmd3`)
- リダイレクト (`>`, `<`, `>>`, `2>`)
- クォート処理 (`"..."`, `'...'`)

### Phase 3: Builtins ✅
- cd, pwd, echo, export, unset, exit
- 環境変数展開 (`$HOME`, `$PATH`)

### Phase 4: Job Control ✅
- バックグラウンド実行 (`&`)
- Ctrl+Z (SIGTSTP) / fg / bg
- ジョブ一覧 (jobs)

### Phase 5: Line Editing ✅
- カーソル移動、履歴 (↑↓)
- Tab補完
- シンタックスハイライト

### Phase 6: Performance Tuning ✅
- `posix_spawnp()` 導入 (`spawn.rs`): fork+exec → posix_spawn で外部コマンド起動を高速化
- スタック配列化 (`executor.rs`): パイプ `[[i32;2]; 7]` / PID `[pid_t; 8]` / close fd `[i32; 16]`
- PATH キャッシュ統合 (`shell.rs`): `PathCache` を Shell に追加
- ベンチマーク整備 (`benches/bench_main.rs`): パーサー・ビルトイン・spawn・E2E 計測

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

## Benchmarking

```bash
cargo bench
```

## License

MIT
