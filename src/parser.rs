//! トークナイザ + パーサー: 入力文字列からコマンドリスト AST を構築する。
//!
//! 手書きトークナイザでゼロコピー（[`Cow::Borrowed`]）のトークン列を生成し、
//! ループベースのパーサーで [`CommandList`] AST に変換する。
//!
//! ## 対応構文
//!
//! - パイプライン: `cmd1 | cmd2 | cmd3`
//! - リダイレクト: `>`, `>>`, `<`, `2>`
//! - クォート: シングル (`'...'`) / ダブル (`"..."`)
//! - 変数展開: `$VAR`, `${VAR}`, `$?`（ダブルクォート内・裸ワードで展開、シングルクォートではリテラル）
//! - チルダ展開: `~` → `$HOME`, `~/path`, `~user`, `VAR=~/path`
//! - コマンド置換パススルー: `$(cmd)`, `` `cmd` `` — パーサーでは展開せずリテラル保持、executor で展開
//! - バックグラウンド実行: `cmd &`（パイプラインの末尾に `&` を指定）
//! - 複合コマンド: `&&` (AND), `||` (OR), `;` (順次実行)
//! - fd 複製: `2>&1`, `>&2`（fd 複製リダイレクト）
//! - エスケープ: `\"`, `\\`, `\$`（ダブルクォート内）, `\X`（裸ワード）

use std::borrow::Cow;
use std::fmt;

// ── AST ─────────────────────────────────────────────────────────────

/// コマンドリスト: パイプラインを `&&`, `||`, `;` で連結した最上位構文。
/// `echo hello && ls | head -1 ; echo done` → 3 要素。
#[derive(Debug, PartialEq)]
pub struct CommandList<'a> {
    pub items: Vec<ListItem<'a>>,
}

/// リスト内の 1 要素。
#[derive(Debug, PartialEq)]
pub struct ListItem<'a> {
    pub pipeline: Pipeline<'a>,
    /// 次のパイプラインとの接続。最後の要素は `Connector::Seq`。
    pub connector: Connector,
}

/// パイプライン間の接続子。
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Connector {
    /// `;` またはリスト末尾 — 無条件に次を実行
    Seq,
    /// `&&` — 直前が成功 (status == 0) の場合のみ次を実行
    And,
    /// `||` — 直前が失敗 (status != 0) の場合のみ次を実行
    Or,
}

/// パイプラインで接続されたコマンド列。`cmd1 | cmd2 | cmd3` → 3要素。
///
/// `background` が `true` のとき、executor はジョブをバックグラウンドで実行する。
#[derive(Debug, PartialEq)]
pub struct Pipeline<'a> {
    pub commands: Vec<Command<'a>>,
    /// 末尾に `&` が指定された場合に `true`。
    pub background: bool,
}

/// 単一コマンド。引数リストとリダイレクト指定を持つ。
///
/// `Cow<'a, str>` を採用: クォートなしトークンは `Borrowed`（ゼロコピー）。
/// 変数展開が発生すると `Owned` になる。
#[derive(Debug, PartialEq)]
pub struct Command<'a> {
    pub args: Vec<Cow<'a, str>>,
    pub redirects: Vec<Redirect<'a>>,
}

/// ファイルリダイレクト指定。種別とターゲットファイルパスを持つ。
#[derive(Debug, PartialEq)]
pub struct Redirect<'a> {
    pub kind: RedirectKind,
    pub target: Cow<'a, str>,
}

/// リダイレクトの種別。
#[derive(Debug, PartialEq)]
pub enum RedirectKind {
    /// `>` — stdout を上書き
    Output,
    /// `>>` — stdout を追記
    Append,
    /// `<` — stdin をファイルから読み取り
    Input,
    /// `2>` — stderr を上書き
    Stderr,
    /// `N>&M` — fd 複製（src_fd を dst_fd のコピーにする）
    FdDup { src_fd: i32, dst_fd: i32 },
}

// ── Error ───────────────────────────────────────────────────────────

/// パース時に発生しうるエラー。
#[derive(Debug, PartialEq)]
pub enum ParseError {
    /// クォートが閉じられていない。引数は開始クォート文字（`'` or `"`）。
    UnterminatedQuote(char),
    /// リダイレクト演算子の後にターゲットファイル名がない。
    MissingRedirectTarget,
    /// パイプ、`&&`、`||` の前後にコマンドがない。
    EmptyPipelineSegment,
    /// fd 複製リダイレクトの dst_fd が不正。
    BadFdRedirect,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnterminatedQuote(c) => write!(f, "unexpected EOF while looking for matching `{c}`"),
            Self::MissingRedirectTarget => write!(f, "syntax error: missing redirect target"),
            Self::EmptyPipelineSegment => write!(f, "syntax error near unexpected token"),
            Self::BadFdRedirect => write!(f, "syntax error: invalid file descriptor in redirect"),
        }
    }
}

// ── Tilde expansion ─────────────────────────────────────────────────

/// チルダ展開: `~` → $HOME, `~/path` → $HOME/path, `~user` → user のホーム。
/// `=` の後のチルダも展開する（`export VAR=~/foo`）。
pub fn expand_tilde(s: &str) -> Cow<'_, str> {
    if !s.starts_with('~') {
        // `=` の後にチルダがあるケースをチェック
        if let Some(eq) = s.find('=') {
            if s[eq + 1..].starts_with('~') {
                let (key, val) = s.split_at(eq + 1);
                if let Cow::Owned(expanded) = expand_tilde_prefix(val) {
                    return Cow::Owned(format!("{}{}", key, expanded));
                }
            }
        }
        return Cow::Borrowed(s);
    }
    expand_tilde_prefix(s)
}

