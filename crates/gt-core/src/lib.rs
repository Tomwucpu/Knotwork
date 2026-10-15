//! `gt-core` —— 领域模型层。
//!
//! 承载 `Commit` / `Ref` / `Change` / `RepoId` 等领域类型，**不依赖任何 I/O 或 Git 内核**。
//! 保持零第三方依赖是刻意约束：该 crate 被所有其他 crate 引用，任何传递依赖都会
//! 放大到整个工作区的编译成本。
//!
//! - 设计约束见《轻量级Git工具-技术选型与架构规划.md》4.4 节。
//! - 完整领域模型（Commit / Ref / Change）按《轻量级Git工具-实施计划.md》M1 任务 1-1 落地。

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
