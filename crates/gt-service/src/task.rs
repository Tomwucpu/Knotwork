//! 请求与更新的消息模型（架构规划 4.3 第 1、5 点）。
//!
//! UI 线程只做两件事：投递 [`RequestKind`]，消费 [`Delivery`]。
//! 消息里**不含任何句柄**（不携带 `GitRepo`、不携带 `Arc<Vec<..>>` 之外的共享状态），
//! 因此可以在任意线程之间自由移动。

use gt_backend::{BranchOp, CommitRequest, IgnoreTarget, RepoLocation, WriteOutcome};
use gt_core::{ChangeSet, FileChange, HeadInfo, LogNodePage, LogPageRequest, RefInfo, RepoState};
use gt_diff::HunkSelection;

use crate::generation::Generation;

/// 差异的一侧。
///
/// M1 用 `old`/`new` 两个修订串 + 一个 `worktree` 布尔表示两侧来源，但这无法表达
/// Local Changes 需要的三种组合（工作区 vs 索引、索引 vs HEAD）。M2 起改为
/// 显式枚举，避免"某个布尔组合非法"这类需要在运行期才发现的错误。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DiffSide {
    /// 空树（根提交的旧侧）。
    Empty,
    /// 某个修订（分支名 / 标签 / 提交号 / `HEAD~1`）。
    Rev(String),
    /// 索引（暂存区）。
    Index,
    /// 工作区文件。
    Worktree,
}

impl DiffSide {
    /// 修订写法（用于差异标题与审计）。
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::Empty => "空树",
            Self::Rev(spec) => spec,
            Self::Index => "索引",
            Self::Worktree => "工作区",
        }
    }
}

/// hunk / 行级选择的作用方式（FR-LC-03、FR-DIF-05）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMode {
    /// 暂存所选（`git apply --cached`）。
    Stage,
    /// 反暂存所选（`git apply --cached --reverse`）。
    Unstage,
    /// 丢弃所选（`git apply --reverse`，直接改工作区，不可逆）。
    Discard,
}

impl SelectionMode {
    /// 中文标签。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Stage => "暂存所选",
            Self::Unstage => "反暂存所选",
            Self::Discard => "丢弃所选",
        }
    }

    /// 是否需要二次确认（不可逆操作）。
    #[must_use]
    pub const fn needs_confirmation(self) -> bool {
        matches!(self, Self::Discard)
    }
}

/// 一次写请求（M2 起）。
///
/// 与 [`RequestKind`] 分开：读请求会经过代次闸门（过期即丢弃），
/// **写请求绝对不能**——丢弃一次写操作的结果等于让用户以为没生效。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteRequest {
    /// 暂存路径（`git add -A`）。
    Stage(Vec<String>),
    /// 反暂存路径（`git restore --staged`）。
    Unstage(Vec<String>),
    /// 丢弃已跟踪文件的修改（不可逆）。
    Discard(Vec<String>),
    /// 删除未跟踪文件 / 目录（不可逆）。
    DeleteUntracked(Vec<String>),
    /// 暂存 / 反暂存 / 丢弃所选 hunk 与行。
    Selection {
        /// 仓库相对路径。
        path: String,
        /// 左侧来源（暂存时是索引，反暂存时是 HEAD）。
        left: DiffSide,
        /// 右侧来源（暂存时是工作区，反暂存时是索引）。
        right: DiffSide,
        /// 选择集。
        selections: Vec<HunkSelection>,
        /// 界面展示的那份差异的指纹（防止用过期选择打补丁）。
        expected_fingerprint: u64,
        /// 作用方式。
        mode: SelectionMode,
    },
    /// 提交 / 修订。
    Commit(CommitRequest),
    /// 分支操作。
    Branch(BranchOp),
    /// 写入忽略规则。
    Ignore {
        /// 目标文件。
        target: IgnoreTarget,
        /// 被忽略的路径。
        path: String,
    },
}