fn expand_tilde_prefix(s: &str) -> Cow<'_, str> {
    if !s.starts_with('~') {
        return Cow::Borrowed(s);
    }
    let rest_start = s[1..].find('/').map(|i| i + 1).unwrap_or(s.len());
    let user_part = &s[1..rest_start];
    let rest = &s[rest_start..];

    if user_part.is_empty() {
        // ~ or ~/path → $HOME
        match std::env::var("HOME") {
            Ok(home) => Cow::Owned(format!("{}{}", home, rest)),
            Err(_) => Cow::Borrowed(s),
        }
    } else {
        // ~user → getpwnam
        let c_user = match std::ffi::CString::new(user_part) {
            Ok(c) => c,
            Err(_) => return Cow::Borrowed(s),
        };
        let pw = unsafe { libc::getpwnam(c_user.as_ptr()) };
        if pw.is_null() {
            return Cow::Borrowed(s);
        }
        let home = unsafe { std::ffi::CStr::from_ptr((*pw).pw_dir) };
        match home.to_str() {
            Ok(h) => Cow::Owned(format!("{}{}", h, rest)),
            Err(_) => Cow::Borrowed(s),
        }
    }
}

// ── Variable expansion (crate-private) ──────────────────────────────

/// `$VAR` / `${VAR}` / `$?` を展開する。`$` が含まれなければゼロコピーの `Cow::Borrowed` を返す。
fn expand_variables(s: &str, last_status: i32) -> Cow<'_, str> {
    if !s.contains('$') {
        return Cow::Borrowed(s);
    }

    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut result = String::new();
    let mut pos = 0;
    let mut start = 0; // コピーされていない部分の先頭

    while pos < len {
        if bytes[pos] != b'$' {
            pos += 1;
            continue;
        }

        // `$` の前の部分をコピー
        result.push_str(&s[start..pos]);
        pos += 1; // skip '$'

        if pos >= len {
            // 末尾の裸の `$` → リテラル
            result.push('$');
            start = pos;
            continue;
        }

        match bytes[pos] {
            b'(' => {
                // $(...) — コマンド置換。executor で展開するのでリテラル保持。
                result.push_str("$(");
                pos += 1; // skip '('
                let mut depth = 1;
                while pos < len && depth > 0 {
                    match bytes[pos] {
                        b'(' => depth += 1,
                        b')' => {
                            depth -= 1;
                            if depth == 0 {
                                pos += 1;
                                break;
                            }
                        }
                        b'\'' => {
                            result.push(bytes[pos] as char);
                            pos += 1;
                            while pos < len && bytes[pos] != b'\'' {
                                result.push(bytes[pos] as char);
                                pos += 1;
                            }
                        }
                        _ => {}
                    }
                    if depth > 0 {
                        result.push(bytes[pos] as char);
                        pos += 1;
                    }
                }
                result.push(')');
            }
            b'?' => {
                result.push_str(&last_status.to_string());
                pos += 1;
            }
            b'{' => {
                pos += 1; // skip '{'
                let var_start = pos;
                while pos < len && bytes[pos] != b'}' && is_var_char(bytes[pos]) {
                    pos += 1;
                }
                if pos < len && bytes[pos] == b'}' {
                    let var_name = &s[var_start..pos];
                    pos += 1; // skip '}'
                    if !var_name.is_empty() {
                        if let Ok(val) = std::env::var(var_name) {
                            result.push_str(&val);
                        }
                    }
                    // 未定義 → 空文字（何も追加しない）
                } else {
                    // 閉じ '}' がない or 不正文字 → リテラル "${"
                    result.push_str("${");
                    pos = var_start; // 戻して再スキャン
                }
            }
            b if is_var_start(b) => {
                let var_start = pos;
                while pos < len && is_var_char(bytes[pos]) {
                    pos += 1;
                }
                let var_name = &s[var_start..pos];
                if let Ok(val) = std::env::var(var_name) {
                    result.push_str(&val);
                }
                // 未定義 → 空文字（何も追加しない）
            }
            _ => {
                // `$` の後が識別子文字でない → リテラル `$`
                result.push('$');
            }
        }

        start = pos;
    }

    // 残りの部分をコピー
    result.push_str(&s[start..]);

    Cow::Owned(result)
}

/// 変数名の先頭文字として有効か（ASCII英字 or `_`）
fn is_var_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

/// 変数名の継続文字として有効か（ASCII英数字 or `_`）
fn is_var_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

// ── Tokenizer (crate-private) ───────────────────────────────────────

/// トークナイザが生成する内部トークン型。
enum Token<'a> {
    Word(Cow<'a, str>),
    Pipe,           // |
    And,            // &&
    Or,             // ||
    Semi,           // ;
    Ampersand,      // &
    RedirectOut,    // >
    RedirectAppend, // >>
    RedirectIn,     // <
    RedirectErr,    // 2>
    FdDupPrefix(i32), // N>& — src_fd は N、次の Word が dst_fd
}

/// 入力文字列をトークン列に変換するイテレータ。
///
/// 空白をスキップし、演算子・クォート・通常ワードを識別する。
/// `Iterator<Item = Result<Token, ParseError>>` を実装。
struct Tokenizer<'a> {
    input: &'a str,
    pos: usize,
    last_status: i32,
}

impl<'a> Tokenizer<'a> {
    fn new(input: &'a str, last_status: i32) -> Self {
        Self { input, pos: 0, last_status }
    }

