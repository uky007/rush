//! 行エディタ: raw モード、キー入力、バッファ操作、表示更新。
//!
//! ターミナルを raw モードに切り替え、自前の行エディタを提供する。
//! 外部クレートに依存せず `libc`（termios, `read(2)`, `write(2)`, `poll(2)`）のみで実装。
//!
//! ## アーキテクチャ
//!
//! ```text
//! ┌─────────────────────────────────────────────────┐
//! │ LineEditor::read_line(prompt)                    │
//! │  ┌──────────┐  ┌──────────┐  ┌───────────────┐  │
//! │  │ RawMode  │  │ read_key │  │ refresh_line  │  │
//! │  │ (RAII)   │  │ (入力)   │  │ (表示更新)    │  │
//! │  └──────────┘  └──────────┘  └───────────────┘  │
//! │       │              │              │            │
//! │  termios 操作    libc::read     libc::write     │
//! │                      │              ↑            │
//! │               ┌──────┴──────┐  ┌────┴─────┐     │
//! │               │ History     │  │highlight │     │
//! │               │ (↑↓ 履歴)  │  │(着色)    │     │
//! │               └─────────────┘  └──────────┘     │
//! │               ┌─────────────┐                    │
//! │               │ complete    │                    │
//! │               │ (Tab 補完)  │                    │
//! │               └─────────────┘                    │
//! └─────────────────────────────────────────────────┘
//! ```
//!
//! ## raw モードの範囲
//!
//! raw モードは [`LineEditor::read_line`] 内でのみ有効。
//! [`RawMode`] の RAII ガードにより、`read_line` から抜けると（正常復帰でもパニックでも）
//! 自動的に元の termios 設定が復元される。
//! これにより、コマンド実行中は子プロセスに正常な cooked モードのターミナルが提供される。
//!
//! ## 表示更新
//!
//! 全行再描画方式を採用。[`LineEditor::refresh_line`] がプロンプト + ハイライト済みバッファを
//! 1 回の `write(2)` で出力し、フリッカーを防止する。
//! カーソル位置は raw バッファの文字数で計算し、ANSI エスケープシーケンスのバイト数を含めない。

use crate::complete;
use crate::highlight::{self, PathCache};
use crate::history::History;

// ── RawMode ガード ────────────────────────────────────────────────

/// RAII ガードで raw モードを管理する。Drop で元の termios を自動復元する。
///
/// ## termios 設定
///
/// | フラグ | 操作 | 理由 |
/// |--------|------|------|
/// | `c_iflag` | `BRKINT\|ICRNL\|INPCK\|ISTRIP\|IXON` OFF | CR→LF 変換を無効化、フロー制御を無効化 |
/// | `c_oflag` | `OPOST` ON のまま | `\n` → `\r\n` 自動変換を維持し、既存コードへの影響を回避 |
/// | `c_cflag` | `CS8` ON | 8 ビットクリーンな入力 |
/// | `c_lflag` | `ECHO\|ICANON\|IEXTEN\|ISIG` OFF | エコー無効、1 バイトずつ読み取り、Ctrl+C/Z をキー入力として受信 |
/// | `VMIN`/`VTIME` | `1` / `0` | 最低 1 バイトで即座に返る |
struct RawMode {
    /// `tcgetattr` で保存した元の termios 設定。Drop で復元する。
    orig: libc::termios,
    /// 操作対象のファイルディスクリプタ（通常 `STDIN_FILENO`）。
    fd: i32,
}

impl RawMode {
    /// `tcgetattr` で現在の設定を保存し、raw モードを `tcsetattr(TCSAFLUSH)` で適用する。
    fn enable(fd: i32) -> Self {
        let mut orig: libc::termios = unsafe { std::mem::zeroed() };
        unsafe {
            libc::tcgetattr(fd, &mut orig);
        }
        let mut raw = orig;
        raw.c_iflag &= !(libc::BRKINT | libc::ICRNL | libc::INPCK | libc::ISTRIP | libc::IXON);
        // OPOST は ON のまま（\n → \r\n 自動変換を維持）
        raw.c_cflag |= libc::CS8;
        raw.c_lflag &= !(libc::ECHO | libc::ICANON | libc::IEXTEN | libc::ISIG);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        unsafe {
            libc::tcsetattr(fd, libc::TCSAFLUSH, &raw);
        }
        Self { orig, fd }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSAFLUSH, &self.orig);
        }
    }
}

