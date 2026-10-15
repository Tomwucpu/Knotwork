//! Console 审计面板的数据源（FR-CON-01）。
//!
//! 把内核投递的 [`CommandRecord`] 收进一个有界环形缓冲，供 Console 面板展示与导出。
//! 缓冲**有界**是硬要求：一次大仓库操作可能产生上千条记录，无界列表会变成内存泄漏。
//!
//! 本类型实现 [`CommandObserver`]，因此可以直接作为 `gt-backend` 的审计回调注入 ——
//! 这意味着"内置引擎"与"git 子进程"两条路由的记录会自动汇总到同一处，
//! 用户看到的是完整的操作序列，而不是只有真的起了进程的那些。

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use gt_backend::{CommandObserver, CommandRecord, SharedObserver};

/// 默认环形缓冲容量。
pub const DEFAULT_CONSOLE_CAPACITY: usize = 512;

/// 环形缓冲式的命令审计收集器。
#[derive(Debug)]
pub struct ConsoleBuffer {
    capacity: usize,
    records: Mutex<VecDeque<CommandRecord>>,
    dropped: AtomicU64,
}

impl ConsoleBuffer {
    /// 新建一个容量为 `capacity` 的缓冲（至少保留 1 条）。
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            records: Mutex::new(VecDeque::new()),
            dropped: AtomicU64::new(0),
        }
    }

    /// 新建并包成 `Arc`，便于同时作为观察者与查询句柄使用。
    #[must_use]
    pub fn shared(capacity: usize) -> Arc<Self> {
        Arc::new(Self::new(capacity))
    }

    /// 把 `Arc<Self>` 转成内核需要的观察者句柄。
    #[must_use]
    pub fn observer(buffer: &Arc<Self>) -> SharedObserver {
        Arc::clone(buffer) as SharedObserver
    }

    /// 容量。
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// 当前记录数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 因超出容量而被丢弃的记录累计数。
    ///
    /// 这个数字必须能被用户看到：Console 里"缺了一段"和"什么都没发生"
    /// 是完全不同的诊断结论。
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Acquire)
    }

    /// 全部记录，按发生顺序（最早的在前）。
    #[must_use]
    pub fn snapshot(&self) -> Vec<CommandRecord> {
        self.lock().iter().cloned().collect()
    }

    /// 取最近的 `count` 条，按发生顺序返回。
    #[must_use]
    pub fn latest(&self, count: usize) -> Vec<CommandRecord> {
        let guard = self.lock();
        let skip = guard.len().saturating_sub(count);
        guard.iter().skip(skip).cloned().collect()
    }

    /// 取走并清空全部记录。
    pub fn drain(&self) -> Vec<CommandRecord> {
        self.lock().drain(..).collect()
    }

    /// 清空（同时重置被丢弃计数）。
    pub fn clear(&self) {
        self.lock().clear();
        self.dropped.store(0, Ordering::Release);
    }

    /// 导出为可粘贴的文本（FR-CON 的导出入口）。
    ///
    /// 每条记录包含摘要行、可复现命令行，失败时附带错误与标准错误尾部 ——
    /// 目的是让用户把这段文本贴进 issue 就能复现问题。
    #[must_use]
    pub fn export_text(&self) -> String {
        let guard = self.lock();
        let mut out = String::with_capacity(guard.len() * 128);
        out.push_str("# Knotwork 命令审计导出\n");
        let _ = writeln!(
            out,
            "# 共 {} 条记录，因超出容量丢弃 {} 条",
            guard.len(),
            self.dropped.load(Ordering::Acquire)
        );

        for record in guard.iter() {
            out.push_str(&record.summary_line());
            out.push('\n');
            out.push_str("    复现：");
            out.push_str(&record.reproducible_line());
            out.push('\n');
            if let Some(error) = record.error.as_deref() {
                out.push_str("    错误：");
                out.push_str(error);
                out.push('\n');
            }
            if let Some(stderr) = record
                .stderr_tail
                .as_deref()
                .filter(|tail| !tail.is_empty())
            {
                out.push_str("    标准错误：");
                out.push_str(&stderr.replace('\n', "\n              "));
                out.push('\n');
            }
            if record.truncated {
                out.push_str("    （输出过长，以上仅为尾部）\n");
            }
        }
        out
    }

    /// 取锁；被 poison 时沿用内部数据（审计信息比"锁的完整性"更重要）。
    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<CommandRecord>> {
        self.records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl CommandObserver for ConsoleBuffer {
    fn observe(&self, record: &CommandRecord) {
        let mut guard = self.lock();
        if guard.len() >= self.capacity {
            guard.pop_front();
            self.dropped.fetch_add(1, Ordering::AcqRel);
        }
        guard.push_back(record.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::{ConsoleBuffer, DEFAULT_CONSOLE_CAPACITY};
    use gt_backend::{CommandObserver, CommandRecord, CommandRoute};
    use std::path::PathBuf;
    use std::time::Duration;

    fn record(command_line: &str) -> CommandRecord {
        CommandRecord::internal(
            CommandRoute::Gix,
            Some(PathBuf::from("C:/repo")),
            command_line,
            Duration::from_millis(3),
        )
    }

    #[test]
    fn ring_buffer_keeps_the_newest_and_counts_drops() {
        let buffer = ConsoleBuffer::new(3);
        for index in 0..5 {
            buffer.observe(&record(&format!("op-{index}")));
        }

        assert_eq!(buffer.len(), 3);
        assert_eq!(buffer.dropped(), 2);
        let lines: Vec<String> = buffer
            .snapshot()
            .iter()
            .map(|record| record.command_line.clone())
            .collect();
        assert_eq!(lines, vec!["op-2", "op-3", "op-4"], "应保留最新的 3 条");
    }

    #[test]
    fn capacity_is_at_least_one_and_default_is_documented() {
        let buffer = ConsoleBuffer::new(0);
        assert_eq!(buffer.capacity(), 1);
        buffer.observe(&record("a"));
        assert_eq!(buffer.len(), 1);
        assert_eq!(DEFAULT_CONSOLE_CAPACITY, 512);
    }

    #[test]
    fn latest_drain_and_clear_behave() {
        let buffer = ConsoleBuffer::new(8);
        for index in 0..4 {
            buffer.observe(&record(&format!("op-{index}")));
        }

        let latest: Vec<String> = buffer
            .latest(2)
            .iter()
            .map(|record| record.command_line.clone())
            .collect();
        assert_eq!(latest, vec!["op-2", "op-3"]);

        assert_eq!(buffer.drain().len(), 4);
        assert!(buffer.is_empty());
        assert_eq!(buffer.dropped(), 0, "drain 后应回到干净状态");

        buffer.observe(&record("after"));
        buffer.clear();
        assert!(buffer.is_empty());
        assert_eq!(buffer.dropped(), 0);
        assert_eq!(buffer.latest(5).len(), 0);
    }

    #[test]
    fn export_includes_reproduction_and_failure_details() {
        let buffer = ConsoleBuffer::new(8);
        buffer.observe(&record("git status --porcelain"));

        let mut failed = record("git rev-parse HEAD");
        failed.route = CommandRoute::Cli;
        failed.program = Some("git".to_owned());
        failed.args = vec!["rev-parse".to_owned(), "HEAD".to_owned()];
        failed.exit_code = Some(128);
        failed.error = Some("不是有效的仓库".to_owned());
        failed.stderr_tail = Some("fatal: not a git repository".to_owned());
        buffer.observe(&failed);

        let text = buffer.export_text();
        assert!(text.starts_with("# Knotwork 命令审计导出"));
        assert!(text.contains("共 2 条记录"));
        assert!(
            text.contains("# git status --porcelain（由内置引擎执行）"),
            "内置引擎路由应给出语义等价的复现命令：\n{text}"
        );
        assert!(
            text.contains("git rev-parse HEAD"),
            "CLI 路由应给出真实命令行"
        );
        assert!(text.contains("不是有效的仓库"));
        assert!(text.contains("fatal: not a git repository"));
    }

    #[test]
    fn shared_buffer_can_serve_as_the_kernel_observer() {
        let buffer = ConsoleBuffer::shared(4);
        let observer = ConsoleBuffer::observer(&buffer);
        observer.observe(&record("via-observer"));
        // 通过 trait object 写入的记录应能从同一个 Arc 读到。
        assert_eq!(buffer.len(), 1);
        assert_eq!(buffer.snapshot()[0].command_line, "via-observer");
    }
}
