//! ジョブテーブルとジョブ制御ヘルパー。
//!
//! フォアグラウンド待機 ([`wait_for_fg`])、バックグラウンド reap ([`reap_jobs`])、
//! 完了通知 ([`notify_and_clean`])、ターミナル制御 ([`give_terminal_to`] / [`take_terminal_back`])
//! を提供する。executor と builtins の両方から利用し、循環依存を回避する。

use libc::pid_t;

// ── データ構造 ───────────────────────────────────────────────────────

/// ジョブ内の個別プロセス。パイプライン中の各コマンドに対応する。
pub struct JobProcess {
    /// プロセス ID。
    pub pid: pid_t,
    /// 正常終了またはシグナルで終了した場合に `true`。
    pub completed: bool,
    /// SIGTSTP 等で停止中の場合に `true`。
    pub stopped: bool,
    /// `waitpid` が返した raw status（`WIFEXITED` / `WIFSIGNALED` 等で解釈）。
    pub status: i32,
}

/// ジョブの集約状態。個別プロセスの状態から導出される。
#[derive(Debug, PartialEq)]
pub enum JobStatus {
    /// 少なくとも1つのプロセスが実行中で、停止プロセスがない。
    Running,
    /// 少なくとも1つのプロセスが停止中（SIGTSTP 等）。
    Stopped,
    /// 全プロセスが完了。引数は最終コマンドの終了ステータス。
    Done(i32),
}

/// ジョブ。パイプラインのプロセスグループに対応する。
///
/// バックグラウンド実行（`&`）または Ctrl+Z による停止でジョブテーブルに登録される。
/// `jobs` / `fg` / `bg` ビルトインからジョブ ID で参照される。
pub struct Job {
    /// `[N]` 形式で表示されるジョブ番号。最小未使用 ID が割り当てられる。
    pub id: usize,
    /// プロセスグループ ID。`kill(-pgid, sig)` や `waitpid(-pgid, ...)` で使用。
    pub pgid: pid_t,
    /// 表示用コマンド文字列（ユーザ入力から `&` を除いたもの）。
    pub command: String,
    /// パイプライン内の各プロセス。
    pub processes: Vec<JobProcess>,
    /// Done 通知をユーザに表示済みかどうか。`true` なら次回の [`JobTable::remove_done`] で削除。
    pub notified: bool,
}

impl Job {
    /// ジョブの現在のステータスを返す。
    ///
    /// 判定優先度: Stopped > Done > Running。
    /// - いずれかのプロセスが停止中 → [`JobStatus::Stopped`]
    /// - 全プロセスが完了 → [`JobStatus::Done`]（最終コマンドのステータスを使用）
    /// - それ以外 → [`JobStatus::Running`]
    pub fn status(&self) -> JobStatus {
        // 停止しているプロセスがあれば Stopped
        if self.processes.iter().any(|p| p.stopped) {
            return JobStatus::Stopped;
        }
        // 全プロセスが完了していれば Done
        if self.processes.iter().all(|p| p.completed) {
            // 最後のプロセスの終了ステータスを使用
            let last = self.processes.last().unwrap();
            let exit_code = if libc::WIFEXITED(last.status) {
                libc::WEXITSTATUS(last.status)
            } else if libc::WIFSIGNALED(last.status) {
                128 + libc::WTERMSIG(last.status)
            } else {
                1
            };
            return JobStatus::Done(exit_code);
        }
        JobStatus::Running
    }

    /// ジョブのステータス表示文字列を返す。
    fn status_str(&self) -> &'static str {
        match self.status() {
            JobStatus::Running => "Running",
            JobStatus::Stopped => "Stopped",
            JobStatus::Done(_) => "Done",
        }
    }
}

// ── JobTable ─────────────────────────────────────────────────────────

/// ジョブテーブル。ジョブの追加・検索・状態更新・削除を管理する。
///
/// [`Shell`](crate::shell::Shell) が所有し、executor と builtins の両方からアクセスされる。
pub struct JobTable {
    jobs: Vec<Job>,
    next_id: usize,
}

