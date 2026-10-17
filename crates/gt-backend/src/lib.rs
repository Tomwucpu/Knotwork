//! `gt-backend` —— Git 内核抽象层。
//!
//! 对上暴露统一的 Git 能力接口，对下按操作类型路由到三种实现：
//!
//! - [`GitRepo`]：进程内 `gix` 读热路径（packfile 走 mmap，不产生进程）；
//! - `git2`：libgit2 绑定，承担 `gix` 尚未覆盖的写操作（M2 引入）；
//! - [`GitCli`]：git 可执行文件子进程，作为语义完整性的兜底与真相标准。
//!
//! 路由表见架构规划 3.2 节。该 crate 独立存在的核心理由是：内核路由是本项目最大的
//! 技术风险，独立后可在不启动 GUI 的前提下对真实仓库执行
//! "gix 结果 vs git CLI 结果"的对比测试（架构规划 4.4、7.1）——
//! `tests/consistency.rs` 即该测试套件。
//!
//! # M1 覆盖范围
//!
//! | 能力 | 实现 | 需求 |
//! |---|---|---|
//! | 打开 / 发现仓库（含 worktree） | [`GitRepo::open`] | FR-MR-01 |
//! | HEAD 与引用列表 | [`GitRepo::head`]、[`GitRepo::refs`] | FR-LOG-02 |
//! | revwalk 与提交元数据 | [`GitRepo::walk_ids`]、[`GitRepo::commit_metas`] | FR-LOG-01 |
//! | 工作区状态（porcelain 口径） | [`GitRepo::status`] | FR-LC-01 |
//! | 树差异与 blob 读取 | [`GitRepo::tree_changes`]、[`GitRepo::blob_at`] | FR-DIF-01/03 |
//! | 进行中操作探测 | [`GitRepo::repo_state`] | FR-STA-02 |
//! | git 可执行文件探测与执行 | [`GitCli`] | FR-STA-04、FR-RMT-05 |
//! | 命令审计记录 | [`CommandRecord`]、[`CommandObserver`] | FR-CON-01 |
//!
//! # M2 新增
//!
//! | 能力 | 实现 | 需求 |
//! |---|---|---|
//! | 暂存 / 反暂存 / 丢弃 / 删除未跟踪 | [`RepoWriter`] | FR-LC-02 |
//! | hunk 与行级暂存（`git apply --cached`） | [`RepoWriter::apply_patch_cached`] | FR-LC-03、FR-DIF-05 |
//! | 提交与 amend（hooks 由 CLI 执行） | [`RepoWriter::commit`] | FR-CMT-01/02/03 |
//! | 分支增删改 / 检出 / 上游 | [`RepoWriter::branch`] | FR-BR-01/02 |
//! | 忽略规则写入 | [`RepoWriter::append_ignore`] | FR-LC-04 |
//! | 提交消息搜索 | [`RepoWriter::search_commits`] | FR-LOG-03 |
//! | 仓库写锁（只读降级） | [`WriteLock`]、[`LockAttempt`] | FR-MR-04 |

pub mod cli;
pub mod error;
pub mod lock;
pub mod record;
pub mod repo;
pub mod work;

pub use cli::{CancelProbe, CliOutput, GitCli, LOCAL_TIMEOUT, NETWORK_TIMEOUT};
pub use error::{BackendError, Result};
pub use lock::{LockAttempt, WriteLock};
pub use record::{
    noop_observer, CommandObserver, CommandRecord, CommandRoute, NoopObserver, SharedObserver,
    MAX_CAPTURE_BYTES,
};
pub use repo::{
    collapse_linear, topo_order, BlobPair, GitRepo, GraphNode, RepoLocation, SharedGitRepo,
    TOPO_MAX_NODES,
};
pub use work::{
    BranchOp, CommitRequest, IgnoreTarget, RepoWriter, UpstreamRow, WriteOutcome, LOCK_FILE_NAME,
    SEARCH_TIMEOUT,
};

use gt_core::RepoId;

/// Git 能力的内核路由目标。
///
/// 标记为 `#[non_exhaustive]`：后续若引入第四条路由（例如针对超大仓库的
/// 预计算索引），不需要破坏下游匹配。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BackendRoute {
    /// 纯 Rust 内核，进程内执行，用于读密集热路径。
    Gix,
    /// libgit2 绑定，用于 gix 尚未覆盖的写操作。
    Git2,
    /// git 可执行文件子进程，语义完整性的兜底与真相标准。
    Cli,
}

impl BackendRoute {
    /// 转换为审计记录使用的路由标签。
    #[must_use]
    pub const fn as_command_route(self) -> CommandRoute {
        match self {
            Self::Gix => CommandRoute::Gix,
            Self::Git2 => CommandRoute::Git2,
            Self::Cli => CommandRoute::Cli,
        }
    }
}

/// 一次 Git 操作的执行上下文。
///
/// M2 起扩展为"请求代次 + 取消令牌 + 仓库句柄"的组合，以支撑架构规划 4.3
/// 的非阻塞请求生命周期；M1 只承载仓库与路由。
#[derive(Debug, Clone, Copy)]
pub struct RequestContext {
    /// 目标仓库。
    pub repo: RepoId,
    /// 本次操作选定的内核路由。
    pub route: BackendRoute,
}

#[cfg(test)]
mod tests {
    use super::{BackendRoute, RequestContext};
    use gt_core::RepoId;

    #[test]
    fn request_context_keeps_route() {
        let ctx = RequestContext {
            repo: RepoId::from_raw(1),
            route: BackendRoute::Cli,
        };
        assert_eq!(ctx.route, BackendRoute::Cli);
        assert_eq!(ctx.route.as_command_route().label(), "git 子进程");
    }
}
