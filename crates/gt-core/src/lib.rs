//! `gt-core` —— 领域模型层。
//!
//! 承载 `Commit` / `Ref` / `Change` / `RepoId` 等领域类型，**不依赖任何 I/O 或 Git 内核**。
//! 保持零第三方依赖是刻意约束：该 crate 被所有其他 crate 引用，任何传递依赖都会
//! 放大到整个工作区的编译成本（架构规划 4.4 节）。
//!
//! # 模块划分
//!
//! | 模块 | 内容 |
//! |---|---|
//! | [`oid`] | 对象标识 [`Oid`]（SHA-1，20 字节定长） |
//! | [`commit`] | 提交元数据 [`CommitMeta`] 与图输入 [`LogNode`] |
//! | [`refs`] | 引用/分支/标签与 HEAD 状态 |
//! | [`change`] | 树差异条目 [`FileChange`] |
//! | [`status`] | 工作区状态条目与 porcelain XY 映射 |
//! | [`state`] | 仓库进行中操作状态（merge/rebase 等，FR-STA-02） |
//! | [`query`] | Log 筛选/排序条件与分页模型 |
//! | [`commit_check`] | 提交前检查（空消息 / detached / 进行中，FR-CMT-03） |
//!
//! 设计约定：本 crate 中的类型**不持有仓库句柄、不做 I/O**；字符串一律为拥有型
//! （`String`），以便跨线程传递。热路径上的字符串驻留与索引化（架构规划 5.8）
//! 属于 `gt-service` / `gt-ui` 的职责，不在本层实现。

pub mod change;
pub mod commit;
pub mod commit_check;
pub mod oid;
pub mod query;
pub mod refs;
pub mod state;
pub mod status;

pub use change::{ChangeKind, FileChange};
pub use commit::{CommitMeta, LogNode};
pub use commit_check::{check_commit, CommitCheck, CommitCheckInput};
pub use oid::{Oid, ParseOidError, OID_LEN};
pub use query::{
    BranchScope, GraphOptions, LogFilter, LogNodePage, LogPageRequest, LogSort, DEFAULT_PAGE_SIZE,
    MAX_PAGE_SIZE, MIN_PAGE_SIZE,
};
pub use refs::{HeadInfo, HeadKind, RefInfo, RefKind};
pub use state::{MarkerPresence, RepoState, ResumableActions};
pub use status::{
    quote_path, ChangeSet, ConflictKind, PartitionedChanges, PorcelainFormat, StatusBucket,
    StatusEntry, StatusFlag,
};

use std::fmt;

/// 仓库标识：进程内唯一的仓库句柄。
///
/// 采用 `u32` 内部编号而非 `String` 路径，避免在热路径（提交图节点、diff 缓存键）
/// 中反复比较字符串 —— 对应架构规划 5.8 的"字符串驻留"约束。
///
/// 编号由 `gt-service` 的仓库注册表分配，保证同一路径在一次运行内得到同一个 `RepoId`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RepoId(u32);

impl RepoId {
    /// 由仓库注册表分配的编号构造。
    #[must_use]
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// 返回底层编号，用于索引缓存分区与日志输出。
    #[must_use]
    pub const fn as_raw(self) -> u32 {
        self.0
    }
}

impl fmt::Display for RepoId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "repo#{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::RepoId;

    #[test]
    fn repo_id_round_trips_raw_value() {
        let id = RepoId::from_raw(7);
        assert_eq!(id.as_raw(), 7);
        assert_eq!(id.to_string(), "repo#7");
    }
}
