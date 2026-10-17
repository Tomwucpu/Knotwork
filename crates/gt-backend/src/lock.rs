//! 仓库写锁（FR-MR-04）：标记"本仓库正被某个实例写入"。
//!
//! # 为什么需要它
//!
//! 两个窗口同时写同一个仓库，最容易出现的不是"数据损坏"，而是**互相覆盖**：
//! A 暂存了文件、B 提交了别的索引快照、A 再提交 —— 用户看到的提交内容与他
//! 点击时的不一致。git 自身的 `.git/index.lock` 只覆盖单次命令，覆盖不了
//! "一个窗口持续持有仓库"这个更长的区间。
//!
//! # 语义边界（必须诚实说明）
//!
//! 本锁是**协作锁**：它约束的是"本工具的多个实例"，不是 git 本身。用户完全
//! 可以在终端里同时操作同一个仓库 —— 那种情况靠 `.git/index.lock` 与操作后的
//! 状态刷新兜底，而不是靠这把锁。
//!
//! # 判定规则
//!
//! - 锁文件存在且记录的 PID **存活** → 只读模式，界面给出占用者 PID；
//! - 锁文件存在但 PID 已死（上次崩溃留下的残锁）→ 视为过期，直接接管；
//! - 锁文件损坏（读不出 PID）→ 同样视为过期，接管但记录告警。

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::{BackendError, Result};

/// 锁文件名（随 [`crate::work::LOCK_FILE_NAME`] 保持一致）。
pub const LOCK_FILE_NAME: &str = "knotwork.lock";

/// 写锁获取结果。
#[derive(Debug)]
pub enum LockAttempt {
    /// 已获得写锁；丢弃 [`WriteLock`] 即释放。
    Acquired(WriteLock),
    /// 被其他存活实例持有（只读模式的原因）。
    Held {
        /// 占用者 PID（解析失败时为 [`None`]）。
        pid: Option<u32>,
    },
}

impl LockAttempt {
    /// 是否读写可用。
    #[must_use]
    pub const fn is_writable(&self) -> bool {
        matches!(self, Self::Acquired(_))
    }

    /// 只读原因（面向用户的说明）；可写时为 [`None`]。
    #[must_use]
    pub fn read_only_reason(&self) -> Option<String> {
        match self {
            Self::Acquired(_) => None,
            Self::Held { pid } => Some(match pid {
                Some(pid) => format!("仓库正被其他实例写入（PID {pid}），当前为只读模式"),
                None => "仓库正被其他实例写入，当前为只读模式".to_owned(),
            }),
        }
    }
}

/// 持有的写锁；`Drop` 时释放。
///
/// 只删除"仍属于自己"的锁文件：若期间锁被判定过期并被另一个实例接管，
/// 直接删除会把对方的锁误删（用户会看到"两个实例都可写"）。
#[derive(Debug)]
pub struct WriteLock {
    path: PathBuf,
    token: String,
    released: bool,
}

impl WriteLock {
    /// 尝试获取写锁。
    ///
    /// # Errors
    /// 锁目录不可写（权限、磁盘只读）时返回 [`BackendError::Io`]。
    pub fn acquire(git_dir: &Path) -> Result<LockAttempt> {
        let path = git_dir.join(LOCK_FILE_NAME);

        for _ in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    let token = make_token();
                    let body = format!("pid={}\ntoken={}\n", std::process::id(), token);
                    file.write_all(body.as_bytes())
                        .map_err(|err| BackendError::Io {
                            path: path.clone(),
                            reason: err.to_string(),
                        })?;
                    return Ok(LockAttempt::Acquired(Self {
                        path,
                        token,
                        released: false,
                    }));
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    let existing = std::fs::read_to_string(&path).unwrap_or_default();
                    let pid = parse_pid(&existing);
                    match pid {
                        Some(pid) if pid != std::process::id() && process_alive(pid) => {
                            return Ok(LockAttempt::Held { pid: Some(pid) });
                        }
                        // 自己的 PID 出现在锁里（同进程重复打开同一仓库）：
                        // 这不是"被别的实例占用"，但我们也不该抢自己的锁。
                        Some(pid) if pid == std::process::id() => {
                            return Ok(LockAttempt::Held { pid: Some(pid) });
                        }
                        _ => {
                            tracing::warn!(
                                path = %path.display(),
                                "检测到过期写锁（占用进程已不存在），接管"
                            );
                            let _ = std::fs::remove_file(&path);
                            // 下一轮循环重新 create_new；被别人抢先则如实报告。
                        }
                    }
                }
                Err(err) => {
                    return Err(BackendError::Io {
                        path: path.clone(),
                        reason: err.to_string(),
                    })
                }
            }
        }

        // 两轮都没抢到：说明锁在被别人反复刷新，按"被占用"处理。
        let pid = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| parse_pid(&text));
        Ok(LockAttempt::Held { pid })
    }

    /// 锁文件路径。
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 显式释放（与 `Drop` 等价，便于在关闭窗口时立刻释放而不等结构体析构）。
    pub fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        if let Ok(text) = std::fs::read_to_string(&self.path) {
            if !text.contains(&self.token) {
                tracing::debug!("锁文件已不属于本实例，跳过删除");
                return;
            }
        }
        if let Err(err) = std::fs::remove_file(&self.path) {
            tracing::debug!(%err, path = %self.path.display(), "删除写锁失败");
        }
    }
}