// ── Key 入力 ──────────────────────────────────────────────────────

/// raw モードで読み取ったキー入力を表す。
///
/// 制御文字（`0x01`〜`0x1a`）、エスケープシーケンス（`\x1b[A` 等）、
/// および通常文字（ASCII + UTF-8 マルチバイト）を統一的に扱う。
pub enum Key {
    /// 通常の印字可能文字（ASCII + UTF-8）。
    Char(char),
    /// Enter キー（CR `\r` または LF `\n`）。
    Enter,
    /// Backspace（DEL `0x7f` または BS `0x08`）。
    Backspace,
    /// Delete キー（`ESC [ 3 ~`）。
    Delete,
    /// 左矢印（`ESC [ D`）。
    Left,
    /// 右矢印（`ESC [ C`）。
    Right,
    /// 上矢印（`ESC [ A`）— 履歴を遡る。
    Up,
    /// 下矢印（`ESC [ B`）— 履歴を進む。
    Down,
    /// Home キー（`ESC [ H` / `ESC [ 1 ~`）。
    Home,
    /// End キー（`ESC [ F` / `ESC [ 4 ~`）。
    End,
    /// Tab（`0x09`）— 補完トリガー。
    Tab,
    /// Ctrl+A（`0x01`）— 行頭へ移動。
    CtrlA,
    /// Ctrl+C（`0x03`）— 現在の入力を破棄して新プロンプト。
    CtrlC,
    /// Ctrl+D（`0x04`）— 空バッファなら EOF、それ以外は無視。
    CtrlD,
    /// Ctrl+E（`0x05`）— 行末へ移動。
    CtrlE,
    /// Ctrl+K（`0x0b`）— カーソルから行末まで削除。
    CtrlK,
    /// Ctrl+L（`0x0c`）— 画面クリア + 再描画。
    CtrlL,
    /// Ctrl+U（`0x15`）— 行頭からカーソルまで削除。
    CtrlU,
    /// Ctrl+W（`0x17`）— 直前の単語を削除。
    CtrlW,
    /// 未対応のバイト列。無視される。
    Unknown,
}

/// `libc::read` で 1 バイト読み取る。EOF またはエラー時は `None`。
fn read_byte(fd: i32) -> Option<u8> {
    let mut buf = [0u8; 1];
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, 1) };
    if n == 1 {
        Some(buf[0])
    } else {
        None
    }
}

/// ESC (`\x1b`) 後のエスケープシーケンスを解析する。
///
/// `poll(fd, POLLIN, 50ms)` で後続バイトの有無を判定し、
/// タイムアウトすれば ESC 単独として `Unknown` を返す。
/// 対応シーケンス: `[A`〜`[D`（矢印）, `[H`/`[F`（Home/End）,
/// `[1~`/`[4~`（Home/End VT 形式）, `[3~`（Delete）。
fn read_escape_seq(fd: i32) -> Key {
    // ESC 後にデータがあるかタイムアウト判定
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut pfd, 1, 50) };
    if ready <= 0 {
        return Key::Unknown; // ESC 単独
    }

    match read_byte(fd) {
        Some(b'[') => {}
        _ => return Key::Unknown,
    }

    match read_byte(fd) {
        Some(b'A') => Key::Up,
        Some(b'B') => Key::Down,
        Some(b'C') => Key::Right,
        Some(b'D') => Key::Left,
        Some(b'H') => Key::Home,
        Some(b'F') => Key::End,
        Some(b'1') => {
            if read_byte(fd) == Some(b'~') {
                Key::Home
            } else {
                Key::Unknown
            }
        }
        Some(b'3') => {
            if read_byte(fd) == Some(b'~') {
                Key::Delete
            } else {
                Key::Unknown
            }
        }
        Some(b'4') => {
            if read_byte(fd) == Some(b'~') {
                Key::End
            } else {
                Key::Unknown
            }
        }
        _ => Key::Unknown,
    }
}

