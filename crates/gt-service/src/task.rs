//! 请求与更新的消息模型（架构规划 4.3 第 1、5 点）。
//!
//! UI 线程只做两件事：投递 [`RequestKind`]，消费 [`Delivery`]。
//! 消息里**不含任何句柄**（不携带 `GitRepo`、不携带 `Arc<Vec<..>>` 之外的共享状态），
//! 因此可以在任意线程之间自由移动。

use gt_backend::RepoLocation;
use gt_core::{ChangeSet, FileChange, HeadInfo, LogNodePage, LogPageRequest, RefInfo, RepoState};

use crate::generation::Generation;

/// 一次读取请求的类别。
///
/// M1 只有只读请求；写请求在 M2 引入，届时本枚举扩容而不改变消息通道的形状。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestKind {
    /// 仓库概览：HEAD + 引用列表 + 进行中的操作状态。
    ///
    /// 合成一个请求是因为这三项总是一起用（窗口标题、分支面板、状态横幅），
    /// 分成三次请求只会多两次线程往返。
    Overview,
    /// 工作区状态（Local Changes 视图）。
    Status,
    /// 日志页。
    LogPage(LogPageRequest),
    /// 两个修订之间的文件级变更。
    TreeChanges {
        /// 旧侧修订；[`None`] 表示空树（根提交）。
        old: Option<String>,
        /// 新侧修订。
        new: String,
    },
    /// 单个文件的差异。
    FileDiff {
        /// 旧侧修订；[`None`] 表示空树或工作区侧（由 `worktree` 决定）。
        old: Option<String>,
        /// 新侧修订。
        new: String,
        /// 仓库相对路径。
        path: String,
        /// 旧、新两侧是否都取自工作区。
        worktree: bool,
    },
}

/// 请求所属的**视图通道**。
///
/// 代次闸门按通道隔离：Log 的新请求只作废旧的 Log 请求，不会把同时进行的
/// "工作区状态"请求一起废掉。少了这层隔离，一次首屏装载里连发的三个请求
/// 会互相作废，只有最后一个能落地（实测踩到过）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RequestLane {
    /// 仓库概览（HEAD / 引用 / 进行中操作）。
    Overview,
    /// 工作区状态。
    Status,
    /// 日志页。
    Log,
    /// 文件差异。
    Diff,
}

impl RequestLane {
    /// 全部通道。
    pub const ALL: [Self; 4] = [Self::Overview, Self::Status, Self::Log, Self::Diff];

    /// 诊断标签。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Overview => "概览",
            Self::Status => "状态",
            Self::Log => "日志",
            Self::Diff => "差异",
        }
    }
}

impl RequestKind {
    /// 该请求走哪条通道。
    #[must_use]
    pub const fn lane(&self) -> RequestLane {
        match self {
            Self::Overview => RequestLane::Overview,
            Self::Status => RequestLane::Status,
            Self::LogPage(_) => RequestLane::Log,
            Self::TreeChanges { .. } | Self::FileDiff { .. } => RequestLane::Diff,
        }
    }

    /// 简短标签，用于诊断日志与 Console。
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Overview => "仓库概览",
            Self::Status => "工作区状态",
            Self::LogPage(_) => "日志页",
            Self::TreeChanges { .. } => "文件级变更",
            Self::FileDiff { .. } => "文件差异",
        }
    }
}

/// 仓库概览结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoOverview {
    /// HEAD 状态。
    pub head: HeadInfo,
    /// 全部引用。
    pub refs: Vec<RefInfo>,
    /// 进行中的操作。
    pub state: RepoState,
    /// 仓库位置（用于窗口标题与状态栏）。
    pub location: RepoLocation,
}

/// 日志页结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogPageResult {
    /// 本页节点。
    pub page: LogNodePage,
    /// 当前筛选口径下的总提交数（供滚动条与"共 N 条"使用）。
    pub total: usize,
    /// 该 id 列表是否来自缓存（诊断用：首次枚举大仓库需要数秒）。
    pub from_cache: bool,
}

/// 工作线程回传给 UI 的一条更新。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    /// 发起时的代次。UI 侧可用它对账（正常情况下过期结果已被服务层丢弃）。
    pub generation: Generation,
    /// 请求类别。
    pub kind: RequestKind,
    /// 执行结果。
    pub outcome: Outcome,
}

