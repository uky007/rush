//! シンタックスハイライト（ANSI カラー付き出力生成）。
//!
//! バッファを文字単位でスキャンし、各トークンに色を付与する。
//! [`PathCache`] を使ってコマンドの有効性を判定する。
//!
//! ## カラースキーム
//!
//! | 要素 | 色 | ANSI コード |
//! |------|------|------------|
//! | 有効なコマンド（ビルトイン or PATH 内） | 太字緑 | `\x1b[1;32m` |
//! | 無効なコマンド | 太字赤 | `\x1b[1;31m` |
//! | 文字列（クォート内） | 黄 | `\x1b[33m` |
//! | 演算子（`\|`, `>`, `<`, `&`） | シアン | `\x1b[36m` |
//! | 変数（`$VAR`, `$?`） | マゼンタ | `\x1b[35m` |
//! | 引数・リダイレクト先 | デフォルト | （色なし） |
//!
//! ## 状態機械
//!
//! `command_position` と `redirect_target` の 2 フラグで状態を管理する:
//! - `command_position = true`: 次のワードをコマンドとして着色（`|` 後 or 行頭）
//! - `redirect_target = true`: 次のワードをリダイレクト先として着色なし（`>` / `<` 後）

use std::collections::HashSet;
use std::os::unix::fs::PermissionsExt;

use crate::builtins;

// ── ANSI カラーコード ─────────────────────────────────────────────

const GREEN_BOLD: &str = "\x1b[1;32m";
const RED_BOLD: &str = "\x1b[1;31m";
const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";
const MAGENTA: &str = "\x1b[35m";
const RESET: &str = "\x1b[0m";

// ── PATH キャッシュ ───────────────────────────────────────────────

/// `$PATH` 内の実行可能コマンド名をキャッシュする。
/// `$PATH` が変更されたら自動的に再構築する。
pub struct PathCache {
    /// `$PATH` 内の全実行可能コマンド名。
    commands: HashSet<String>,
    /// キャッシュ構築時の `$PATH` 値。変更検出に使う。
    path_str: String,
}

impl PathCache {
    pub fn new() -> Self {
        let mut cache = Self {
            commands: HashSet::new(),
            path_str: String::new(),
        };
        cache.refresh();
        cache
    }

    /// `$PATH` が変更されていればキャッシュを再構築する。
    pub fn refresh(&mut self) {
        let current = std::env::var("PATH").unwrap_or_default();
        if current == self.path_str && !self.commands.is_empty() {
            return;
        }
        self.path_str = current;
        self.commands.clear();
        for dir in self.path_str.split(':') {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    if let Ok(name) = entry.file_name().into_string() {
                        if is_executable(&entry.path()) {
                            self.commands.insert(name);
                        }
                    }
                }
            }
        }
    }

    /// コマンド名がキャッシュに存在するか判定する。
    pub fn has_command(&self, name: &str) -> bool {
        self.commands.contains(name)
    }

    /// `prefix` で始まるコマンド名をソート済みで返す。
    pub fn commands_with_prefix(&self, prefix: &str) -> Vec<String> {
        let mut matches: Vec<String> = self
            .commands
            .iter()
            .filter(|cmd| cmd.starts_with(prefix))
            .cloned()
            .collect();
        matches.sort();
        matches
    }
}

/// ファイルが実行可能か判定する（Unix パーミッションビット `0o111`）。
fn is_executable(path: &std::path::Path) -> bool {
    if let Ok(meta) = path.metadata() {
        if meta.is_file() {
            return meta.permissions().mode() & 0o111 != 0;
        }
    }
    false
}

/// コマンド名が有効か（ビルトイン or PATH 内に存在）。
pub fn is_valid_command(word: &str, cache: &PathCache) -> bool {
    builtins::is_builtin(word) || cache.has_command(word)
}

// ── ハイライト本体 ────────────────────────────────────────────────

