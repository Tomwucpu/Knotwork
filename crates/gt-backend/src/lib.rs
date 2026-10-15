//! `gt-backend` —— Git 内核抽象层。
//!
//! 对上暴露统一的 Git 能力接口，对下按操作类型路由到三种实现：
//!
//! - [`BackendRoute::Gix`]：进程内纯 Rust 读热路径（packfile 走 mmap，不产生进程）；
//! - [`BackendRoute::Git2`]：libgit2 绑定，承担 gix 尚未覆盖的写操作；
//! - [`BackendRoute::Cli`]：git 可执行文件子进程，作为语义完整性的兜底与真相标准。
//!
//! 路由表见架构规划 3.2 节。该 crate 独立存在的核心理由是：内核路由是本项目最大的
//! 技术风险，独立后可在不启动 GUI 的前提下对真实仓库执行
//! "gix 结果 vs git CLI 结果"的对比测试（架构规划 4.4、7.1）。
//!
//! 本 crate 在阶段 0 仅完成骨架，读路径实现按实施计划 M1 任务 1-2 落地。

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

/// 一次 Git 操作的执行上下文。
///
/// 阶段 0 仅含最小字段；M1 将扩展为"请求代次 + 取消令牌 + 仓库句柄"的组合，
/// 以支撑架构规划 4.3 的非阻塞请求生命周期。
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
    }
}
