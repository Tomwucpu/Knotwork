//! 有界工作线程池（架构规划 3.3 / 4.1）。
//!
//! # 为什么不用现成的线程池 crate
//!
//! 需求非常简单，但约束很硬：**提交必须是非阻塞的**（UI 线程上不允许等待），
//! 队列必须有界（大仓库的 revwalk 任务能把内存吃光），工作线程必须能在
//! 单个任务 panic 后存活（崩溃隔离）。自己实现 50 行比引入依赖再包一层
//! 更好控制。
//!
//! # 关键约定
//!
//! - 线程数 = `min(4, 可用并行度)`（架构规划 3.3：子进程并发 ≤ 4）；
//! - 队列满时 [`WorkerPool::submit`] 返回 [`SubmitError::QueueFull`]，
//!   **不阻塞、不丢弃静默**——调用方（UI 线程）据此提示"操作过于频繁"或
//!   先取消上一个请求；
//! - 任务用 `catch_unwind` 包裹：某个仓库的对象库损坏导致 panic 时，
//!   只丢这一个任务的结果，池子与其他窗口不受影响（FR-STA-01 的崩溃隔离）。

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};

/// 线程数上限（架构规划 3.3：子进程并发 ≤ 4）。
pub const MAX_THREADS: usize = 4;

/// 默认队列容量。
///
/// 一次交互通常只会同时挂几个请求（滚动加载、状态刷新、diff 计算），
/// 64 已经远超正常水位；真正的意义是给"用户狂拖滚动条"设一个上界。
pub const DEFAULT_QUEUE_CAPACITY: usize = 64;

/// 提交任务失败的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SubmitError {
    /// 队列已满；调用方应稍后重试或先取消未完成的请求。
    #[error("任务队列已满（容量 {capacity}），请稍后重试")]
    QueueFull {
        /// 队列容量。
        capacity: usize,
    },
    /// 线程池已关闭（应用退出中）。
    #[error("线程池已关闭")]
    Closed,
}

/// 一个待执行的任务。
pub type Job = Box<dyn FnOnce() + Send + 'static>;

/// 有界工作线程池。
#[derive(Debug)]
pub struct WorkerPool {
    /// 关闭输入侧即可让工作线程取完队列后退出，因此用 `Option` 表达"已关闭"。
    sender: Option<Sender<Job>>,
    workers: Vec<JoinHandle<()>>,
    capacity: usize,
    pending: Arc<AtomicUsize>,
}

impl WorkerPool {
    /// 按架构约束建池：线程数 = `min(4, 可用并行度)`。
    ///
    /// # Errors
    /// 系统线程创建失败时返回错误。
    pub fn new() -> std::io::Result<Self> {
        Self::with_threads(default_thread_count(), DEFAULT_QUEUE_CAPACITY)
    }

    /// 建一个指定线程数与队列容量的池（测试与基准用）。
    ///
    /// `threads` 会被夹到 `1..=MAX_THREADS`：允许测试用单线程取得确定性，
    /// 但不允许任何调用方越过架构约束。
    ///
    /// # Errors
    /// 系统线程创建失败时返回错误。
    pub fn with_threads(threads: usize, capacity: usize) -> std::io::Result<Self> {
        let threads = threads.clamp(1, MAX_THREADS);
        let capacity = capacity.max(1);
        let (sender, receiver) = bounded::<Job>(capacity);
        let pending = Arc::new(AtomicUsize::new(0));

        let mut workers = Vec::with_capacity(threads);
        for index in 0..threads {
            let receiver: Receiver<Job> = receiver.clone();
            let pending = Arc::clone(&pending);
            let handle = std::thread::Builder::new()
                .name(format!("knotwork-worker-{index}"))
                .spawn(move || worker_loop(&receiver, &pending))?;
            workers.push(handle);
        }

        Ok(Self {
            sender: Some(sender),
            workers,
            capacity,
            pending,
        })
    }