/// バッファ全体をハイライトし、ANSI エスケープ付き文字列を返す。
///
/// 返り値の可視文字数は `buf` と同一（エスケープシーケンスは端末が解釈する）。
/// カーソル位置の計算には元の `buf` の文字数を使うこと。
pub fn highlight(buf: &str, cache: &PathCache) -> String {
    let bytes = buf.as_bytes();
    let len = bytes.len();
    let mut result = String::with_capacity(buf.len() * 2);
    let mut pos = 0;
    let mut command_position = true;
    let mut redirect_target = false;

    while pos < len {
        match bytes[pos] {
            b' ' | b'\t' => {
                result.push(bytes[pos] as char);
                pos += 1;
            }
            b'|' => {
                result.push_str(CYAN);
                result.push('|');
                result.push_str(RESET);
                pos += 1;
                command_position = true;
                redirect_target = false;
            }
            b'&' => {
                result.push_str(CYAN);
                result.push('&');
                result.push_str(RESET);
                pos += 1;
            }
            b'>' => {
                result.push_str(CYAN);
                result.push('>');
                pos += 1;
                if pos < len && bytes[pos] == b'>' {
                    result.push('>');
                    pos += 1;
                }
                result.push_str(RESET);
                redirect_target = true;
            }
            b'<' => {
                result.push_str(CYAN);
                result.push('<');
                result.push_str(RESET);
                pos += 1;
                redirect_target = true;
            }
            b'\'' => {
                result.push_str(YELLOW);
                result.push('\'');
                pos += 1;
                while pos < len && bytes[pos] != b'\'' {
                    result.push(bytes[pos] as char);
                    pos += 1;
                }
                if pos < len {
                    result.push('\'');
                    pos += 1;
                }
                result.push_str(RESET);
                command_position = false;
                redirect_target = false;
            }
            b'"' => {
                result.push_str(YELLOW);
                result.push('"');
                pos += 1;
                while pos < len && bytes[pos] != b'"' {
                    if bytes[pos] == b'$' {
                        result.push_str(MAGENTA);
                        result.push('$');
                        pos += 1;
                        while pos < len
                            && (bytes[pos].is_ascii_alphanumeric()
                                || bytes[pos] == b'_'
                                || bytes[pos] == b'?')
                        {
                            result.push(bytes[pos] as char);
                            pos += 1;
                        }
                        result.push_str(YELLOW);
                    } else {
                        result.push(bytes[pos] as char);
                        pos += 1;
                    }
                }
                if pos < len {
                    result.push('"');
                    pos += 1;
                }
                result.push_str(RESET);
                command_position = false;
                redirect_target = false;
            }
            _ => {
                // 通常ワード（変数 $VAR を含む可能性あり）
                let word_start = pos;
                while pos < len
                    && !matches!(
                        bytes[pos],
                        b' ' | b'\t' | b'|' | b'&' | b'>' | b'<' | b'\'' | b'"'
                    )
                {
                    pos += 1;
                }
                let word = &buf[word_start..pos];

                if redirect_target {
                    result.push_str(word);
                    redirect_target = false;
                } else if command_position {
                    if word.starts_with('$') {
                        highlight_with_vars(&mut result, word);
                    } else if is_valid_command(word, cache) {
                        result.push_str(GREEN_BOLD);
                        result.push_str(word);
                        result.push_str(RESET);
                    } else {
                        result.push_str(RED_BOLD);
                        result.push_str(word);
                        result.push_str(RESET);
                    }
                    command_position = false;
                } else {
                    highlight_with_vars(&mut result, word);
                }
            }
        }
    }

    result
}

/// ワード内の `$VAR` / `$?` をマゼンタで着色する。
fn highlight_with_vars(result: &mut String, word: &str) {
    let bytes = word.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        if bytes[i] == b'$'
            && i + 1 < len
            && (bytes[i + 1].is_ascii_alphabetic() || bytes[i + 1] == b'_' || bytes[i + 1] == b'?')
        {
            result.push_str(MAGENTA);
            result.push('$');
            i += 1;
            if i < len && bytes[i] == b'?' {
                result.push('?');
                i += 1;
            } else {
                while i < len && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    result.push(bytes[i] as char);
                    i += 1;
                }
            }
            result.push_str(RESET);
        } else {
            result.push(bytes[i] as char);
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_builtin_is_green() {
        let cache = PathCache {
            commands: HashSet::new(),
            path_str: String::new(),
        };
        let out = highlight("echo", &cache);
        assert!(out.contains(GREEN_BOLD));
        assert!(out.contains("echo"));
    }

    #[test]
    fn invalid_command_is_red() {
        let cache = PathCache {
            commands: HashSet::new(),
            path_str: String::new(),
        };
        let out = highlight("nosuchcmd", &cache);
        assert!(out.contains(RED_BOLD));
    }

    #[test]
    fn pipe_is_cyan() {
        let cache = PathCache {
            commands: HashSet::new(),
            path_str: String::new(),
        };
        let out = highlight("echo hello | exit", &cache);
        assert!(out.contains(&format!("{}|{}", CYAN, RESET)));
    }

    #[test]
    fn variable_is_magenta() {
        let cache = PathCache {
            commands: HashSet::new(),
            path_str: String::new(),
        };
        let out = highlight("echo $HOME", &cache);
        assert!(out.contains(MAGENTA));
        assert!(out.contains("$HOME"));
    }

    #[test]
    fn quoted_string_is_yellow() {
        let cache = PathCache {
            commands: HashSet::new(),
            path_str: String::new(),
        };
        let out = highlight("echo \"hello\"", &cache);
        assert!(out.contains(YELLOW));
    }

    #[test]
    fn command_after_pipe_is_colored() {
        let cache = PathCache {
            commands: HashSet::new(),
            path_str: String::new(),
        };
        let out = highlight("echo hello | exit", &cache);
        // "exit" after pipe should be green (valid builtin)
        assert!(out.contains(&format!("{}exit{}", GREEN_BOLD, RESET)));
    }

    #[test]
    fn longest_common_prefix_basic() {
        let candidates = vec!["foobar".to_string(), "foobaz".to_string()];
        assert_eq!(
            crate::complete::longest_common_prefix(&candidates),
            "fooba"
        );
    }
}
