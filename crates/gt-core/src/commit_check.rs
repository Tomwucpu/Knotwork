//! 提交前检查（FR-CMT-03）。
//!
//! 纯函数：输入当前状态快照，输出"阻塞项"与"警告项"。
//! 之所以不放后端：这些规则全部来自界面已知的状态（HEAD、进行中的操作、
//! 提交消息），没有任何 I/O；放在 `gt-core` 既能被单测覆盖，也能被界面
//! 在用户输入时实时调用（不需要往返工作线程）。
//!
//! # 检查项与依据
//!
//! | 项 | 级别 | 依据 |
//! |---|---|---|
//! | 提交消息为空 | **阻塞** | git 本身也会拒绝，提前阻止可省一次子进程 |
//! | 分离头指针 | 警告 | 提交会产生游离提交（FR-CMT-03 明示） |
//! | merge / rebase / cherry-pick 进行中 | 警告 | 此时的提交语义与平时不同（FR-STA-02） |
//! | 修订（amend）但没有暂存内容 | 警告 | 用户很可能以为"改了文件就算修订" |
//!
//! CRLF 警告（FR-CMT-03 的第三项）**未在本阶段实现**：判定需要感知
//! `.gitattributes` 与 `core.autocrlf` 的换行规范化结果，用"文件里有没有 `\r`"
//! 这种启发式会产生大量误报（Windows 上几乎所有文件都有）。理由与替代路径
//! 记录在 M2 验收报告，与 FR-CON-01 的滤镜支持一并处理。

use crate::refs::{HeadInfo, HeadKind};
use crate::state::RepoState;

/// 检查输入。
#[derive(Debug, Clone, Copy)]
pub struct CommitCheckInput<'a> {
    /// 提交消息原文。
    pub message: &'a str,
    /// 当前 HEAD。
    pub head: &'a HeadInfo,
    /// 进行中的操作状态。
    pub state: RepoState,
    /// 是否为修订（amend）。
    pub amend: bool,
    /// 暂存区是否为空（由调用方依据状态快照判断）。
    pub staged_empty: bool,
    /// 是否启用"分离头指针告警"设置。
    pub warn_detached: bool,
}

/// 检查结论。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommitCheck {
    /// 阻塞原因；[`Some`] 时不允许提交。
    pub blocking: Option<String>,
    /// 警告（不阻止提交，但要在界面上说清楚）。
    pub warnings: Vec<String>,
}

impl CommitCheck {
    /// 是否允许提交。
    #[must_use]
    pub fn is_allowed(&self) -> bool {
        self.blocking.is_none()
    }

    /// 是否存在警告。
    #[must_use]
    pub fn has_warnings(&self) -> bool {
        !self.warnings.is_empty()
    }

    /// 面向界面的整段提示（空字符串表示无需提示）。
    #[must_use]
    pub fn summary(&self) -> String {
        match &self.blocking {
            Some(reason) => format!("无法提交：{reason}"),
            None if self.warnings.is_empty() => String::new(),
            None => self.warnings.join("；"),
        }
    }
}

/// 执行检查。
#[must_use]
pub fn check_commit(input: &CommitCheckInput<'_>) -> CommitCheck {
    let mut check = CommitCheck::default();

    // 不做"遇到阻塞项就提前返回"：一次把所有问题列全，用户只需要看一遍。
    if input.message.trim().is_empty() {
        check.blocking = Some("提交消息不能为空".to_owned());
    }

    if input.warn_detached && input.head.kind == HeadKind::Detached {
        check.warnings.push(
            "当前处于分离头指针状态：提交后不会属于任何分支，请确认是否要新建分支".to_owned(),
        );
    }

    if input.state.is_in_progress() {
        check.warnings.push(format!(
            "「{}」尚未结束：此时的提交会推进该操作，请确认",
            input.state.label()
        ));
    }

    if input.amend && input.staged_empty {
        check
            .warnings
            .push("暂存区为空：本次修订只会改写提交消息".to_owned());
    }

    check
}

#[cfg(test)]
mod tests {
    use super::{check_commit, CommitCheckInput};
    use crate::oid::Oid;
    use crate::refs::{HeadInfo, HeadKind};
    use crate::state::RepoState;

    fn oid(seed: u8) -> Oid {
        Oid::from_bytes(&[seed; crate::OID_LEN]).expect("Oid")
    }

    fn head(kind: HeadKind) -> HeadInfo {
        HeadInfo {
            kind,
            target: Some(oid(1)),
            branch: match kind {
                HeadKind::Branch => Some("main".to_owned()),
                _ => None,
            },
        }
    }

    fn input<'a>(message: &'a str, head: &'a HeadInfo) -> CommitCheckInput<'a> {
        CommitCheckInput {
            message,
            head,
            state: RepoState::Clean,
            amend: false,
            staged_empty: false,
            warn_detached: true,
        }
    }

    #[test]
    fn empty_message_blocks_the_commit() {
        let head = head(HeadKind::Branch);
        let check = check_commit(&input("   \n\t", &head));
        assert!(!check.is_allowed());
        assert!(check.summary().contains("不能为空"));
    }

    #[test]
    fn a_plain_commit_has_no_noise() {
        let head = head(HeadKind::Branch);
        let check = check_commit(&input("feat: 正常提交", &head));
        assert!(check.is_allowed());
        assert!(!check.has_warnings(), "{check:?}");
        assert!(check.summary().is_empty());
    }

    #[test]
    fn detached_head_warns_unless_disabled() {
        let detached = head(HeadKind::Detached);
        let check = check_commit(&input("fix: 游离提交", &detached));
        assert!(check.is_allowed());
        assert!(check.summary().contains("分离头指针"), "{check:?}");

        let quiet = check_commit(&CommitCheckInput {
            warn_detached: false,
            ..input("fix: 游离提交", &detached)
        });
        assert!(!quiet.has_warnings());
    }

    #[test]
    fn in_progress_operations_are_warned() {
        let head = head(HeadKind::Branch);
        for state in [
            RepoState::Merge,
            RepoState::RebaseMerge,
            RepoState::CherryPick,
        ] {
            let check = check_commit(&CommitCheckInput {
                state,
                ..input("chore: 处理冲突", &head)
            });
            assert!(
                check.summary().contains(state.label()),
                "应提示 {} 进行中：{check:?}",
                state.label()
            );
        }
    }

    #[test]
    fn amending_without_staged_changes_warns() {
        let head = head(HeadKind::Branch);
        let check = check_commit(&CommitCheckInput {
            amend: true,
            staged_empty: true,
            ..input("chore: 改消息", &head)
        });
        assert!(check.summary().contains("只会改写提交消息"), "{check:?}");
    }

    #[test]
    fn blocking_takes_precedence_over_warnings() {
        let detached = head(HeadKind::Detached);
        let check = check_commit(&CommitCheckInput {
            state: RepoState::Merge,
            ..input("", &detached)
        });
        assert!(!check.is_allowed());
        assert!(check.has_warnings());
        assert!(check.summary().starts_with("无法提交："));
    }
}
