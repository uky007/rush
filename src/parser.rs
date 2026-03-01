//! トークナイザ + パーサー: 入力文字列からコマンドリスト AST を構築する。
//!
//! 手書きトークナイザでゼロコピー（[`Cow::Borrowed`]）のトークン列を生成し、
//! ループベースのパーサーで [`CommandList`] AST に変換する。
//!
//! ## 対応構文
//!
//! - パイプライン: `cmd1 | cmd2 | cmd3`
//! - リダイレクト: `>`, `>>`, `<`, `2>`, `2>>`, `<<DELIM`（ヒアドキュメント）, `<<<`（ヒアストリング）
//! - クォート: シングル (`'...'`) / ダブル (`"..."`)
//! - 変数展開: `$VAR`, `${VAR}`, `$?`, `$$`, `$!`, `$0`, `$RANDOM`, `$SECONDS`,
//!   `$1`〜`$9`（位置パラメータ）, `$@`, `$*`（全引数）, `$#`（引数個数）
//!   （ダブルクォート内・裸ワードで展開、シングルクォートではリテラル）
//! - パラメータ展開: `${var:-default}`, `${var:=val}`, `${var:+alt}`, `${var:?msg}`,
//!   `${#var}`, `${var%pat}`, `${var%%pat}`, `${var#pat}`, `${var##pat}`,
//!   `${var/pat/repl}`, `${var//pat/repl}`
//! - チルダ展開: `~` → `$HOME`, `~/path`, `~user`, `VAR=~/path`
//! - コマンド置換パススルー: `$(cmd)`, `` `cmd` `` — パーサーでは展開せずリテラル保持、executor で展開
//! - 算術展開: `$((expr))` — 四則演算・剰余・括弧・変数参照を i64 で計算
//! - バックグラウンド実行: `cmd &`（パイプラインの末尾に `&` を指定）
//! - 複合コマンド: `&&` (AND), `||` (OR), `;` (順次実行)
//! - fd 複製: `2>&1`, `>&2`（fd 複製リダイレクト）
//! - エスケープ: `\"`, `\\`, `\$`（ダブルクォート内）, `\X`（裸ワード）
//! - インライン代入: `VAR=val cmd`（コマンド先頭の `VAR=val` を代入として検出）
//! - 継続行検出: 末尾の `|`, `&&`, `||` を [`ParseError::IncompleteInput`] として報告

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
///
/// `assignments` はコマンド行先頭の `VAR=val` 形式の代入。
/// コマンドが続く場合は子プロセスの環境変数としてのみ設定し、
/// コマンドがない場合はシェル自身の環境変数として設定する。
#[derive(Debug, PartialEq)]
pub struct Command<'a> {
    pub args: Vec<Cow<'a, str>>,
    pub redirects: Vec<Redirect<'a>>,
    /// コマンド先頭の `VAR=val` 代入リスト。
    pub assignments: Vec<(String, String)>,
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
    /// `2>>` — stderr を追記
    StderrAppend,
    /// `N>&M` — fd 複製（src_fd を dst_fd のコピーにする）
    FdDup { src_fd: i32, dst_fd: i32 },
    /// `<<DELIM` — ヒアドキュメント（stdin にテキストブロックを供給）
    HereDoc,
    /// `<<<` — ヒアストリング（stdin に文字列を供給）
    HereString,
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
    /// 入力が不完全（末尾の `|`, `&&`, `||` 等）。対話モードでは継続行入力のトリガー。
    IncompleteInput,
    /// `set -u` (nounset) で未定義変数を参照した。
    UnboundVariable(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnterminatedQuote(c) => write!(f, "unexpected EOF while looking for matching `{c}`"),
            Self::MissingRedirectTarget => write!(f, "syntax error: missing redirect target"),
            Self::EmptyPipelineSegment => write!(f, "syntax error near unexpected token"),
            Self::BadFdRedirect => write!(f, "syntax error: invalid file descriptor in redirect"),
            Self::IncompleteInput => write!(f, "syntax error: unexpected end of input"),
            Self::UnboundVariable(name) => write!(f, "{}: unbound variable", name),
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
fn expand_variables<'a>(s: &'a str, last_status: i32, pos_args: &[String], nounset: bool) -> Result<Cow<'a, str>, String> {
    if !s.contains('$') {
        return Ok(Cow::Borrowed(s));
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
                if pos + 1 < len && bytes[pos + 1] == b'(' {
                    // $((expr)) — 算術展開
                    pos += 2; // skip '(('
                    let expr_start = pos;
                    let mut paren_depth: i32 = 0;
                    let mut found = false;
                    while pos < len {
                        match bytes[pos] {
                            b'(' => paren_depth += 1,
                            b')' => {
                                if paren_depth > 0 {
                                    paren_depth -= 1;
                                } else if pos + 1 < len && bytes[pos + 1] == b')' {
                                    let expr = &s[expr_start..pos];
                                    result.push_str(&eval_arithmetic(expr, last_status, pos_args, nounset)?);
                                    pos += 2; // skip '))'
                                    found = true;
                                    break;
                                } else {
                                    break;
                                }
                            }
                            _ => {}
                        }
                        pos += 1;
                    }
                    if !found {
                        result.push_str("$((");
                        pos = expr_start;
                    }
                } else {
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
            }
            b'?' => {
                result.push_str(&last_status.to_string());
                pos += 1;
            }
            b'$' => {
                result.push_str(&unsafe { libc::getpid() }.to_string());
                pos += 1;
            }
            b'!' => {
                let bg_pid = std::env::var("RUSH_LAST_BG_PID").unwrap_or_else(|_| "0".into());
                result.push_str(&bg_pid);
                pos += 1;
            }
            b'0' => {
                result.push_str("rush");
                pos += 1;
            }
            b'1'..=b'9' => {
                // 位置パラメータ $1〜$9
                let n = (bytes[pos] - b'0') as usize;
                pos += 1;
                if let Some(val) = pos_args.get(n - 1) {
                    result.push_str(val);
                } else if nounset {
                    return Err(format!("${}", n));
                }
            }
            b'@' => {
                // $@ — 全位置パラメータ（個別の単語として展開）
                for (i, arg) in pos_args.iter().enumerate() {
                    if i > 0 { result.push(' '); }
                    result.push_str(arg);
                }
                pos += 1;
            }
            b'*' => {
                // $* — 全位置パラメータ（単一文字列として展開）
                for (i, arg) in pos_args.iter().enumerate() {
                    if i > 0 { result.push(' '); }
                    result.push_str(arg);
                }
                pos += 1;
            }
            b'#' => {
                // $# — 位置パラメータの個数
                result.push_str(&pos_args.len().to_string());
                pos += 1;
            }
            b'{' => {
                pos += 1; // skip '{'
                let var_start = pos;
                // 閉じ '}' を探す（ネストしないが、`:`, `#`, `%`, `/` 等を含む）
                let mut depth = 1;
                while pos < len && depth > 0 {
                    match bytes[pos] {
                        b'{' => depth += 1,
                        b'}' => {
                            depth -= 1;
                            if depth == 0 { break; }
                        }
                        _ => {}
                    }
                    pos += 1;
                }
                if pos < len && bytes[pos] == b'}' {
                    let inner = &s[var_start..pos];
                    pos += 1; // skip '}'
                    if !inner.is_empty() {
                        result.push_str(&expand_braced_param(inner, last_status, pos_args, nounset)?);
                    }
                } else {
                    // 閉じ '}' がない → リテラル "${"
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
                if let Some(val) = resolve_special_var(var_name) {
                    result.push_str(&val);
                } else if let Ok(val) = std::env::var(var_name) {
                    result.push_str(&val);
                } else if nounset {
                    return Err(var_name.to_string());
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

    Ok(Cow::Owned(result))
}

/// 変数名の先頭文字として有効か（ASCII英字 or `_`）
/// シェル起動時刻（`$SECONDS` 用）。プロセス起動からの秒数を返す。
static SHELL_START: std::sync::LazyLock<std::time::Instant> =
    std::sync::LazyLock::new(std::time::Instant::now);

/// 動的特殊変数を解決する。該当しなければ `None`。
fn resolve_special_var(name: &str) -> Option<String> {
    match name {
        "RANDOM" => {
            // 簡易乱数: PID ^ 時刻ベース
            let t = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            let pid = unsafe { libc::getpid() } as u64;
            let val = ((t ^ (pid.wrapping_mul(2654435761))) % 32768) as u16;
            Some(val.to_string())
        }
        "SECONDS" => {
            let elapsed = SHELL_START.elapsed().as_secs();
            Some(elapsed.to_string())
        }
        _ => None,
    }
}

fn is_var_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

/// 変数名の継続文字として有効か（ASCII英数字 or `_`）
fn is_var_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// `${...}` 内のパラメータ展開を処理する。
/// 対応: `${var:-default}`, `${var:=default}`, `${var:+alt}`, `${var:?msg}`,
///       `${#var}`, `${var%pat}`, `${var%%pat}`, `${var#pat}`, `${var##pat}`,
///       `${var/pat/repl}`, `${var//pat/repl}`
/// 変数名から値を取得する。動的特殊変数を優先し、なければ環境変数を参照。
fn get_var(name: &str) -> String {
    resolve_special_var(name).unwrap_or_else(|| std::env::var(name).unwrap_or_default())
}

fn expand_braced_param(inner: &str, last_status: i32, pos_args: &[String], nounset: bool) -> Result<String, String> {
    // ${#var} — 文字数
    if let Some(var_name) = inner.strip_prefix('#') {
        let val = get_var(var_name);
        return Ok(val.chars().count().to_string());
    }

    // 変数名を先に抽出（英数字 + _）
    let bytes = inner.as_bytes();
    let mut name_end = 0;
    while name_end < bytes.len() && is_var_char(bytes[name_end]) {
        name_end += 1;
    }

    // 変数名の後に演算子がなければ通常の ${VAR}
    if name_end == bytes.len() {
        let val = get_var(inner);
        if val.is_empty() && nounset && resolve_special_var(inner).is_none() && std::env::var(inner).is_err() {
            return Err(inner.to_string());
        }
        return Ok(val);
    }

    let var_name = &inner[..name_end];
    let val = get_var(var_name);
    let op_and_rest = &inner[name_end..];

    // ${var:-default}, ${var:=default}, ${var:+alt}, ${var:?msg}
    if op_and_rest.starts_with(":-") {
        let operand = &op_and_rest[2..];
        return Ok(if val.is_empty() { expand_variables(operand, last_status, pos_args, false)?.into_owned() } else { val });
    }
    if op_and_rest.starts_with(":=") {
        let operand = &op_and_rest[2..];
        if val.is_empty() {
            let def = expand_variables(operand, last_status, pos_args, false)?.into_owned();
            std::env::set_var(var_name, &def);
            return Ok(def);
        }
        return Ok(val);
    }
    if op_and_rest.starts_with(":+") {
        let operand = &op_and_rest[2..];
        return Ok(if val.is_empty() { String::new() } else { expand_variables(operand, last_status, pos_args, false)?.into_owned() });
    }
    if op_and_rest.starts_with(":?") {
        let operand = &op_and_rest[2..];
        if val.is_empty() {
            let msg = if operand.is_empty() { "parameter null or not set" } else { operand };
            eprintln!("rush: {}: {}", var_name, msg);
            return Ok(String::new());
        }
        return Ok(val);
    }
    // ${var%%pat} — 最長後方一致を削除
    if op_and_rest.starts_with("%%") {
        return Ok(strip_suffix_longest(&val, &op_and_rest[2..]));
    }
    // ${var%pat} — 最短後方一致を削除
    if op_and_rest.starts_with('%') {
        return Ok(strip_suffix_shortest(&val, &op_and_rest[1..]));
    }
    // ${var##pat} — 最長前方一致を削除
    if op_and_rest.starts_with("##") {
        return Ok(strip_prefix_longest(&val, &op_and_rest[2..]));
    }
    // ${var#pat} — 最短前方一致を削除
    if op_and_rest.starts_with('#') {
        return Ok(strip_prefix_shortest(&val, &op_and_rest[1..]));
    }
    // ${var//pat/repl} or ${var/pat/repl}
    if op_and_rest.starts_with('/') {
        let rest = &op_and_rest[1..];
        let (global, pattern_rest) = if rest.starts_with('/') {
            (true, &rest[1..])
        } else {
            (false, rest)
        };
        let (pattern, replacement) = if let Some(sep) = pattern_rest.find('/') {
            (&pattern_rest[..sep], &pattern_rest[sep + 1..])
        } else {
            (pattern_rest, "")
        };
        return Ok(if global {
            glob_replace_all(&val, pattern, replacement)
        } else {
            glob_replace_first(&val, pattern, replacement)
        });
    }

    // フォールバック: 通常の ${VAR}
    Ok(val)
}

/// glob パターンで最短前方一致を削除する。
fn strip_prefix_shortest(val: &str, pattern: &str) -> String {
    for end in 0..=val.len() {
        if !val.is_char_boundary(end) { continue; }
        if crate::glob::matches_pattern(pattern, &val[..end]) {
            return val[end..].to_string();
        }
    }
    val.to_string()
}

/// glob パターンで最長前方一致を削除する。
fn strip_prefix_longest(val: &str, pattern: &str) -> String {
    for end in (0..=val.len()).rev() {
        if !val.is_char_boundary(end) { continue; }
        if crate::glob::matches_pattern(pattern, &val[..end]) {
            return val[end..].to_string();
        }
    }
    val.to_string()
}

/// glob パターンで最短後方一致を削除する。
fn strip_suffix_shortest(val: &str, pattern: &str) -> String {
    for start in (0..=val.len()).rev() {
        if !val.is_char_boundary(start) { continue; }
        if crate::glob::matches_pattern(pattern, &val[start..]) {
            return val[..start].to_string();
        }
    }
    val.to_string()
}

/// glob パターンで最長後方一致を削除する。
fn strip_suffix_longest(val: &str, pattern: &str) -> String {
    for start in 0..=val.len() {
        if !val.is_char_boundary(start) { continue; }
        if crate::glob::matches_pattern(pattern, &val[start..]) {
            return val[..start].to_string();
        }
    }
    val.to_string()
}

/// glob パターンで最初の一致を置換する。
fn glob_replace_first(val: &str, pattern: &str, replacement: &str) -> String {
    for start in 0..val.len() {
        if !val.is_char_boundary(start) { continue; }
        for end in start + 1..=val.len() {
            if !val.is_char_boundary(end) { continue; }
            if crate::glob::matches_pattern(pattern, &val[start..end]) {
                return format!("{}{}{}", &val[..start], replacement, &val[end..]);
            }
        }
    }
    val.to_string()
}

/// glob パターンで全ての一致を置換する。
fn glob_replace_all(val: &str, pattern: &str, replacement: &str) -> String {
    let mut result = String::new();
    let mut pos = 0;
    while pos < val.len() {
        if !val.is_char_boundary(pos) { pos += 1; continue; }
        let mut matched = false;
        for end in (pos + 1..=val.len()).rev() {
            if !val.is_char_boundary(end) { continue; }
            if crate::glob::matches_pattern(pattern, &val[pos..end]) {
                result.push_str(replacement);
                pos = end;
                matched = true;
                break;
            }
        }
        if !matched {
            let ch = val[pos..].chars().next().unwrap();
            result.push(ch);
            pos += ch.len_utf8();
        }
    }
    result
}

// ── Arithmetic expansion ────────────────────────────────────────────

/// `$((expr))` の算術式を評価し、結果を文字列で返す。
/// 式中の `$VAR` は先に変数展開し、裸の変数名は環境変数として参照する。
fn eval_arithmetic(expr: &str, last_status: i32, pos_args: &[String], nounset: bool) -> Result<String, String> {
    let expanded = expand_variables(expr, last_status, pos_args, nounset)?;
    let mut parser = ArithParser::new(&expanded);
    match parser.parse_expr() {
        Some(val) => Ok(val.to_string()),
        None => Ok("0".to_string()),
    }
}

/// 算術式の再帰下降パーサー。
/// 優先順位: 加減算 < 乗除剰余 < 単項 +/- < 括弧・数値・変数
struct ArithParser<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> ArithParser<'a> {
    fn new(s: &'a str) -> Self {
        Self { input: s.as_bytes(), pos: 0 }
    }

    fn skip_ws(&mut self) {
        while self.pos < self.input.len() && self.input[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    /// 最上位: 加減算
    fn parse_expr(&mut self) -> Option<i64> {
        let mut left = self.parse_term()?;
        loop {
            self.skip_ws();
            if self.pos >= self.input.len() { break; }
            match self.input[self.pos] {
                b'+' => {
                    self.pos += 1;
                    let right = self.parse_term()?;
                    left = left.wrapping_add(right);
                }
                b'-' => {
                    self.pos += 1;
                    let right = self.parse_term()?;
                    left = left.wrapping_sub(right);
                }
                _ => break,
            }
        }
        Some(left)
    }

    /// 乗除算・剰余
    fn parse_term(&mut self) -> Option<i64> {
        let mut left = self.parse_unary()?;
        loop {
            self.skip_ws();
            if self.pos >= self.input.len() { break; }
            match self.input[self.pos] {
                b'*' => {
                    self.pos += 1;
                    let right = self.parse_unary()?;
                    left = left.wrapping_mul(right);
                }
                b'/' => {
                    self.pos += 1;
                    let right = self.parse_unary()?;
                    if right == 0 {
                        eprintln!("rush: division by 0");
                        return Some(0);
                    }
                    left /= right;
                }
                b'%' => {
                    self.pos += 1;
                    let right = self.parse_unary()?;
                    if right == 0 {
                        eprintln!("rush: division by 0");
                        return Some(0);
                    }
                    left %= right;
                }
                _ => break,
            }
        }
        Some(left)
    }

    /// 単項演算子: +, -
    fn parse_unary(&mut self) -> Option<i64> {
        self.skip_ws();
        if self.pos >= self.input.len() { return Some(0); }
        match self.input[self.pos] {
            b'-' => {
                self.pos += 1;
                let val = self.parse_unary()?;
                Some(val.wrapping_neg())
            }
            b'+' => {
                self.pos += 1;
                self.parse_unary()
            }
            _ => self.parse_primary(),
        }
    }

    /// 基本要素: 数値リテラル、変数名、括弧
    fn parse_primary(&mut self) -> Option<i64> {
        self.skip_ws();
        if self.pos >= self.input.len() { return Some(0); }
        match self.input[self.pos] {
            b'(' => {
                self.pos += 1;
                let val = self.parse_expr()?;
                self.skip_ws();
                if self.pos < self.input.len() && self.input[self.pos] == b')' {
                    self.pos += 1;
                }
                Some(val)
            }
            b'0'..=b'9' => {
                let start = self.pos;
                while self.pos < self.input.len() && self.input[self.pos].is_ascii_digit() {
                    self.pos += 1;
                }
                let num_str = std::str::from_utf8(&self.input[start..self.pos]).ok()?;
                Some(num_str.parse::<i64>().unwrap_or(0))
            }
            b if is_var_start(b) => {
                // 変数参照（算術コンテキストでは裸の名前も変数として扱う）
                let start = self.pos;
                while self.pos < self.input.len() && is_var_char(self.input[self.pos]) {
                    self.pos += 1;
                }
                let var_name = std::str::from_utf8(&self.input[start..self.pos]).ok()?;
                let val = std::env::var(var_name).unwrap_or_default();
                Some(val.parse::<i64>().unwrap_or(0))
            }
            _ => Some(0),
        }
    }
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
    RedirectErrAppend, // 2>>
    FdDupPrefix(i32), // N>& — src_fd は N、次の Word が dst_fd
    HereDoc,          // <<
    HereString,       // <<<
}

/// 入力文字列をトークン列に変換するイテレータ。
///
/// 空白をスキップし、演算子・クォート・通常ワードを識別する。
/// `Iterator<Item = Result<Token, ParseError>>` を実装。
struct Tokenizer<'a, 'b> {
    input: &'a str,
    pos: usize,
    last_status: i32,
    pos_args: &'b [String],
    nounset: bool,
    nounset_error: Option<String>,
}

impl<'a, 'b> Tokenizer<'a, 'b> {
    fn new(input: &'a str, last_status: i32, pos_args: &'b [String], nounset: bool) -> Self {
        Self { input, pos: 0, last_status, pos_args, nounset, nounset_error: None }
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
                if self.pos + 1 < len && bytes[self.pos + 1] == b'(' {
                    // $((expr)) — 算術展開
                    self.pos += 2; // skip '(('
                    let expr_start = self.pos;
                    let mut paren_depth: i32 = 0;
                    let mut found = false;
                    while self.pos < len {
                        match bytes[self.pos] {
                            b'(' => paren_depth += 1,
                            b')' => {
                                if paren_depth > 0 {
                                    paren_depth -= 1;
                                } else if self.pos + 1 < len && bytes[self.pos + 1] == b')' {
                                    let expr = &self.input[expr_start..self.pos];
                                    match eval_arithmetic(expr, self.last_status, self.pos_args, self.nounset) {
                                        Ok(val) => buf.push_str(&val),
                                        Err(var_name) => {
                                            if self.nounset_error.is_none() { self.nounset_error = Some(var_name); }
                                        }
                                    }
                                    self.pos += 2; // skip '))'
                                    found = true;
                                    break;
                                } else {
                                    break;
                                }
                            }
                            _ => {}
                        }
                        self.pos += 1;
                    }
                    if !found {
                        buf.push_str("$((");
                        self.pos = expr_start;
                    }
                } else {
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
            }
            b'?' => {
                buf.push_str(&self.last_status.to_string());
                self.pos += 1;
            }
            b'$' => {
                buf.push_str(&unsafe { libc::getpid() }.to_string());
                self.pos += 1;
            }
            b'!' => {
                let bg_pid = std::env::var("RUSH_LAST_BG_PID").unwrap_or_else(|_| "0".into());
                buf.push_str(&bg_pid);
                self.pos += 1;
            }
            b'0' => {
                buf.push_str("rush");
                self.pos += 1;
            }
            b'{' => {
                self.pos += 1; // skip '{'
                let var_start = self.pos;
                // 閉じ '}' を探す（パラメータ展開の特殊文字を含む可能性あり）
                let mut depth = 1;
                while self.pos < len && depth > 0 {
                    match bytes[self.pos] {
                        b'{' => depth += 1,
                        b'}' => {
                            depth -= 1;
                            if depth == 0 { break; }
                        }
                        _ => {}
                    }
                    self.pos += 1;
                }
                if self.pos < len && bytes[self.pos] == b'}' {
                    let inner = &self.input[var_start..self.pos];
                    self.pos += 1; // skip '}'
                    if !inner.is_empty() {
                        match expand_braced_param(inner, self.last_status, self.pos_args, self.nounset) {
                            Ok(val) => buf.push_str(&val),
                            Err(var_name) => {
                                if self.nounset_error.is_none() { self.nounset_error = Some(var_name); }
                            }
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
                if let Some(val) = resolve_special_var(var_name) {
                    buf.push_str(&val);
                } else if let Ok(val) = std::env::var(var_name) {
                    buf.push_str(&val);
                } else if self.nounset {
                    if self.nounset_error.is_none() { self.nounset_error = Some(var_name.to_string()); }
                }
            }
            _ => {
                buf.push('$');
            }
        }
    }
}

impl<'a, 'b> Iterator for Tokenizer<'a, 'b> {
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
                if self.peek_at(1) == Some(b'<') && self.peek_at(2) == Some(b'<') {
                    self.pos += 3;
                    Some(Ok(Token::HereString))
                } else if self.peek_at(1) == Some(b'<') {
                    self.pos += 2;
                    Some(Ok(Token::HereDoc))
                } else {
                    self.pos += 1;
                    Some(Ok(Token::RedirectIn))
                }
            }
            // トークン先頭の `2>` のみ。`file2>` 等の途中はWordとして読まれる。
            b'2' if self.peek_at(1) == Some(b'>') && self.peek_at(2) == Some(b'&') => {
                self.pos += 3;
                Some(Ok(Token::FdDupPrefix(2)))
            }
            b'2' if self.peek_at(1) == Some(b'>') && self.peek_at(2) == Some(b'>') => {
                self.pos += 3;
                Some(Ok(Token::RedirectErrAppend))
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
                            match expand_variables(word, self.last_status, self.pos_args, self.nounset) {
                                Ok(cow) => return Some(Ok(Token::Word(cow))),
                                Err(var_name) => {
                                    if self.nounset_error.is_none() { self.nounset_error = Some(var_name); }
                                    return Some(Ok(Token::Word(Cow::Borrowed(word))));
                                }
                            }
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
                    match expand_variables(&buf, self.last_status, self.pos_args, self.nounset) {
                        Ok(expanded) => Some(Ok(Token::Word(Cow::Owned(expanded.into_owned())))),
                        Err(var_name) => {
                            if self.nounset_error.is_none() { self.nounset_error = Some(var_name); }
                            Some(Ok(Token::Word(Cow::Owned(buf))))
                        }
                    }
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
                    match expand_variables(word, self.last_status, self.pos_args, self.nounset) {
                        Ok(cow) => Some(Ok(Token::Word(cow))),
                        Err(var_name) => {
                            if self.nounset_error.is_none() { self.nounset_error = Some(var_name); }
                            Some(Ok(Token::Word(Cow::Borrowed(word))))
                        }
                    }
                }
            }
        }
    }
}

// ── Parser ──────────────────────────────────────────────────────────

/// コマンドリスト内にヒアドキュメントのデリミタを返す。
/// ヒアドキュメントがなければ空の Vec を返す。
pub fn heredoc_delimiters(list: &CommandList<'_>) -> Vec<String> {
    let mut delims = Vec::new();
    for item in &list.items {
        for cmd in &item.pipeline.commands {
            for r in &cmd.redirects {
                if r.kind == RedirectKind::HereDoc {
                    delims.push(r.target.to_string());
                }
            }
        }
    }
    delims
}

/// ヒアドキュメントの body を target に設定する（デリミタ → 本文テキストに置換）。
pub fn fill_heredoc_bodies(list: &mut CommandList<'_>, bodies: &[String]) {
    let mut idx = 0;
    for item in &mut list.items {
        for cmd in &mut item.pipeline.commands {
            for r in &mut cmd.redirects {
                if r.kind == RedirectKind::HereDoc {
                    if idx < bodies.len() {
                        r.target = Cow::Owned(bodies[idx].clone());
                    }
                    idx += 1;
                }
            }
        }
    }
}

/// 入力文字列をパースして `CommandList` AST を返す。
///
/// - 空入力 → `Ok(None)`
/// - 正常なコマンド → `Ok(Some(CommandList))`
/// - 構文エラー → `Err(ParseError)`
///
/// `last_status` は `$?` 展開に使用される。
pub fn parse<'a>(input: &'a str, last_status: i32, pos_args: &[String], nounset: bool) -> Result<Option<CommandList<'a>>, ParseError> {
    let mut tokens = Tokenizer::new(input, last_status, pos_args, nounset);
    let mut items: Vec<ListItem<'_>> = Vec::new();
    let mut commands: Vec<Command<'_>> = Vec::new();
    let mut args: Vec<Cow<'_, str>> = Vec::new();
    let mut redirects: Vec<Redirect<'_>> = Vec::new();
    let mut assignments: Vec<(String, String)> = Vec::new();
    let mut background = false;

    while let Some(result) = tokens.next() {
        let token = result?;
        match token {
            Token::Word(w) => {
                // コマンド先頭の VAR=val を代入として検出
                // 条件: args が空（まだコマンド名を見ていない）かつ有効な識別子=値の形式
                if args.is_empty() {
                    if let Some(eq_pos) = w.find('=') {
                        let name = &w[..eq_pos];
                        if !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                            && !name.as_bytes()[0].is_ascii_digit()
                        {
                            let value = w[eq_pos + 1..].to_string();
                            assignments.push((name.to_string(), value));
                            continue;
                        }
                    }
                }
                args.push(w);
            }
            Token::Pipe => {
                if args.is_empty() && assignments.is_empty() {
                    return Err(ParseError::EmptyPipelineSegment);
                }
                commands.push(Command {
                    args: std::mem::take(&mut args),
                    redirects: std::mem::take(&mut redirects),
                    assignments: std::mem::take(&mut assignments),
                });
            }
            Token::And | Token::Or | Token::Semi => {
                let connector = match token {
                    Token::And => Connector::And,
                    Token::Or => Connector::Or,
                    _ => Connector::Seq,
                };

                // `;` の前に何もなくてもスキップ（bash 互換）
                if args.is_empty() && commands.is_empty() && assignments.is_empty() {
                    if matches!(connector, Connector::Seq) {
                        continue; // 先頭 `;` や `;;` はスキップ
                    }
                    return Err(ParseError::EmptyPipelineSegment);
                }

                if !args.is_empty() || !assignments.is_empty() {
                    commands.push(Command {
                        args: std::mem::take(&mut args),
                        redirects: std::mem::take(&mut redirects),
                        assignments: std::mem::take(&mut assignments),
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
                if args.is_empty() && commands.is_empty() && assignments.is_empty() {
                    return Err(ParseError::EmptyPipelineSegment);
                }

                // `&` の後にコマンドが続くケースをサポート（`cmd1 & cmd2`）
                if !args.is_empty() || !assignments.is_empty() {
                    commands.push(Command {
                        args: std::mem::take(&mut args),
                        redirects: std::mem::take(&mut redirects),
                        assignments: std::mem::take(&mut assignments),
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
            Token::RedirectOut | Token::RedirectAppend | Token::RedirectIn | Token::RedirectErr | Token::RedirectErrAppend => {
                let kind = match token {
                    Token::RedirectOut => RedirectKind::Output,
                    Token::RedirectAppend => RedirectKind::Append,
                    Token::RedirectIn => RedirectKind::Input,
                    Token::RedirectErr => RedirectKind::Stderr,
                    Token::RedirectErrAppend => RedirectKind::StderrAppend,
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
            Token::HereDoc => {
                // <<DELIM — ヒアドキュメント（デリミタをターゲットに格納）
                match tokens.next() {
                    Some(Ok(Token::Word(delim))) => {
                        redirects.push(Redirect { kind: RedirectKind::HereDoc, target: delim });
                    }
                    Some(Err(e)) => return Err(e),
                    _ => return Err(ParseError::MissingRedirectTarget),
                }
            }
            Token::HereString => {
                // <<<word — ヒアストリング
                match tokens.next() {
                    Some(Ok(Token::Word(word))) => {
                        redirects.push(Redirect { kind: RedirectKind::HereString, target: word });
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

    // 末尾パイプ: commands があるが args がない → 継続行入力のトリガー
    if !commands.is_empty() && args.is_empty() && redirects.is_empty() && assignments.is_empty() {
        return Err(ParseError::IncompleteInput);
    }

    // 最終パイプラインの処理
    if !args.is_empty() || !assignments.is_empty() {
        commands.push(Command { args, redirects, assignments });
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

    // 末尾 &&/|| チェック: 最後の項目が And/Or なら継続行入力のトリガー
    if let Some(last) = items.last() {
        if matches!(last.connector, Connector::And | Connector::Or) {
            return Err(ParseError::IncompleteInput);
        }
    }

    // nounset エラーチェック
    if let Some(var_name) = tokens.nounset_error {
        return Err(ParseError::UnboundVariable(var_name));
    }

    Ok(Some(CommandList { items }))
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// パース結果から最初のパイプラインの各コマンドの引数を文字列ベクタとして取り出す。
    fn parse_args(input: &str) -> Vec<Vec<String>> {
        let list = parse(input, 0, &[], false).unwrap().unwrap();
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
        let list = parse("echo hello > out.txt", 0, &[], false).unwrap().unwrap();
        let p = &list.items[0].pipeline;
        assert_eq!(p.commands.len(), 1);
        assert_eq!(p.commands[0].args.len(), 2);
        assert_eq!(p.commands[0].redirects.len(), 1);
        assert_eq!(p.commands[0].redirects[0].kind, RedirectKind::Output);
        assert_eq!(p.commands[0].redirects[0].target, "out.txt");
    }

    #[test]
    fn redirect_append() {
        let list = parse("echo hello >> out.txt", 0, &[], false).unwrap().unwrap();
        let p = &list.items[0].pipeline;
        assert_eq!(p.commands[0].redirects[0].kind, RedirectKind::Append);
        assert_eq!(p.commands[0].redirects[0].target, "out.txt");
    }

    #[test]
    fn redirect_input() {
        let list = parse("cat < in.txt", 0, &[], false).unwrap().unwrap();
        let p = &list.items[0].pipeline;
        assert_eq!(p.commands[0].redirects[0].kind, RedirectKind::Input);
        assert_eq!(p.commands[0].redirects[0].target, "in.txt");
    }

    #[test]
    fn redirect_stderr() {
        let list = parse("ls 2> err.txt", 0, &[], false).unwrap().unwrap();
        let p = &list.items[0].pipeline;
        assert_eq!(p.commands[0].redirects[0].kind, RedirectKind::Stderr);
        assert_eq!(p.commands[0].redirects[0].target, "err.txt");
    }

    #[test]
    fn redirect_stderr_append() {
        let list = parse("cmd 2>> err.log", 0, &[], false).unwrap().unwrap();
        let p = &list.items[0].pipeline;
        assert_eq!(p.commands[0].redirects[0].kind, RedirectKind::StderrAppend);
        assert_eq!(p.commands[0].redirects[0].target, "err.log");
    }

    #[test]
    fn here_string() {
        let list = parse("cat <<<hello", 0, &[], false).unwrap().unwrap();
        let p = &list.items[0].pipeline;
        assert_eq!(p.commands[0].args[0], "cat");
        assert_eq!(p.commands[0].redirects[0].kind, RedirectKind::HereString);
        assert_eq!(p.commands[0].redirects[0].target, "hello");
    }

    #[test]
    fn here_string_with_space() {
        let list = parse("cat <<< word", 0, &[], false).unwrap().unwrap();
        let p = &list.items[0].pipeline;
        assert_eq!(p.commands[0].redirects[0].kind, RedirectKind::HereString);
        assert_eq!(p.commands[0].redirects[0].target, "word");
    }

    #[test]
    fn here_doc_delimiter() {
        let list = parse("cat <<EOF", 0, &[], false).unwrap().unwrap();
        let p = &list.items[0].pipeline;
        assert_eq!(p.commands[0].redirects[0].kind, RedirectKind::HereDoc);
        assert_eq!(p.commands[0].redirects[0].target, "EOF");
    }

    #[test]
    fn here_doc_delimiters_fn() {
        let list = parse("cat <<EOF", 0, &[], false).unwrap().unwrap();
        let delims = heredoc_delimiters(&list);
        assert_eq!(delims, vec!["EOF"]);
    }

    #[test]
    fn redirect_no_space() {
        let list = parse("echo hello >out.txt", 0, &[], false).unwrap().unwrap();
        let p = &list.items[0].pipeline;
        assert_eq!(p.commands[0].redirects[0].target, "out.txt");
    }

    #[test]
    fn multiple_redirects() {
        let list = parse("cmd < in.txt > out.txt 2> err.txt", 0, &[], false).unwrap().unwrap();
        let p = &list.items[0].pipeline;
        assert_eq!(p.commands[0].redirects.len(), 3);
        assert_eq!(p.commands[0].redirects[0].kind, RedirectKind::Input);
        assert_eq!(p.commands[0].redirects[1].kind, RedirectKind::Output);
        assert_eq!(p.commands[0].redirects[2].kind, RedirectKind::Stderr);
    }

    // ── パイプライン + リダイレクト複合 ──

    #[test]
    fn pipeline_with_redirects() {
        let list = parse("cat < in.txt | grep hello > out.txt", 0, &[], false).unwrap().unwrap();
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
        let list = parse("echo 2 > file", 0, &[], false).unwrap().unwrap();
        let p = &list.items[0].pipeline;
        assert_eq!(p.commands[0].args.len(), 2);
        assert_eq!(p.commands[0].args[1], "2");
        assert_eq!(p.commands[0].redirects[0].kind, RedirectKind::Output);
    }

    // ── 空入力 ──

    #[test]
    fn empty_input() {
        assert!(parse("", 0, &[], false).unwrap().is_none());
        assert!(parse("   ", 0, &[], false).unwrap().is_none());
        assert!(parse("\t\n", 0, &[], false).unwrap().is_none());
    }

    // ── エラーケース ──

    #[test]
    fn err_unterminated_single_quote() {
        assert_eq!(
            parse("echo 'hello", 0, &[], false),
            Err(ParseError::UnterminatedQuote('\'')),
        );
    }

    #[test]
    fn err_unterminated_double_quote() {
        assert_eq!(
            parse("echo \"hello", 0, &[], false),
            Err(ParseError::UnterminatedQuote('"')),
        );
    }

    #[test]
    fn err_missing_redirect_target() {
        assert_eq!(parse("echo >", 0, &[], false), Err(ParseError::MissingRedirectTarget));
    }

    #[test]
    fn err_redirect_followed_by_pipe() {
        assert_eq!(parse("echo > | cat", 0, &[], false), Err(ParseError::MissingRedirectTarget));
    }

    #[test]
    fn err_leading_pipe() {
        assert_eq!(parse("| ls", 0, &[], false), Err(ParseError::EmptyPipelineSegment));
    }

    #[test]
    fn err_trailing_pipe() {
        assert_eq!(parse("ls |", 0, &[], false), Err(ParseError::IncompleteInput));
    }

    #[test]
    fn err_double_pipe_operator() {
        // `ls | | grep` → first `|` consumed as Pipe, then `| grep` → EmptyPipelineSegment
        // because after Pipe, args is empty and next token is `|` (Pipe)
        assert_eq!(parse("ls | | grep", 0, &[], false), Err(ParseError::EmptyPipelineSegment));
    }

    // ── background (&) ──

    #[test]
    fn background_simple() {
        let list = parse("sleep 10 &", 0, &[], false).unwrap().unwrap();
        let p = &list.items[0].pipeline;
        assert!(p.background);
        assert_eq!(p.commands.len(), 1);
        assert_eq!(p.commands[0].args[0], "sleep");
    }

    #[test]
    fn background_pipeline() {
        let list = parse("ls | grep foo &", 0, &[], false).unwrap().unwrap();
        let p = &list.items[0].pipeline;
        assert!(p.background);
        assert_eq!(p.commands.len(), 2);
    }

    #[test]
    fn background_bare_ampersand() {
        assert_eq!(parse("&", 0, &[], false), Err(ParseError::EmptyPipelineSegment));
    }

    #[test]
    fn background_followed_by_command() {
        // `cmd & extra` → 2 items: cmd (background), extra (foreground)
        let list = parse("cmd & extra", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items.len(), 2);
        assert!(list.items[0].pipeline.background);
        assert_eq!(list.items[0].pipeline.commands[0].args[0], "cmd");
        assert!(!list.items[1].pipeline.background);
        assert_eq!(list.items[1].pipeline.commands[0].args[0], "extra");
    }

    #[test]
    fn no_background_flag() {
        let list = parse("ls", 0, &[], false).unwrap().unwrap();
        assert!(!list.items[0].pipeline.background);
    }

    // ── Cow はすべて Borrowed（展開不要時） ──

    #[test]
    fn cow_is_borrowed() {
        let list = parse("echo hello", 0, &[], false).unwrap().unwrap();
        for arg in &list.items[0].pipeline.commands[0].args {
            assert!(matches!(arg, Cow::Borrowed(_)), "expected Borrowed, got Owned");
        }
    }

    #[test]
    fn cow_quoted_is_borrowed() {
        let list = parse("echo 'hello world'", 0, &[], false).unwrap().unwrap();
        assert!(matches!(&list.items[0].pipeline.commands[0].args[1], Cow::Borrowed(_)));
    }

    // ── 変数展開テスト ──

    #[test]
    fn expand_env_var() {
        std::env::set_var("RUSH_TEST_VAR", "hello");
        let list = parse("echo $RUSH_TEST_VAR", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "hello");
        std::env::remove_var("RUSH_TEST_VAR");
    }

    #[test]
    fn expand_last_status() {
        let list = parse("echo $?", 42, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "42");
    }

    #[test]
    fn expand_dollar_dollar() {
        let list = parse("echo $$", 0, &[], false).unwrap().unwrap();
        let val: i32 = list.items[0].pipeline.commands[0].args[1].parse().unwrap();
        assert!(val > 0); // should be a valid PID
    }

    #[test]
    fn expand_dollar_bang() {
        std::env::set_var("RUSH_LAST_BG_PID", "12345");
        let list = parse("echo $!", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "12345");
        std::env::remove_var("RUSH_LAST_BG_PID");
    }

    #[test]
    fn expand_dollar_zero() {
        let list = parse("echo $0", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "rush");
    }

    #[test]
    fn expand_random() {
        let list = parse("echo $RANDOM", 0, &[], false).unwrap().unwrap();
        let val: u64 = list.items[0].pipeline.commands[0].args[1]
            .parse()
            .expect("$RANDOM should be a number");
        assert!(val < 32768, "$RANDOM should be 0..32767, got {}", val);
    }

    #[test]
    fn expand_seconds() {
        let list = parse("echo $SECONDS", 0, &[], false).unwrap().unwrap();
        let val: u64 = list.items[0].pipeline.commands[0].args[1]
            .parse()
            .expect("$SECONDS should be a number");
        // テスト実行中なので 0 以上であること
        assert!(val < 1_000_000, "$SECONDS should be reasonable, got {}", val);
    }

    #[test]
    fn expand_random_in_braces() {
        let list = parse("echo ${RANDOM}", 0, &[], false).unwrap().unwrap();
        let val: u64 = list.items[0].pipeline.commands[0].args[1]
            .parse()
            .expect("${RANDOM} should be a number");
        assert!(val < 32768);
    }

    #[test]
    fn expand_seconds_in_braces() {
        let list = parse("echo ${SECONDS}", 0, &[], false).unwrap().unwrap();
        let val: u64 = list.items[0].pipeline.commands[0].args[1]
            .parse()
            .expect("${SECONDS} should be a number");
        assert!(val < 1_000_000);
    }

    #[test]
    fn expand_undefined_var() {
        std::env::remove_var("RUSH_NONEXISTENT_VAR_XYZ");
        let list = parse("echo $RUSH_NONEXISTENT_VAR_XYZ", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "");
    }

    #[test]
    fn single_quote_no_expand() {
        std::env::set_var("RUSH_TEST_VAR2", "expanded");
        let list = parse("echo '$RUSH_TEST_VAR2'", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "$RUSH_TEST_VAR2");
        assert!(matches!(&list.items[0].pipeline.commands[0].args[1], Cow::Borrowed(_)));
        std::env::remove_var("RUSH_TEST_VAR2");
    }

    #[test]
    fn double_quote_expand() {
        std::env::set_var("RUSH_TEST_VAR3", "world");
        let list = parse("echo \"hello $RUSH_TEST_VAR3\"", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "hello world");
        std::env::remove_var("RUSH_TEST_VAR3");
    }

    #[test]
    fn redirect_target_expand() {
        std::env::set_var("RUSH_TEST_DIR", "/tmp");
        let list = parse("echo hello > $RUSH_TEST_DIR/out.txt", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].redirects[0].target, "/tmp/out.txt");
        std::env::remove_var("RUSH_TEST_DIR");
    }

    #[test]
    fn bare_dollar_at_end() {
        let list = parse("echo $", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "$");
    }

    #[test]
    fn dollar_at_expands_positional() {
        // $@ expands to all positional parameters (empty when none set)
        let list = parse("echo $@", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "");
    }

    #[test]
    fn no_dollar_cow_borrowed() {
        let list = parse("echo hello", 0, &[], false).unwrap().unwrap();
        assert!(matches!(&list.items[0].pipeline.commands[0].args[0], Cow::Borrowed(_)));
        assert!(matches!(&list.items[0].pipeline.commands[0].args[1], Cow::Borrowed(_)));
    }

    #[test]
    fn double_quote_no_dollar_cow_borrowed() {
        let list = parse("echo \"hello\"", 0, &[], false).unwrap().unwrap();
        assert!(matches!(&list.items[0].pipeline.commands[0].args[1], Cow::Borrowed(_)));
    }

    // ── && / || / ; テスト ──

    #[test]
    fn and_connector() {
        let list = parse("echo a && echo b", 0, &[], false).unwrap().unwrap();
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
        let list = parse("false || echo ok", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items.len(), 2);
        assert_eq!(list.items[0].connector, Connector::Or);
    }

    #[test]
    fn seq_connector() {
        let list = parse("echo a ; echo b", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items.len(), 2);
        assert_eq!(list.items[0].connector, Connector::Seq);
    }

    #[test]
    fn mixed_connectors() {
        let list = parse("a && b || c ; d", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items.len(), 4);
        assert_eq!(list.items[0].connector, Connector::And);
        assert_eq!(list.items[1].connector, Connector::Or);
        assert_eq!(list.items[2].connector, Connector::Seq);
        assert_eq!(list.items[3].connector, Connector::Seq);
    }

    #[test]
    fn background_then_command() {
        let list = parse("sleep 1 & echo done", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items.len(), 2);
        assert!(list.items[0].pipeline.background);
        assert!(!list.items[1].pipeline.background);
        assert_eq!(list.items[1].pipeline.commands[0].args[0], "echo");
    }

    #[test]
    fn leading_semi_skipped() {
        let list = parse("; echo hello", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items.len(), 1);
        assert_eq!(list.items[0].pipeline.commands[0].args[0], "echo");
    }

    #[test]
    fn trailing_semi_ok() {
        let list = parse("echo hello ;", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items.len(), 1);
    }

    #[test]
    fn double_semi_skipped() {
        let list = parse("echo a ;; echo b", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items.len(), 2);
    }

    #[test]
    fn only_semicolons() {
        assert!(parse(";;;", 0, &[], false).unwrap().is_none());
    }

    #[test]
    fn err_leading_and() {
        assert_eq!(parse("&& cmd", 0, &[], false), Err(ParseError::EmptyPipelineSegment));
    }

    #[test]
    fn err_trailing_and() {
        assert_eq!(parse("cmd &&", 0, &[], false), Err(ParseError::IncompleteInput));
    }

    #[test]
    fn err_leading_or() {
        // `||` at start: first `||` is Or token, empty pipeline before it
        assert_eq!(parse("|| cmd", 0, &[], false), Err(ParseError::EmptyPipelineSegment));
    }

    #[test]
    fn err_trailing_or() {
        assert_eq!(parse("cmd ||", 0, &[], false), Err(ParseError::IncompleteInput));
    }

    // ── エスケープテスト ──

    #[test]
    fn escape_double_quote_in_dquote() {
        let list = parse(r#"echo "hello\"world""#, 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "hello\"world");
    }

    #[test]
    fn escape_backslash_in_dquote() {
        let list = parse(r#"echo "a\\b""#, 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "a\\b");
    }

    #[test]
    fn escape_dollar_in_dquote() {
        let list = parse(r#"echo "\$HOME""#, 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "$HOME");
    }

    #[test]
    fn escape_space_in_bare_word() {
        let list = parse(r"echo file\ name", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "file name");
    }

    // ── ${VAR} テスト ──

    #[test]
    fn expand_braced_var() {
        std::env::set_var("RUSH_TEST_BRACE", "braced");
        let list = parse("echo ${RUSH_TEST_BRACE}", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "braced");
        std::env::remove_var("RUSH_TEST_BRACE");
    }

    #[test]
    fn expand_braced_var_with_suffix() {
        std::env::set_var("RUSH_TEST_BSUF", "val");
        let list = parse("echo ${RUSH_TEST_BSUF}suffix", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "valsuffix");
        std::env::remove_var("RUSH_TEST_BSUF");
    }

    #[test]
    fn expand_braced_undefined() {
        std::env::remove_var("RUSH_TEST_BUNDEF_XYZ");
        let list = parse("echo ${RUSH_TEST_BUNDEF_XYZ}", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "");
    }

    #[test]
    fn braced_unclosed() {
        // `${` without closing `}` → literal "${" then rest
        let list = parse("echo ${abc", 0, &[], false).unwrap().unwrap();
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
        let list = parse("cmd 2>&1", 0, &[], false).unwrap().unwrap();
        let r = &list.items[0].pipeline.commands[0].redirects[0];
        assert_eq!(r.kind, RedirectKind::FdDup { src_fd: 2, dst_fd: 1 });
    }

    #[test]
    fn fd_dup_stdout_to_stderr() {
        let list = parse("cmd >&2", 0, &[], false).unwrap().unwrap();
        let r = &list.items[0].pipeline.commands[0].redirects[0];
        assert_eq!(r.kind, RedirectKind::FdDup { src_fd: 1, dst_fd: 2 });
    }

    #[test]
    fn fd_dup_with_file_redirect() {
        let list = parse("cmd > out 2>&1", 0, &[], false).unwrap().unwrap();
        let redirects = &list.items[0].pipeline.commands[0].redirects;
        assert_eq!(redirects.len(), 2);
        assert_eq!(redirects[0].kind, RedirectKind::Output);
        assert_eq!(redirects[0].target, "out");
        assert_eq!(redirects[1].kind, RedirectKind::FdDup { src_fd: 2, dst_fd: 1 });
    }

    #[test]
    fn fd_dup_bad_target() {
        assert_eq!(parse("cmd 2>&abc", 0, &[], false), Err(ParseError::BadFdRedirect));
    }

    // ── コマンド置換パススルーテスト ──

    #[test]
    fn cmd_sub_passthrough() {
        let list = parse("echo $(date)", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "$(date)");
    }

    #[test]
    fn backtick_passthrough() {
        let list = parse("echo `date`", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "`date`");
    }

    #[test]
    fn cmd_sub_nested() {
        let list = parse("echo $(echo $(whoami))", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "$(echo $(whoami))");
    }

    #[test]
    fn cmd_sub_in_double_quotes() {
        let list = parse("echo \"today is $(date)\"", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "today is $(date)");
    }

    // ── パラメータ展開テスト ──

    #[test]
    fn param_default() {
        std::env::remove_var("RUSH_TEST_PDEF");
        let list = parse("echo ${RUSH_TEST_PDEF:-hello}", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "hello");

        std::env::set_var("RUSH_TEST_PDEF", "world");
        let list = parse("echo ${RUSH_TEST_PDEF:-hello}", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "world");
        std::env::remove_var("RUSH_TEST_PDEF");
    }

    #[test]
    fn param_alt() {
        std::env::remove_var("RUSH_TEST_PALT");
        let list = parse("echo ${RUSH_TEST_PALT:+yes}", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "");

        std::env::set_var("RUSH_TEST_PALT", "val");
        let list = parse("echo ${RUSH_TEST_PALT:+yes}", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "yes");
        std::env::remove_var("RUSH_TEST_PALT");
    }

    #[test]
    fn param_length() {
        std::env::set_var("RUSH_TEST_PLEN", "hello");
        let list = parse("echo ${#RUSH_TEST_PLEN}", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "5");
        std::env::remove_var("RUSH_TEST_PLEN");
    }

    #[test]
    fn param_strip_suffix() {
        std::env::set_var("RUSH_TEST_PSUF", "hello.tar.gz");
        let list = parse("echo ${RUSH_TEST_PSUF%.*}", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "hello.tar");
        let list = parse("echo ${RUSH_TEST_PSUF%%.*}", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "hello");
        std::env::remove_var("RUSH_TEST_PSUF");
    }

    #[test]
    fn param_strip_prefix() {
        std::env::set_var("RUSH_TEST_PPRE", "/usr/local/bin");
        let list = parse("echo ${RUSH_TEST_PPRE#*/}", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "usr/local/bin");
        let list = parse("echo ${RUSH_TEST_PPRE##*/}", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "bin");
        std::env::remove_var("RUSH_TEST_PPRE");
    }

    #[test]
    fn param_replace() {
        std::env::set_var("RUSH_TEST_PREP", "hello world hello");
        let list = parse("echo ${RUSH_TEST_PREP/hello/bye}", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "bye world hello");
        let list = parse("echo ${RUSH_TEST_PREP//hello/bye}", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "bye world bye");
        std::env::remove_var("RUSH_TEST_PREP");
    }

    // ── 算術展開テスト ──

    #[test]
    fn arith_basic() {
        let list = parse("echo $((1+2))", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "3");
    }

    #[test]
    fn arith_precedence() {
        let list = parse("echo $((2+3*4))", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "14");
    }

    #[test]
    fn arith_parens() {
        let list = parse("echo $((2*(3+4)))", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "14");
    }

    #[test]
    fn arith_div_mod() {
        let list = parse("echo $((10/3))", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "3");
        let list = parse("echo $((10%3))", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "1");
    }

    #[test]
    fn arith_negative() {
        let list = parse("echo $((-5+3))", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "-2");
    }

    #[test]
    fn arith_spaces() {
        let list = parse("echo $(( 10 + 20 ))", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "30");
    }

    #[test]
    fn arith_variable() {
        std::env::set_var("RUSH_TEST_ARITH", "7");
        let list = parse("echo $((RUSH_TEST_ARITH+3))", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "10");
        std::env::remove_var("RUSH_TEST_ARITH");
    }

    #[test]
    fn arith_dollar_variable() {
        std::env::set_var("RUSH_TEST_ARITH2", "5");
        let list = parse("echo $(($RUSH_TEST_ARITH2*2))", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "10");
        std::env::remove_var("RUSH_TEST_ARITH2");
    }

    #[test]
    fn arith_in_double_quotes() {
        let list = parse("echo \"result=$((1+2))\"", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "result=3");
    }

    // ── 継続行入力テスト ──

    #[test]
    fn incomplete_trailing_pipe() {
        assert_eq!(parse("ls |", 0, &[], false), Err(ParseError::IncompleteInput));
        // 継続入力後の再パースは成功する
        let list = parse("ls |\ngrep foo", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands.len(), 2);
    }

    #[test]
    fn incomplete_trailing_and() {
        assert_eq!(parse("true &&", 0, &[], false), Err(ParseError::IncompleteInput));
        let list = parse("true &&\necho ok", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items.len(), 2);
        assert_eq!(list.items[0].connector, Connector::And);
    }

    #[test]
    fn incomplete_trailing_or() {
        assert_eq!(parse("false ||", 0, &[], false), Err(ParseError::IncompleteInput));
        let list = parse("false ||\necho ok", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items.len(), 2);
        assert_eq!(list.items[0].connector, Connector::Or);
    }

    #[test]
    fn multiline_quoted_string() {
        // 最初のパースは UnterminatedQuote
        assert!(matches!(parse("echo \"hello", 0, &[], false), Err(ParseError::UnterminatedQuote('"'))));
        // 継続入力後は成功
        let list = parse("echo \"hello\nworld\"", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "hello\nworld");
    }

    #[test]
    fn inline_assignment_only() {
        let list = parse("FOO=bar", 0, &[], false).unwrap().unwrap();
        let cmd = &list.items[0].pipeline.commands[0];
        assert!(cmd.args.is_empty());
        assert_eq!(cmd.assignments, vec![("FOO".to_string(), "bar".to_string())]);
    }

    #[test]
    fn inline_assignment_with_command() {
        let list = parse("FOO=bar echo hello", 0, &[], false).unwrap().unwrap();
        let cmd = &list.items[0].pipeline.commands[0];
        assert_eq!(cmd.args[0], "echo");
        assert_eq!(cmd.args[1], "hello");
        assert_eq!(cmd.assignments, vec![("FOO".to_string(), "bar".to_string())]);
    }

    #[test]
    fn multiple_assignments() {
        let list = parse("A=1 B=2 cmd", 0, &[], false).unwrap().unwrap();
        let cmd = &list.items[0].pipeline.commands[0];
        assert_eq!(cmd.args[0], "cmd");
        assert_eq!(cmd.assignments.len(), 2);
        assert_eq!(cmd.assignments[0], ("A".to_string(), "1".to_string()));
        assert_eq!(cmd.assignments[1], ("B".to_string(), "2".to_string()));
    }

    #[test]
    fn assignment_not_after_command() {
        // FOO=bar should not be treated as assignment when after a command word
        let list = parse("echo FOO=bar", 0, &[], false).unwrap().unwrap();
        let cmd = &list.items[0].pipeline.commands[0];
        assert!(cmd.assignments.is_empty());
        assert_eq!(cmd.args[1], "FOO=bar");
    }

    // ── 位置パラメータ展開テスト ──

    #[test]
    fn dollar_1_no_positional() {
        // $1 with no positional args → empty
        let list = parse("echo $1", 0, &[], false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "");
    }

    #[test]
    fn dollar_1_with_positional() {
        let args = vec!["hello".to_string(), "world".to_string()];
        let list = parse("echo $1 $2", 0, &args, false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "hello");
        assert_eq!(list.items[0].pipeline.commands[0].args[2], "world");
    }

    #[test]
    fn dollar_hash_count() {
        let args = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let list = parse("echo $#", 0, &args, false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "3");
    }

    #[test]
    fn dollar_star_all_args() {
        let args = vec!["a".to_string(), "b".to_string()];
        let list = parse("echo $*", 0, &args, false).unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "a b");
    }

    // ── set -u (nounset) ──

    #[test]
    fn nounset_undefined_var_error() {
        std::env::remove_var("RUSH_NOUNSET_TEST_UNDEF");
        let result = parse("echo $RUSH_NOUNSET_TEST_UNDEF", 0, &[], true);
        assert!(result.is_err());
        match result.unwrap_err() {
            ParseError::UnboundVariable(name) => assert_eq!(name, "RUSH_NOUNSET_TEST_UNDEF"),
            other => panic!("expected UnboundVariable, got {:?}", other),
        }
    }

    #[test]
    fn nounset_defined_var_ok() {
        std::env::set_var("RUSH_NOUNSET_TEST_DEF", "hello");
        let result = parse("echo $RUSH_NOUNSET_TEST_DEF", 0, &[], true);
        assert!(result.is_ok());
        let list = result.unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "hello");
        std::env::remove_var("RUSH_NOUNSET_TEST_DEF");
    }

    #[test]
    fn nounset_default_operator_exempt() {
        std::env::remove_var("RUSH_NOUNSET_TEST_DFLT");
        // ${var:-default} は nounset エラーにならない
        let result = parse("echo ${RUSH_NOUNSET_TEST_DFLT:-ok}", 0, &[], true);
        assert!(result.is_ok());
        let list = result.unwrap().unwrap();
        assert_eq!(list.items[0].pipeline.commands[0].args[1], "ok");
    }

    #[test]
    fn nounset_special_vars_exempt() {
        // $@, $#, $?, $$ 等は nounset 対象外
        let result = parse("echo $@ $# $? $$", 0, &[], true);
        assert!(result.is_ok());
    }

    #[test]
    fn nounset_disabled_no_error() {
        std::env::remove_var("RUSH_NOUNSET_TEST_OFF");
        // nounset=false なら未定義変数はエラーにならない
        let result = parse("echo $RUSH_NOUNSET_TEST_OFF", 0, &[], false);
        assert!(result.is_ok());
    }
}
