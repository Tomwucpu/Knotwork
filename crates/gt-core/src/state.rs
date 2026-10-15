//! 仓库"进行中操作"状态（FR-STA-02）。
//!
//! Git 把未完成的 merge / rebase / cherry-pick 等状态记录为 `.git` 下的标记文件。
//! 本模块把这些标记抽象为纯数据判定（无 I/O），以便单元测试穷举组合；
//! 实际的标记文件探测由 `gt-backend` 完成。

/// 未完成操作对应的恢复命令集合（FR-STA-02 的"明确恢复路径"）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumableActions {
    /// 继续命令（`git <continue_command>`）。
    pub continue_command: Option<&'static str>,
    /// 跳过当前提交命令。
    pub skip_command: Option<&'static str>,
    /// 中止并回到操作前状态。
    pub abort_command: Option<&'static str>,
}

impl ResumableActions {
    /// 无进行中操作时的空集合。
    pub const NONE: Self = Self {
        continue_command: None,
        skip_command: None,
        abort_command: None,
    };

    /// 是否提供了任何可用动作。
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.continue_command.is_none()
            && self.skip_command.is_none()
            && self.abort_command.is_none()
    }
}

/// 各标记文件的存在性。
///
/// 刻意使用独立布尔字段而非位集合：字段名即 `.git` 下的文件名，
/// 与 `gt-backend` 的探测代码一一对应，可读性优先。
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MarkerPresence {
    /// `.git/MERGE_HEAD`。
    pub merge_head: bool,
    /// `.git/rebase-merge/` 目录（交互式 / `--merge` 变基）。
    pub rebase_merge: bool,
    /// `.git/rebase-apply/` 目录（`am` / `--apply` 变基）。
    pub rebase_apply: bool,
    /// `.git/CHERRY_PICK_HEAD`。
    pub cherry_pick_head: bool,
    /// `.git/REVERT_HEAD`。
    pub revert_head: bool,
    /// `.git/BISECT_LOG`。
    pub bisect_log: bool,
}

/// 仓库当前所处的操作状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RepoState {
    /// 正常状态，无进行中操作。
    Clean,
    /// 合并进行中。
    Merge,
    /// 变基进行中（`rebase-merge` 目录）。
    RebaseMerge,
    /// 补丁应用 / `am` 式变基进行中（`rebase-apply` 目录）。
    RebaseApply,
    /// 拣选进行中。
    CherryPick,
    /// 回滚进行中。
    Revert,
    /// 二分查找进行中。
    Bisect,
}

impl RepoState {
    /// 是否存在未完成的操作。
    #[must_use]
    pub const fn is_in_progress(self) -> bool {
        !matches!(self, Self::Clean)
    }

    /// 是否属于变基族（continue/skip/abort 三键语义，FR-MRG-02）。
    #[must_use]
    pub const fn is_rebase(self) -> bool {
        matches!(self, Self::RebaseMerge | Self::RebaseApply)
    }