impl WriteRequest {
    /// 审计标签。
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Stage(_) => "暂存",
            Self::Unstage(_) => "反暂存",
            Self::Discard(_) => "丢弃修改",
            Self::DeleteUntracked(_) => "删除未跟踪文件",
            Self::Selection { mode, .. } => mode.label(),
            Self::Commit(_) => "提交",
            Self::Branch(_) => "分支操作",
            Self::Ignore { .. } => "写入忽略规则",
        }
    }

    /// 是否不可逆（供高危确认清单使用）。
    #[must_use]
    pub const fn is_destructive(&self) -> bool {
        match self {
            Self::Discard(_)
            | Self::DeleteUntracked(_)
            | Self::Branch(
                BranchOp::CheckoutDetached { .. } | BranchOp::Delete { force: true, .. },
            ) => true,
            Self::Selection { mode, .. } => mode.needs_confirmation(),
            _ => false,
        }
    }
}

/// 一次搜索请求（FR-LOG-03）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRequest {
    /// 关键字 / 正则 / 哈希前缀。
    pub query: String,
    /// 是否按正则解释。
    pub regex: bool,
    /// 是否大小写敏感。
    pub case_sensitive: bool,
    /// 是否搜索全部引用（含远程分支）。
    pub all_refs: bool,
    /// 结果上限。
    pub limit: usize,
}

impl Default for SearchRequest {
    fn default() -> Self {
        Self {
            query: String::new(),
            regex: false,
            case_sensitive: false,
            all_refs: false,
            limit: 500,
        }
    }
}

/// 一次读取请求的类别。
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
        /// 左侧来源。
        left: DiffSide,
        /// 右侧来源。
        right: DiffSide,
        /// 仓库相对路径。
        path: String,
    },
    /// 提交消息搜索。
    Search(SearchRequest),
    /// 写操作（不经代次闸门）。
    Write(Box<WriteRequest>),
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
    /// 写操作。
    ///
    /// 单独一条通道，且**不参与代次作废**：写请求的结果必须送达界面，
    /// 无论在此期间用户又点了几次别的东西。
    Write,
}

impl RequestLane {
    /// 全部通道。
    pub const ALL: [Self; 5] = [
        Self::Overview,
        Self::Status,
        Self::Log,
        Self::Diff,
        Self::Write,
    ];

    /// 诊断标签。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Overview => "概览",
            Self::Status => "状态",
            Self::Log => "日志",
            Self::Diff => "差异",
            Self::Write => "写操作",
        }
    }

    /// 该通道的结果是否受代次闸门约束。
    #[must_use]
    pub const fn is_gated(self) -> bool {
        !matches!(self, Self::Write)
    }
}

impl RequestKind {
    /// 该请求走哪条通道。
    #[must_use]
    pub const fn lane(&self) -> RequestLane {
        match self {
            Self::Overview => RequestLane::Overview,
            Self::Status => RequestLane::Status,
            Self::LogPage(_) | Self::Search(_) => RequestLane::Log,
            Self::TreeChanges { .. } | Self::FileDiff { .. } => RequestLane::Diff,
            Self::Write(_) => RequestLane::Write,
        }
    }

    /// 简短标签，用于诊断日志与 Console。
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Overview => "仓库概览",
            Self::Status => "工作区状态",
            Self::LogPage(_) => "日志页",
            Self::TreeChanges { .. } => "文件级变更",
            Self::FileDiff { .. } => "文件差异",
            Self::Search(_) => "提交搜索",
            Self::Write(write) => write.label(),
        }
    }
}

/// 仓库概览结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoOverview {
    /// HEAD 状态。
    pub head: HeadInfo,
    /// 全部引用（本地分支的 ahead/behind 已填充）。
    pub refs: Vec<RefInfo>,
    /// 进行中的操作。
    pub state: RepoState,
    /// 仓库位置（用于窗口标题与状态栏）。
    pub location: RepoLocation,
    /// 只读原因（FR-MR-04）；[`None`] 表示可写。
    pub read_only: Option<String>,
    /// 仓库配置的 `user.email`（FR-LOG-06 的"我的提交"判定）。
    pub user_email: Option<String>,
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
    /// 降级说明（例如拓扑排序因规模退回日期序）。
    pub note: Option<String>,
}

