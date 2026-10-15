//! 命令审计记录（FR-CON-01 的底座）。
//!
//! 每一次底层操作 —— 无论走 CLI 子进程还是进程内 `gix` —— 都产出
//! 一条 [`CommandRecord`]，由 `gt-service` 的环形缓冲收集并投递到 Console 面板。
//!
//! 对 `gix` 路径记录的是**等价的语义命令**（例如 `git status --porcelain`），
//! 并标注"（内置引擎）"，使用户既能复现操作、又不会被误导为真的起了子进程。

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// 操作实际执行的内核路由。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommandRoute {
    /// 进程内纯 Rust 内核。
    Gix,
    /// libgit2 绑定。
    Git2,
    /// git 可执行文件子进程。
    Cli,
}

impl CommandRoute {
    /// 是否产生了子进程。
    #[must_use]
    pub const fn spawns_process(self) -> bool {
        matches!(self, Self::Cli)
    }

    /// Console 中展示的路由标签。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Gix => "内置引擎",
            Self::Git2 => "内置引擎(git2)",
            Self::Cli => "git 子进程",
        }
    }
}

/// 捕获的输出上限。超出即截断并标注，避免大仓库命令把内存吃光
/// （架构规划 3.3："禁止一次性读入完整 stdout"）。
pub const MAX_CAPTURE_BYTES: usize = 4 * 1024 * 1024;

/// 一条操作审计记录。
#[derive(Debug, Clone)]
pub struct CommandRecord {
    /// 执行路由。
    pub route: CommandRoute,
    /// 实际执行的程序（仅 CLI 路由）。
    pub program: Option<String>,
    /// 实际参数（仅 CLI 路由）。
    pub args: Vec<String>,
    /// 等价语义命令，或 CLI 路由下的完整命令行。
    pub command_line: String,
    /// 工作目录。
    pub cwd: Option<PathBuf>,
    /// 退出码；进程内路由为 [`None`]。
    pub exit_code: Option<i32>,
    /// 耗时。
    pub duration: Duration,
    /// 标准输出的尾部（截断后）。
    pub stdout_tail: Option<String>,
    /// 标准错误的尾部（截断后）。
    pub stderr_tail: Option<String>,
    /// 输出是否因超过 [`MAX_CAPTURE_BYTES`] 被截断。
    ///
    /// Console 必须能区分"命令没有输出"与"输出被我们截掉了"，
    /// 否则用户会把后者当成前者去排查。
    pub truncated: bool,
    /// 失败原因（成功时为 [`None`]）。
    pub error: Option<String>,
}

impl CommandRecord {
    /// 构造一条进程内路由记录。
    #[must_use]
    pub fn internal(
        route: CommandRoute,
        cwd: Option<PathBuf>,
        command_line: impl Into<String>,
        duration: Duration,
    ) -> Self {
        Self {
            route,
            program: None,
            args: Vec::new(),
            command_line: command_line.into(),
            cwd,
            exit_code: None,
            duration,
            stdout_tail: None,
            stderr_tail: None,
            truncated: false,
            error: None,
        }
    }

    /// 操作是否成功。
    ///
    /// 进程内路由以"是否记录了错误"为准；CLI 路由以退出码为准。
    #[must_use]
    pub fn is_success(&self) -> bool {
        match self.exit_code {
            Some(code) => code == 0 && self.error.is_none(),
            None => self.error.is_none(),
        }
    }

    /// 耗时毫秒数（四舍五入）。
    #[must_use]
    pub fn duration_ms(&self) -> u64 {
        u64::try_from(self.duration.as_millis()).unwrap_or(u64::MAX)
    }

    /// Console 面板中展示的单行文本。
    #[must_use]
    pub fn summary_line(&self) -> String {
        let status = match (self.is_success(), self.exit_code) {
            (true, Some(code)) => format!("exit {code}"),
            (false, Some(code)) => format!("exit {code}（失败）"),
            (true, None) => "ok".to_owned(),
            (false, None) => "失败".to_owned(),
        };
        let suffix = if self.truncated {
            "（输出已截断）"
        } else {
            ""
        };
        match &self.cwd {
            Some(cwd) => format!(
                "[{}] {}  ·  {status}  ·  {}ms{suffix}  ·  {}",
                self.route.label(),
                self.command_line,
                self.duration_ms(),
                display_path(cwd)
            ),
            None => format!(
                "[{}] {}  ·  {status}  ·  {}ms{suffix}",
                self.route.label(),
                self.command_line,
                self.duration_ms()
            ),
        }
    }

