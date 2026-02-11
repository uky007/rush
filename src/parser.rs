//! トークナイザ + パーサー: 入力文字列からパイプライン AST を構築する。
//!
//! 手書きトークナイザでゼロコピー（[`Cow::Borrowed`]）のトークン列を生成し、
//! ループベースのパーサーで [`Pipeline`] AST に変換する。
//!
//! ## 対応構文
//!
//! - パイプライン: `cmd1 | cmd2 | cmd3`
//! - リダイレクト: `>`, `>>`, `<`, `2>`
//! - クォート: シングル (`'...'`) / ダブル (`"..."`)
//!
//! ## 未対応（将来拡張）
//!
//! エスケープ (`\"`, `\\`)、変数展開 (`$HOME`)、コマンド置換、
//! ヒアドキュメント、`&&` / `||` / `;`、隣接トークン結合 (`foo"bar"`)。

use std::borrow::Cow;
use std::fmt;

// ── AST ─────────────────────────────────────────────────────────────

/// パイプラインで接続されたコマンド列。`cmd1 | cmd2 | cmd3` → 3要素。
#[derive(Debug, PartialEq)]
pub struct Pipeline<'a> {
    pub commands: Vec<Command<'a>>,
}

/// 単一コマンド。引数リストとリダイレクト指定を持つ。
///
/// `Cow<'a, str>` を採用: クォートなしトークンは `Borrowed`（ゼロコピー）。
/// 将来のエスケープ/変数展開で `Owned` が必要になる場面に備える。
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
}

// ── Error ───────────────────────────────────────────────────────────

/// パース時に発生しうるエラー。
#[derive(Debug, PartialEq)]
pub enum ParseError {
    /// クォートが閉じられていない。引数は開始クォート文字（`'` or `"`）。
    UnterminatedQuote(char),
    /// リダイレクト演算子の後にターゲットファイル名がない。
    MissingRedirectTarget,
    /// パイプの前後にコマンドがない（`| ls`, `ls |`, `ls | | grep` 等）。
    EmptyPipelineSegment,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnterminatedQuote(c) => write!(f, "unexpected EOF while looking for matching `{c}`"),
            Self::MissingRedirectTarget => write!(f, "syntax error: missing redirect target"),
            Self::EmptyPipelineSegment => write!(f, "syntax error near unexpected token `|`"),
        }
    }
}

// ── Tokenizer (crate-private) ───────────────────────────────────────

/// トークナイザが生成する内部トークン型。
enum Token<'a> {
    Word(Cow<'a, str>),
    Pipe,
    RedirectOut,
    RedirectAppend,
    RedirectIn,
    RedirectErr,
}

/// 入力文字列をトークン列に変換するイテレータ。
///
/// 空白をスキップし、演算子・クォート・通常ワードを識別する。
/// `Iterator<Item = Result<Token, ParseError>>` を実装。
struct Tokenizer<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> Tokenizer<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, pos: 0 }
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
}

impl<'a> Iterator for Tokenizer<'a> {
    type Item = Result<Token<'a>, ParseError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.skip_whitespace();
        let ch = self.peek()?;

        match ch {
            b'|' => {
                self.pos += 1;
                Some(Ok(Token::Pipe))
            }
            b'>' => {
                self.pos += 1;
                if self.peek() == Some(b'>') {
                    self.pos += 1;
                    Some(Ok(Token::RedirectAppend))
                } else {
                    Some(Ok(Token::RedirectOut))
                }
            }
            b'<' => {
                self.pos += 1;
                Some(Ok(Token::RedirectIn))
            }
            // トークン先頭の `2>` のみ。`file2>` 等の途中はWordとして読まれる。
            b'2' if self.peek_at(1) == Some(b'>') => {
                self.pos += 2;
                Some(Ok(Token::RedirectErr))
            }
            b'\'' | b'"' => {
                let quote = ch as char;
                self.pos += 1; // skip opening quote
                let start = self.pos;
                loop {
                    if self.pos >= self.input.len() {
                        return Some(Err(ParseError::UnterminatedQuote(quote)));
                    }
                    if self.input.as_bytes()[self.pos] == ch {
                        let word = &self.input[start..self.pos];
                        self.pos += 1; // skip closing quote
                        return Some(Ok(Token::Word(Cow::Borrowed(word))));
                    }
                    self.pos += 1;
                }
            }
            _ => {
                let start = self.pos;
                while self.pos < self.input.len() {
                    match self.input.as_bytes()[self.pos] {
                        b' ' | b'\t' | b'\n' | b'\r' | b'|' | b'>' | b'<' | b'\'' | b'"' => {
                            break;
                        }
                        _ => self.pos += 1,
                    }
                }
                Some(Ok(Token::Word(Cow::Borrowed(&self.input[start..self.pos]))))
            }
        }
    }
}

// ── Parser ──────────────────────────────────────────────────────────