impl Drop for WriteLock {
    fn drop(&mut self) {
        self.release();
    }
}

/// 从锁文件内容解析 PID。
fn parse_pid(text: &str) -> Option<u32> {
    text.lines()
        .find_map(|line| line.trim().strip_prefix("pid="))
        .and_then(|value| value.trim().parse().ok())
}

/// 生成一个难撞车的令牌（PID + 时间戳纳秒 + 计数）。
fn make_token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!(
        "{}-{nanos}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// 判断进程是否存活。
///
/// 不用 `libc`/`unsafe`（工作区禁止 `unsafe`），因此走各自平台的命令行工具：
/// Windows 用 `tasklist`，Unix 用 `kill -0`。两者都是系统自带。
fn process_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        let output = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output();
        let Ok(output) = output else {
            // 无法判定时保守地认为"存活"：宁可只读，也不要两个实例同时写。
            return true;
        };
        let text = String::from_utf8_lossy(&output.stdout);
        let needle = format!("\"{pid}\"");
        text.contains(&needle) || text.contains(&format!(",{pid},"))
    }
    #[cfg(not(windows))]
    {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(true)
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_pid, process_alive, LockAttempt, WriteLock};
    use std::path::PathBuf;

    fn temp_git_dir(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("knotwork-lock-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建目录");
        dir
    }

    #[test]
    fn acquiring_creates_and_releasing_removes_the_file() {
        let dir = temp_git_dir("basic");
        let attempt = WriteLock::acquire(&dir).expect("获取锁");
        assert!(attempt.is_writable());
        assert_eq!(attempt.read_only_reason(), None);

        let LockAttempt::Acquired(lock) = attempt else {
            panic!("应获得写锁");
        };
        assert!(lock.path().exists(), "锁文件应已创建");
        let path = lock.path().to_path_buf();
        drop(lock);
        assert!(!path.exists(), "释放后锁文件应被删除");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_live_owner_blocks_a_second_acquire() {
        let dir = temp_git_dir("held");
        let first = WriteLock::acquire(&dir).expect("获取锁");
        assert!(first.is_writable());
        // 同一进程的第二次获取视为被占用（不抢自己的锁）。
        let second = WriteLock::acquire(&dir).expect("第二次获取");
        assert!(!second.is_writable());
        let reason = second.read_only_reason().expect("必须给出原因");
        assert!(reason.contains("只读"), "{reason}");

        drop(first);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stale_lock_is_taken_over() {
        let dir = temp_git_dir("stale");
        // 写一个不可能存活的 PID（进程号上限远小于该值）。
        std::fs::write(dir.join(super::LOCK_FILE_NAME), "pid=4294967290\ntoken=x\n")
            .expect("写残锁");
        let attempt = WriteLock::acquire(&dir).expect("接管残锁");
        assert!(attempt.is_writable(), "死亡进程的锁必须被视为过期");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn acquiring_twice_in_a_row_works_after_release() {
        let dir = temp_git_dir("reuse");
        {
            let attempt = WriteLock::acquire(&dir).expect("第一次");
            assert!(attempt.is_writable());
        }
        let attempt = WriteLock::acquire(&dir).expect("第二次");
        assert!(attempt.is_writable(), "释放后应能再次获取");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pid_parsing_is_total() {
        assert_eq!(parse_pid("pid=1234\ntoken=abc"), Some(1234));
        assert_eq!(parse_pid("garbage"), None);
        assert_eq!(parse_pid(""), None);
    }

    #[test]
    fn the_current_process_is_alive() {
        assert!(process_alive(std::process::id()), "自己必须被判为存活");
    }
}
