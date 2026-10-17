//! `gt-service` —— 服务调度层。
//!
//! 负责把 UI 线程的请求转成后台工作，并把结果安全地送回 UI 线程。职责边界：
//!
//! - 有界工作线程池（线程数 = `min(4, CPU 核数)`，架构规划 3.3 / 4.1）；
//! - 单调递增的**请求代次号**与**取消令牌**（架构规划 4.3）；
//! - 文件变更监听与去抖（200–300ms 合并窗口 + `.git` 内部路径过滤，FR-STA-03）；
//! - 全局共享的硬上限 LRU 缓存（默认 32MB 预算，架构规划 5.4）；
//! - 命令审计环形缓冲（FR-CON-01）。
//!
//! 铁律：**UI 线程上不允许出现任何文件 I/O、网络或进程启动**（架构规划 4.2）。
//! 本 crate 的所有公开入口都遵循"入队即返回"：要么立刻拿到结果，要么立刻
//! 拿到一个错误（队列满 / 池已关闭），绝不阻塞。
//!
//! # 模块分工
//!
//! | 模块 | 职责 |
//! |---|---|
//! | [`cancel`] | 取消令牌（"还要不要继续算"） |
//! | [`generation`] | 请求代次闸门（"算出来的还算不算数"） |
//! | [`pool`] | 有界工作线程池 + 崩溃隔离 |
//! | [`cache`] | 硬上限分段 LRU |
//! | [`console`] | 命令审计环形缓冲与导出 |
//! | [`watch`] | 文件变更分类（纯函数）与去抖监听 |
//! | [`task`] | 请求 / 更新的消息模型 |
//! | [`service`] | 仓库注册表、请求派发、只读降级 |

pub mod cache;
pub mod cancel;
pub mod console;
pub mod generation;
pub mod pool;
pub mod service;
pub mod task;
pub mod watch;

pub use cache::{
    CacheBudget, CacheSegment, CacheStats, InsertOutcome, LruCache, BLOB_TEXT_PERCENT,
    COMMIT_GRAPH_PERCENT, DEFAULT_TOTAL_MIB, DIFF_HUNKS_PERCENT,
};
pub use cancel::{CancellationToken, Cancelled};
pub use console::{ConsoleBuffer, DEFAULT_CONSOLE_CAPACITY};
pub use generation::{Generation, RequestGate};
pub use pool::{
    default_thread_count, Job, SubmitError, WorkerPool, DEFAULT_QUEUE_CAPACITY, MAX_THREADS,
};
pub use service::{
    default_branch_scope, default_debounce, first_log_page, silent_observer, Capabilities,
    ChangeImpact, GitService, RepoHandle, MAX_BLOB_BYTES,
};
pub use task::{
    Delivery, DiffSide, LogPageResult, Outcome, RepoOverview, RequestError, RequestKind,
    RequestLane, SearchRequest, SearchResult, SelectionMode, WriteRequest,
};
pub use watch::{
    merge_events, ChangeBatch, ChangeSink, Relevance, RepoPaths, RepoWatcher, WatchError,
    DEBOUNCE_MAX, DEBOUNCE_MIN, DEFAULT_DEBOUNCE,
};