/// UTF-8 マルチバイト文字の残りのバイトを読み取り、`Key::Char` に変換する。
///
/// 先頭バイトから期待されるバイト数を判定済みの状態で呼ばれる。
/// 途中で読み取りに失敗するか、不正な UTF-8 であれば `Key::Unknown` を返す。
fn read_utf8(fd: i32, first: u8, expected_len: usize) -> Key {
    let mut buf = [0u8; 4];
    buf[0] = first;
    for i in 1..expected_len {
        match read_byte(fd) {
            Some(b) => buf[i] = b,
            None => return Key::Unknown,
        }
    }
    match std::str::from_utf8(&buf[..expected_len]) {
        Ok(s) => s.chars().next().map_or(Key::Unknown, Key::Char),
        Err(_) => Key::Unknown,
    }
}

/// `fd` から 1 キー分のバイト列を読み取り、[`Key`] に変換する。
///
/// 先頭バイトで分岐:
/// - `\r` / `\n` → Enter
/// - `0x7f` / `0x08` → Backspace
/// - `0x1b` → [`read_escape_seq`] でエスケープシーケンスを解析
/// - `0x01`〜`0x17` → 各種 Ctrl キー
/// - `0x20`〜`0x7e` → ASCII 印字可能文字
/// - `0xC0`〜`0xF7` → [`read_utf8`] で UTF-8 マルチバイト文字を読み取り
fn read_key(fd: i32) -> Key {
    let byte = match read_byte(fd) {
        Some(b) => b,
        None => return Key::Unknown,
    };

    match byte {
        b'\r' | b'\n' => Key::Enter,
        0x7f | 0x08 => Key::Backspace,
        0x1b => read_escape_seq(fd),
        0x09 => Key::Tab,
        1 => Key::CtrlA,
        3 => Key::CtrlC,
        4 => Key::CtrlD,
        5 => Key::CtrlE,
        11 => Key::CtrlK,
        12 => Key::CtrlL,
        21 => Key::CtrlU,
        23 => Key::CtrlW,
        b if b >= 32 && b < 127 => Key::Char(b as char),
        // UTF-8 マルチバイト
        b if b & 0xE0 == 0xC0 => read_utf8(fd, b, 2),
        b if b & 0xF0 == 0xE0 => read_utf8(fd, b, 3),
        b if b & 0xF8 == 0xF0 => read_utf8(fd, b, 4),
        _ => Key::Unknown,
    }
}

// ── LineEditor ────────────────────────────────────────────────────

/// 行エディタ本体。入力バッファ、カーソル位置、履歴、PATH キャッシュを保持する。
///
/// REPL ループの開始時に [`LineEditor::new`] で生成し、
/// 毎プロンプトで [`LineEditor::read_line`] を呼ぶ。
/// raw モードは `read_line` 内でのみ有効であり、コマンド実行中は cooked モードに戻る。
///
/// [`PathCache`] は [`Shell`](crate::shell::Shell) とは別インスタンスで保持する。
/// エディタのライフタイムとシェルのライフタイムを分離し、borrow checker との
/// 整合性を維持するための設計判断。
pub struct LineEditor {
    /// 現在の入力テキスト。UTF-8 文字列。
    buf: String,
    /// カーソルのバイトオフセット（`0` = 行頭、`buf.len()` = 行末）。
    /// 常に UTF-8 文字境界上にある。
    cursor: usize,
    /// コマンド履歴。`~/.rush_history` に永続化される。
    history: History,
    /// 入力に使うファイルディスクリプタ（通常 `STDIN_FILENO`）。
    fd: i32,
    /// `$PATH` 内コマンドのキャッシュ。ハイライトと補完で共有。
    /// Shell の PathCache とは別インスタンス（ライフタイム分離）。
    path_cache: PathCache,
}

impl LineEditor {
    /// 新しい `LineEditor` を作成する。
    ///
    /// `~/.rush_history` から履歴を読み込み、`$PATH` キャッシュを初期化する。
    pub fn new() -> Self {
        Self {
            buf: String::new(),
            cursor: 0,
            history: History::new(),
            fd: libc::STDIN_FILENO,
            path_cache: PathCache::new(),
        }
    }

    /// コマンド履歴にエントリを追加する。空行・直前と同一のコマンドはスキップ。
    pub fn add_history(&mut self, line: &str) {
        self.history.add(line);
    }