    fn skip_whitespace(&mut self) {
        let bytes = self.input.as_bytes();
        while self.pos < bytes.len() && bytes[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.as_bytes().get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<u8> {
        self.input.as_bytes().get(self.pos + offset).copied()
    }

    /// `$` の直後から変数名を読み取り、展開結果を `buf` に追加する。
    /// 呼び出し前に `self.pos` は `$` の次を指していること。
    fn expand_var_inline(&mut self, buf: &mut String) {
        let bytes = self.input.as_bytes();
        let len = bytes.len();
        match bytes[self.pos] {
            b'(' => {
                // $(...) — コマンド置換。リテラル保持。
                buf.push_str("$(");
                self.pos += 1; // skip '('
                let mut depth = 1;
                while self.pos < len && depth > 0 {
                    match bytes[self.pos] {
                        b'(' => depth += 1,
                        b')' => {
                            depth -= 1;
                            if depth == 0 {
                                self.pos += 1;
                                break;
                            }
                        }
                        b'\'' => {
                            buf.push(bytes[self.pos] as char);
                            self.pos += 1;
                            while self.pos < len && bytes[self.pos] != b'\'' {
                                buf.push(bytes[self.pos] as char);
                                self.pos += 1;
                            }
                        }
                        _ => {}
                    }
                    if depth > 0 {
                        buf.push(bytes[self.pos] as char);
                        self.pos += 1;
                    }
                }
                buf.push(')');
            }
            b'?' => {
                buf.push_str(&self.last_status.to_string());
                self.pos += 1;
            }
            b'{' => {
                self.pos += 1; // skip '{'
                let var_start = self.pos;
                while self.pos < len
                    && bytes[self.pos] != b'}'
                    && is_var_char(bytes[self.pos])
                {
                    self.pos += 1;
                }
                if self.pos < len && bytes[self.pos] == b'}' {
                    let var_name = &self.input[var_start..self.pos];
                    self.pos += 1; // skip '}'
                    if !var_name.is_empty() {
                        if let Ok(val) = std::env::var(var_name) {
                            buf.push_str(&val);
                        }
                    }
                } else {
                    buf.push_str("${");
                    self.pos = var_start;
                }
            }
            b if is_var_start(b) => {
                let var_start = self.pos;
                while self.pos < len && is_var_char(bytes[self.pos]) {
                    self.pos += 1;
                }
                let var_name = &self.input[var_start..self.pos];
                if let Ok(val) = std::env::var(var_name) {
                    buf.push_str(&val);
                }
            }
            _ => {
                buf.push('$');
            }
        }
    }
}

impl<'a> Iterator for Tokenizer<'a> {
    type Item = Result<Token<'a>, ParseError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.skip_whitespace();
        let ch = self.peek()?;

        match ch {
            b'|' => {
                self.pos += 1;
                if self.peek() == Some(b'|') {
                    self.pos += 1;
                    Some(Ok(Token::Or))
                } else {
                    Some(Ok(Token::Pipe))
                }
            }
            b'&' => {
                self.pos += 1;
                if self.peek() == Some(b'&') {
                    self.pos += 1;
                    Some(Ok(Token::And))
                } else {
                    Some(Ok(Token::Ampersand))
                }
            }
            b';' => {
                self.pos += 1;
                Some(Ok(Token::Semi))
            }
            b'>' => {
                self.pos += 1;
                if self.peek() == Some(b'>') {
                    self.pos += 1;
                    Some(Ok(Token::RedirectAppend))
                } else if self.peek() == Some(b'&') {
                    self.pos += 1;
                    Some(Ok(Token::FdDupPrefix(1))) // >&M は 1>&M の省略形
                } else {
                    Some(Ok(Token::RedirectOut))
                }
            }
            b'<' => {
                self.pos += 1;
                Some(Ok(Token::RedirectIn))
            }
            // トークン先頭の `2>` のみ。`file2>` 等の途中はWordとして読まれる。
            b'2' if self.peek_at(1) == Some(b'>') && self.peek_at(2) == Some(b'&') => {
                self.pos += 3;
                Some(Ok(Token::FdDupPrefix(2)))
            }
            b'2' if self.peek_at(1) == Some(b'>') => {
                self.pos += 2;
                Some(Ok(Token::RedirectErr))
            }
            // シングルクォート: 展開なし → Borrowed
            b'\'' => {
                self.pos += 1; // skip opening quote
                let start = self.pos;
                loop {
                    if self.pos >= self.input.len() {
                        return Some(Err(ParseError::UnterminatedQuote('\'')));
                    }
                    if self.input.as_bytes()[self.pos] == b'\'' {
                        let word = &self.input[start..self.pos];
                        self.pos += 1; // skip closing quote
                        return Some(Ok(Token::Word(Cow::Borrowed(word))));
                    }
                    self.pos += 1;
                }
            }
            // ダブルクォート: 変数展開 + エスケープあり
            b'"' => {
                self.pos += 1; // skip opening quote
                let start = self.pos;
                // まずエスケープの有無をスキャン
                let mut has_escape = false;
                let mut scan = self.pos;
                while scan < self.input.len() {
                    match self.input.as_bytes()[scan] {
                        b'"' => break,
                        b'\\' if scan + 1 < self.input.len() => {
                            let next = self.input.as_bytes()[scan + 1];
                            if matches!(next, b'"' | b'\\' | b'$') {
                                has_escape = true;
                            }
                            scan += 2;
                        }
                        _ => scan += 1,
                    }
                }

                if has_escape {
                    // エスケープあり → 変数展開もインラインで処理
                    // （\$ で生成した $ が再展開されるのを防ぐため）
                    let mut buf = String::new();
                    while self.pos < self.input.len() {
                        match self.input.as_bytes()[self.pos] {
                            b'"' => {
                                self.pos += 1; // skip closing quote
                                return Some(Ok(Token::Word(Cow::Owned(buf))));
                            }
                            b'`' => {
                                // バッククォート → リテラル保持
                                buf.push('`');
                                self.pos += 1;
                                while self.pos < self.input.len() && self.input.as_bytes()[self.pos] != b'`' {
                                    buf.push(self.input.as_bytes()[self.pos] as char);
                                    self.pos += 1;
                                }
                                if self.pos < self.input.len() {
                                    buf.push('`');
                                    self.pos += 1;
                                }
                            }
                            b'\\' if self.pos + 1 < self.input.len() => {
                                let next = self.input.as_bytes()[self.pos + 1];
                                match next {
                                    b'"' | b'\\' | b'$' | b'`' => {
                                        buf.push(next as char);
                                        self.pos += 2;
                                    }
                                    _ => {
                                        buf.push('\\');
                                        buf.push(next as char);
                                        self.pos += 2;
                                    }
                                }
                            }
                            b'$' => {
                                // インライン変数展開
                                self.pos += 1;
                                if self.pos >= self.input.len()
                                    || self.input.as_bytes()[self.pos] == b'"'
                                {
                                    buf.push('$');
                                } else {
                                    self.expand_var_inline(&mut buf);
                                }
                            }
                            _ => {
                                buf.push(self.input.as_bytes()[self.pos] as char);
                                self.pos += 1;
                            }
                        }
                    }
                    return Some(Err(ParseError::UnterminatedQuote('"')));
                } else {
                    // エスケープなし → 既存ロジック
                    loop {
                        if self.pos >= self.input.len() {
                            return Some(Err(ParseError::UnterminatedQuote('"')));
                        }
                        if self.input.as_bytes()[self.pos] == b'"' {
                            let word = &self.input[start..self.pos];
                            self.pos += 1; // skip closing quote
                            return Some(Ok(Token::Word(expand_variables(word, self.last_status))));
                        }
                        self.pos += 1;
                    }
                }
            }
            _ => {
                // 裸ワード: エスケープ + 変数展開
                let start = self.pos;
                let mut has_escape = false;

                // エスケープの有無をスキャン
                {
                    let mut scan = self.pos;
                    while scan < self.input.len() {
                        match self.input.as_bytes()[scan] {
                            b' ' | b'\t' | b'\n' | b'\r' | b'|' | b'&' | b'>' | b'<'
                            | b'\'' | b'"' | b';' => break,
                            b'\\' => {
                                has_escape = true;
                                break;
                            }
                            _ => scan += 1,
                        }
                    }
                }

                if has_escape {
                    // エスケープあり → String 構築
                    let mut buf = String::new();
                    while self.pos < self.input.len() {
                        match self.input.as_bytes()[self.pos] {
                            b' ' | b'\t' | b'\n' | b'\r' | b'|' | b'&' | b'>' | b'<'
                            | b'\'' | b'"' | b';' => break,
                            b'$' if self.pos + 1 < self.input.len()
                                && self.input.as_bytes()[self.pos + 1] == b'(' =>
                            {
                                // $(...) をリテラル保持
                                buf.push('$');
                                buf.push('(');
                                self.pos += 2;
                                let mut depth = 1;
                                while self.pos < self.input.len() && depth > 0 {
                                    match self.input.as_bytes()[self.pos] {
                                        b'(' => depth += 1,
                                        b')' => depth -= 1,
                                        b'\'' => {
                                            buf.push(self.input.as_bytes()[self.pos] as char);
                                            self.pos += 1;
                                            while self.pos < self.input.len()
                                                && self.input.as_bytes()[self.pos] != b'\''
                                            {
                                                buf.push(self.input.as_bytes()[self.pos] as char);
                                                self.pos += 1;
                                            }
                                        }
                                        _ => {}
                                    }
                                    if depth > 0 {
                                        buf.push(self.input.as_bytes()[self.pos] as char);
                                    }
                                    self.pos += 1;
                                }
                                buf.push(')');
                            }
                            b'`' => {
                                // バッククォート内をリテラル保持
                                buf.push('`');
                                self.pos += 1;
                                while self.pos < self.input.len() && self.input.as_bytes()[self.pos] != b'`' {
                                    buf.push(self.input.as_bytes()[self.pos] as char);
                                    self.pos += 1;
                                }
                                if self.pos < self.input.len() {
                                    buf.push('`');
                                    self.pos += 1;
                                }
                            }
                            b'\\' if self.pos + 1 < self.input.len() => {
                                // `\X` → リテラル `X`
                                self.pos += 1;
                                buf.push(self.input.as_bytes()[self.pos] as char);
                                self.pos += 1;
                            }
                            b'\\' => {
                                // 末尾のバックスラッシュ → そのまま
                                buf.push('\\');
                                self.pos += 1;
                            }
                            _ => {
                                buf.push(self.input.as_bytes()[self.pos] as char);
                                self.pos += 1;
                            }
                        }
                    }
                    let expanded = expand_variables(&buf, self.last_status);
                    Some(Ok(Token::Word(Cow::Owned(expanded.into_owned()))))
                } else {
                    // エスケープなし → 従来通り
                    while self.pos < self.input.len() {
                        match self.input.as_bytes()[self.pos] {
                            b' ' | b'\t' | b'\n' | b'\r' | b'|' | b'&' | b'>' | b'<'
                            | b'\'' | b'"' | b';' => break,
                            b'$' if self.pos + 1 < self.input.len()
                                && self.input.as_bytes()[self.pos + 1] == b'(' =>
                            {
                                // $(...) をまとめてスキップ
                                self.pos += 2; // skip "$("
                                let mut depth = 1;
                                while self.pos < self.input.len() && depth > 0 {
                                    match self.input.as_bytes()[self.pos] {
                                        b'(' => depth += 1,
                                        b')' => depth -= 1,
                                        b'\'' => {
                                            self.pos += 1;
                                            while self.pos < self.input.len()
                                                && self.input.as_bytes()[self.pos] != b'\''
                                            { self.pos += 1; }
                                        }
                                        _ => {}
                                    }
                                    self.pos += 1;
                                }
                            }
                            b'`' => {
                                // バッククォートをスキップ
                                self.pos += 1;
                                while self.pos < self.input.len() && self.input.as_bytes()[self.pos] != b'`' {
                                    self.pos += 1;
                                }
                                if self.pos < self.input.len() { self.pos += 1; }
                            }
                            _ => self.pos += 1,
                        }
                    }
                    let word = &self.input[start..self.pos];
                    Some(Ok(Token::Word(expand_variables(word, self.last_status))))
                }
            }
        }
    }
}

// ── Parser ──────────────────────────────────────────────────────────

/// 入力文字列をパースして `CommandList` AST を返す。
///
/// - 空入力 → `Ok(None)`
/// - 正常なコマンド → `Ok(Some(CommandList))`
/// - 構文エラー → `Err(ParseError)`
///
/// `last_status` は `$?` 展開に使用される。
pub fn parse(input: &str, last_status: i32) -> Result<Option<CommandList<'_>>, ParseError> {
    let mut tokens = Tokenizer::new(input, last_status);
    let mut items: Vec<ListItem<'_>> = Vec::new();
    let mut commands: Vec<Command<'_>> = Vec::new();
    let mut args: Vec<Cow<'_, str>> = Vec::new();
    let mut redirects: Vec<Redirect<'_>> = Vec::new();
    let mut background = false;