    /// 可直接粘贴到终端复现的命令行；进程内路由返回其语义等价形式并加注释。
    #[must_use]
    pub fn reproducible_line(&self) -> String {
        match self.route {
            CommandRoute::Cli => {
                let mut line = self.program.clone().unwrap_or_else(|| "git".to_owned());
                for arg in &self.args {
                    line.push(' ');
                    line.push_str(&quote_if_needed(arg));
                }
                line
            }
            CommandRoute::Gix | CommandRoute::Git2 => {
                format!("# {}（由内置引擎执行）", self.command_line)
            }
        }
    }
}

/// 参数含空白时补上引号，保证复制出来的命令行可直接执行。
fn quote_if_needed(arg: &str) -> String {
    if arg.contains([' ', '\t', '"']) {
        format!("\"{}\"", arg.replace('"', "\\\""))
    } else {
        arg.to_owned()
    }
}

/// 审计回调。
///
/// 实现必须轻量且不阻塞：它在工作线程上被同步调用，用于把记录推入
/// `gt-service` 的环形缓冲。实现方负责内部同步与限长。
pub trait CommandObserver: Send + Sync {
    /// 观察一条记录。
    fn observe(&self, record: &CommandRecord);
}

/// 丢弃全部记录的观察者（默认，用于测试与无 Console 场景）。
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopObserver;

impl CommandObserver for NoopObserver {
    fn observe(&self, _record: &CommandRecord) {}
}

/// 可共享的观察者句柄。
pub type SharedObserver = Arc<dyn CommandObserver>;

/// 构造一个不做任何事的共享观察者。
#[must_use]
pub fn noop_observer() -> SharedObserver {
    Arc::new(NoopObserver)
}

/// 路径转显示字符串（用于 cwd 记录）。
pub(crate) fn display_path(path: &Path) -> String {
    path.display().to_string()
}

impl fmt::Display for CommandRoute {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

#[cfg(test)]
mod tests {
    use super::{CommandRecord, CommandRoute, MAX_CAPTURE_BYTES};
    use std::path::PathBuf;
    use std::time::Duration;

    #[test]
    fn route_labels_and_process_flag() {
        assert_eq!(CommandRoute::Gix.label(), "内置引擎");
        assert_eq!(CommandRoute::Git2.label(), "内置引擎(git2)");
        assert_eq!(CommandRoute::Cli.label(), "git 子进程");
        assert!(CommandRoute::Cli.spawns_process());
        assert!(!CommandRoute::Gix.spawns_process());
        assert_eq!(MAX_CAPTURE_BYTES, 4 * 1024 * 1024);
    }

    #[test]
    fn internal_record_has_no_exit_code() {
        let record = CommandRecord::internal(
            CommandRoute::Gix,
            Some(PathBuf::from("C:/repo")),
            "git status --porcelain",
            Duration::from_millis(12),
        );
        assert!(record.is_success());
        assert_eq!(record.exit_code, None);
        assert_eq!(record.duration_ms(), 12);
        assert_eq!(
            record.summary_line(),
            "[内置引擎] git status --porcelain  ·  ok  ·  12ms  ·  C:/repo"
        );
        assert_eq!(
            record.reproducible_line(),
            "# git status --porcelain（由内置引擎执行）"
        );
    }

    #[test]
    fn cli_record_reports_exit_code_and_reproducible_line() {
        let mut record = CommandRecord::internal(
            CommandRoute::Cli,
            None,
            "git rev-parse HEAD",
            Duration::from_millis(0),
        );
        record.program = Some("git".to_owned());
        record.args = vec![
            "-C".to_owned(),
            "C:/my repo".to_owned(),
            "rev-parse".to_owned(),
        ];
        record.exit_code = Some(0);
        assert!(record.is_success());
        assert_eq!(
            record.reproducible_line(),
            "git -C \"C:/my repo\" rev-parse",
            "含空格的参数必须加引号才能直接复现"
        );

        record.exit_code = Some(128);
        assert!(!record.is_success());
        assert!(record.summary_line().contains("失败"));
    }

    #[test]
    fn error_marks_record_as_failure_even_without_exit_code() {
        let mut record =
            CommandRecord::internal(CommandRoute::Gix, None, "git log", Duration::from_millis(1));
        record.error = Some("对象损坏".to_owned());
        assert!(!record.is_success());
    }
}