impl JobTable {
    pub fn new() -> Self {
        Self {
            jobs: Vec::new(),
            next_id: 1,
        }
    }

    /// ジョブを追加し、割り当てた ID を返す。最小未使用 ID を再利用する。
    pub fn insert(&mut self, pgid: pid_t, cmd: String, pids: Vec<pid_t>) -> usize {
        // 最小未使用 ID を探す
        let mut id = 1;
        loop {
            if !self.jobs.iter().any(|j| j.id == id) {
                break;
            }
            id += 1;
        }

        let processes = pids
            .into_iter()
            .map(|pid| JobProcess {
                pid,
                completed: false,
                stopped: false,
                status: 0,
            })
            .collect();

        self.jobs.push(Job {
            id,
            pgid,
            command: cmd,
            processes,
            notified: false,
        });

        if id >= self.next_id {
            self.next_id = id + 1;
        }
        id
    }

    /// ID でジョブを検索する。
    pub fn get(&self, id: usize) -> Option<&Job> {
        self.jobs.iter().find(|j| j.id == id)
    }

    /// ID でジョブを検索する（可変参照）。
    pub fn get_mut(&mut self, id: usize) -> Option<&mut Job> {
        self.jobs.iter_mut().find(|j| j.id == id)
    }

    /// 最新の非 Done ジョブの ID を返す。
    pub fn current_job_id(&self) -> Option<usize> {
        self.jobs
            .iter()
            .rev()
            .find(|j| !matches!(j.status(), JobStatus::Done(_)))
            .map(|j| j.id)
    }

    /// `waitpid` の結果でプロセスの状態を更新する。
    ///
    /// `WIFSTOPPED` なら停止、それ以外（正常終了・シグナル終了）なら完了としてマークする。
    /// 該当 PID がテーブルに存在しない場合は何もしない。
    pub fn mark_pid(&mut self, pid: pid_t, raw_status: i32) {
        for job in &mut self.jobs {
            for proc in &mut job.processes {
                if proc.pid == pid {
                    proc.status = raw_status;
                    if libc::WIFSTOPPED(raw_status) {
                        proc.stopped = true;
                        proc.completed = false;
                    } else {
                        proc.completed = true;
                        proc.stopped = false;
                    }
                    return;
                }
            }
        }
    }

    /// 通知済み Done ジョブを削除する。
    pub fn remove_done(&mut self) {
        self.jobs.retain(|j| {
            !(j.notified && matches!(j.status(), JobStatus::Done(_)))
        });
    }

    /// 全ジョブのイテレータ。
    pub fn iter(&self) -> impl Iterator<Item = &Job> {
        self.jobs.iter()
    }
}

// ── 待機ヘルパー ─────────────────────────────────────────────────────

