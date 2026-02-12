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
| `parser` | 入力をAST（コマンド列）に変換。変数展開、パラメータ展開、算術展開、継続行検出 | ゼロコピー (`Cow::Borrowed`) |
| `executor` | コマンド実行・パイプライン接続。展開パイプライン: コマンド置換 → チルダ → ブレース → glob | ビルトイン in-process、スタック配列 |
| `spawn` | 外部コマンドの起動 (`posix_spawnp`)。fd 複製 (`2>&1`) 対応 | fork+exec 回避、RAII ラッパー |
| `builtins` | cd, pwd, echo, export, unset, source, read, exec, wait, type, command, builtin 等 20 種 | fork 不要、直接実行 |
| `job` | ジョブコントロール (bg/fg/jobs/wait) | waitpid 手動 reap |
| `editor` | 行編集 (raw モード、Ctrl+R 逆方向検索、Tab 補完、シンタックスハイライト) | libc 直接操作、1 回の write(2) |
| `highlight` | シンタックスハイライト・PATH キャッシュ | HashSet キャッシュ、変更検出 |
| `complete` | Tab 補完（コマンド名、ファイル名、`&&`/`||`/`;` 後のコマンド位置認識） | PATH キャッシュ共有 |
| `history` | コマンド履歴 (~/.rush_history)、逆方向検索、ナビゲーション | 追記モード永続化 |
| `shell` | シェルのグローバル状態（エイリアスマップ含む） | PATH キャッシュ統合 |

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

### Phase 7: Advanced Syntax ✅
- 複合コマンド: `&&` (AND), `||` (OR), `;` (順次実行)
- エスケープ: `\"`, `\\`, `\$`（ダブルクォート内）, `\X`（裸ワード内）
- `${VAR}` 展開（接尾辞安全な変数展開）
- glob 展開 (`*.rs`, `?` パターン)

### Phase 8: Shell Extensions ✅
- コマンド置換: `$(cmd)`, `` `cmd` ``（fork + pipe + waitpid で stdout をキャプチャ）
- チルダ展開: `~` → `$HOME`, `~/path`, `~user`（`getpwnam`）, `VAR=~/path`
- fd 複製リダイレクト: `2>&1`, `>&2`（`posix_spawn_file_actions_adddup2` で実装）
- `type` ビルトイン: コマンドの所在表示（ビルトイン / 外部コマンドの PATH 検索）
- `~/.rushrc` 読み込み: 起動時に RC ファイルを行単位で実行（コメント `#` 対応）
- Tab 補完のチルダ対応: `~/` プレフィックスを展開してディレクトリを検索
- シンタックスハイライト拡張: `$(cmd)` / バッククォートをシアン、`2>&1` をシアンで着色
- 展開パイプライン統一: `expand_args_full`（コマンド置換 → チルダ → glob の順序で適用）
- ベンチマーク追加: チルダ展開の計測

### Phase 9: Practical Daily-Use Features ✅
- **`cd -`**: `OLDPWD` 追跡、`cd -` で前ディレクトリに移動
- **`source` / `.`**: ファイルを行単位で実行（RC ファイル読み込みと同パターン）
- **`alias` / `unalias`**: エイリアス定義・一覧・削除（再帰ガード付き展開、`unalias -a` 対応）
- **`history` ビルトイン**: `history` / `history N` / `history -c`（editor 所有の履歴に直接アクセス）
- **Ctrl+R 逆方向検索**: `(reverse-i-search)'query': match` 形式のインクリメンタル検索
- **`command` / `builtin`**: `command -v` でパス表示、`command` でエイリアスバイパス、`builtin` でビルトインのみ実行
- **`read`**: `read VAR` / `read -p "prompt" VAR` / 複数変数 IFS 分割 / `REPLY` デフォルト
- **`exec`**: `execvp` でシェルプロセスを置換（シグナル復元付き）
- **`wait`**: `wait` で全ジョブ待機 / `wait %N` で特定ジョブ待機
- **文字列パラメータ展開**: `${var:-default}`, `${var:=default}`, `${var:+alt}`, `${var:?msg}`, `${#var}`, `${var%pat}`, `${var%%pat}`, `${var#pat}`, `${var##pat}`, `${var/pat/repl}`, `${var//pat/repl}`
- **算術展開 `$(( ))`**: 再帰下降パーサー（`+`, `-`, `*`, `/`, `%`, 括弧、変数参照、i64 演算）
- **ブレース展開**: `{a,b,c}` カンマ展開、`{1..5}` 数値レンジ、`{a..z}` 文字レンジ、ネスト対応
- **継続行入力**: 末尾 `\`・未完了パイプ/演算子・未閉クォートで `> ` プロンプトによる複数行入力

### Phase 10: Extended Shell Features (進行中)
- **`true` / `false` / `:` / `return`**: フロー制御ビルトイン（`return` は `source` 内でのみ有効）
- **特殊変数 `$$`, `$!`, `$0`**: プロセス ID、直前バックグラウンド PID、シェル名
- **`2>>` stderr 追記リダイレクト**: 標準エラーを追記モードで開く
- **キルリング・ワード移動**: Ctrl+Y (yank), Alt+F/B (ワード前後移動), Alt+D (ワード前方削除)
- **`[a-z]` 文字クラス glob**: `[abc]`、`[a-z]` 範囲、`[!...]`/`[^...]` 否定
- **`VAR=val cmd` インライン代入**: コマンド先頭の変数代入（一時環境変数、ビルトイン・外部コマンド対応）
- **非インタラクティブモード**: `rush -c 'cmd'`、`rush script.sh`（スクリプト実行対応）
- **`$RANDOM` / `$SECONDS`**: 動的特殊変数（疑似乱数 0-32767、起動からの経過秒数）
- **`test` / `[` ビルトイン**: 条件判定（`-n`/`-z` 文字列、`=`/`!=`、`-eq`/`-lt`/`-gt` 等整数比較、`-e`/`-f`/`-d`/`-r`/`-w`/`-x`/`-s` ファイル、`!` 否定）
- **`printf` ビルトイン**: フォーマット出力（`%s`/`%d`/`%x`/`%o`、幅指定、ゼロパディング、`\n`/`\t` エスケープ）

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