    while let Some(result) = tokens.next() {
        let token = result?;
        match token {
            Token::Word(w) => args.push(w),
            Token::Pipe => {
                if args.is_empty() {
                    return Err(ParseError::EmptyPipelineSegment);
                }
                commands.push(Command {
                    args: std::mem::take(&mut args),
                    redirects: std::mem::take(&mut redirects),
                });
            }
            Token::And | Token::Or | Token::Semi => {
                let connector = match token {
                    Token::And => Connector::And,
                    Token::Or => Connector::Or,
                    _ => Connector::Seq,
                };

                // `;` の前に何もなくてもスキップ（bash 互換）
                if args.is_empty() && commands.is_empty() {
                    if matches!(connector, Connector::Seq) {
                        continue; // 先頭 `;` や `;;` はスキップ
                    }
                    return Err(ParseError::EmptyPipelineSegment);
                }

                if !args.is_empty() {
                    commands.push(Command {
                        args: std::mem::take(&mut args),
                        redirects: std::mem::take(&mut redirects),
                    });
                }

                items.push(ListItem {
                    pipeline: Pipeline {
                        commands: std::mem::take(&mut commands),
                        background,
                    },
                    connector,
                });
                background = false;
            }
            Token::Ampersand => {
                if args.is_empty() && commands.is_empty() {
                    return Err(ParseError::EmptyPipelineSegment);
                }

                // `&` の後にコマンドが続くケースをサポート（`cmd1 & cmd2`）
                if !args.is_empty() {
                    commands.push(Command {
                        args: std::mem::take(&mut args),
                        redirects: std::mem::take(&mut redirects),
                    });
                }

                items.push(ListItem {
                    pipeline: Pipeline {
                        commands: std::mem::take(&mut commands),
                        background: true,
                    },
                    connector: Connector::Seq,
                });
                background = false;
            }
            Token::RedirectOut | Token::RedirectAppend | Token::RedirectIn | Token::RedirectErr => {
                let kind = match token {
                    Token::RedirectOut => RedirectKind::Output,
                    Token::RedirectAppend => RedirectKind::Append,
                    Token::RedirectIn => RedirectKind::Input,
                    Token::RedirectErr => RedirectKind::Stderr,
                    _ => unreachable!(),
                };
                match tokens.next() {
                    Some(Ok(Token::Word(target))) => {
                        redirects.push(Redirect { kind, target });
                    }
                    Some(Err(e)) => return Err(e),
                    _ => return Err(ParseError::MissingRedirectTarget),
                }
            }
            Token::FdDupPrefix(src_fd) => {
                match tokens.next() {
                    Some(Ok(Token::Word(w))) => {
                        let dst_fd = w.parse::<i32>().map_err(|_| ParseError::BadFdRedirect)?;
                        redirects.push(Redirect {
                            kind: RedirectKind::FdDup { src_fd, dst_fd },
                            target: Cow::Borrowed(""),
                        });
                    }
                    Some(Err(e)) => return Err(e),
                    _ => return Err(ParseError::MissingRedirectTarget),
                }
            }
        }
    }

