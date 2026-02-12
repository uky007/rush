//! `posix_spawnp()` の安全な Rust ラッパー。
//!
//! 外部コマンド起動を `std::process::Command`（内部 fork+exec）から
//! `posix_spawnp`（macOS で 2-5 倍高速）に置き換える。
//!
//! ## 構成
//!
//! | 型 | 役割 |
//! |-----|------|
//! | [`SpawnAttr`] | `posix_spawnattr_t` の RAII ラッパー（プロセスグループ、シグナル設定） |
//! | [`FileActions`] | `posix_spawn_file_actions_t` の RAII ラッパー（fd 操作） |
//! | [`CStringVec`] | argv/envp 用の NULL 終端ポインタ配列 |
//! | [`spawn`] | 上記を組み合わせて `posix_spawnp` を呼ぶ公開関数 |

use std::ffi::CString;
use std::fmt;

// ── エラー型 ──────────────────────────────────────────────────────

/// `posix_spawnp` の失敗を表すエラー。
pub struct SpawnError {
    /// errno 値。
    pub errno: i32,
    /// コマンド名（エラーメッセージ用）。
    pub command: String,
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self.errno {
            libc::ENOENT => "command not found",
            libc::EACCES => "permission denied",
            _ => "spawn failed",
        };
        write!(f, "rush: {}: {}", self.command, msg)
    }
}

impl SpawnError {
    /// エラーに対応する終了ステータスを返す。
    /// 127 = command not found, 126 = permission denied, 1 = その他。
    pub fn exit_status(&self) -> i32 {
        match self.errno {
            libc::ENOENT => 127,
            libc::EACCES => 126,
            _ => 1,
        }
    }
}

// ── SpawnAttr ─────────────────────────────────────────────────────

/// `posix_spawnattr_t` の RAII ラッパー。Drop で自動 destroy。
struct SpawnAttr {
    inner: libc::posix_spawnattr_t,
}

impl SpawnAttr {
    /// `posix_spawnattr_init` で初期化する。
    fn new() -> Self {
        unsafe {
            let mut attr: libc::posix_spawnattr_t = std::mem::zeroed();
            libc::posix_spawnattr_init(&mut attr);
            Self { inner: attr }
        }
    }

    /// プロセスグループを設定する。
    ///
    /// `POSIX_SPAWN_SETPGROUP` フラグを立て、子プロセスのプロセスグループを `pgid` に設定する。
    /// `pgid == 0` の場合、子の PID がグループリーダーになる。
    fn set_pgroup(&mut self, pgid: libc::pid_t) {
        unsafe {
            let mut flags: libc::c_short = 0;
            libc::posix_spawnattr_getflags(&self.inner, &mut flags);
            flags |= libc::POSIX_SPAWN_SETPGROUP as libc::c_short;
            libc::posix_spawnattr_setflags(&mut self.inner, flags);
            libc::posix_spawnattr_setpgroup(&mut self.inner, pgid);
        }
    }

    /// シグナルをデフォルトにリセットする。
    ///
    /// `POSIX_SPAWN_SETSIGDEF` フラグを立て、指定シグナルを `SIG_DFL` にリセットする。
    /// シェルが無視している SIGINT, SIGTSTP, SIGTTOU, SIGTTIN を子で復元するために使う。
    fn set_sigdefault(&mut self) {
        unsafe {
            let mut flags: libc::c_short = 0;
            libc::posix_spawnattr_getflags(&self.inner, &mut flags);
            flags |= libc::POSIX_SPAWN_SETSIGDEF as libc::c_short;
            libc::posix_spawnattr_setflags(&mut self.inner, flags);

            let mut sigset: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut sigset);
            libc::sigaddset(&mut sigset, libc::SIGINT);
            libc::sigaddset(&mut sigset, libc::SIGTSTP);
            libc::sigaddset(&mut sigset, libc::SIGTTOU);
            libc::sigaddset(&mut sigset, libc::SIGTTIN);
            libc::posix_spawnattr_setsigdefault(&mut self.inner, &sigset);
        }
    }

    fn as_ptr(&self) -> *const libc::posix_spawnattr_t {
        &self.inner
    }
}

impl Drop for SpawnAttr {
    fn drop(&mut self) {
        unsafe {
            libc::posix_spawnattr_destroy(&mut self.inner);
        }
    }
}

// ── FileActions ───────────────────────────────────────────────────

/// `posix_spawn_file_actions_t` の RAII ラッパー。Drop で自動 destroy。
struct FileActions {
    inner: libc::posix_spawn_file_actions_t,
}

impl FileActions {
    /// `posix_spawn_file_actions_init` で初期化する。
    fn new() -> Self {
        unsafe {
            let mut actions: libc::posix_spawn_file_actions_t = std::mem::zeroed();
            libc::posix_spawn_file_actions_init(&mut actions);
            Self { inner: actions }
        }
    }

    /// `dup2(fd, newfd)` アクションを追加する。パイプ接続・リダイレクト用。
    fn add_dup2(&mut self, fd: i32, newfd: i32) {
        unsafe {
            libc::posix_spawn_file_actions_adddup2(&mut self.inner, fd, newfd);
        }
    }

