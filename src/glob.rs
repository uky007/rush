//! パス名展開（glob）: `*`, `?`, `[...]` によるファイル名マッチング。
//!
//! ## 対応パターン
//!
//! - `*` — 0 文字以上の任意の文字にマッチ
//! - `?` — 任意の 1 文字にマッチ
//! - `[abc]` — 文字クラス（列挙された任意の 1 文字にマッチ）
//! - `[a-z]` — 範囲指定（ASCII 範囲の任意の 1 文字にマッチ）
//! - `[!...]` / `[^...]` — 否定文字クラス（マッチしない文字にマッチ）
//!
//! `.` で始まるファイルはパターンが `.` で始まる場合のみマッチ（bash 互換）。

/// パターンにグロブ文字（`*`, `?`）が含まれるか判定する。
pub fn has_glob_chars(s: &str) -> bool {
    s.bytes().any(|b| b == b'*' || b == b'?' || b == b'[')
}

/// パターンを展開し、マッチするファイルパスをソート済みで返す。
/// マッチなし → 元のパターンを含む Vec を返す。
pub fn expand(pattern: &str) -> Vec<String> {
    let results = if let Some(slash_pos) = pattern.rfind('/') {
        // パターンに `/` が含まれる場合
        let dir_part = &pattern[..slash_pos];
        let file_part = &pattern[slash_pos + 1..];

        if has_glob_chars(dir_part) {
            // ディレクトリ部分にもグロブがある → 再帰的に展開
            let dir_candidates = expand(dir_part);
            let mut matches = Vec::new();
            for dir in &dir_candidates {
                if let Ok(meta) = std::fs::metadata(dir) {
                    if meta.is_dir() {
                        matches.extend(expand_in_dir(dir, file_part));
                    }
                }
            }
            matches
        } else {
            // ディレクトリ部分にグロブなし
            let dir = if dir_part.is_empty() { "/" } else { dir_part };
            expand_in_dir(dir, file_part)
        }
    } else {
        // パターンに `/` がない → カレントディレクトリ
        expand_in_dir(".", pattern)
    };

    if results.is_empty() {
        vec![pattern.to_string()]
    } else {
        results
    }
}

/// 指定ディレクトリ内でファイル名パターンにマッチするエントリを返す。
fn expand_in_dir(dir: &str, file_pattern: &str) -> Vec<String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut matches = Vec::new();
    for entry in entries.flatten() {
        if let Ok(name) = entry.file_name().into_string() {
            // `.` で始まるファイルはパターンが `.` で始まる場合のみマッチ
            if name.starts_with('.') && !file_pattern.starts_with('.') {
                continue;
            }
            if matches_pattern(file_pattern, &name) {
                if dir == "." {
                    matches.push(name);
                } else {
                    matches.push(format!("{}/{}", dir, name));
                }
            }
        }
    }
    matches.sort();
    matches
}

/// パターン文字列とファイル名を照合する。
/// `*` は 0 文字以上、`?` は任意の 1 文字にマッチ。
pub fn matches_pattern(pattern: &str, name: &str) -> bool {
    let pat = pattern.as_bytes();
    let nam = name.as_bytes();
    matches_recursive(pat, 0, nam, 0)
}