    // 末尾パイプ: commands があるが args がない → パイプの後にコマンドがない
    if !commands.is_empty() && args.is_empty() && redirects.is_empty() {
        return Err(ParseError::EmptyPipelineSegment);
    }

    // 最終パイプラインの処理
    if !args.is_empty() {
        commands.push(Command { args, redirects });
    } else if !redirects.is_empty() {
        // リダイレクトのみ（コマンドなし）
        return Err(ParseError::EmptyPipelineSegment);
    }

    if !commands.is_empty() {
        items.push(ListItem {
            pipeline: Pipeline { commands, background },
            connector: Connector::Seq,
        });
    }

    if items.is_empty() {
        return Ok(None);
    }

    // 末尾 &&/|| チェック: 最後の項目が And/Or なら後続コマンドなしエラー
    if let Some(last) = items.last() {
        if matches!(last.connector, Connector::And | Connector::Or) {
            return Err(ParseError::EmptyPipelineSegment);
        }
    }

    Ok(Some(CommandList { items }))
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// パース結果から最初のパイプラインの各コマンドの引数を文字列ベクタとして取り出す。
    fn parse_args(input: &str) -> Vec<Vec<String>> {
        let list = parse(input, 0).unwrap().unwrap();
        list.items[0]
            .pipeline
            .commands
            .iter()
            .map(|cmd| cmd.args.iter().map(|a| a.to_string()).collect())
            .collect()
    }

    // ── 単純コマンド ──

    #[test]
    fn simple_command() {
        assert_eq!(
            parse_args("echo hello world"),
            vec![vec!["echo", "hello", "world"]],
        );
    }

    #[test]
    fn single_arg() {
        assert_eq!(parse_args("ls"), vec![vec!["ls"]]);
    }

    #[test]
    fn extra_whitespace() {
        assert_eq!(
            parse_args("  echo   hello  "),
            vec![vec!["echo", "hello"]],
        );
    }

    // ── クォート ──

    #[test]
    fn single_quotes() {
        assert_eq!(
            parse_args("echo 'hello world'"),
            vec![vec!["echo", "hello world"]],
        );
    }

    #[test]
    fn double_quotes() {
        assert_eq!(
            parse_args("echo \"hello world\""),
            vec![vec!["echo", "hello world"]],
        );
    }