    /// `close(fd)` アクションを追加する。不要な fd のクローズ用。
    fn add_close(&mut self, fd: i32) {
        unsafe {
            libc::posix_spawn_file_actions_addclose(&mut self.inner, fd);
        }
    }

    fn as_ptr(&self) -> *const libc::posix_spawn_file_actions_t {
        &self.inner
    }
}

impl Drop for FileActions {
    fn drop(&mut self) {
        unsafe {
            libc::posix_spawn_file_actions_destroy(&mut self.inner);
        }
    }
}

// ── CStringVec ────────────────────────────────────────────────────

/// argv/envp 用の CString ベクタ。NULL 終端のポインタ配列を構築する。
struct CStringVec {
    _strings: Vec<CString>,
    ptrs: Vec<*mut libc::c_char>,
}

impl CStringVec {
    /// 引数リストから構築する。各要素を `CString` に変換し、NULL 終端ポインタ配列を作る。
    fn from_args(args: &[&str]) -> Self {
        let strings: Vec<CString> = args
            .iter()
            .map(|s| CString::new(*s).unwrap_or_else(|_| CString::new("").unwrap()))
            .collect();
        let mut ptrs: Vec<*mut libc::c_char> = strings
            .iter()
            .map(|s| s.as_ptr() as *mut libc::c_char)
            .collect();
        ptrs.push(std::ptr::null_mut()); // NULL 終端
        Self {
            _strings: strings,
            ptrs,
        }
    }

    /// NULL 終端ポインタ配列を返す。
    fn as_ptr(&self) -> *const *mut libc::c_char {
        self.ptrs.as_ptr()
    }
}

// ── spawn 関数 ────────────────────────────────────────────────────

/// `posix_spawnp` で子プロセスを起動する。成功時は子 PID を返す。
///
/// - `args`: コマンドと引数（`args[0]` がコマンド名、PATH 検索付き）
/// - `pgid`: プロセスグループ ID（0 なら子 PID をリーダーにする）
/// - `stdin_fd`: stdin に接続する fd（`None` なら継承）
/// - `stdout_fd`: stdout に接続する fd（`None` なら継承）
/// - `stderr_fd`: stderr に接続する fd（`None` なら継承）
/// - `fds_to_close`: 子プロセスで閉じる fd のリスト（パイプの未使用端など）
/// - `extra_dup2s`: 追加の fd 複製リスト（`2>&1` 等）。各タプル `(src_fd, dst_fd)` で `dup2(dst, src)` を実行
pub fn spawn(
    args: &[&str],
    pgid: libc::pid_t,
    stdin_fd: Option<i32>,
    stdout_fd: Option<i32>,
    stderr_fd: Option<i32>,
    fds_to_close: &[i32],
    extra_dup2s: &[(i32, i32)],
) -> Result<libc::pid_t, SpawnError> {
    let argv = CStringVec::from_args(args);

    // 属性: プロセスグループ + シグナルリセット
    let mut attr = SpawnAttr::new();
    attr.set_pgroup(pgid);
    attr.set_sigdefault();

    // ファイルアクション: fd のリダイレクト + クローズ
    let mut actions = FileActions::new();

    if let Some(fd) = stdin_fd {
        actions.add_dup2(fd, libc::STDIN_FILENO);
        if fd != libc::STDIN_FILENO {
            actions.add_close(fd);
        }
    }
    if let Some(fd) = stdout_fd {
        actions.add_dup2(fd, libc::STDOUT_FILENO);
        if fd != libc::STDOUT_FILENO {
            actions.add_close(fd);
        }
    }
    if let Some(fd) = stderr_fd {
        actions.add_dup2(fd, libc::STDERR_FILENO);
        if fd != libc::STDERR_FILENO {
            actions.add_close(fd);
        }
    }

    // fd 複製: 2>&1 等の処理。dup2(dst, src) で src が dst のコピーを指す。
    for &(src, dst) in extra_dup2s {
        actions.add_dup2(dst, src);
    }

    for &fd in fds_to_close {
        // dup2 で既に close 済みの fd を再 close しないようチェック
        let already_closed = [stdin_fd, stdout_fd, stderr_fd]
            .iter()
            .any(|&redir_fd| redir_fd == Some(fd));
        if !already_closed {
            actions.add_close(fd);
        }
    }

    // environ を継承（std::env::set_var で設定済みの環境がそのまま渡る）
    extern "C" {
        static environ: *const *mut libc::c_char;
    }

    let mut pid: libc::pid_t = 0;

    let ret = unsafe {
        libc::posix_spawnp(
            &mut pid,
            argv.as_ptr().read() as *const libc::c_char,
            actions.as_ptr(),
            attr.as_ptr(),
            argv.as_ptr(),
            environ as *const *mut libc::c_char,
        )
    };

    if ret != 0 {
        return Err(SpawnError {
            errno: ret,
            command: args[0].to_string(),
        });
    }

    Ok(pid)
}