    /// 提交任务；队列满或池已关闭时立即返回错误（**绝不阻塞**）。
    ///
    /// # Errors
    /// 队列满 → [`SubmitError::QueueFull`]；池已关闭 → [`SubmitError::Closed`]。
    pub fn submit(&self, job: Job) -> Result<(), SubmitError> {
        let Some(sender) = self.sender.as_ref() else {
            return Err(SubmitError::Closed);
        };

        // 计数在入队前加：即使任务还没被取走，"在飞任务数"也应当包含它。
        self.pending.fetch_add(1, Ordering::AcqRel);
        match sender.try_send(job) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.pending.fetch_sub(1, Ordering::AcqRel);
                Err(SubmitError::QueueFull {
                    capacity: self.capacity,
                })
            }
            Err(TrySendError::Disconnected(_)) => {
                self.pending.fetch_sub(1, Ordering::AcqRel);
                Err(SubmitError::Closed)
            }
        }
    }

    /// 工作线程数。
    #[must_use]
    pub fn thread_count(&self) -> usize {
        self.workers.len()
    }

    /// 队列容量。
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// 已提交但尚未跑完的任务数（含仍在队列中的）。
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.load(Ordering::Acquire)
    }

    /// 队列中等待执行的任务数（不含正在执行的）。
    #[must_use]
    pub fn queued(&self) -> usize {
        self.sender.as_ref().map_or(0, Sender::len)
    }

    /// 队列是否已经填满（用于诊断"UI 投递太快"）。
    #[must_use]
    pub fn is_saturated(&self) -> bool {
        self.queued() >= self.capacity
    }

    /// 关闭输入侧并等待全部工作线程收尾（幂等）。
    ///
    /// 队列中尚未执行的任务会被跑完 —— 它们的收尾写入（缓存、审计记录）
    /// 仍然有价值，硬砍掉只会让诊断信息出现空洞。
    pub fn shutdown(&mut self) {
        // 丢弃唯一的 `Sender` 即关闭通道：工作线程取完队列后 `recv` 返回
        // `Err`，循环自然结束。工作线程持有的是 `Receiver` 克隆，不是 `Sender`。
        drop(self.sender.take());

        for worker in self.workers.drain(..) {
            if worker.join().is_err() {
                tracing::error!("工作线程异常退出");
            }
        }
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// 工作线程主循环。
fn worker_loop(receiver: &Receiver<Job>, pending: &AtomicUsize) {
    while let Ok(job) = receiver.recv() {
        // 崩溃隔离：任务 panic 不能带走工作线程，否则池子会逐渐萎缩。
        let outcome = catch_unwind(AssertUnwindSafe(job));
        if outcome.is_err() {
            tracing::error!("后台任务 panic，已被隔离（仅该任务的结果丢失）");
        }
        pending.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 默认线程数：`min(4, 可用并行度)`。
#[must_use]
pub fn default_thread_count() -> usize {
    std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(MAX_THREADS)
}

#[cfg(test)]
mod tests {
    use super::{default_thread_count, SubmitError, WorkerPool, MAX_THREADS};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn thread_count_respects_the_architecture_cap() {
        assert!(default_thread_count() >= 1);
        assert!(default_thread_count() <= MAX_THREADS, "线程数不得超过 4");

        // 越界的请求被夹回合法区间，而不是照单全收。
        let pool = WorkerPool::with_threads(64, 8).expect("建池应成功");
        assert_eq!(pool.thread_count(), MAX_THREADS);
        assert_eq!(
            WorkerPool::with_threads(0, 0)
                .expect("建池应成功")
                .thread_count(),
            1,
            "线程数与队列容量都会被夹到至少 1"
        );
    }

    #[test]
    fn jobs_run_and_pending_count_drains() {
        let pool = WorkerPool::with_threads(2, 8).expect("建池应成功");
        let (sender, receiver) = mpsc::channel();

        for index in 0..4u32 {
            let sender = sender.clone();
            pool.submit(Box::new(move || {
                sender.send(index).expect("发送结果");
            }))
            .expect("提交应成功");
        }
        drop(sender);

        let mut got: Vec<u32> = receiver.into_iter().collect();
        got.sort_unstable();
        assert_eq!(got, vec![0, 1, 2, 3]);

        // `pending` 在工作线程收尾后归零；给一线程调度余量。
        for _ in 0..100 {
            if pool.pending() == 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(pool.pending(), 0, "全部任务完成后计数应归零");
    }

    #[test]
    fn saturated_queue_rejects_instead_of_blocking() {
        // 单线程 + 容量 1：先占住唯一的工作线程，再灌满队列，第三次提交必然被拒。
        let pool = WorkerPool::with_threads(1, 1).expect("建池应成功");
        let (hold, release) = mpsc::channel::<()>();
        let (started, started_rx) = mpsc::channel::<()>();

        pool.submit(Box::new(move || {
            started.send(()).expect("通知已开始");
            let _ = release.recv();
        }))
        .expect("第一个任务应入队");
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("第一个任务应开始执行");

        pool.submit(Box::new(|| {})).expect("第二个任务占满队列");
        let before = pool.pending();
        let err = pool.submit(Box::new(|| {})).expect_err("队列满时必须拒绝");
        assert_eq!(err, SubmitError::QueueFull { capacity: 1 });
        assert_eq!(
            pool.pending(),
            before,
            "被拒的任务不得计入在飞计数（此时=执行中的 1 + 排队的 1）"
        );
        assert_eq!(before, 2);

        hold.send(()).expect("放行第一个任务");
        drop(hold);
    }

    #[test]
    fn panicking_job_does_not_kill_the_worker() {
        let pool = WorkerPool::with_threads(1, 4).expect("建池应成功");
        let (sender, receiver) = mpsc::channel();

        pool.submit(Box::new(|| panic!("模拟对象库损坏导致的 panic")))
            .expect("提交应成功");
        pool.submit(Box::new(move || {
            sender.send("存活").expect("发送结果");
        }))
        .expect("提交应成功");

        assert_eq!(
            receiver
                .recv_timeout(Duration::from_secs(5))
                .expect("池应存活"),
            "存活",
            "单个任务 panic 不得让工作线程退出"
        );
        assert_eq!(pool.thread_count(), 1);
    }

    #[test]
    fn dropping_the_pool_waits_for_inflight_jobs() {
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        {
            let pool = WorkerPool::with_threads(2, 8).expect("建池应成功");
            for _ in 0..8 {
                let counter = Arc::clone(&counter);
                pool.submit(Box::new(move || {
                    std::thread::sleep(Duration::from_millis(5));
                    counter.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                }))
                .expect("提交应成功");
            }
        }
        // `Drop` 会 join 全部工作线程，因此退出作用域时任务必须已跑完。
        assert_eq!(counter.load(std::sync::atomic::Ordering::Acquire), 8);
    }
}