    #[test]
    fn empty_quotes() {
        assert_eq!(parse_args("echo ''"), vec![vec!["echo", ""]]);
    }

    // ── パイプライン ──

    #[test]
    fn two_stage_pipeline() {
        assert_eq!(
            parse_args("ls | grep Cargo"),
            vec![vec!["ls"], vec!["grep", "Cargo"]],
        );
    }

    #[test]
    fn three_stage_pipeline() {
        assert_eq!(
            parse_args("cat file | grep name | head -1"),
            vec![
                vec!["cat", "file"],
                vec!["grep", "name"],
                vec!["head", "-1"],
            ],
        );
    }

    // ── リダイレクト ──

    #[test]
    fn redirect_output() {
        let list = parse("echo hello > out.txt", 0).unwrap().unwrap();
        let p = &list.items[0].pipeline;
        assert_eq!(p.commands.len(), 1);
        assert_eq!(p.commands[0].args.len(), 2);
        assert_eq!(p.commands[0].redirects.len(), 1);
        assert_eq!(p.commands[0].redirects[0].kind, RedirectKind::Output);
        assert_eq!(p.commands[0].redirects[0].target, "out.txt");
    }

    #[test]
    fn redirect_append() {
        let list = parse("echo hello >> out.txt", 0).unwrap().unwrap();
        let p = &list.items[0].pipeline;
        assert_eq!(p.commands[0].redirects[0].kind, RedirectKind::Append);
        assert_eq!(p.commands[0].redirects[0].target, "out.txt");
    }

    #[test]
    fn redirect_input() {
        let list = parse("cat < in.txt", 0).unwrap().unwrap();
        let p = &list.items[0].pipeline;
        assert_eq!(p.commands[0].redirects[0].kind, RedirectKind::Input);
        assert_eq!(p.commands[0].redirects[0].target, "in.txt");
    }

    #[test]
    fn redirect_stderr() {
        let list = parse("ls 2> err.txt", 0).unwrap().unwrap();
        let p = &list.items[0].pipeline;
        assert_eq!(p.commands[0].redirects[0].kind, RedirectKind::Stderr);
        assert_eq!(p.commands[0].redirects[0].target, "err.txt");
    }

    #[test]
    fn redirect_no_space() {
        let list = parse("echo hello >out.txt", 0).unwrap().unwrap();
        let p = &list.items[0].pipeline;
        assert_eq!(p.commands[0].redirects[0].target, "out.txt");
    }

    #[test]
    fn multiple_redirects() {
        let list = parse("cmd < in.txt > out.txt 2> err.txt", 0).unwrap().unwrap();
        let p = &list.items[0].pipeline;
        assert_eq!(p.commands[0].redirects.len(), 3);
        assert_eq!(p.commands[0].redirects[0].kind, RedirectKind::Input);
        assert_eq!(p.commands[0].redirects[1].kind, RedirectKind::Output);
        assert_eq!(p.commands[0].redirects[2].kind, RedirectKind::Stderr);
    }

    // ── パイプライン + リダイレクト複合 ──

    #[test]
    fn pipeline_with_redirects() {
        let list = parse("cat < in.txt | grep hello > out.txt", 0).unwrap().unwrap();
        let p = &list.items[0].pipeline;
        assert_eq!(p.commands.len(), 2);
        assert_eq!(p.commands[0].redirects[0].kind, RedirectKind::Input);
        assert_eq!(p.commands[0].redirects[0].target, "in.txt");
        assert_eq!(p.commands[1].redirects[0].kind, RedirectKind::Output);
        assert_eq!(p.commands[1].redirects[0].target, "out.txt");
    }

    // ── 2> はトークン先頭のみ ──

    #[test]
    fn two_is_not_stderr_redirect_with_space() {
        let list = parse("echo 2 > file", 0).unwrap().unwrap();
        let p = &list.items[0].pipeline;
        assert_eq!(p.commands[0].args.len(), 2);
        assert_eq!(p.commands[0].args[1], "2");
        assert_eq!(p.commands[0].redirects[0].kind, RedirectKind::Output);
    }

    // ── 空入力 ──

    #[test]
    fn empty_input() {
        assert!(parse("", 0).unwrap().is_none());
        assert!(parse("   ", 0).unwrap().is_none());
        assert!(parse("\t\n", 0).unwrap().is_none());
    }

    // ── エラーケース ──

    #[test]
    fn err_unterminated_single_quote() {
        assert_eq!(
            parse("echo 'hello", 0),
            Err(ParseError::UnterminatedQuote('\'')),
        );
    }

    #[test]
    fn err_unterminated_double_quote() {
        assert_eq!(
            parse("echo \"hello", 0),
            Err(ParseError::UnterminatedQuote('"')),
        );
    }

    #[test]
    fn err_missing_redirect_target() {
        assert_eq!(parse("echo >", 0), Err(ParseError::MissingRedirectTarget));
    }

    #[test]
    fn err_redirect_followed_by_pipe() {
        assert_eq!(parse("echo > | cat", 0), Err(ParseError::MissingRedirectTarget));
    }

    #[test]
    fn err_leading_pipe() {
        assert_eq!(parse("| ls", 0), Err(ParseError::EmptyPipelineSegment));
    }

    #[test]
    fn err_trailing_pipe() {
        assert_eq!(parse("ls |", 0), Err(ParseError::EmptyPipelineSegment));
    }

    #[test]
    fn err_double_pipe_operator() {
        // `ls | | grep` → first `|` consumed as Pipe, then `| grep` → EmptyPipelineSegment
        // because after Pipe, args is empty and next token is `|` (Pipe)
        assert_eq!(parse("ls | | grep", 0), Err(ParseError::EmptyPipelineSegment));
    }

    // ── background (&) ──

    #[test]
    fn background_simple() {
        let list = parse("sleep 10 &", 0).unwrap().unwrap();
        let p = &list.items[0].pipeline;
        assert!(p.background);
        assert_eq!(p.commands.len(), 1);
        assert_eq!(p.commands[0].args[0], "sleep");
    }