/// 搜索结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    /// 命中的提交（按搜索顺序）。
    pub nodes: Vec<gt_core::CommitMeta>,
    /// 原始查询串（供界面回显）。
    pub query: String,
    /// 是否因超时降级（FR-LOG-03：提示可构建历史索引）。
    pub timed_out: bool,
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
    /// 搜索结果。
    Search(Box<SearchResult>),
    /// 写操作结果。
    Write(Box<WriteOutcome>),
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

    /// 是否为写操作结果（供界面触发写后刷新）。
    #[must_use]
    pub const fn is_write(&self) -> bool {
        matches!(self, Self::Write(_))
    }
}

/// 请求无法入队的原因。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RequestError {
    /// 仓库 id 未注册（窗口可能已经关闭）。
    #[error("仓库 {0:?} 未注册")]
    UnknownRepo(gt_core::RepoId),

    /// 仓库以只读模式打开，写操作被拒（FR-MR-04）。
    #[error("{0}")]
    ReadOnly(String),

    /// 工作队列已满。
    #[error("请求过多，请稍后重试（{0}）")]
    QueueFull(#[from] crate::pool::SubmitError),
}

#[cfg(test)]
mod tests {
    use super::{
        Delivery, DiffSide, Outcome, RequestError, RequestKind, RequestLane, SearchRequest,
        SelectionMode, WriteRequest,
    };
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
                left: DiffSide::Index,
                right: DiffSide::Worktree,
                path: "src/main.rs".to_owned(),
            },
            RequestKind::Search(SearchRequest::default()),
            RequestKind::Write(Box::new(WriteRequest::Stage(vec!["a.txt".to_owned()]))),
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
        assert_eq!(
            RequestKind::Search(SearchRequest::default()).lane(),
            RequestLane::Log,
            "搜索复用日志通道：它替换的是同一块列表"
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
            RequestKind::Write(Box::new(WriteRequest::Commit(
                gt_backend::CommitRequest::default()
            )))
            .lane(),
            RequestLane::Write
        );

        assert_eq!(RequestLane::ALL.len(), 5);
        for lane in RequestLane::ALL {
            assert!(!lane.label().is_empty());
            // 写通道必须不受代次闸门约束，其余通道必须受约束。
            assert_eq!(lane.is_gated(), lane != RequestLane::Write);
        }
    }

    #[test]
    fn writes_classify_destructive_operations() {
        assert!(WriteRequest::Discard(vec!["a".to_owned()]).is_destructive());
        assert!(WriteRequest::DeleteUntracked(vec!["a".to_owned()]).is_destructive());
        assert!(!WriteRequest::Stage(vec!["a".to_owned()]).is_destructive());
        assert!(!WriteRequest::Unstage(vec!["a".to_owned()]).is_destructive());
        assert!(!WriteRequest::Branch(gt_backend::BranchOp::Checkout {
            name: "main".to_owned()
        })
        .is_destructive());
        assert!(WriteRequest::Branch(gt_backend::BranchOp::Delete {
            name: "wip".to_owned(),
            force: true
        })
        .is_destructive());
        assert!(SelectionMode::Discard.needs_confirmation());
        assert!(!SelectionMode::Stage.needs_confirmation());
    }

    #[test]
    fn failed_outcome_carries_a_user_facing_message() {
        let failed = Outcome::Failed("不是有效的 Git 仓库".to_owned());
        assert!(!failed.is_ok());
        assert_eq!(failed.error(), Some("不是有效的 Git 仓库"));
        assert!(!failed.is_write());

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

        let read_only = RequestError::ReadOnly("仓库正被其他实例写入".to_owned());
        assert!(read_only.to_string().contains("其他实例"));
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

    #[test]
    fn diff_side_labels_are_readable() {
        assert_eq!(DiffSide::Index.label(), "索引");
        assert_eq!(DiffSide::Worktree.label(), "工作区");
        assert_eq!(DiffSide::Empty.label(), "空树");
        assert_eq!(DiffSide::Rev("main".to_owned()).label(), "main");
    }
}
