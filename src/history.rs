//! コマンド履歴の管理。
//!
//! `~/.rush_history` にプレーンテキスト（1 行 1 コマンド）で永続化し、起動時に読み込む。
//! ↑↓キーによるナビゲーションで過去のコマンドを呼び出せる。
//!
//! ## ファイル形式
//!
//! - パス: `$HOME/.rush_history`（`$HOME` 未設定時は `/tmp/.rush_history`）
//! - 書き込み: 追記モード（[`OpenOptions::append`]）で 1 コマンドずつ追記
//! - 最大エントリ数: 1000（超過時は古いエントリから削除）
//! - 直前と同一のコマンドは追加しない（連続重複排除）
//!
//! ## ナビゲーション
//!
//! `nav_index` は `entries` のインデックスで、`entries.len()` は「現在の入力」を指す。
//! ↑で `nav_index` を減少、↓で増加し、末尾に到達すると `saved_buf`（保存した入力）を復元する。

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

/// コマンド履歴。エントリの永続化とナビゲーション状態を管理する。
pub struct History {
    /// 履歴エントリのリスト（古い順）。
    entries: Vec<String>,
    /// 保持する最大エントリ数。
    max_size: usize,
    /// 現在のナビゲーション位置。`entries.len()` は「現在の入力」を意味する。
    nav_index: usize,
    /// ↑で履歴に入る前の入力バッファ。↓で末尾に戻ったときに復元する。
    saved_buf: String,
    /// 履歴ファイルのパス（`~/.rush_history`）。
    path: PathBuf,
}

impl History {
    /// 新しい `History` を作成し、`~/.rush_history` から既存エントリを読み込む。
    pub fn new() -> Self {
        let path = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp"))
            .join(".rush_history");
        let mut h = Self {
            entries: Vec::new(),
            max_size: 1000,
            nav_index: 0,
            saved_buf: String::new(),
            path,
        };
        h.load();
        h
    }

    /// 履歴ファイルからエントリを読み込む。ファイルが存在しなければ何もしない。
    fn load(&mut self) {
        if let Ok(file) = fs::File::open(&self.path) {
            let reader = BufReader::new(file);
            for line in reader.lines().flatten() {
                if !line.is_empty() {
                    self.entries.push(line);
                }
            }
            if self.entries.len() > self.max_size {
                let start = self.entries.len() - self.max_size;
                self.entries = self.entries[start..].to_vec();
            }
        }
        self.nav_index = self.entries.len();
    }

    /// エントリ追加 + ファイル追記。空行・直前との重複はスキップ。
    pub fn add(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        if self.entries.last().map_or(false, |last| last == line) {
            return;
        }
        self.entries.push(line.to_string());
        if self.entries.len() > self.max_size {
            self.entries.remove(0);
        }
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(file, "{}", line);
        }
    }

    /// ナビゲーション状態をリセット（`read_line` 開始時に呼ぶ）。
    pub fn reset_nav(&mut self) {
        self.nav_index = self.entries.len();
        self.saved_buf.clear();
    }

    /// 現在の入力バッファを保存（初回 Up 時）。
    pub fn save_current(&mut self, buf: &str) {
        self.saved_buf = buf.to_string();
    }

    /// ナビゲーション位置が末尾（= まだ履歴に入っていない）か。
    pub fn at_end(&self) -> bool {
        self.nav_index == self.entries.len()
    }

    /// ↑: 一つ前のエントリを返す。先頭なら None。
    pub fn prev(&mut self) -> Option<&str> {
        if self.nav_index > 0 {
            self.nav_index -= 1;
            Some(&self.entries[self.nav_index])
        } else {
            None
        }
    }

    /// ↓: 一つ次のエントリを返す。末尾到達時は saved_buf を復元。
    pub fn next(&mut self) -> Option<&str> {
        if self.nav_index < self.entries.len() {
            self.nav_index += 1;
            if self.nav_index == self.entries.len() {
                Some(&self.saved_buf)
            } else {
                Some(&self.entries[self.nav_index])
            }
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_history(entries: &[&str]) -> History {
        History {
            entries: entries.iter().map(|s| s.to_string()).collect(),
            max_size: 1000,
            nav_index: entries.len(),
            saved_buf: String::new(),
            path: PathBuf::from("/dev/null"),
        }
    }

    #[test]
    fn prev_next_navigation() {
        let mut h = make_history(&["first", "second", "third"]);
        h.save_current("current");

        assert_eq!(h.prev(), Some("third"));
        assert_eq!(h.prev(), Some("second"));
        assert_eq!(h.prev(), Some("first"));
        assert_eq!(h.prev(), None);

        assert_eq!(h.next(), Some("second"));
        assert_eq!(h.next(), Some("third"));
        assert_eq!(h.next(), Some("current"));
        assert_eq!(h.next(), None);
    }

    #[test]
    fn add_skips_empty_and_duplicates() {
        let mut h = make_history(&[]);
        h.add("");
        assert!(h.entries.is_empty());

        h.add("  ");
        assert!(h.entries.is_empty());

        h.add("echo hello");
        assert_eq!(h.entries.len(), 1);

        h.add("echo hello");
        assert_eq!(h.entries.len(), 1); // duplicate skipped

        h.add("echo world");
        assert_eq!(h.entries.len(), 2);
    }

    #[test]
    fn at_end_and_save() {
        let mut h = make_history(&["a", "b"]);
        assert!(h.at_end());

        h.prev();
        assert!(!h.at_end());

        h.next();
        assert!(h.at_end());
    }

    #[test]
    fn reset_nav_goes_to_end() {
        let mut h = make_history(&["a", "b"]);
        h.prev();
        h.prev();
        assert_eq!(h.nav_index, 0);

        h.reset_nav();
        assert!(h.at_end());
    }
}