    /// プロンプトを表示し、1 行読み取る。
    /// Enter → `Some(line)`, Ctrl+D (空バッファ) → `None` (EOF)。
    pub fn read_line(&mut self, prompt: &str) -> Option<String> {
        self.buf.clear();
        self.cursor = 0;
        self.history.reset_nav();
        self.path_cache.refresh();

        let _raw = RawMode::enable(self.fd);
        self.refresh_line(prompt);

        loop {
            let key = read_key(self.fd);
            match key {
                Key::Enter => {
                    write_all("\n");
                    return Some(self.buf.clone());
                }
                Key::CtrlD => {
                    if self.buf.is_empty() {
                        return None;
                    }
                }
                Key::CtrlC => {
                    write_all("^C\n");
                    self.buf.clear();
                    self.cursor = 0;
                    self.history.reset_nav();
                    self.refresh_line(prompt);
                    continue;
                }
                Key::Char(ch) => self.insert_char(ch),
                Key::Backspace => self.delete_char_before(),
                Key::Delete => self.delete_char_at(),
                Key::Left => self.move_left(),
                Key::Right => self.move_right(),
                Key::Home | Key::CtrlA => self.move_home(),
                Key::End | Key::CtrlE => self.move_end(),
                Key::Up => self.history_prev(),
                Key::Down => self.history_next(),
                Key::Tab => {
                    self.do_complete(prompt);
                    continue;
                }
                Key::CtrlK => self.kill_to_end(),
                Key::CtrlU => self.kill_to_start(),
                Key::CtrlW => self.kill_word_back(),
                Key::CtrlL => {
                    self.clear_screen(prompt);
                    continue;
                }
                Key::Unknown => continue,
            }
            self.refresh_line(prompt);
        }
    }

    // ── バッファ操作 ──────────────────────────────────────────────