/// フォアグラウンドジョブを待機する。
///
/// `waitpid(-pgid, WUNTRACED)` をループし、プロセスグループ内の全プロセスが
/// 完了または停止するまでブロックする。
///
/// ジョブテーブルに登録済みのジョブ（パイプライン）は [`mark_pid`](JobTable::mark_pid) で
/// 各プロセスの状態を更新し、全体の完了/停止を判定する。
/// フォアグラウンド単発コマンドはジョブテーブルに未登録のため、
/// `waitpid` の `raw_status` から `WIFEXITED`/`WIFSIGNALED`/`WIFSTOPPED` で
/// 直接終了ステータスを抽出する。
///
/// 戻り値: `(終了ステータス, 停止したか)`。
/// 停止時のステータスは 148（128 + SIGTSTP）。
pub fn wait_for_fg(jobs: &mut JobTable, pgid: pid_t) -> (i32, bool) {
    let mut last_raw_status: i32 = 0;
    loop {
        let mut raw_status: i32 = 0;
        let pid = unsafe { libc::waitpid(-pgid, &mut raw_status, libc::WUNTRACED) };

        if pid <= 0 {
            break;
        }

        last_raw_status = raw_status;
        jobs.mark_pid(pid, raw_status);

        // ジョブの全プロセスの状態を確認
        let job = jobs.iter().find(|j| j.pgid == pgid);
        if let Some(job) = job {
            match job.status() {
                JobStatus::Done(code) => return (code, false),
                JobStatus::Stopped => return (148, true), // 128 + SIGTSTP(20) = 148
                JobStatus::Running => continue,
            }
        } else {
            // フォアグラウンドジョブはジョブテーブルに登録されていないため、
            // raw_status から直接ステータスを抽出する
            if libc::WIFSTOPPED(raw_status) {
                return (148, true);
            }
            if libc::WIFEXITED(raw_status) {
                return (libc::WEXITSTATUS(raw_status), false);
            }
            if libc::WIFSIGNALED(raw_status) {
                return (128 + libc::WTERMSIG(raw_status), false);
            }
            break;
        }
    }

    // waitpid が即座に返った場合（プロセスが既に終了済み）
    if libc::WIFEXITED(last_raw_status) {
        return (libc::WEXITSTATUS(last_raw_status), false);
    }
    if libc::WIFSIGNALED(last_raw_status) {
        return (128 + libc::WTERMSIG(last_raw_status), false);
    }

    (0, false)
}

/// 非ブロッキングでバックグラウンドジョブを reap する。
///
/// `waitpid(-1, WNOHANG | WUNTRACED)` を reap できるプロセスがなくなるまで繰り返し、
/// 各プロセスの状態をジョブテーブルに反映する。プロンプト表示前と `execute()` 冒頭で呼ばれる。
pub fn reap_jobs(jobs: &mut JobTable) {
    loop {
        let mut raw_status: i32 = 0;
        let pid = unsafe {
            libc::waitpid(-1, &mut raw_status, libc::WNOHANG | libc::WUNTRACED)
        };

        if pid <= 0 {
            break;
        }

        jobs.mark_pid(pid, raw_status);
    }
}

/// Done ジョブの通知を stderr に出力し、テーブルから削除する。
///
/// `[N]   Done   command` 形式で表示後、`notified` フラグを立てて [`JobTable::remove_done`] で削除。
/// プロンプト表示前に呼ばれ、bash と同様のタイミングでユーザに完了を通知する。
pub fn notify_and_clean(jobs: &mut JobTable) {
    for job in jobs.iter() {
        if matches!(job.status(), JobStatus::Done(_)) && !job.notified {
            eprintln!("[{}]   {}   {}", job.id, job.status_str(), job.command);
        }
    }
    // notified フラグを立ててから削除
    for job in &mut jobs.jobs {
        if matches!(job.status(), JobStatus::Done(_)) {
            job.notified = true;
        }
    }
    jobs.remove_done();
}

// ── ターミナル制御ヘルパー ───────────────────────────────────────────

/// `tcsetpgrp` でターミナルのフォアグラウンドプロセスグループを `pgid` に設定する。
///
/// フォアグラウンドジョブの実行前、および `fg` ビルトインから呼ばれる。
/// シェルが SIGTTOU を無視しているため、バックグラウンドからの呼び出しでもブロックしない。
pub fn give_terminal_to(terminal_fd: i32, pgid: pid_t) {
    unsafe {
        libc::tcsetpgrp(terminal_fd, pgid);
    }
}

/// `tcsetpgrp` でターミナルのフォアグラウンドプロセスグループをシェルに戻す。
///
/// フォアグラウンドジョブの完了後・停止後に呼ばれ、シェルがターミナル入力を再び受け取れるようにする。
pub fn take_terminal_back(terminal_fd: i32, shell_pgid: pid_t) {
    unsafe {
        libc::tcsetpgrp(terminal_fd, shell_pgid);
    }
}
