//! Tab 補完（コマンド名、ファイル名、チルダ展開、`&&`/`||`/`;` 後のコマンド位置認識）。
//!
//! カーソル位置の単語を抽出し、コマンドリスト・パイプライン内での位置に応じて
//! コマンド名補完またはファイル名補完を行う。
//!
//! ## 補完の種類
//!
//! - **コマンド名補完**（行頭 or `|`/`&&`/`||`/`;` の後の最初の単語）:
//!   ビルトイン一覧 + `$PATH` 内の実行可能ファイルから候補を収集
//! - **ファイル名補完**（それ以外の位置）:
//!   カレントディレクトリまたは指定ディレクトリのファイル名から候補を収集。
//!   `~/` プレフィックスはチルダ展開してディレクトリを検索し、
//!   表示用にはオリジナルの `~` プレフィックスを維持する。
//!
//! ## 候補の適用（[`editor`](crate::editor) 側で処理）
//!
//! - 候補 0 件 → ベル
//! - 候補 1 件 → 単語を置換 + 末尾にスペース（ディレクトリなら `/`）
//! - 候補複数 → 共通接頭辞まで補完 + 候補一覧を表示

use crate::highlight::PathCache;
use crate::parser;

/// コマンド名補完に使うビルトイン一覧（アルファベット順）。
///
/// [`builtins::is_builtin`](crate::builtins::is_builtin) と同期させること。
const BUILTINS: &[&str] = &[".", ":", "[", "alias", "bg", "builtin", "cd", "command", "dirs", "echo", "exec", "exit", "export", "false", "fg", "history", "jobs", "popd", "printf", "pushd", "pwd", "read", "return", "source", "test", "trap", "true", "type", "unalias", "unset", "wait"];

/// Tab 補完の結果。候補リストと補完対象の単語位置を持つ。
pub struct CompletionResult {
    /// 補完候補のリスト（ソート済み・重複なし）。
    pub candidates: Vec<String>,
    /// 補完対象の単語の開始バイトオフセット（バッファ内）。
    pub word_start: usize,
    /// 補完対象の単語の終了バイトオフセット（= カーソル位置）。
    pub word_end: usize,
}

/// カーソル位置の単語に対する補完候補を返す。
pub fn complete(buf: &str, cursor: usize, cache: &PathCache) -> CompletionResult {
    let (word_start, word, is_command) = current_word(buf, cursor);

    let candidates = if is_command {
        find_commands(word, cache)
    } else {
        find_files(word)
    };

    CompletionResult {
        candidates,
        word_start,
        word_end: cursor,
    }
}

/// カーソル位置の単語を抽出する。
/// 戻り値: (word_start_byte, word, is_first_word_in_segment)
fn current_word(buf: &str, cursor: usize) -> (usize, &str, bool) {
    let before = &buf[..cursor];
    let word_start = before
        .rfind(|c: char| c == ' ' || c == '\t')
        .map(|i| i + 1)
        .unwrap_or(0);
    let word = &buf[word_start..cursor];

    // パイプ / &&  / || / ; の後 or 行頭ならコマンド位置
    let prefix = buf[..word_start].trim_end();
    let is_command = prefix.is_empty()
        || prefix.ends_with('|')
        || prefix.ends_with("&&")
        || prefix.ends_with("||")
        || prefix.ends_with(';');

    (word_start, word, is_command)
}

/// ビルトイン + PATH コマンドから prefix に一致するものを返す。
fn find_commands(prefix: &str, cache: &PathCache) -> Vec<String> {
    let mut results: Vec<String> = BUILTINS
        .iter()
        .filter(|&&b| b.starts_with(prefix))
        .map(|&b| b.to_string())
        .collect();

    results.extend(cache.commands_with_prefix(prefix));
    results.sort();
    results.dedup();
    results
}

