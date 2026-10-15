//! 引用、分支、标签与 HEAD 状态。

use crate::oid::Oid;

/// 引用的类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RefKind {
    /// `refs/heads/*` 本地分支。
    LocalBranch,
    /// `refs/remotes/*` 远程跟踪分支。
    RemoteBranch,
    /// `refs/tags/*` 标签（轻量与附注统一表示，指向剥离后的提交）。
    Tag,
    /// 其他引用（如 `refs/stash`、`refs/notes/*`）。
    Other,
}

impl RefKind {
    /// 用于列表分组的默认排序权重：本地分支 → 远程分支 → 标签 → 其他。
    #[must_use]
    pub const fn group_order(self) -> u8 {
        match self {
            Self::LocalBranch => 0,
            Self::RemoteBranch => 1,
            Self::Tag => 2,
            Self::Other => 3,
        }
    }

    /// 中文分组名，供 UI 直接展示。
    #[must_use]
    pub const fn group_label(self) -> &'static str {
        match self {
            Self::LocalBranch => "本地分支",
            Self::RemoteBranch => "远程分支",
            Self::Tag => "标签",
            Self::Other => "其他引用",
        }
    }
}

/// 一条引用的展示信息。
///
/// `ahead` / `behind` 仅在存在上游跟踪分支时才有意义（对应
/// `git rev-list --left-right --count <upstream>...<branch>`），
/// 无上游时两者均为 0。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefInfo {
    /// 短名：本地分支为 `main`，远程分支为 `origin/main`，标签为标签名。
    pub name: String,
    /// 引用类别。
    pub kind: RefKind,
    /// 指向的提交。附注标签已被剥离到提交对象。
    pub target: Oid,
    /// 是否为当前 HEAD 指向的分支。
    pub is_head: bool,
    /// 上游跟踪引用名（仅本地分支可能有）。
    pub upstream: Option<String>,
    /// 领先上游的提交数。
    pub ahead: u32,
    /// 落后上游的提交数。
    pub behind: u32,
}

impl RefInfo {
    /// 是否配置了上游。
    #[must_use]
    pub fn has_upstream(&self) -> bool {
        self.upstream.is_some()
    }

    /// 是否存在未同步的提交（ahead 或 behind 非零），供 UI 高亮。
    #[must_use]
    pub fn is_diverged(&self) -> bool {
        self.ahead > 0 || self.behind > 0
    }

    /// `↑a ↓b` 形式的同步摘要；无上游时返回 [`None`]。
    #[must_use]
    pub fn sync_summary(&self) -> Option<String> {
        self.upstream.as_ref()?;
        Some(format!("↑{} ↓{}", self.ahead, self.behind))
    }
}

/// HEAD 的状态类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadKind {
    /// 指向某个分支。
    Branch,
    /// 分离头指针（detached HEAD）。
    Detached,
    /// 尚未有提交（空仓库 / unborn branch）。
    Unborn,
}

impl HeadKind {
    /// 是否允许直接提交（unborn 需要允许创建首个提交，因此也返回 `true`）。
    ///
    /// 分离头指针下提交会产生"游离提交"，需要 UI 给出警告（FR-CMT-03）。
    #[must_use]
    pub const fn needs_detached_warning(self) -> bool {
        matches!(self, Self::Detached)
    }

    /// 中文描述，供 UI 直接展示。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Branch => "分支",
            Self::Detached => "分离头指针",
            Self::Unborn => "空仓库",
        }
    }
}

/// HEAD 解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadInfo {
    /// 状态类别。
    pub kind: HeadKind,
    /// 目标提交；[`HeadKind::Unborn`] 时为 [`None`]。
    pub target: Option<Oid>,
    /// 分支短名；[`HeadKind::Detached`] 与 [`HeadKind::Unborn`] 时可能仍为 [`None`]。
    ///
    /// 空仓库的 unborn 分支（如刚 `git init` 的 `main`）也会填写名称。
    pub branch: Option<String>,
}

impl HeadInfo {
    /// 窗口标题用的 `仓库名 (分支名)` 里的分支片段。
    #[must_use]
    pub fn display_branch(&self) -> &str {
        self.branch.as_deref().unwrap_or(match self.kind {
            HeadKind::Detached => "(分离)",
            HeadKind::Unborn | HeadKind::Branch => "(未知)",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{HeadInfo, HeadKind, RefInfo, RefKind};
    use crate::oid::Oid;

    fn oid(seed: u8) -> Oid {
        let mut bytes = [0u8; crate::OID_LEN];
        bytes[0] = seed;
        Oid::from_bytes(&bytes).expect("长度正确")
    }

    fn branch(name: &str, ahead: u32, behind: u32, upstream: Option<&str>) -> RefInfo {
        RefInfo {
            name: name.to_owned(),
            kind: RefKind::LocalBranch,
            target: oid(1),
            is_head: true,
            upstream: upstream.map(str::to_owned),
            ahead,
            behind,
        }
    }

    #[test]
    fn sync_summary_requires_upstream() {
        assert_eq!(
            branch("main", 2, 3, Some("origin/main")).sync_summary(),
            Some("↑2 ↓3".to_owned())
        );
        assert_eq!(branch("topic", 2, 3, None).sync_summary(), None);
        assert!(!branch("topic", 0, 0, None).is_diverged());
        assert!(branch("main", 1, 0, Some("origin/main")).is_diverged());
    }

    #[test]
    fn ref_kind_group_order_is_stable() {
        assert!(RefKind::LocalBranch.group_order() < RefKind::RemoteBranch.group_order());
        assert!(RefKind::RemoteBranch.group_order() < RefKind::Tag.group_order());
        assert_eq!(RefKind::Tag.group_label(), "标签");
    }

    #[test]
    fn detached_head_needs_warning() {
        assert!(HeadKind::Detached.needs_detached_warning());
        assert!(!HeadKind::Branch.needs_detached_warning());
        assert!(!HeadKind::Unborn.needs_detached_warning());
    }

    #[test]
    fn head_display_branch_falls_back_to_kind() {
        let detached = HeadInfo {
            kind: HeadKind::Detached,
            target: Some(oid(9)),
            branch: None,
        };
        assert_eq!(detached.display_branch(), "(分离)");

        let on_branch = HeadInfo {
            kind: HeadKind::Branch,
            target: Some(oid(9)),
            branch: Some("main".to_owned()),
        };
        assert_eq!(on_branch.display_branch(), "main");

        let unborn = HeadInfo {
            kind: HeadKind::Unborn,
            target: None,
            branch: Some("main".to_owned()),
        };
        assert_eq!(unborn.display_branch(), "main");
    }
}