    /// カーソル位置に 1 文字挿入し、カーソルをその文字の直後に進める。
    fn insert_char(&mut self, ch: char) {
        self.buf.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    /// Backspace: カーソル直前の 1 文字を削除する。行頭では何もしない。
    fn delete_char_before(&mut self) {
        if self.cursor > 0 {
            let prev = self.buf[..self.cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.buf.remove(prev);
            self.cursor = prev;
        }
    }

    /// Delete: カーソル位置の 1 文字を削除する。行末では何もしない。
    fn delete_char_at(&mut self) {
        if self.cursor < self.buf.len() {
            self.buf.remove(self.cursor);
        }
    }

    /// カーソルを 1 文字左に移動する。行頭では何もしない。
    fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = self.buf[..self.cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
        }
    }

    /// カーソルを 1 文字右に移動する。行末では何もしない。
    fn move_right(&mut self) {
        if self.cursor < self.buf.len() {
            let ch = self.buf[self.cursor..].chars().next().unwrap();
            self.cursor += ch.len_utf8();
        }
    }

    /// Ctrl+A / Home: カーソルを行頭に移動する。
    fn move_home(&mut self) {
        self.cursor = 0;
    }

    /// Ctrl+E / End: カーソルを行末に移動する。
    fn move_end(&mut self) {
        self.cursor = self.buf.len();
    }

    /// Ctrl+K: カーソルから行末まで削除。
    fn kill_to_end(&mut self) {
        self.buf.truncate(self.cursor);
    }

    /// Ctrl+U: 行頭からカーソルまで削除。
    fn kill_to_start(&mut self) {
        self.buf.drain(..self.cursor);
        self.cursor = 0;
    }

    /// Ctrl+W: 直前の単語を削除する。
    ///
    /// カーソル手前の空白をスキップし、次の空白まで（または行頭まで）を削除する。
    /// UTF-8 文字境界を正しく処理する。
    fn kill_word_back(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let before = &self.buf[..self.cursor];
        let chars: Vec<(usize, char)> = before.char_indices().collect();
        let mut idx = chars.len();

        // 末尾の空白をスキップ
        while idx > 0 && chars[idx - 1].1 == ' ' {
            idx -= 1;
        }
        // 単語文字をスキップ
        while idx > 0 && chars[idx - 1].1 != ' ' {
            idx -= 1;
        }

        let byte_pos = if idx == 0 { 0 } else { chars[idx].0 };
        self.buf.drain(byte_pos..self.cursor);
        self.cursor = byte_pos;
    }

    /// Ctrl+L: 画面クリア + 再描画。
    fn clear_screen(&mut self, prompt: &str) {
        write_all("\x1b[2J\x1b[H");
        self.refresh_line(prompt);
    }

    // ── 履歴ナビゲーション ────────────────────────────────────────

    /// ↑: 履歴を一つ遡る。初回は現在のバッファを保存する。
    fn history_prev(&mut self) {
        if self.history.at_end() {
            let buf = self.buf.clone();
            self.history.save_current(&buf);
        }
        if let Some(entry) = self.history.prev().map(|s| s.to_string()) {
            self.buf = entry;
            self.cursor = self.buf.len();
        }
    }

    /// ↓: 履歴を一つ進む。末尾到達時は保存しておいたバッファを復元する。
    fn history_next(&mut self) {
        if let Some(entry) = self.history.next().map(|s| s.to_string()) {
            self.buf = entry;
            self.cursor = self.buf.len();
        }
    }

    // ── Tab 補完 ──────────────────────────────────────────────────

    /// Tab 補完を実行する。
    ///
    /// - 候補 0 件 → ベル (`\x07`) を鳴らす
    /// - 候補 1 件 → 単語を候補で置換し、末尾にスペース（ディレクトリなら `/`）を付加
    /// - 候補複数 → 共通接頭辞まで補完し、候補一覧を表示
    fn do_complete(&mut self, prompt: &str) {
        let result = complete::complete(&self.buf, self.cursor, &self.path_cache);

        match result.candidates.len() {
            0 => {
                write_all("\x07"); // ベル
            }
            1 => {
                let candidate = &result.candidates[0];
                let suffix = if candidate.ends_with('/') { "" } else { " " };
                let new_word = format!("{}{}", candidate, suffix);
                self.buf
                    .replace_range(result.word_start..result.word_end, &new_word);
                self.cursor = result.word_start + new_word.len();
                self.refresh_line(prompt);
            }
            _ => {
                // 共通接頭辞まで補完
                let common = complete::longest_common_prefix(&result.candidates).to_string();
                let current_word_len = result.word_end - result.word_start;
                if common.len() > current_word_len {
                    self.buf
                        .replace_range(result.word_start..result.word_end, &common);
                    self.cursor = result.word_start + common.len();
                }
                // 候補一覧を表示
                let mut display = String::from("\n");
                for (i, candidate) in result.candidates.iter().enumerate() {
                    if i > 0 {
                        display.push_str("  ");
                    }
                    display.push_str(candidate);
                }
                display.push('\n');
                write_all(&display);
                self.refresh_line(prompt);
            }
        }
    }

    // ── 表示更新 ──────────────────────────────────────────────────

    /// 全行を再描画する（1 回の `write(2)` で出力しフリッカーを防止）。
    ///
    /// 処理手順:
    /// 1. `\r` で行頭へ移動
    /// 2. プロンプトを出力
    /// 3. [`highlight::highlight`] でハイライト済みバッファを出力
    /// 4. `\x1b[K` で行末までクリア（前回より短い入力のゴミを消す）
    /// 5. `\x1b[{N}D` でカーソルを正しい位置に戻す
    fn refresh_line(&self, prompt: &str) {
        let highlighted = highlight::highlight(&self.buf, &self.path_cache);

        let buf_chars = self.buf.chars().count();
        let cursor_chars = self.buf[..self.cursor].chars().count();
        let move_back = buf_chars - cursor_chars;

        let mut out = String::new();
        out.push('\r');
        out.push_str(prompt);
        out.push_str(&highlighted);
        out.push_str("\x1b[K"); // 行末までクリア
        if move_back > 0 {
            out.push_str(&format!("\x1b[{}D", move_back));
        }

        write_all(&out);
    }
}

/// libc::write で直接出力する（Rust の stdout バッファをバイパス）。
fn write_all(s: &str) {
    let bytes = s.as_bytes();
    let mut written = 0;
    while written < bytes.len() {
        let n = unsafe {
            libc::write(
                libc::STDOUT_FILENO,
                bytes[written..].as_ptr() as *const libc::c_void,
                bytes.len() - written,
            )
        };
        if n <= 0 {
            break;
        }
        written += n as usize;
    }
}

// ── Tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト用 LineEditor を作成（履歴ファイルなし）。
    fn test_editor() -> LineEditor {
        LineEditor {
            buf: String::new(),
            cursor: 0,
            history: History::new(),
            fd: libc::STDIN_FILENO,
            path_cache: PathCache::new(),
        }
    }