    #[test]
    fn background_pipeline() {
        let list = parse("ls | grep foo &", 0).unwrap().unwrap();
        let p = &list.items[0].pipeline;
        assert!(p.background);
        assert_eq!(p.commands.len(), 2);
    }

    #[test]
    fn background_bare_ampersand() {
        assert_eq!(parse("&", 0), Err(ParseError::EmptyPipelineSegment));
    }

    #[test]
    fn background_followed_by_command() {
        // `cmd & extra` → 2 items: cmd (background), extra (foreground)
        let list = parse("cmd & extra", 0).unwrap().unwrap();
        assert_eq!(list.items.len(), 2);
        assert!(list.items[0].pipeline.background);
        assert_eq!(list.items[0].pipeline.commands[0].args[0], "cmd");
        assert!(!list.items[1].pipeline.background);
        assert_eq!(list.items[1].pipeline.commands[0].args[0], "extra");
    }

    #[test]
    fn no_background_flag() {
        let list = parse("ls", 0).unwrap().unwrap();
        assert!(!list.items[0].pipeline.background);
    }

    // ── Cow はすべて Borrowed（展開不要時） ──

    #[test]
    fn cow_is_borrowed() {
        let list = parse("echo hello", 0).unwrap().unwrap();
        for arg in &list.items[0].pipeline.commands[0].args {
            assert!(matches!(arg, Cow::Borrowed(_)), "expected Borrowed, got Owned");
        }
    }

    #[test]
    fn cow_quoted_is_borrowed() {
        let list = parse("echo 'hello world'", 0).unwrap().unwrap();
        assert!(matches!(&list.items[0].pipeline.commands[0].args[1], Cow::Borrowed(_)));
    }

    // ── 変数展開テスト ──