/// 一次请求的执行结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// 仓库概览。
    Overview(Box<RepoOverview>),
    /// 工作区状态。
    Status(Box<ChangeSet>),
    /// 日志页。
    LogPage(Box<LogPageResult>),
    /// 文件级变更。
    TreeChanges(Vec<FileChange>),
    /// 文件差异。
    FileDiff(Box<gt_diff::FileDiff>),
    /// 失败：错误文本已经过"面向用户"的转换，可直接展示。
    Failed(String),
}

impl Outcome {
    /// 是否成功。
    #[must_use]
    pub const fn is_ok(&self) -> bool {
        !matches!(self, Self::Failed(_))
    }

    /// 失败原因；成功时为 [`None`]。
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        match self {
            Self::Failed(message) => Some(message),
            _ => None,
        }
    }
}

/// 请求无法入队的原因。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RequestError {
    /// 仓库 id 未注册（窗口可能已经关闭）。
    #[error("仓库 {0:?} 未注册")]
    UnknownRepo(gt_core::RepoId),

    /// 工作队列已满。
    #[error("请求过多，请稍后重试（{0}）")]
    QueueFull(#[from] crate::pool::SubmitError),
}

#[cfg(test)]
mod tests {
    use super::{Delivery, Outcome, RequestError, RequestKind, RequestLane};
    use crate::generation::Generation;
    use gt_core::{LogPageRequest, RepoId};

    #[test]
    fn request_labels_cover_every_kind() {
        let kinds = [
            RequestKind::Overview,
            RequestKind::Status,
            RequestKind::LogPage(LogPageRequest::default()),
            RequestKind::TreeChanges {
                old: Some("HEAD~1".to_owned()),
                new: "HEAD".to_owned(),
            },
            RequestKind::FileDiff {
                old: None,
                new: "HEAD".to_owned(),
                path: "src/main.rs".to_owned(),
                worktree: false,
            },
        ];
        for kind in &kinds {
            assert!(!kind.label().is_empty(), "{kind:?} 缺少标签");
        }
    }

    #[test]
    fn every_kind_maps_to_exactly_one_lane() {
        assert_eq!(RequestKind::Overview.lane(), RequestLane::Overview);
        assert_eq!(RequestKind::Status.lane(), RequestLane::Status);
        assert_eq!(
            RequestKind::LogPage(LogPageRequest::default()).lane(),
            RequestLane::Log
        );
        // 提交级与文件级差异共用一条通道：它们都在填同一个面板。
        assert_eq!(
            RequestKind::TreeChanges {
                old: None,
                new: "HEAD".to_owned()
            }
            .lane(),
            RequestLane::Diff
        );
        assert_eq!(
            RequestKind::FileDiff {
                old: None,
                new: "HEAD".to_owned(),
                path: "a".to_owned(),
                worktree: true
            }
            .lane(),
            RequestLane::Diff
        );
        assert_eq!(RequestLane::ALL.len(), 4);
        for lane in RequestLane::ALL {
            assert!(!lane.label().is_empty());
        }
    }

    #[test]
    fn failed_outcome_carries_a_user_facing_message() {
        let failed = Outcome::Failed("不是有效的 Git 仓库".to_owned());
        assert!(!failed.is_ok());
        assert_eq!(failed.error(), Some("不是有效的 Git 仓库"));

        let ok = Outcome::TreeChanges(Vec::new());
        assert!(ok.is_ok());
        assert_eq!(ok.error(), None);
    }

    #[test]
    fn request_error_renders_for_users() {
        let unknown = RequestError::UnknownRepo(RepoId::from_raw(3));
        assert!(unknown.to_string().contains("未注册"));

        let full = RequestError::QueueFull(crate::pool::SubmitError::QueueFull { capacity: 64 });
        assert!(full.to_string().contains("请求过多"));
    }

    #[test]
    fn delivery_is_send_and_cheap_to_clone() {
        // 消息必须能跨线程移动；`Clone` 用于"同一结果给多个订阅者"的诊断场景。
        fn assert_send<T: Send>() {}
        assert_send::<Delivery>();
        assert_send::<Outcome>();

        let delivery = Delivery {
            generation: Generation::INITIAL,
            kind: RequestKind::Status,
            outcome: Outcome::Failed("x".to_owned()),
        };
        assert_eq!(delivery.clone(), delivery);
    }
}