fn matches_recursive(pat: &[u8], pi: usize, nam: &[u8], ni: usize) -> bool {
    let plen = pat.len();
    let nlen = nam.len();

    let mut pi = pi;
    let mut ni = ni;

    while pi < plen {
        match pat[pi] {
            b'*' => {
                // 連続する * をスキップ
                while pi < plen && pat[pi] == b'*' {
                    pi += 1;
                }
                // パターン末尾が * → 残り全部マッチ
                if pi == plen {
                    return true;
                }
                // 残りのパターンを name の全接尾辞と照合
                for start in ni..=nlen {
                    if matches_recursive(pat, pi, nam, start) {
                        return true;
                    }
                }
                return false;
            }
            b'?' => {
                if ni >= nlen {
                    return false;
                }
                pi += 1;
                ni += 1;
            }
            b'[' => {
                if ni >= nlen {
                    return false;
                }
                pi += 1; // skip '['
                let negate = pi < plen && (pat[pi] == b'!' || pat[pi] == b'^');
                if negate {
                    pi += 1;
                }
                let mut matched = false;
                let ch = nam[ni];
                // `]` を文字クラスの最初に置ける（bash 互換）
                let first = true;
                let mut first_iter = first;
                while pi < plen && (pat[pi] != b']' || first_iter) {
                    first_iter = false;
                    // range: a-z
                    if pi + 2 < plen && pat[pi + 1] == b'-' && pat[pi + 2] != b']' {
                        let lo = pat[pi];
                        let hi = pat[pi + 2];
                        if (lo <= ch && ch <= hi) || (hi <= ch && ch <= lo) {
                            matched = true;
                        }
                        pi += 3;
                    } else {
                        if pat[pi] == ch {
                            matched = true;
                        }
                        pi += 1;
                    }
                }
                // skip closing ']'
                if pi < plen && pat[pi] == b']' {
                    pi += 1;
                } else {
                    // 閉じ括弧がない → リテラルとして不一致
                    return false;
                }
                if negate {
                    matched = !matched;
                }
                if !matched {
                    return false;
                }
                ni += 1;
            }
            ch => {
                if ni >= nlen || nam[ni] != ch {
                    return false;
                }
                pi += 1;
                ni += 1;
            }
        }
    }

    ni == nlen
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pattern_star_suffix() {
        assert!(matches_pattern("*.txt", "hello.txt"));
        assert!(!matches_pattern("*.txt", "hello.rs"));
    }

    #[test]
    fn pattern_question_mark() {
        assert!(matches_pattern("h?llo", "hello"));
        assert!(!matches_pattern("h?llo", "hllo"));
    }

    #[test]
    fn pattern_star_anything() {
        assert!(matches_pattern("*", "anything"));
    }

    #[test]
    fn pattern_star_in_middle() {
        assert!(matches_pattern("foo*bar", "foobazbar"));
        assert!(matches_pattern("foo*bar", "foobar"));
        assert!(!matches_pattern("foo*bar", "foobaz"));
    }

    #[test]
    fn pattern_exact_match() {
        assert!(matches_pattern("hello", "hello"));
        assert!(!matches_pattern("hello", "world"));
    }

    #[test]
    fn pattern_empty() {
        assert!(matches_pattern("", ""));
        assert!(!matches_pattern("", "a"));
        assert!(matches_pattern("*", ""));
    }

    #[test]
    fn pattern_multiple_stars() {
        assert!(matches_pattern("*.*", "foo.bar"));
        assert!(!matches_pattern("*.*", "foobar"));
    }

    #[test]
    fn has_glob_chars_true() {
        assert!(has_glob_chars("*.txt"));
        assert!(has_glob_chars("h?llo"));
        assert!(has_glob_chars("a*b?c"));
        assert!(has_glob_chars("[abc]"));
    }

    #[test]
    fn has_glob_chars_false() {
        assert!(!has_glob_chars("hello"));
        assert!(!has_glob_chars(""));
        assert!(!has_glob_chars("path/to/file.txt"));
    }

    #[test]
    fn expand_no_match_returns_pattern() {
        let result = expand("nosuch_xyz_pattern_*.qqqq");
        assert_eq!(result, vec!["nosuch_xyz_pattern_*.qqqq"]);
    }

    #[test]
    fn bracket_char_list() {
        assert!(matches_pattern("[abc]", "a"));
        assert!(matches_pattern("[abc]", "b"));
        assert!(matches_pattern("[abc]", "c"));
        assert!(!matches_pattern("[abc]", "d"));
    }

    #[test]
    fn bracket_range() {
        assert!(matches_pattern("[a-z]", "m"));
        assert!(matches_pattern("[a-z]", "a"));
        assert!(matches_pattern("[a-z]", "z"));
        assert!(!matches_pattern("[a-z]", "A"));
        assert!(!matches_pattern("[a-z]", "0"));
    }

    #[test]
    fn bracket_range_digits() {
        assert!(matches_pattern("file[0-9].txt", "file3.txt"));
        assert!(!matches_pattern("file[0-9].txt", "filea.txt"));
    }

    #[test]
    fn bracket_negate() {
        assert!(!matches_pattern("[!abc]", "a"));
        assert!(matches_pattern("[!abc]", "d"));
        assert!(matches_pattern("[^0-9]", "x"));
        assert!(!matches_pattern("[^0-9]", "5"));
    }

    #[test]
    fn bracket_with_star() {
        assert!(matches_pattern("[A-Z]*", "Hello"));
        assert!(!matches_pattern("[A-Z]*", "hello"));
    }

    #[test]
    fn bracket_multiple_ranges() {
        assert!(matches_pattern("[a-zA-Z]", "G"));
        assert!(matches_pattern("[a-zA-Z]", "g"));
        assert!(!matches_pattern("[a-zA-Z]", "5"));
    }
}