    #[test]
    fn expand_env_var() {
        std::env::set_var("RUSH_TEST_VAR", "hello");
        let list = parse("echo $RUSH_TEST_VAR", 0).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "hello");
        std::env::remove_var("RUSH_TEST_VAR");
    }

    #[test]
    fn expand_last_status() {
        let list = parse("echo $?", 42).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "42");
    }

    #[test]
    fn expand_undefined_var() {
        std::env::remove_var("RUSH_NONEXISTENT_VAR_XYZ");
        let list = parse("echo $RUSH_NONEXISTENT_VAR_XYZ", 0).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "");
    }

    #[test]
    fn single_quote_no_expand() {
        std::env::set_var("RUSH_TEST_VAR2", "expanded");
        let list = parse("echo '$RUSH_TEST_VAR2'", 0).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "$RUSH_TEST_VAR2");
        assert!(matches!(&list.items[0].pipeline.commands[0].args[1], Cow::Borrowed(_)));
        std::env::remove_var("RUSH_TEST_VAR2");
    }

    #[test]
    fn double_quote_expand() {
        std::env::set_var("RUSH_TEST_VAR3", "world");
        let list = parse("echo \"hello $RUSH_TEST_VAR3\"", 0).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "hello world");
        std::env::remove_var("RUSH_TEST_VAR3");
    }

    #[test]
    fn redirect_target_expand() {
        std::env::set_var("RUSH_TEST_DIR", "/tmp");
        let list = parse("echo hello > $RUSH_TEST_DIR/out.txt", 0).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].redirects[0].target, "/tmp/out.txt");
        std::env::remove_var("RUSH_TEST_DIR");
    }

    #[test]
    fn bare_dollar_at_end() {
        let list = parse("echo $", 0).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "$");
    }

    #[test]
    fn dollar_before_non_ident() {
        let list = parse("echo $!", 0).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "$!");
    }

    #[test]
    fn no_dollar_cow_borrowed() {
        let list = parse("echo hello", 0).unwrap().unwrap();
        assert!(matches!(&list.items[0].pipeline.commands[0].args[0], Cow::Borrowed(_)));
        assert!(matches!(&list.items[0].pipeline.commands[0].args[1], Cow::Borrowed(_)));
    }

    #[test]
    fn double_quote_no_dollar_cow_borrowed() {
        let list = parse("echo \"hello\"", 0).unwrap().unwrap();
        assert!(matches!(&list.items[0].pipeline.commands[0].args[1], Cow::Borrowed(_)));
    }

    // ── && / || / ; テスト ──

    #[test]
    fn and_connector() {
        let list = parse("echo a && echo b", 0).unwrap().unwrap();
        assert_eq!(list.items.len(), 2);
        assert_eq!(list.items[0].connector, Connector::And);
        assert_eq!(list.items[0].pipeline.commands[0].args[0], "echo");
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "a");
        assert_eq!(list.items[1].pipeline.commands[0].args[0], "echo");
        assert_eq!(list.items[1].pipeline.commands[0].args[1], "b");
        assert_eq!(list.items[1].connector, Connector::Seq);
    }

    #[test]
    fn or_connector() {
        let list = parse("false || echo ok", 0).unwrap().unwrap();
        assert_eq!(list.items.len(), 2);
        assert_eq!(list.items[0].connector, Connector::Or);
    }

    #[test]
    fn seq_connector() {
        let list = parse("echo a ; echo b", 0).unwrap().unwrap();
        assert_eq!(list.items.len(), 2);
        assert_eq!(list.items[0].connector, Connector::Seq);
    }

    #[test]
    fn mixed_connectors() {
        let list = parse("a && b || c ; d", 0).unwrap().unwrap();
        assert_eq!(list.items.len(), 4);
        assert_eq!(list.items[0].connector, Connector::And);
        assert_eq!(list.items[1].connector, Connector::Or);
        assert_eq!(list.items[2].connector, Connector::Seq);
        assert_eq!(list.items[3].connector, Connector::Seq);
    }

    #[test]
    fn background_then_command() {
        let list = parse("sleep 1 & echo done", 0).unwrap().unwrap();
        assert_eq!(list.items.len(), 2);
        assert!(list.items[0].pipeline.background);
        assert!(!list.items[1].pipeline.background);
        assert_eq!(list.items[1].pipeline.commands[0].args[0], "echo");
    }

    #[test]
    fn leading_semi_skipped() {
        let list = parse("; echo hello", 0).unwrap().unwrap();
        assert_eq!(list.items.len(), 1);
        assert_eq!(list.items[0].pipeline.commands[0].args[0], "echo");
    }

    #[test]
    fn trailing_semi_ok() {
        let list = parse("echo hello ;", 0).unwrap().unwrap();
        assert_eq!(list.items.len(), 1);
    }

    #[test]
    fn double_semi_skipped() {
        let list = parse("echo a ;; echo b", 0).unwrap().unwrap();
        assert_eq!(list.items.len(), 2);
    }

    #[test]
    fn only_semicolons() {
        assert!(parse(";;;", 0).unwrap().is_none());
    }

    #[test]
    fn err_leading_and() {
        assert_eq!(parse("&& cmd", 0), Err(ParseError::EmptyPipelineSegment));
    }

    #[test]
    fn err_trailing_and() {
        assert_eq!(parse("cmd &&", 0), Err(ParseError::EmptyPipelineSegment));
    }

    #[test]
    fn err_leading_or() {
        // `||` at start: first `||` is Or token, empty pipeline before it
        assert_eq!(parse("|| cmd", 0), Err(ParseError::EmptyPipelineSegment));
    }

    #[test]
    fn err_trailing_or() {
        assert_eq!(parse("cmd ||", 0), Err(ParseError::EmptyPipelineSegment));
    }

    // ── エスケープテスト ──

    #[test]
    fn escape_double_quote_in_dquote() {
        let list = parse(r#"echo "hello\"world""#, 0).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "hello\"world");
    }

    #[test]
    fn escape_backslash_in_dquote() {
        let list = parse(r#"echo "a\\b""#, 0).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "a\\b");
    }

    #[test]
    fn escape_dollar_in_dquote() {
        let list = parse(r#"echo "\$HOME""#, 0).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "$HOME");
    }

    #[test]
    fn escape_space_in_bare_word() {
        let list = parse(r"echo file\ name", 0).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "file name");
    }

    // ── ${VAR} テスト ──

    #[test]
    fn expand_braced_var() {
        std::env::set_var("RUSH_TEST_BRACE", "braced");
        let list = parse("echo ${RUSH_TEST_BRACE}", 0).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "braced");
        std::env::remove_var("RUSH_TEST_BRACE");
    }

    #[test]
    fn expand_braced_var_with_suffix() {
        std::env::set_var("RUSH_TEST_BSUF", "val");
        let list = parse("echo ${RUSH_TEST_BSUF}suffix", 0).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "valsuffix");
        std::env::remove_var("RUSH_TEST_BSUF");
    }

    #[test]
    fn expand_braced_undefined() {
        std::env::remove_var("RUSH_TEST_BUNDEF_XYZ");
        let list = parse("echo ${RUSH_TEST_BUNDEF_XYZ}", 0).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "");
    }

    #[test]
    fn braced_unclosed() {
        // `${` without closing `}` → literal "${" then rest
        let list = parse("echo ${abc", 0).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "${abc");
    }

    // ── チルダ展開テスト ──

    #[test]
    fn tilde_home() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_tilde("~"), Cow::Owned::<str>(home));
    }

    #[test]
    fn tilde_home_path() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_tilde("~/foo"), Cow::Owned::<str>(format!("{}/foo", home)));
    }

    #[test]
    fn tilde_no_change() {
        assert!(matches!(expand_tilde("hello"), Cow::Borrowed(_)));
    }

    #[test]
    fn tilde_after_equals() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_tilde("X=~/bar"), Cow::Owned::<str>(format!("X={}/bar", home)));
    }

    #[test]
    fn tilde_no_equals_tilde() {
        assert!(matches!(expand_tilde("X=hello"), Cow::Borrowed(_)));
    }

    // ── fd 複製テスト ──

    #[test]
    fn fd_dup_2_to_1() {
        let list = parse("cmd 2>&1", 0).unwrap().unwrap();
        let r = &list.items[0].pipeline.commands[0].redirects[0];
        assert_eq!(r.kind, RedirectKind::FdDup { src_fd: 2, dst_fd: 1 });
    }

    #[test]
    fn fd_dup_stdout_to_stderr() {
        let list = parse("cmd >&2", 0).unwrap().unwrap();
        let r = &list.items[0].pipeline.commands[0].redirects[0];
        assert_eq!(r.kind, RedirectKind::FdDup { src_fd: 1, dst_fd: 2 });
    }

    #[test]
    fn fd_dup_with_file_redirect() {
        let list = parse("cmd > out 2>&1", 0).unwrap().unwrap();
        let redirects = &list.items[0].pipeline.commands[0].redirects;
        assert_eq!(redirects.len(), 2);
        assert_eq!(redirects[0].kind, RedirectKind::Output);
        assert_eq!(redirects[0].target, "out");
        assert_eq!(redirects[1].kind, RedirectKind::FdDup { src_fd: 2, dst_fd: 1 });
    }

    #[test]
    fn fd_dup_bad_target() {
        assert_eq!(parse("cmd 2>&abc", 0), Err(ParseError::BadFdRedirect));
    }

    // ── コマンド置換パススルーテスト ──

    #[test]
    fn cmd_sub_passthrough() {
        let list = parse("echo $(date)", 0).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "$(date)");
    }

    #[test]
    fn backtick_passthrough() {
        let list = parse("echo `date`", 0).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "`date`");
    }

    #[test]
    fn cmd_sub_nested() {
        let list = parse("echo $(echo $(whoami))", 0).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "$(echo $(whoami))");
    }

    #[test]
    fn cmd_sub_in_double_quotes() {
        let list = parse("echo \"today is $(date)\"", 0).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "today is $(date)");
    }
}