/// 入力文字列をパースして `Pipeline` AST を返す。
///
/// - 空入力 → `Ok(None)`
/// - 正常なコマンド → `Ok(Some(Pipeline))`
/// - 構文エラー → `Err(ParseError)`
pub fn parse(input: &str) -> Result<Option<Pipeline<'_>>, ParseError> {
    let mut tokens = Tokenizer::new(input);
    let mut commands: Vec<Command<'_>> = Vec::new();
    let mut args: Vec<Cow<'_, str>> = Vec::new();
    let mut redirects: Vec<Redirect<'_>> = Vec::new();

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
            _ => {
                // Redirect token — next token must be a Word (target)
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
        }
    }

    // 最終コマンドの処理
    if args.is_empty() {
        if redirects.is_empty() && commands.is_empty() {
            return Ok(None); // 空入力
        }
        // 末尾パイプ or リダイレクトのみ（コマンドなし）
        return Err(ParseError::EmptyPipelineSegment);
    }

    commands.push(Command { args, redirects });
    Ok(Some(Pipeline { commands }))
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// パース結果から各コマンドの引数を文字列ベクタとして取り出す。
    fn parse_args(input: &str) -> Vec<Vec<String>> {
        let pipeline = parse(input).unwrap().unwrap();
        pipeline
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
        let p = parse("echo hello > out.txt").unwrap().unwrap();
        assert_eq!(p.commands.len(), 1);
        assert_eq!(p.commands[0].args.len(), 2);
        assert_eq!(p.commands[0].redirects.len(), 1);
        assert_eq!(p.commands[0].redirects[0].kind, RedirectKind::Output);
        assert_eq!(p.commands[0].redirects[0].target, "out.txt");
    }

    #[test]
    fn redirect_append() {
        let p = parse("echo hello >> out.txt").unwrap().unwrap();
        assert_eq!(p.commands[0].redirects[0].kind, RedirectKind::Append);
        assert_eq!(p.commands[0].redirects[0].target, "out.txt");
    }

    #[test]
    fn redirect_input() {
        let p = parse("cat < in.txt").unwrap().unwrap();
        assert_eq!(p.commands[0].redirects[0].kind, RedirectKind::Input);
        assert_eq!(p.commands[0].redirects[0].target, "in.txt");
    }

    #[test]
    fn redirect_stderr() {
        let p = parse("ls 2> err.txt").unwrap().unwrap();
        assert_eq!(p.commands[0].redirects[0].kind, RedirectKind::Stderr);
        assert_eq!(p.commands[0].redirects[0].target, "err.txt");
    }

    #[test]
    fn redirect_no_space() {
        let p = parse("echo hello >out.txt").unwrap().unwrap();
        assert_eq!(p.commands[0].redirects[0].target, "out.txt");
    }

    #[test]
    fn multiple_redirects() {
        let p = parse("cmd < in.txt > out.txt 2> err.txt").unwrap().unwrap();
        assert_eq!(p.commands[0].redirects.len(), 3);
        assert_eq!(p.commands[0].redirects[0].kind, RedirectKind::Input);
        assert_eq!(p.commands[0].redirects[1].kind, RedirectKind::Output);
        assert_eq!(p.commands[0].redirects[2].kind, RedirectKind::Stderr);
    }

    // ── パイプライン + リダイレクト複合 ──

    #[test]
    fn pipeline_with_redirects() {
        let p = parse("cat < in.txt | grep hello > out.txt").unwrap().unwrap();
        assert_eq!(p.commands.len(), 2);
        assert_eq!(p.commands[0].redirects[0].kind, RedirectKind::Input);
        assert_eq!(p.commands[0].redirects[0].target, "in.txt");
        assert_eq!(p.commands[1].redirects[0].kind, RedirectKind::Output);
        assert_eq!(p.commands[1].redirects[0].target, "out.txt");
    }

    // ── 2> はトークン先頭のみ ──

    #[test]
    fn two_is_not_stderr_redirect_with_space() {
        // "echo 2 > file" → args=["echo", "2"], redirect Output "file"
        let p = parse("echo 2 > file").unwrap().unwrap();
        assert_eq!(p.commands[0].args.len(), 2);
        assert_eq!(p.commands[0].args[1], "2");
        assert_eq!(p.commands[0].redirects[0].kind, RedirectKind::Output);
    }

    // ── 空入力 ──

    #[test]
    fn empty_input() {
        assert!(parse("").unwrap().is_none());
        assert!(parse("   ").unwrap().is_none());
        assert!(parse("\t\n").unwrap().is_none());
    }

    // ── エラーケース ──

    #[test]
    fn err_unterminated_single_quote() {
        assert_eq!(
            parse("echo 'hello"),
            Err(ParseError::UnterminatedQuote('\'')),
        );
    }

    #[test]
    fn err_unterminated_double_quote() {
        assert_eq!(
            parse("echo \"hello"),
            Err(ParseError::UnterminatedQuote('"')),
        );
    }

    #[test]
    fn err_missing_redirect_target() {
        assert_eq!(parse("echo >"), Err(ParseError::MissingRedirectTarget));
    }

    #[test]
    fn err_redirect_followed_by_pipe() {
        assert_eq!(parse("echo > | cat"), Err(ParseError::MissingRedirectTarget));
    }

    #[test]
    fn err_leading_pipe() {
        assert_eq!(parse("| ls"), Err(ParseError::EmptyPipelineSegment));
    }

    #[test]
    fn err_trailing_pipe() {
        assert_eq!(parse("ls |"), Err(ParseError::EmptyPipelineSegment));
    }

    #[test]
    fn err_double_pipe() {
        assert_eq!(parse("ls | | grep"), Err(ParseError::EmptyPipelineSegment));
    }

    // ── Cow はすべて Borrowed ──

    #[test]
    fn cow_is_borrowed() {
        let p = parse("echo hello").unwrap().unwrap();
        for arg in &p.commands[0].args {
            assert!(matches!(arg, Cow::Borrowed(_)), "expected Borrowed, got Owned");
        }
    }

    #[test]
    fn cow_quoted_is_borrowed() {
        let p = parse("echo 'hello world'").unwrap().unwrap();
        assert!(matches!(&p.commands[0].args[1], Cow::Borrowed(_)));
    }
}