/// ファイル名補完。ディレクトリには末尾 `/` を付加する。
///
/// `prefix` に `/` が含まれればそのディレクトリを基準に検索し、
/// 含まれなければカレントディレクトリを検索する。
/// `.` で始まる隠しファイルは `prefix` が `.` で始まる場合のみ候補に含める。
fn find_files(prefix: &str) -> Vec<String> {
    // チルダ展開してディレクトリ検索
    let expanded_prefix = parser::expand_tilde(prefix);

    let (dir_str, file_prefix, display_dir) = if let Some(slash_pos) = expanded_prefix.rfind('/') {
        let search_dir = &expanded_prefix[..slash_pos + 1];
        let file_part = &expanded_prefix[slash_pos + 1..];
        // 表示用はオリジナルの ~ プレフィックスを維持
        let orig_dir = if prefix.starts_with('~') {
            if let Some(orig_slash) = prefix.rfind('/') {
                &prefix[..orig_slash + 1]
            } else {
                // ~/... のスラッシュが展開で変わるケース
                search_dir
            }
        } else {
            search_dir
        };
        (search_dir.to_string(), file_part.to_string(), orig_dir.to_string())
    } else {
        ("".to_string(), expanded_prefix.to_string(), "".to_string())
    };

    let search_dir = if dir_str.is_empty() {
        "."
    } else if dir_str == "/" {
        "/"
    } else {
        &dir_str[..dir_str.len() - 1]
    };

    let mut results = Vec::new();
    if let Ok(entries) = std::fs::read_dir(search_dir) {
        for entry in entries.flatten() {
            if let Ok(name) = entry.file_name().into_string() {
                // 隠しファイルは prefix が '.' で始まる場合のみ表示
                if !name.starts_with(file_prefix.as_str()) {
                    continue;
                }
                if name.starts_with('.') && !file_prefix.starts_with('.') {
                    continue;
                }
                let is_dir = entry.file_type().map_or(false, |t| t.is_dir());
                let candidate = format!(
                    "{}{}{}",
                    display_dir,
                    name,
                    if is_dir { "/" } else { "" }
                );
                results.push(candidate);
            }
        }
    }

    results.sort();
    results
}

/// 候補群の最長共通接頭辞を返す。UTF-8 文字境界を考慮する。
///
/// 複数候補がある場合にまず共通部分まで補完するために使用する。
/// 候補が空なら空文字列を返す。
pub fn longest_common_prefix(candidates: &[String]) -> &str {
    if candidates.is_empty() {
        return "";
    }
    let first = &candidates[0];
    let mut prefix_len = first.len();
    for candidate in &candidates[1..] {
        prefix_len = first
            .bytes()
            .zip(candidate.bytes())
            .take(prefix_len)
            .take_while(|(a, b)| a == b)
            .count();
    }
    // UTF-8 境界に合わせる
    while prefix_len > 0 && !first.is_char_boundary(prefix_len) {
        prefix_len -= 1;
    }
    &first[..prefix_len]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lcp_basic() {
        let c = vec!["foobar".to_string(), "foobaz".to_string()];
        assert_eq!(longest_common_prefix(&c), "fooba");
    }

    #[test]
    fn lcp_single() {
        let c = vec!["hello".to_string()];
        assert_eq!(longest_common_prefix(&c), "hello");
    }

    #[test]
    fn lcp_empty() {
        let c: Vec<String> = vec![];
        assert_eq!(longest_common_prefix(&c), "");
    }

    #[test]
    fn lcp_no_common() {
        let c = vec!["abc".to_string(), "xyz".to_string()];
        assert_eq!(longest_common_prefix(&c), "");
    }

    #[test]
    fn current_word_first() {
        let (start, word, is_cmd) = current_word("ec", 2);
        assert_eq!(start, 0);
        assert_eq!(word, "ec");
        assert!(is_cmd);
    }

    #[test]
    fn current_word_after_space() {
        let (start, word, is_cmd) = current_word("echo hel", 8);
        assert_eq!(start, 5);
        assert_eq!(word, "hel");
        assert!(!is_cmd);
    }

    #[test]
    fn current_word_after_pipe() {
        let (start, word, is_cmd) = current_word("echo hello | gr", 15);
        assert_eq!(word, "gr");
        assert!(is_cmd);
        assert_eq!(start, 13);
    }

    #[test]
    fn find_commands_matches_builtins() {
        let cache = PathCache::new();
        let results = find_commands("ech", &cache);
        assert!(results.contains(&"echo".to_string()));
    }
}