    /// 对应的 `.git` 标记路径（相对 `.git` 目录），用于"如何恢复"的提示文案。
    #[must_use]
    pub const fn marker_path(self) -> Option<&'static str> {
        match self {
            Self::Clean => None,
            Self::Merge => Some("MERGE_HEAD"),
            Self::RebaseMerge => Some("rebase-merge/"),
            Self::RebaseApply => Some("rebase-apply/"),
            Self::CherryPick => Some("CHERRY_PICK_HEAD"),
            Self::Revert => Some("REVERT_HEAD"),
            Self::Bisect => Some("BISECT_LOG"),
        }
    }

    /// 中文描述，供中断横幅直接展示。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Clean => "正常",
            Self::Merge => "合并进行中",
            Self::RebaseMerge => "变基进行中",
            Self::RebaseApply => "补丁应用（am）进行中",
            Self::CherryPick => "拣选进行中",
            Self::Revert => "回滚进行中",
            Self::Bisect => "二分查找进行中",
        }
    }

    /// 该状态下可用的继续 / 跳过 / 中止命令（`git <verb>` 的 verb）。
    #[must_use]
    pub const fn resumable_actions(self) -> ResumableActions {
        match self {
            Self::Clean => ResumableActions::NONE,
            Self::Merge => ResumableActions {
                continue_command: Some("commit"),
                skip_command: None,
                abort_command: Some("merge --abort"),
            },
            Self::RebaseMerge | Self::RebaseApply => ResumableActions {
                continue_command: Some("rebase --continue"),
                skip_command: Some("rebase --skip"),
                abort_command: Some("rebase --abort"),
            },
            Self::CherryPick => ResumableActions {
                continue_command: Some("cherry-pick --continue"),
                skip_command: Some("cherry-pick --skip"),
                abort_command: Some("cherry-pick --abort"),
            },
            Self::Revert => ResumableActions {
                continue_command: Some("revert --continue"),
                skip_command: Some("revert --skip"),
                abort_command: Some("revert --abort"),
            },
            Self::Bisect => ResumableActions {
                continue_command: Some("bisect reset"),
                skip_command: None,
                abort_command: Some("bisect reset"),
            },
        }
    }

    /// 依据标记文件存在性判定状态。
    ///
    /// 优先级对齐 `git` 自身的行为：变基族先于 merge 判定（变基期间也会产生
    /// `MERGE_HEAD`，此时应报告为变基而不是 merge）。
    #[must_use]
    pub const fn from_markers(markers: MarkerPresence) -> Self {
        if markers.rebase_apply {
            return Self::RebaseApply;
        }
        if markers.rebase_merge {
            return Self::RebaseMerge;
        }
        if markers.merge_head {
            return Self::Merge;
        }
        if markers.cherry_pick_head {
            return Self::CherryPick;
        }
        if markers.revert_head {
            return Self::Revert;
        }
        if markers.bisect_log {
            return Self::Bisect;
        }
        Self::Clean
    }

    /// 是否需要在启动时弹出中断横幅（FR-STA-02）。
    #[must_use]
    pub const fn needs_banner(self) -> bool {
        self.is_in_progress()
    }
}

#[cfg(test)]
mod tests {
    use super::{MarkerPresence, RepoState};

    fn only(field: fn(&mut MarkerPresence)) -> MarkerPresence {
        let mut markers = MarkerPresence::default();
        field(&mut markers);
        markers
    }

    #[test]
    fn no_markers_means_clean() {
        assert_eq!(
            RepoState::from_markers(MarkerPresence::default()),
            RepoState::Clean
        );
        assert!(!RepoState::Clean.is_in_progress());
        assert!(!RepoState::Clean.needs_banner());
        assert_eq!(RepoState::Clean.marker_path(), None);
    }

    #[test]
    fn single_marker_maps_to_state() {
        assert_eq!(
            RepoState::from_markers(only(|m| m.merge_head = true)),
            RepoState::Merge
        );
        assert_eq!(
            RepoState::from_markers(only(|m| m.cherry_pick_head = true)),
            RepoState::CherryPick
        );
        assert_eq!(
            RepoState::from_markers(only(|m| m.revert_head = true)),
            RepoState::Revert
        );
        assert_eq!(
            RepoState::from_markers(only(|m| m.bisect_log = true)),
            RepoState::Bisect
        );
    }

    #[test]
    fn rebase_wins_over_merge_head() {
        // 变基期间 MERGE_HEAD 也会出现；必须优先报告为变基。
        let markers = MarkerPresence {
            merge_head: true,
            rebase_merge: true,
            ..MarkerPresence::default()
        };
        assert_eq!(RepoState::from_markers(markers), RepoState::RebaseMerge);
    }

    #[test]
    fn rebase_apply_wins_over_rebase_merge() {
        let markers = MarkerPresence {
            rebase_merge: true,
            rebase_apply: true,
            ..MarkerPresence::default()
        };
        assert_eq!(RepoState::from_markers(markers), RepoState::RebaseApply);
    }

    #[test]
    fn rebase_family_exposes_three_verbs() {
        for state in [RepoState::RebaseMerge, RepoState::RebaseApply] {
            assert!(state.is_rebase());
            let actions = state.resumable_actions();
            assert_eq!(actions.continue_command, Some("rebase --continue"));
            assert_eq!(actions.skip_command, Some("rebase --skip"));
            assert_eq!(actions.abort_command, Some("rebase --abort"));
        }
        assert!(!RepoState::Merge.is_rebase());
        assert!(RepoState::Clean.resumable_actions().is_empty());
    }

    #[test]
    fn in_progress_states_need_banner() {
        for state in [
            RepoState::Merge,
            RepoState::RebaseMerge,
            RepoState::RebaseApply,
            RepoState::CherryPick,
            RepoState::Revert,
            RepoState::Bisect,
        ] {
            assert!(state.needs_banner(), "{state:?} 应触发中断横幅");
            assert!(state.marker_path().is_some());
        }
    }
}