    #[test]
    fn insert_char_at_end() {
        let mut ed = test_editor();
        ed.insert_char('a');
        ed.insert_char('b');
        ed.insert_char('c');
        assert_eq!(ed.buf, "abc");
        assert_eq!(ed.cursor, 3);
    }

    #[test]
    fn insert_char_at_middle() {
        let mut ed = test_editor();
        ed.buf = "ac".to_string();
        ed.cursor = 1;
        ed.insert_char('b');
        assert_eq!(ed.buf, "abc");
        assert_eq!(ed.cursor, 2);
    }

    #[test]
    fn delete_char_before() {
        let mut ed = test_editor();
        ed.buf = "abc".to_string();
        ed.cursor = 3;
        ed.delete_char_before();
        assert_eq!(ed.buf, "ab");
        assert_eq!(ed.cursor, 2);
    }

    #[test]
    fn delete_char_before_at_start() {
        let mut ed = test_editor();
        ed.buf = "abc".to_string();
        ed.cursor = 0;
        ed.delete_char_before(); // no-op
        assert_eq!(ed.buf, "abc");
        assert_eq!(ed.cursor, 0);
    }

    #[test]
    fn delete_char_at_cursor() {
        let mut ed = test_editor();
        ed.buf = "abc".to_string();
        ed.cursor = 1;
        ed.delete_char_at();
        assert_eq!(ed.buf, "ac");
        assert_eq!(ed.cursor, 1);
    }

    #[test]
    fn move_left_right() {
        let mut ed = test_editor();
        ed.buf = "abc".to_string();
        ed.cursor = 3;
        ed.move_left();
        assert_eq!(ed.cursor, 2);
        ed.move_left();
        assert_eq!(ed.cursor, 1);
        ed.move_right();
        assert_eq!(ed.cursor, 2);
    }

    #[test]
    fn move_home_end() {
        let mut ed = test_editor();
        ed.buf = "hello".to_string();
        ed.cursor = 3;
        ed.move_home();
        assert_eq!(ed.cursor, 0);
        ed.move_end();
        assert_eq!(ed.cursor, 5);
    }

    #[test]
    fn kill_to_end() {
        let mut ed = test_editor();
        ed.buf = "hello world".to_string();
        ed.cursor = 5;
        ed.kill_to_end();
        assert_eq!(ed.buf, "hello");
        assert_eq!(ed.cursor, 5);
    }

    #[test]
    fn kill_to_start() {
        let mut ed = test_editor();
        ed.buf = "hello world".to_string();
        ed.cursor = 5;
        ed.kill_to_start();
        assert_eq!(ed.buf, " world");
        assert_eq!(ed.cursor, 0);
    }

    #[test]
    fn kill_word_back() {
        let mut ed = test_editor();
        ed.buf = "echo hello world".to_string();
        ed.cursor = 16;
        ed.kill_word_back();
        assert_eq!(ed.buf, "echo hello ");
        assert_eq!(ed.cursor, 11);
    }

    #[test]
    fn kill_word_back_multiple_spaces() {
        let mut ed = test_editor();
        ed.buf = "echo   hello".to_string();
        ed.cursor = 12;
        ed.kill_word_back();
        assert_eq!(ed.buf, "echo   ");
        assert_eq!(ed.cursor, 7);
    }

    #[test]
    fn kill_word_back_at_start() {
        let mut ed = test_editor();
        ed.buf = "hello".to_string();
        ed.cursor = 0;
        ed.kill_word_back(); // no-op
        assert_eq!(ed.buf, "hello");
    }

    #[test]
    fn utf8_insert_and_move() {
        let mut ed = test_editor();
        ed.insert_char('あ');
        ed.insert_char('い');
        assert_eq!(ed.buf, "あい");
        assert_eq!(ed.cursor, 6); // 2 * 3 bytes
        ed.move_left();
        assert_eq!(ed.cursor, 3);
        ed.move_left();
        assert_eq!(ed.cursor, 0);
        ed.move_right();
        assert_eq!(ed.cursor, 3);
    }

    #[test]
    fn utf8_delete() {
        let mut ed = test_editor();
        ed.buf = "あいう".to_string();
        ed.cursor = 6; // after 'い'
        ed.delete_char_before();
        assert_eq!(ed.buf, "あう");
        assert_eq!(ed.cursor, 3);
    }
}
