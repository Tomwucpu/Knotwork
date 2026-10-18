//! 每个窗口一个的会话：把服务层的异步结果合并成界面状态，并承载写操作意图。
//!
//! # 职责边界
//!
//! - **只做数据搬运与状态合并**：不打开仓库、不读文件、不起进程（架构规划 4.2
//!   的"UI 线程零 I/O"）；所有数据来自 `gt-service` 的 [`Delivery`]；
//! - **不重建模型除非真的变了**：界面每 100ms 轮询一次，若无条件重建 500 行的
//!   模型，滚动会明显掉帧。因此用脏标记区分"数据变了"与"只是又轮询了一次"；
//! - **写操作先过检查再投递**：不可逆操作（丢弃、删除未跟踪、强制删分支）
//!   必须先走 [`Confirm`] 遮罩取得强确认（D4 默认强确认）；
//! - **滚动加载按需触发**：Log 视图只在接近已加载末尾时请求下一页。
//!
//! # 写操作的完整链路
//!
//! 1. 界面回调 → `Session::request_*`；
//! 2. 需要确认的进 [`Confirm`] 队列，界面显示遮罩；
//! 3. 确认后构造 [`WriteRequest`] 投给服务层（写通道，不作废、不丢弃结果）；
//! 4. 收到 [`Outcome::Write`]：提示结果并刷新状态 / 概览 / 日志。
//!
//! 第 3 步刻意不在 UI 线程做任何 I/O；第 4 步的刷新是"重新发起读请求"，
//! 而不是在写路径里夹带读 —— 这样 Console 里一次用户操作只有一条记录。

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender};
use gt_backend::{
    CherryPickRequest, FetchRequest, MergeRequest, MergeStrategy, PatchApplyRequest, PatchSource,
    PullRequest, PullStrategy, PushRequest, PushTarget, RebaseAction, RebaseControl, RebasePlan,
    RebaseProgress, RebaseRequest, RemoteEdit, RemoteInfo, RemoteListing, ResetMode, ResetRequest,
    RevertRequest, StashEntry, StashOp, TagOp,
};
use gt_core::{ChangeSet, CommitCheck, CommitCheckInput, CommitMeta, HeadInfo, LogNode, RepoState};
use gt_diff::{FileDiff, HunkSelection};
use gt_graph::LaneLayout;
use gt_service::{
    first_log_page, AdvancedResult, ConflictedEntry, ConsoleBuffer, Delivery, DiffSide, GitService,
    NetworkFailure, NetworkOutcome, NetworkRequest, NetworkSnapshot, Outcome, PushPreviewRequest,
    RepoHandle, RequestKind, SearchRequest, SelectionMode, WriteRequest,
};

use crate::generated::{DiffRow, MainWindow};
use crate::text;
use crate::view_model::{
    branch_text, build_blame_rows, build_change_rows, build_compare_rows,
    build_conflict_file_rows,
    build_conflict_rows, build_console_rows, build_diff_rows, build_log_rows, build_push_rows,
    build_history_rows, build_rebase_step_rows, build_ref_rows, build_remote_rows,
    build_stash_rows, conflict_summary,
    credential_help, diff_selection_hint, discard_confirmation, discard_selection_confirmation,
    head_text, network_error_text, ordered_refs, push_hints, push_preview_text, rc_model,
    rebase_action_from_code, rebase_skip_confirmation, reset_confirmation, resolution_from_code,
    stash_drop_confirmation, state_banner, take_side_confirmation,
};

/// 距离已加载末尾多少行时预取下一页。
const PREFETCH_MARGIN: usize = 50;

/// 单页提交数。
pub const LOG_PAGE_SIZE: usize = 200;

/// 提示条的显示时长（以轮询次数计，100ms/次 → 约 6 秒）。
const TOAST_TICKS: u8 = 60;

/// 会话希望写回配置的事件。
///
/// 凭据是唯一会改配置的远端操作（"哪些主机存过 token"要落盘，token 本身不落盘）。
/// 会话不直接持有配置文件路径 —— 那是应用层的职责，这里只排队、由应用层消费。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigEvent {
    /// 某主机已保存 token。
    TokenSaved(String),
    /// 某主机已删除 token。
    TokenForgotten(String),
}

/// 需要在执行前取得强确认的动作。
///
/// 放在会话里而不是界面里：确认框只是表现，**"哪些操作必须确认"是业务规则**
/// （需求文档第 9 节 L5 危险操作审查清单），必须与操作本身待在一个地方。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirm {
    /// 丢弃已跟踪文件的修改。
    DiscardChanges(Vec<String>),
    /// 删除未跟踪文件 / 目录。
    DeleteUntracked(Vec<String>),
    /// 丢弃所选 hunk / 行。
    DiscardSelection {
        /// 目标文件。
        path: String,
        /// 选中的行数（仅用于文案）。
        lines: usize,
    },
    /// 删除分支。
    DeleteBranch {
        /// 分支名。
        name: String,
        /// 是否强制（未合并也删）。
        force: bool,
    },
    /// 检出某个提交（分离头指针）。
    CheckoutDetached {
        /// 修订。
        revision: String,
    },
    /// 重置当前分支（FR-MRG-06；hard 不可逆）。
    Reset {
        /// 目标修订。
        target: String,
        /// 模式。
        mode: ResetMode,
    },
    /// 整体用某一侧覆盖冲突文件（FR-CFL-03）。
    TakeSide {
        /// 仓库相对路径。
        path: String,
        /// 2 = 当前分支，3 = 合入分支。
        stage: u8,
    },
    /// 删除一条贮藏（FR-STH）。
    StashDrop {
        /// 选择子（`stash@{0}`）。
        selector: String,
        /// 下标。
        index: u32,
    },
    /// 跳过变基中的当前提交（会丢弃该提交的改动）。
    RebaseSkip,
    /// 中止进行中的操作。
    AbortInProgress {
        /// 进行中的状态。
        state: RepoState,
    },
    /// 强制推送到受保护/已存在的分支（FR-RMT-03）。
    ForcePush {
        /// 远端名。
        remote: String,
        /// 远端分支。
        branch: String,
        /// 待推提交数（仅用于文案）。
        commits: usize,
    },
}

impl Confirm {
    /// 遮罩文案：(标题, 正文, 是否危险色)。
    #[must_use]
    pub fn text(&self) -> (String, String, bool) {
        match self {
            Self::DiscardChanges(paths) => {
                let (title, detail) = discard_confirmation(paths, false);
                (title, detail, true)
            }
            Self::DeleteUntracked(paths) => {
                let (title, detail) = discard_confirmation(paths, true);
                (title, detail, true)
            }
            Self::DiscardSelection { path, lines } => {
                let (title, detail) = discard_selection_confirmation(path, *lines);
                (title, detail, true)
            }
            Self::DeleteBranch { name, force } => (
                if *force {
                    text::trf("强制删除分支 {}？", &[&name.clone()])
                } else {
                    text::trf("删除分支 {}？", &[&name.clone()])
                },
                if *force {
                    "强制删除会丢弃该分支上未合并的提交，且无法通过 Git 找回。\n\n\
                     替代做法：先合并到目标分支，或用普通删除（未合并时 Git 会拒绝并列出提交）。"
                        .to_owned()
                } else {
                    "未合并的分支会被 Git 拒绝删除；若被拒绝，可在确认框外改用「强制删除」，\
                     但那会永久丢弃该分支上的提交。"
                        .to_owned()
                },
                true,
            ),
            Self::CheckoutDetached { revision } => (
                text::trf("检出修订 {}？", &[revision]),
                "检出提交会让 HEAD 处于分离状态（不属于任何分支）。\
                 在此状态下的新提交不会被任何分支引用，切换分支后可能丢失。\n\n\
                 替代做法：若要在此提交上继续开发，请先「新建」分支。"
                    .to_owned(),
                false,
            ),
            Self::Reset { target, mode } => reset_confirmation(target, *mode),
            Self::TakeSide { path, stage } => {
                let side = if *stage == 3 {
                    text::tr("合入分支")
                } else {
                    text::tr("当前分支")
                };
                take_side_confirmation(path, side)
            }
            Self::StashDrop { selector, .. } => stash_drop_confirmation(selector),
            Self::RebaseSkip => rebase_skip_confirmation(),
            Self::AbortInProgress { state } => (
                text::trf("中止{}？", &[state.label()]),
                "中止会把仓库恢复到操作开始前的状态：已做的冲突解决会被丢弃，\
                 已重放的部分提交也会被撤销。\n\n\
                 替代做法：解决完冲突后「继续」；只想丢掉当前这一个提交时用「跳过」。"
                    .to_owned(),
                true,
            ),
            Self::ForcePush {
                remote,
                branch,
                commits,
            } => (
                text::trf("强制推送到 {}/{}？", &[&remote.clone(), &branch.clone()]),
                format!(
                    "强制推送会用本地历史覆盖远端分支：远端上不在本地的提交将从该分支消失，\
                     其他人已经拉取的历史会出现分叉。本次会推上去 {commits} 个提交。\n\n\
                     替代做法：先 Fetch 后 Update（合并或变基），再普通推送 —— \
                     这样别人的提交会保留在历史里。"
                ),
                true,
            ),
        }
    }
}

/// 提交上下文菜单指向的目标（FR-LOG-07/08）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct CtxTarget {
    /// 完整提交 id。
    id: String,
    /// 缩写 id（展示与预填用）。
    short: String,
    /// 在已加载列表里的下标。
    index: usize,
    /// 是否为当前 HEAD（决定"撤销提交"是否可用）。
    is_head: bool,
    /// 父提交 id；根提交为 [`None`]（不能交互式变基、不能浏览版本）。
    parent: Option<String>,
}

/// 通用操作对话框的状态（合并 / 变基 / 重置 / 补丁 / 标签）。
///
/// 字段与 Slint 侧属性一一对应：界面只是这组字段的渲染，
/// 会话持有真值，避免"界面改了但会话不知道"的双份状态。
#[derive(Debug, Clone, PartialEq, Eq)]
struct OpDialog {
    /// 1 合并 / 2 变基 / 3 重置 / 4 生成补丁 / 5 应用补丁 / 6 新建标签。
    kind: i32,
    /// 关联的提交 id（合并/变基/补丁/标签的默认目标）。
    commit: Option<String>,
    title: String,
    hint: String,
    input: String,
    placeholder: String,
    input2: String,
    placeholder2: String,
    show_input2: bool,
    modes: Vec<String>,
    mode: i32,
    danger: bool,
}

impl OpDialog {
    /// 合并对话框。
    fn merge(commit: Option<String>) -> Self {
        Self {
            kind: 1,
            commit: commit.clone(),
            title: text::tr("合并到当前分支").to_owned(),
            hint: text::tr("填写要合入的修订（分支名 / 标签 / 提交 id）。冲突时会进入三路冲突编辑器。")
                .to_owned(),
            input: commit.unwrap_or_default(),
            placeholder: text::tr("例如 feature/login 或 origin/main").to_owned(),
            input2: String::new(),
            placeholder2: String::new(),
            show_input2: false,
            modes: vec![
                MergeStrategy::Default.label().to_owned(),
                MergeStrategy::NoFastForward.label().to_owned(),
                MergeStrategy::FastForwardOnly.label().to_owned(),
                MergeStrategy::Squash.label().to_owned(),
            ],
            mode: 0,
            danger: false,
        }
    }

    /// 变基对话框。
    fn rebase(commit: Option<String>) -> Self {
        Self {
            kind: 2,
            commit: commit.clone(),
            title: text::tr("变基到指定修订").to_owned(),
            hint: text::tr("当前分支的提交会被重放到该修订之上。冲突时可继续 / 跳过 / 中止。").to_owned(),
            input: commit.unwrap_or_default(),
            placeholder: text::tr("例如 main 或 origin/main").to_owned(),
            input2: String::new(),
            placeholder2: String::new(),
            show_input2: false,
            modes: Vec::new(),
            mode: 0,
            danger: false,
        }
    }

    /// 重置对话框。
    fn reset(commit: Option<String>) -> Self {
        Self {
            kind: 3,
            commit: commit.clone(),
            title: text::tr("重置当前分支").to_owned(),
            hint: text::tr("软重置保留暂存；混合重置取消暂存；硬重置会丢弃全部未提交改动（需二次确认）。")
                .to_owned(),
            input: commit.unwrap_or_default(),
            placeholder: text::tr("例如 HEAD~1 或提交 id").to_owned(),
            input2: String::new(),
            placeholder2: String::new(),
            show_input2: false,
            modes: vec![
                ResetMode::Soft.label().to_owned(),
                ResetMode::Mixed.label().to_owned(),
                ResetMode::Hard.label().to_owned(),
            ],
            mode: 0,
            danger: false,
        }
    }

    /// 生成补丁对话框。
    fn patch(commit: String, short: &str, default_output: String) -> Self {
        Self {
            kind: 4,
            commit: Some(commit),
            title: text::tr("生成补丁").to_owned(),
            hint: "提交补丁（format-patch）带作者与消息，可用 `git am` 应用；\
                   区间差异（diff）是纯文本差异，用 `git apply` 应用。"
                .to_owned(),
            input: default_output,
            placeholder: text::tr("补丁输出路径").to_owned(),
            input2: String::new(),
            placeholder2: text::tr("区间起点（如 origin/main）").to_owned(),
            show_input2: false,
            modes: vec![
                text::trf("提交补丁（{}）", &[short]),
                text::tr("区间差异").to_owned(),
                text::tr("工作区差异").to_owned(),
            ],
            mode: 0,
            danger: false,
        }
    }

    /// 拣选对话框（FR-MRG-04）。
    ///
    /// 输入框里预填所选提交，但**允许修改**：`git cherry-pick` 本身就接受
    /// `A^..B` 这样的区间，用户想拣选一串提交时不必去终端。
    fn cherry_pick(commit: String, short: &str) -> Self {
        Self {
            kind: 7,
            commit: Some(commit),
            title: text::tr("拣选提交").to_owned(),
            hint: "把该提交的改动复制到当前分支（生成新提交）。输入框可填单个提交，\
                   也可填区间（如 `A^..B`）。冲突时可继续或中止。"
                .to_owned(),
            input: short.to_owned(),
            placeholder: text::tr("要拣选的提交或区间").to_owned(),
            input2: String::new(),
            placeholder2: String::new(),
            show_input2: false,
            modes: vec![text::tr("不追加来源行").to_owned(), text::tr("追加来源行（-x）").to_owned()],
            mode: 0,
            danger: false,
        }
    }

    /// 应用补丁对话框。
    fn apply_patch() -> Self {
        Self {
            kind: 5,
            commit: None,
            title: text::tr("应用补丁").to_owned(),
            hint: text::tr("三方合并（--3way）在上下文不匹配时会用 blob 信息尝试合并，冲突进入三路编辑器。")
                .to_owned(),
            input: String::new(),
            placeholder: text::tr("补丁文件路径").to_owned(),
            input2: String::new(),
            placeholder2: String::new(),
            show_input2: false,
            modes: vec![text::tr("普通应用").to_owned(), text::tr("三方合并（--3way）").to_owned()],
            mode: 0,
            danger: false,
        }
    }

    /// 新建标签对话框。
    fn tag(commit: String) -> Self {
        Self {
            kind: 6,
            commit: Some(commit),
            title: text::tr("新建标签").to_owned(),
            hint: text::tr("附注消息留空即创建轻量标签；填写则会创建附注标签（含创建者与日期）。").to_owned(),
            input: String::new(),
            placeholder: text::tr("标签名，例如 v1.0.0").to_owned(),
            input2: String::new(),
            placeholder2: text::tr("附注消息（可留空）").to_owned(),
            show_input2: true,
            modes: Vec::new(),
            mode: 0,
            danger: false,
        }
    }
}

/// 两提交比较 / 快照浏览的状态（FR-LOG-08）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CompareState {
    /// 遮罩标题。
    title: String,
    /// 旧侧（[`None`] 表示空树）。
    old: Option<String>,
    /// 新侧。
    new: String,
    /// 是否为只读浏览（禁止任何写操作）。
    browse: bool,
    /// 文件清单。
    files: Vec<gt_core::FileChange>,
    /// 当前选中项。
    selected: Option<usize>,
}

/// Blame 视图状态（FR-BLM-02/03）。
#[derive(Clone)]
struct BlameState {
    /// 被归因的文件。
    path: String,
    /// 被 blame 的修订；[`None`] = 工作区。
    spec: Option<String>,
    /// 逐行归因。
    rows: Vec<gt_backend::BlameLine>,
    /// 当前选中行。
    selected: Option<usize>,
    /// 是否被截断。
    truncated: bool,
}

/// 文件历史状态（FR-BLM-01）。
#[derive(Clone)]
struct FileHistoryState {
    /// 文件。
    path: String,
    /// 是否扫了全部引用。
    all_refs: bool,
    /// 提交列表（新 → 旧）。
    rows: Vec<gt_backend::FileHistoryEntry>,
    /// 当前选中项。
    selected: Option<usize>,
    /// 是否被截断。
    truncated: bool,
}

/// 把"进行中的操作状态"翻译成一条中止请求。
///
/// 三种族的恢复入口不同：变基走 `rebase --abort`，拣选/回滚走 `--abort`，
/// 合并走 `merge --abort`。把它们收在一处，界面上的"中止"按钮就只有一条路径。
fn abort_request(state: RepoState) -> WriteRequest {
    match state {
        RepoState::RebaseMerge | RepoState::RebaseApply => {
            WriteRequest::RebaseControl(RebaseControl::Abort)
        }
        RepoState::Merge => WriteRequest::AbortMerge,
        other => WriteRequest::ContinueState {
            state: other,
            abort: true,
        },
    }
}

/// 把"进行中的操作状态"翻译成一条继续请求。
fn continue_request(state: RepoState) -> WriteRequest {
    match state {
        RepoState::RebaseMerge | RepoState::RebaseApply => {
            WriteRequest::RebaseControl(RebaseControl::Continue)
        }
        other => WriteRequest::ContinueState {
            state: other,
            abort: false,
        },
    }
}

/// Log 视图的状态。
struct LogState {
    layout: LaneLayout,
    nodes: Vec<CommitMeta>,
    /// 与 `nodes` 一一对应的引用标注（`HEAD -> main, v1.0`）。
    refs_text: Vec<String>,
    total: usize,
    exhausted: bool,
    /// 降级说明（例如拓扑排序因规模退回日期序）。
    note: Option<String>,
}

impl Default for LogState {
    fn default() -> Self {
        Self {
            layout: LaneLayout::new(),
            nodes: Vec::new(),
            refs_text: Vec::new(),
            total: 0,
            exhausted: false,
            note: None,
        }
    }
}

/// 哪些模型需要重新生成。
#[derive(Debug, Clone, Copy, Default)]
#[allow(clippy::struct_excessive_bools)]
struct Dirty {
    header: bool,
    refs: bool,
    changes: bool,
    log: bool,
    diff: bool,
    console: bool,
    commit: bool,
    remotes: bool,
    push: bool,
    network: bool,
    // ---- M4 ----
    ctx: bool,
    op: bool,
    iplan: bool,
    cfl: bool,
    stash: bool,
    compare: bool,
    // ---- M5 ----
    blame: bool,
    history: bool,
}

impl Dirty {
    /// 全部标脏（首次装载）。
    const fn all() -> Self {
        Self {
            header: true,
            refs: true,
            changes: true,
            log: true,
            diff: true,
            console: true,
            commit: true,
            remotes: true,
            push: true,
            network: true,
            ctx: true,
            op: true,
            iplan: true,
            cfl: true,
            stash: true,
            compare: true,
            blame: true,
            history: true,
        }
    }
}

/// 正在进行的写操作类别（用于在结果落地时执行对应的收尾动作）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingWrite {
    /// 提交 / 修订：成功后清空消息框。
    Commit,
    /// 其余写操作：成功后只提示。
    Other,
}

/// 一个仓库窗口的会话。
#[allow(clippy::struct_excessive_bools)]
pub struct Session {
    service: Arc<GitService>,
    handle: Arc<RepoHandle>,
    sender: Sender<Delivery>,
    receiver: Receiver<Delivery>,
    console: Arc<ConsoleBuffer>,
    console_capacity: usize,

    // ---- 仓库状态 ----
    head: Option<HeadInfo>,
    state: RepoState,
    refs: Vec<gt_core::RefInfo>,
    user_email: Option<String>,
    read_only: Option<String>,

    // ---- 非功能设置（来自 gt-config，影响检查与按钮）----
    staging_mode: bool,
    warn_detached: bool,

    // ---- 视图状态 ----
    changes: ChangeSet,
    selected_change: Option<usize>,
    log: LogState,
    selected_log: Option<usize>,

    // ---- 搜索与图选项 ----
    search_query: String,
    search_regex: bool,
    search_active: bool,
    graph: gt_core::GraphOptions,

    // ---- 差异 ----
    diff: Option<FileDiff>,
    /// 图片差异（FR-DIF-04）；有值时差异面板展示图片对比而非文本行。
    image_diff: Option<gt_service::ImageDiffResult>,
    diff_title: String,
    diff_rows: Vec<DiffRow>,
    diff_selection: BTreeSet<usize>,
    diff_sides: (DiffSide, DiffSide),
    diff_path: Option<String>,
    diff_staged_view: bool,
    /// 当前差异能否按 hunk / 行落库（见 [`Session::selection_sides_supported`]）。
    diff_selectable: bool,

    // ---- 提交与分支 ----
    commit_message: String,
    commit_amend: bool,
    commit_reset_author: bool,
    branch_input: String,
    selected_ref: Option<usize>,

    // ---- 远端（FR-RMT）----
    remotes: Vec<RemoteInfo>,
    remote_listing: Option<RemoteListing>,
    remotes_open: bool,
    selected_remote: Option<usize>,
    remote_name_input: String,
    remote_url_input: String,
    remote_help: String,
    token_input: String,
    /// 更新策略下标：0 合并 / 1 变基 / 2 仅快进（与设置项共享）。
    pull_strategy: i32,
    /// 受保护分支正则（来自设置，用于推送拦截与预览提示）。
    protected_patterns: Vec<String>,
    /// 已把 token 存入密钥链的主机（来自配置，仅用于展示）。
    stored_hosts: Vec<String>,

    // ---- 推送预览（FR-RMT-03）----
    push_open: bool,
    push_preview: Option<gt_backend::PushPreview>,
    push_force: bool,
    push_set_upstream: bool,
    push_summary: String,
    push_hint: String,
    push_rows: Vec<crate::generated::PushRow>,
    /// 已请求预览、等结果到达后自动弹窗。
    awaiting_preview: bool,

    // ---- 网络状态 ----
    network: NetworkSnapshot,
    network_failure: Option<NetworkFailure>,
    /// 待应用层消费的配置事件。
    config_events: Vec<ConfigEvent>,

    // ---- 交互 ----
    confirm: Option<Confirm>,
    toast: Option<(String, u8)>,
    pending_write: Option<PendingWrite>,

    // ---- M4：高级操作（FR-MRG / FR-CFL / FR-STH / FR-PTC / FR-LOG-08）----
    /// 上下文菜单目标与展开位置。
    ctx: Option<CtxTarget>,
    ctx_pos: (f32, f32),
    /// 通用操作对话框。
    op: Option<OpDialog>,
    /// 交互式变基计划与其编辑态。
    plan: Option<RebasePlan>,
    plan_selected: Option<usize>,
    plan_message: String,
    /// 冲突文件清单与三路编辑器的状态。
    conflicts: Vec<ConflictedEntry>,
    conflict_selected: Option<usize>,
    conflict_file: Option<gt_diff::ConflictFile>,
    conflict_resolutions: Vec<Option<gt_diff::Resolution>>,
    conflict_block: Option<usize>,
    conflict_manual: String,
    /// 贮藏。
    stashes: Vec<StashEntry>,
    /// Blame 视图的滚动锚点（行下标，0 起）；由下一次 blame 落地时消费。
    blame_pending_anchor: Option<usize>,
    stash_message: String,
    stash_untracked: bool,
    /// 比较 / 快照浏览。
    compare: Option<CompareState>,
    /// Blame 视图（FR-BLM-02/03）。
    blame: Option<BlameState>,
    /// 文件历史（FR-BLM-01）。
    file_history: Option<FileHistoryState>,
    /// 变基进度（来自概览）。
    rebase: Option<RebaseProgress>,
    /// 高级操作忙碌标签（来自仓库句柄）。
    advanced_label: Option<String>,

    // ---- 在飞标记（避免重复投递同一请求）----
    overview_in_flight: bool,
    status_in_flight: bool,
    log_in_flight: bool,
    diff_in_flight: bool,
    write_in_flight: bool,

    // ---- 脏标记（决定哪些模型需要重建）----
    dirty: Dirty,
}

impl Session {
    /// 新建会话；`console_capacity` 是 Console 面板的环形缓冲容量。
    #[must_use]
    pub fn new(
        service: Arc<GitService>,
        handle: Arc<RepoHandle>,
        console: Arc<ConsoleBuffer>,
        console_capacity: usize,
    ) -> Self {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let read_only = handle.read_only_reason().map(str::to_owned);
        Self {
            service,
            handle,
            sender,
            receiver,
            console,
            console_capacity,
            head: None,
            state: RepoState::Clean,
            refs: Vec::new(),
            user_email: None,
            read_only,
            staging_mode: true,
            warn_detached: true,
            changes: ChangeSet::default(),
            selected_change: None,
            log: LogState::default(),
            selected_log: None,
            search_query: String::new(),
            search_regex: false,
            search_active: false,
            graph: gt_core::GraphOptions::default(),
            diff: None,
            image_diff: None,
            diff_title: text::tr("差异").to_owned(),
            diff_rows: Vec::new(),
            diff_selection: BTreeSet::new(),
            diff_sides: (DiffSide::Index, DiffSide::Worktree),
            diff_path: None,
            diff_staged_view: false,
            diff_selectable: false,
            commit_message: String::new(),
            commit_amend: false,
            commit_reset_author: false,
            branch_input: String::new(),
            selected_ref: None,
            remotes: Vec::new(),
            remote_listing: None,
            remotes_open: false,
            selected_remote: None,
            remote_name_input: String::new(),
            remote_url_input: String::new(),
            remote_help: String::new(),
            token_input: String::new(),
            pull_strategy: 0,
            protected_patterns: Vec::new(),
            stored_hosts: Vec::new(),
            push_open: false,
            push_preview: None,
            push_force: false,
            push_set_upstream: false,
            push_summary: String::new(),
            push_hint: String::new(),
            push_rows: Vec::new(),
            awaiting_preview: false,
            network: NetworkSnapshot::default(),
            network_failure: None,
            config_events: Vec::new(),
            confirm: None,
            toast: None,
            pending_write: None,
            ctx: None,
            ctx_pos: (0.0, 0.0),
            op: None,
            plan: None,
            plan_selected: None,
            plan_message: String::new(),
            conflicts: Vec::new(),
            conflict_selected: None,
            conflict_file: None,
            conflict_resolutions: Vec::new(),
            conflict_block: None,
            conflict_manual: String::new(),
            stashes: Vec::new(),
            stash_message: String::new(),
            stash_untracked: false,
            compare: None,
            blame: None,
            file_history: None,
            blame_pending_anchor: None,
            rebase: None,
            advanced_label: None,
            overview_in_flight: false,
            status_in_flight: false,
            log_in_flight: false,
            diff_in_flight: false,
            write_in_flight: false,
            dirty: Dirty::all(),
        }
    }

    /// 仓库句柄。
    #[must_use]
    pub fn handle(&self) -> &Arc<RepoHandle> {
        &self.handle
    }

    /// 仓库显示名。
    #[must_use]
    pub fn display_name(&self) -> String {
        self.handle.display_name()
    }

    /// 应用非功能设置（来自配置文件）。
    ///
    /// 只在值真的变化时标脏：`apply_to` 会被 100ms 的轮询反复调用，
    /// 无条件标脏会让底部面板每一帧都重建模型。
    pub fn set_preferences(&mut self, staging_mode: bool, warn_detached: bool) {
        if self.staging_mode == staging_mode && self.warn_detached == warn_detached {
            return;
        }
        self.staging_mode = staging_mode;
        self.warn_detached = warn_detached;
        self.dirty.commit = true;
    }

    /// 发起首屏请求（概览 + 状态 + 第一页日志）。
    pub fn request_initial(&mut self) {
        self.request_overview();
        self.request_status();
        self.request_log_page(0);
    }

    /// 全部重算（用户点"刷新"或文件系统变化后调用）。
    pub fn refresh(&mut self) {
        // 引用 / 索引 / HEAD 可能已变：日志 id 列表缓存作废，
        // 泳道布局与已加载节点也要重来，否则新旧历史会接不上。
        self.handle.invalidate_log_index();
        self.log = LogState::default();
        self.search_active = false;
        self.dirty.log = true;
        self.clear_in_flight();
        self.request_initial();
        // 差异也重算：写操作后它极可能已经过期（例如暂存了一个 hunk）。
        self.reload_diff();
    }

    /// 清空全部"在飞"标记。
    ///
    /// 必须在**作废在飞请求**的场景下调用（刷新、切换仓库）：被作废的结果
    /// 会因代次过期被服务层丢弃、**永远不会回传**，于是没人再去清标记，
    /// 界面就永久停在"加载中"并且不再发新请求。
    fn clear_in_flight(&mut self) {
        self.overview_in_flight = false;
        self.status_in_flight = false;
        self.log_in_flight = false;
        self.diff_in_flight = false;
    }

    /// 轮询并合并所有已到达的结果。
    ///
    /// 返回本次合并是否改变了视图状态（供调用方决定是否需要重绘）。
    pub fn drain(&mut self) -> bool {
        let mut changed = false;
        while let Ok(delivery) = self.receiver.try_recv() {
            changed |= self.apply(delivery);
        }
        if let Some((_, ticks)) = self.toast.as_mut() {
            *ticks = ticks.saturating_sub(1);
            if *ticks == 0 {
                self.toast = None;
                changed = true;
            }
        }
        // 网络进度以 git 的 stderr 为准，可能在两帧之间变化几十次：
        // 这里只比较快照，不产生额外分配，也不需要工作线程通知。
        // 高级操作的忙碌状态同样来自句柄（工作线程持有），按帧比较即可。
        let advanced = self.handle.advanced_label();
        if advanced != self.advanced_label {
            self.advanced_label = advanced;
            self.dirty.header = true;
            changed = true;
        }
        let snapshot = self.handle.network_snapshot();
        if snapshot != self.network {
            self.network = snapshot;
            self.dirty.network = true;
            changed = true;
        }
        changed |= self.maybe_prefetch();
        changed
    }

    /// 把当前会话状态写进窗口。
    ///
    /// 长度来自"按脏标记逐面板写回"这一张平铺的对照表：拆开会让
    /// "哪些状态在什么时候写回"变得难以一眼核对（同 `execute_network` 的取舍）。
    #[allow(clippy::too_many_lines)]
    pub fn apply_to(&mut self, window: &MainWindow) {
        let dirty = std::mem::take(&mut self.dirty);
        window.set_has_repo(true);

        if dirty.header {
            window.set_repo_title(self.handle.display_name().into());
            window.set_repo_path(
                self.handle
                    .location()
                    .work_dir_or_git_dir()
                    .display()
                    .to_string()
                    .into(),
            );
            if let Some(head) = &self.head {
                window.set_branch_text(branch_text(head).into());
                window.set_head_text(head_text(head).into());
            } else {
                window.set_branch_text("—".into());
                window.set_head_text("—".into());
            }
            // 变基时优先展示"第 x/y 个提交"：只显示"变基进行中"会让用户
            // 不知道还剩多少（FR-MRG-02 的验收口径）。
            let banner = match (&self.rebase, state_banner(self.state)) {
                (Some(progress), _) => Some(progress.label()),
                (None, text) => text,
            };
            match banner {
                Some(text) => {
                    window.set_state_text(text.into());
                    window.set_state_banner_visible(true);
                }
                None => window.set_state_banner_visible(false),
            }
            let git_text = match self.service.capabilities().git_version.as_deref() {
                Some(version) => text::trf("git {} · 内置引擎读取", &[version]),
                None => text::tr("未找到 git（只读浏览）").to_owned(),
            };
            window.set_git_text(git_text.into());

            // 能力降级提示（FR-STA-04）。
            match self.service.capabilities().cli_unavailable_reason() {
                Some(reason) => {
                    window.set_cli_missing(true);
                    window.set_cli_missing_reason(reason.into());
                }
                None => window.set_cli_missing(false),
            }

            // 只读模式（FR-MR-04）。
            match &self.read_only {
                Some(reason) => {
                    window.set_read_only(true);
                    window.set_read_only_reason(reason.clone().into());
                }
                None => window.set_read_only(false),
            }
        }

        if dirty.refs {
            window.set_sidebar_refs(rc_model(build_ref_rows(&self.refs, self.selected_ref)));
            window.set_branch_input(self.branch_input.clone().into());
        }

        if dirty.changes {
            window.set_change_rows(rc_model(build_change_rows(
                &self.changes,
                self.selected_change,
            )));
            window.set_change_summary(self.change_summary().into());
            let untracked = self
                .selected_change
                .and_then(|index| self.changes.entries.get(index))
                .is_some_and(|entry| entry.index == gt_core::StatusFlag::Untracked);
            window.set_selection_is_untracked(untracked);
        }

        if dirty.log {
            let rows = build_log_rows(
                &self.log.nodes,
                self.log.layout.rows(),
                &self.log.refs_text,
                self.selected_log,
                now_unix(),
                self.user_email.as_deref(),
            );
            window.set_log_rows(rc_model(rows));
            window.set_log_total(i32::try_from(self.log.total).unwrap_or(i32::MAX));
            window.set_log_summary(self.log_summary().into());
        }

        if dirty.diff {
            self.apply_diff_panel(window);
        }

        if dirty.console {
            let snapshot = self.console.snapshot();
            window.set_console_rows(rc_model(build_console_rows(&snapshot)));
            window.set_console_summary(
                format!("({}/{})", snapshot.len(), self.console_capacity).into(),
            );
        }

        if dirty.commit {
            self.apply_commit_panel(window);
        }

        if dirty.remotes {
            self.apply_remote_panel(window);
        }

        if dirty.push {
            self.apply_push_panel(window);
        }

        if dirty.network {
            self.apply_network_strip(window);
        }

        // ---- M4 ----
        if dirty.ctx {
            self.apply_ctx_menu(window);
        }
        if dirty.op {
            self.apply_op_dialog(window);
        }
        if dirty.iplan {
            self.apply_rebase_plan(window);
        }
        if dirty.cfl {
            self.apply_conflict_editor(window);
        }
        if dirty.stash {
            self.apply_stash_panel(window);
        }
        if dirty.compare {
            self.apply_compare_panel(window);
        }
        if dirty.blame {
            self.apply_blame_panel(window);
        }
        if dirty.history {
            self.apply_history_panel(window);
        }
        // 变基进度 / 高级操作忙碌标签随时可能变化，跟随头部一起刷新。
        if dirty.header {
            self.apply_advanced_header(window);
        }

        self.apply_overlays(window);
    }

    /// 写 Diff 面板（行模型 / 标题 / 统计 / 按块操作的可用性）。
    fn apply_diff_panel(&self, window: &MainWindow) {
        apply_image_diff_panel(window, self.image_diff.as_ref());
        window.set_diff_rows(rc_model(self.diff_rows.clone()));
        window.set_diff_title(self.diff_title.clone().into());
        window.set_diff_stats(self.diff_stats().into());
        window.set_diff_staged_view(self.diff_staged_view);
        window.set_diff_selectable(self.diff_selectable);
        window.set_diff_hint(
            diff_selection_hint(
                self.diff_selection.len(),
                self.diff_selectable,
                self.diff_staged_view,
            )
            .into(),
        );
    }

    /// 写提交面板（消息提示 / 按钮可用性 / staging 模式）。
    ///
    /// 消息体与两个勾选框也要回写：提交成功后会话会清空消息，
    /// 若不同步，输入框里会留着一条已经提交过的文本。
    fn apply_commit_panel(&self, window: &MainWindow) {
        window.set_commit_hint(self.commit_hint().into());
        window.set_commit_enabled(self.commit_check().is_allowed());
        window.set_staging_mode(self.staging_mode);
        window.set_commit_message(self.commit_message.clone().into());
        window.set_commit_amend(self.commit_amend);
        window.set_commit_reset_author(self.commit_reset_author);
    }

    /// 写远端管理遮罩（列表 / 输入框 / 策略 / 凭据说明）。
    fn apply_remote_panel(&self, window: &MainWindow) {
        window.set_remotes_visible(self.remotes_open);
        window.set_remote_rows(rc_model(build_remote_rows(
            &self.remotes,
            self.selected_remote,
        )));
        let names: Vec<slint::SharedString> = self
            .remotes
            .iter()
            .map(|remote| slint::SharedString::from(remote.name.as_str()))
            .collect();
        window.set_remote_names(rc_model(names));
        window.set_remote_current(self.default_remote().unwrap_or_default().into());
        window.set_remote_selected(
            self.selected_remote
                .and_then(|index| i32::try_from(index).ok())
                .unwrap_or(-1),
        );
        window.set_remote_name_input(self.remote_name_input.clone().into());
        window.set_remote_url_input(self.remote_url_input.clone().into());
        window.set_remote_help(self.remote_help.clone().into());
        window.set_token_input(self.token_input.clone().into());
        window.set_token_host(self.target_host().unwrap_or_default().into());
        window.set_pull_strategy(self.pull_strategy);
        window.set_keyring_available(
            self.remote_listing
                .as_ref()
                .is_none_or(|listing| listing.keyring_available),
        );
    }

    /// 写推送预览遮罩。
    fn apply_push_panel(&self, window: &MainWindow) {
        window.set_push_visible(self.push_open);
        window.set_push_rows(rc_model(self.push_rows.clone()));
        window.set_push_summary(self.push_summary.clone().into());
        window.set_push_hint(self.push_hint.clone().into());
        window.set_push_force(self.push_force);
        window.set_push_set_upstream(self.push_set_upstream);
        window.set_push_creates_branch(
            self.push_preview
                .as_ref()
                .is_some_and(|preview| preview.creates_branch),
        );
        window.set_push_protected(
            self.push_preview
                .as_ref()
                .is_some_and(|preview| preview.protection.is_some()),
        );
    }

    /// 写网络进度条与失败提示。
    fn apply_network_strip(&self, window: &MainWindow) {
        window.set_network_busy(self.network.busy);
        window.set_network_label(self.network.label.clone().into());
        window.set_network_progress(self.network.line.clone().into());
        match self.network_failure.as_ref() {
            Some(failure) => {
                window.set_network_error_visible(true);
                window.set_network_error(network_error_text(failure).into());
            }
            None => window.set_network_error_visible(false),
        }
    }

    /// 写确认遮罩、提示条与加载指示。
    fn apply_overlays(&self, window: &MainWindow) {
        match &self.confirm {
            Some(confirm) => {
                let (title, detail, danger) = confirm.text();
                window.set_confirm_title(title.into());
                window.set_confirm_detail(detail.into());
                window.set_confirm_danger(danger);
                window.set_confirm_visible(true);
            }
            None => window.set_confirm_visible(false),
        }

        window.set_toast(
            self.toast
                .as_ref()
                .map_or("", |(text, _)| text.as_str())
                .into(),
        );
        window.set_log_loading(self.log_in_flight);
        window.set_diff_loading(self.diff_in_flight);
    }

    // ---------------------------------------------------------------- 选择

    /// 选中一条提交并请求它的文件级差异。
    pub fn select_log(&mut self, index: usize) {
        if index >= self.log.nodes.len() {
            return;
        }
        self.selected_log = Some(index);
        self.dirty.log = true;

        let node = self.log.nodes[index].clone();
        self.diff_title = format!("{} — {}", node.short_id(), node.summary);
        self.clear_diff_state();

        // 与第一个父提交比较；根提交用空树口径。
        let left = node
            .first_parent()
            .map_or(DiffSide::Empty, |parent| DiffSide::Rev(parent.to_hex()));
        self.diff_sides = (left, DiffSide::Rev(node.id.to_hex()));
        self.diff_path = None;
        self.diff_staged_view = false;
        self.request_diff(self.diff_sides.clone(), None);
    }

    /// 选中一条本地更改并请求它的差异。
    ///
    /// 差异的两侧按分区决定（M1 一律用 HEAD vs 工作区，导致"已暂存"条目
    /// 显示的是它相对 HEAD 的**全部**改动，反暂存按钮也无从判断语义）。
    pub fn select_change(&mut self, index: usize) {
        let Some(entry) = self.changes.entries.get(index).cloned() else {
            return;
        };
        self.selected_change = Some(index);
        self.dirty.changes = true;
        self.clear_diff_state();

        let untracked = entry.index == gt_core::StatusFlag::Untracked;
        let staged_view = entry.is_staged() && !entry.has_unstaged();
        self.diff_staged_view = staged_view;

        let sides = if untracked {
            (DiffSide::Empty, DiffSide::Worktree)
        } else if staged_view {
            // 已暂存：HEAD（或空树，尚未有提交时）vs 索引。
            let left = if entry.index == gt_core::StatusFlag::Added {
                DiffSide::Empty
            } else {
                DiffSide::Rev("HEAD".to_owned())
            };
            (left, DiffSide::Index)
        } else {
            // 未暂存：索引 vs 工作区。
            if entry.index == gt_core::StatusFlag::Added {
                (DiffSide::Empty, DiffSide::Worktree)
            } else {
                (DiffSide::Index, DiffSide::Worktree)
            }
        };

        self.diff_sides = sides.clone();
        self.diff_path = Some(entry.path.clone());
        self.diff_title = format!(
            "{}（{}）",
            entry.porcelain_path(),
            if staged_view {
                text::tr("已暂存")
            } else {
                text::tr("工作区")
            }
        );
        self.request_diff(sides, Some(entry.path.clone()));
    }

    /// 选中一条引用（分支 / 标签）。
    pub fn select_ref(&mut self, index: usize) {
        let ordered = ordered_refs(&self.refs);
        if index >= ordered.len() {
            return;
        }
        self.selected_ref = Some(index);
        let name = ordered[index].name.clone();
        if self.branch_input.is_empty() {
            self.branch_input.clone_from(&name);
        }
        self.dirty.refs = true;
    }

    // ---------------------------------------------------------------- 提交

    /// 设置提交消息。
    pub fn set_commit_message(&mut self, message: &str) {
        message.clone_into(&mut self.commit_message);
        self.dirty.commit = true;
    }

    /// 切换"修订上次提交"。
    pub fn set_amend(&mut self, amend: bool) {
        self.commit_amend = amend;
        self.dirty.commit = true;
    }

    /// 切换"重置作者"。
    pub fn set_reset_author(&mut self, reset: bool) {
        self.commit_reset_author = reset;
        self.dirty.commit = true;
    }

    /// 请求提交（先过检查，再投递写请求）。
    pub fn request_commit(&mut self) {
        let check = self.commit_check();
        if let Some(reason) = check.blocking {
            self.show_toast(text::trf("无法提交：{}", &[&reason]));
            return;
        }
        if check.has_warnings() {
            // 警告不阻断，但要出现在提示条上 —— 用户点一次提交就能看到。
            self.show_toast(check.warnings.join("；"));
        }
        self.submit_write(
            WriteRequest::Commit(gt_backend::CommitRequest {
                message: self.commit_message.clone(),
                amend: self.commit_amend,
                reset_author: self.commit_reset_author,
                allow_empty: false,
            }),
            PendingWrite::Commit,
        );
    }

    /// 当前提交检查结果。
    #[must_use]
    pub fn commit_check(&self) -> CommitCheck {
        let head = self.head.clone().unwrap_or(HeadInfo {
            kind: gt_core::HeadKind::Unborn,
            target: None,
            branch: None,
        });
        let partition = self.changes.partition();
        gt_core::check_commit(&CommitCheckInput {
            message: &self.commit_message,
            head: &head,
            state: self.state,
            amend: self.commit_amend,
            staged_empty: partition.staged.is_empty() && partition.conflicted.is_empty(),
            warn_detached: self.warn_detached,
        })
    }

    fn commit_hint(&self) -> String {
        let check = self.commit_check();
        if !check.warnings.is_empty() {
            return check.warnings.join("；");
        }
        let partition = self.changes.partition();
        if partition.staged.is_empty() && partition.conflicted.is_empty() {
            return text::tr("暂存区为空：先暂存要提交的内容").to_owned();
        }
        text::trf(
            text::tr("将提交 {} 个已暂存路径"),
            &[&(partition.staged.len() + partition.conflicted.len()).to_string()],
        )
    }

    // ------------------------------------------------------------ 文件级写操作

    /// 暂存当前选中的更改。
    pub fn stage_selected_change(&mut self) {
        let Some(paths) = self.selected_paths() else {
            self.show_toast(text::tr("请先选择一个文件"));
            return;
        };
        self.submit_write(WriteRequest::Stage(paths), PendingWrite::Other);
    }

    /// 反暂存当前选中的更改。
    pub fn unstage_selected_change(&mut self) {
        let Some(paths) = self.selected_paths() else {
            self.show_toast(text::tr("请先选择一个文件"));
            return;
        };
        self.submit_write(WriteRequest::Unstage(paths), PendingWrite::Other);
    }

    /// 请求丢弃当前选中更改的修改（需强确认）。
    pub fn request_discard_change(&mut self) {
        let Some(paths) = self.selected_paths() else {
            self.show_toast(text::tr("请先选择一个文件"));
            return;
        };
        self.confirm = Some(Confirm::DiscardChanges(paths));
        self.dirty.diff = true;
    }

    /// 请求删除选中的未跟踪路径（需强确认）。
    pub fn request_delete_untracked(&mut self) {
        let Some(paths) = self.selected_paths() else {
            self.show_toast(text::tr("请先选择一个未跟踪路径"));
            return;
        };
        self.confirm = Some(Confirm::DeleteUntracked(paths));
    }

    /// 把选中的未跟踪路径写入忽略规则（FR-LC-04）。
    pub fn ignore_selected(&mut self, use_info_exclude: bool) {
        let Some(path) = self
            .selected_change
            .and_then(|index| self.changes.entries.get(index))
            .map(|entry| entry.path.clone())
        else {
            self.show_toast(text::tr("请先选择一个路径"));
            return;
        };
        self.submit_write(
            WriteRequest::Ignore {
                target: if use_info_exclude {
                    gt_backend::IgnoreTarget::InfoExclude
                } else {
                    gt_backend::IgnoreTarget::Gitignore
                },
                path,
            },
            PendingWrite::Other,
        );
    }

    fn selected_paths(&self) -> Option<Vec<String>> {
        let entry = self
            .selected_change
            .and_then(|index| self.changes.entries.get(index))?;
        Some(vec![entry.path.clone()])
    }

    // -------------------------------------------------------- hunk / 行级写操作

    /// 切换某一行（或某个 hunk 头）的选择状态。
    ///
    /// - 点击 hunk 头：整块全选 / 全不选；
    /// - 点击增删行：切换该行；
    /// - 点击上下文行：忽略（`selectable == false`，界面已禁用点击）。
    pub fn toggle_diff_row(&mut self, index: usize) {
        let Some(row) = self.diff_rows.get(index).cloned() else {
            return;
        };
        if !row.selectable || row.hunk_index < 0 {
            return;
        }

        let hunk_index = row.hunk_index as usize;
        if row.line_index < 0 {
            // 整块切换：若当前已全选则全不选，否则全选。
            let changed = self.changed_rows_of_hunk(hunk_index);
            let all_selected = !changed.is_empty()
                && changed
                    .iter()
                    .all(|row_index| self.diff_selection.contains(row_index));
            for row_index in &changed {
                if all_selected {
                    self.diff_selection.remove(row_index);
                } else {
                    self.diff_selection.insert(*row_index);
                }
            }
        } else if self.diff_selection.contains(&index) {
            self.diff_selection.remove(&index);
        } else {
            self.diff_selection.insert(index);
        }
        self.rebuild_diff_rows();
    }

    /// 暂存所选（仅当当前差异是"索引 vs 工作区"）。
    pub fn request_stage_selection(&mut self) {
        self.request_selection(SelectionMode::Stage);
    }

    /// 反暂存所选（仅当当前差异是"HEAD vs 索引"）。
    pub fn request_unstage_selection(&mut self) {
        self.request_selection(SelectionMode::Unstage);
    }

    /// 丢弃所选（需强确认）。
    pub fn request_discard_selection(&mut self) {
        if self.diff_selection.is_empty() {
            self.show_toast(text::tr("请先在差异视图中选择 hunk 或行"));
            return;
        }
        let path = self.diff_path.clone().unwrap_or_default();
        self.confirm = Some(Confirm::DiscardSelection {
            path,
            lines: self.diff_selection.len(),
        });
    }

    fn request_selection(&mut self, mode: SelectionMode) {
        if !self.diff_selectable {
            self.show_toast(text::tr("当前差异不支持按块操作；新增文件请用「暂存」整文件操作"));
            return;
        }
        if self.diff_selection.is_empty() {
            self.show_toast(text::tr("请先在差异视图中选择 hunk 或行"));
            return;
        }
        let Some(diff) = self.diff.as_ref() else {
            return;
        };
        let Some(path) = self.diff_path.clone() else {
            self.show_toast(text::tr("当前差异不支持按块操作"));
            return;
        };
        let selections = self.selections_from_diff(diff);
        if selections.is_empty() {
            self.show_toast(text::tr("所选内容不包含可落库的变更"));
            return;
        }
        self.submit_write(
            WriteRequest::Selection {
                path,
                left: self.diff_sides.0.clone(),
                right: self.diff_sides.1.clone(),
                selections,
                expected_fingerprint: diff.fingerprint(),
                mode,
            },
            PendingWrite::Other,
        );
    }

    /// 把界面行选择换算成 hunk 选择集。
    ///
    /// 一个 hunk 的**全部变更行**都被选中时退回 [`HunkSelection::whole`]：
    /// 整块补丁比逐行补丁更短，也更不容易让 git apply 因为上下文不足而拒绝。
    fn selections_from_diff(&self, diff: &FileDiff) -> Vec<HunkSelection> {
        let Some(hunks) = diff.payload.hunks() else {
            return Vec::new();
        };
        let mut per_hunk: std::collections::BTreeMap<usize, Vec<usize>> =
            std::collections::BTreeMap::new();
        for row_index in &self.diff_selection {
            let Some(row) = self.diff_rows.get(*row_index) else {
                continue;
            };
            if row.hunk_index >= 0 && row.line_index >= 0 {
                per_hunk
                    .entry(row.hunk_index as usize)
                    .or_default()
                    .push(row.line_index as usize);
            }
        }

        let mut out = Vec::new();
        for (hunk_index, mut lines) in per_hunk {
            let Some(hunk) = hunks.get(hunk_index) else {
                continue;
            };
            lines.sort_unstable();
            lines.dedup();
            let changed_total = hunk
                .lines
                .iter()
                .filter(|line| line.kind.is_change())
                .count();
            if lines.len() >= changed_total {
                out.push(HunkSelection::whole(hunk_index));
            } else {
                out.push(HunkSelection::lines(hunk_index, lines));
            }
        }
        out
    }

    /// 取某个 hunk 的全部"可选择行"下标（界面行下标）。
    fn changed_rows_of_hunk(&self, hunk: usize) -> Vec<usize> {
        let hunk = i32::try_from(hunk).unwrap_or(-1);
        self.diff_rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.hunk_index == hunk && row.line_index >= 0 && row.selectable)
            .map(|(index, _)| index)
            .collect()
    }

    // ---------------------------------------------------------------- 分支

    /// 设置分支输入框内容。
    pub fn set_branch_input(&mut self, text: &str) {
        text.clone_into(&mut self.branch_input);
    }

    /// 新建分支（可同时检出）。
    pub fn request_branch_create(&mut self, checkout: bool) {
        let name = self.branch_input.trim().to_owned();
        if name.is_empty() {
            self.show_toast(text::tr("请先在输入框填写新分支名"));
            return;
        }
        let start = self.selected_ref_name();
        self.submit_write(
            WriteRequest::Branch(gt_backend::BranchOp::Create {
                name,
                start,
                checkout,
            }),
            PendingWrite::Other,
        );
    }

    /// 检出选中的引用（仅本地分支可直接检出）。
    pub fn request_branch_checkout(&mut self) {
        let Some(ordered) = self.selected_ref_entry() else {
            self.show_toast(text::tr("请先在侧栏选择一个分支"));
            return;
        };
        if ordered.kind != gt_core::RefKind::LocalBranch {
            // 远程分支与企业惯例：先建本地跟踪分支再检出；M2 只给出明确说明。
            self.show_toast(text::tr("只能直接检出本地分支；远程分支请先「新建」本地分支"));
            return;
        }
        self.submit_write(
            WriteRequest::Branch(gt_backend::BranchOp::Checkout { name: ordered.name }),
            PendingWrite::Other,
        );
    }

    /// 重命名选中的分支。
    pub fn request_branch_rename(&mut self) {
        let Some(entry) = self.selected_ref_entry() else {
            self.show_toast(text::tr("请先在侧栏选择一个分支"));
            return;
        };
        let to = self.branch_input.trim().to_owned();
        if to.is_empty() || to == entry.name {
            self.show_toast(text::tr("请在输入框填写新的分支名"));
            return;
        }
        self.submit_write(
            WriteRequest::Branch(gt_backend::BranchOp::Rename {
                from: entry.name,
                to,
            }),
            PendingWrite::Other,
        );
    }

    /// 请求删除选中的分支（需确认）。
    pub fn request_branch_delete(&mut self, force: bool) {
        let Some(entry) = self.selected_ref_entry() else {
            self.show_toast(text::tr("请先在侧栏选择一个分支"));
            return;
        };
        if entry.is_head {
            self.show_toast(text::tr("不能删除当前所在的分支"));
            return;
        }
        self.confirm = Some(Confirm::DeleteBranch {
            name: entry.name,
            force,
        });
    }

    /// 把选中的分支跟踪到 `origin/<同名>`（M2 的简化口径）。
    pub fn request_branch_upstream(&mut self) {
        let Some(entry) = self.selected_ref_entry() else {
            self.show_toast(text::tr("请先在侧栏选择一个分支"));
            return;
        };
        let upstream = format!("origin/{}", entry.name);
        self.submit_write(
            WriteRequest::Branch(gt_backend::BranchOp::SetUpstream {
                branch: entry.name,
                upstream,
            }),
            PendingWrite::Other,
        );
    }

    /// 检出选中的提交（分离头指针，需确认）。
    pub fn request_checkout_selected_commit(&mut self) {
        let Some(node) = self
            .selected_log
            .and_then(|index| self.log.nodes.get(index))
            .cloned()
        else {
            self.show_toast(text::tr("请先在提交列表中选择一条提交"));
            return;
        };
        self.confirm = Some(Confirm::CheckoutDetached {
            revision: node.id.short(),
        });
    }

    fn selected_ref_entry(&self) -> Option<gt_core::RefInfo> {
        let ordered = ordered_refs(&self.refs);
        self.selected_ref
            .and_then(|index| ordered.get(index).cloned())
    }

    fn selected_ref_name(&self) -> Option<String> {
        self.selected_ref_entry()
            .filter(|entry| entry.kind != gt_core::RefKind::Tag)
            .map(|entry| entry.name)
    }

    /// 删除侧栏里选中的标签（FR-BR-03）。
    ///
    /// 只删本地标签：`git tag -d` 不动远端，推送/删除远端标签属于网络操作，
    /// 需要单独的远端交互（M5 评估）。
    pub fn delete_selected_tag(&mut self) {
        let Some(entry) = self
            .selected_ref_entry()
            .filter(|entry| entry.kind == gt_core::RefKind::Tag)
        else {
            self.show_toast(text::tr("请先在侧栏选中一个标签"));
            return;
        };
        self.submit_write(
            WriteRequest::Tag(TagOp::Delete { name: entry.name }),
            PendingWrite::Other,
        );
    }

    // ---------------------------------------------------------------- 搜索

    /// 设置搜索关键字。
    pub fn set_search_query(&mut self, query: &str) {
        query.clone_into(&mut self.search_query);
    }

    /// 切换正则模式。
    pub fn set_search_regex(&mut self, regex: bool) {
        self.search_regex = regex;
    }

    /// 执行搜索。
    pub fn run_search(&mut self) {
        let query = self.search_query.trim().to_owned();
        if query.is_empty() {
            self.show_toast(text::tr("请输入搜索内容"));
            return;
        }
        self.search_active = true;
        self.dirty.log = true;
        self.log_in_flight = self
            .service
            .request(
                self.handle.id(),
                RequestKind::Search(SearchRequest {
                    query,
                    regex: self.search_regex,
                    case_sensitive: false,
                    all_refs: self.graph.sort == gt_core::LogSort::Topological,
                    limit: 500,
                }),
                self.sender.clone(),
            )
            .is_ok();
    }

    /// 退出搜索，回到正常历史。
    pub fn clear_search(&mut self) {
        if !self.search_active {
            return;
        }
        self.search_active = false;
        self.search_query.clear();
        self.refresh();
    }

    /// 切换图选项（0 拓扑 / 1 第一父 / 2 隐藏合并 / 3 折叠线性）。
    pub fn toggle_option(&mut self, option: usize) {
        match option {
            0 => {
                self.graph.sort = if self.graph.sort == gt_core::LogSort::Topological {
                    gt_core::LogSort::Date
                } else {
                    gt_core::LogSort::Topological
                };
            }
            1 => self.graph.first_parent = !self.graph.first_parent,
            2 => self.graph.hide_merges = !self.graph.hide_merges,
            3 => self.graph.collapse_linear = !self.graph.collapse_linear,
            _ => return,
        }
        // 图选项改变 id 列表：缓存作废、布局重来、从第 0 页重新加载。
        self.handle.invalidate_log_index();
        self.log = LogState::default();
        self.selected_log = None;
        self.search_active = false;
        self.dirty.log = true;
        self.clear_in_flight();
        self.request_log_page(0);
        self.request_overview();
    }

    // ---------------------------------------------------------------- 远端

    /// 应用远端相关设置（更新策略 / 受保护分支 / 已保存 token 的主机）。
    ///
    /// 只在值真的变化时标脏：这些值来自共享配置，每次轮询都会被对齐一次。
    pub fn set_remote_preferences(
        &mut self,
        strategy: PullStrategy,
        patterns: Vec<String>,
        stored_hosts: Vec<String>,
    ) {
        let index = strategy_index(strategy);
        if self.pull_strategy == index
            && self.protected_patterns == patterns
            && self.stored_hosts == stored_hosts
        {
            return;
        }
        self.pull_strategy = index;
        self.protected_patterns = patterns;
        self.stored_hosts = stored_hosts;
        self.dirty.remotes = true;
    }

    /// 当前的更新策略。
    #[must_use]
    pub fn pull_strategy(&self) -> PullStrategy {
        strategy_from_index(self.pull_strategy)
    }

    /// 界面选中的推送/更新目标远端。
    ///
    /// 顺序：用户显式选中的 → `origin` → 第一个远端。空仓库无远端时返回 [`None`]，
    /// 界面据此提示"先添加远端"而不是让按钮点了没反应。
    #[must_use]
    pub fn default_remote(&self) -> Option<String> {
        if let Some(name) = self
            .selected_remote
            .and_then(|index| self.remotes.get(index))
            .map(|remote| remote.name.clone())
        {
            return Some(name);
        }
        if let Some(origin) = self.remotes.iter().find(|remote| remote.name == "origin") {
            return Some(origin.name.clone());
        }
        self.remotes.first().map(|remote| remote.name.clone())
    }

    /// 打开远端管理遮罩并拉取列表。
    pub fn open_remotes(&mut self) {
        self.remotes_open = true;
        self.dirty.remotes = true;
        self.request_remotes();
    }

    /// 关闭远端管理遮罩。
    pub fn close_remotes(&mut self) {
        self.remotes_open = false;
        self.dirty.remotes = true;
    }

    /// 请求远端列表（只读，只读模式下也可用）。
    pub fn request_remotes(&mut self) {
        self.submit_network(NetworkRequest::Remotes, false);
    }

    /// 首屏静默拉取远端列表（不弹"正在读取远端…"）。
    ///
    /// 首屏已经在同时发三个读请求，再弹一条提示只会让用户以为出了问题；
    /// 拉不到列表也不影响任何主流程（远端名在工具栏留空）。
    pub fn prime_remotes(&mut self) {
        self.submit_network(NetworkRequest::Remotes, true);
    }

    /// 选中一个远端，并把它的信息回填到输入框。
    pub fn select_remote(&mut self, index: usize) {
        let Some(remote) = self.remotes.get(index).cloned() else {
            return;
        };
        self.selected_remote = Some(index);
        self.remote_name_input.clone_from(&remote.name);
        remote.display_url().clone_into(&mut self.remote_url_input);
        self.token_input.clear();
        self.dirty.remotes = true;
    }

    /// 编辑远端名输入框。
    pub fn set_remote_name(&mut self, text: &str) {
        text.clone_into(&mut self.remote_name_input);
        self.dirty.remotes = true;
    }

    /// 编辑远端地址输入框。
    pub fn set_remote_url(&mut self, text: &str) {
        text.clone_into(&mut self.remote_url_input);
        self.dirty.remotes = true;
    }

    /// 编辑 token 输入框。
    pub fn set_token_input(&mut self, text: &str) {
        text.clone_into(&mut self.token_input);
        self.dirty.remotes = true;
    }

    /// 新增远端（FR-RMT-01）。
    pub fn add_remote(&mut self) {
        let name = self.remote_name_input.trim().to_owned();
        let url = self.remote_url_input.trim().to_owned();
        if name.is_empty() || url.is_empty() {
            self.show_toast(text::tr("请填写远端名称与地址"));
            return;
        }
        self.submit_network(
            NetworkRequest::EditRemote(RemoteEdit::Add { name, url }),
            false,
        );
    }

    /// 删除选中的远端。
    pub fn remove_remote(&mut self) {
        let Some(name) = self.selected_remote_name() else {
            self.show_toast(text::tr("请先选择一个远端"));
            return;
        };
        self.submit_network(
            NetworkRequest::EditRemote(RemoteEdit::Remove { name }),
            false,
        );
    }

    /// 把选中的远端的地址改成输入框里的值。
    pub fn set_remote_url_from_input(&mut self) {
        let Some(name) = self.selected_remote_name() else {
            self.show_toast(text::tr("请先选择一个远端"));
            return;
        };
        let url = self.remote_url_input.trim().to_owned();
        if url.is_empty() {
            self.show_toast(text::tr("地址不能为空"));
            return;
        }
        self.submit_network(
            NetworkRequest::EditRemote(RemoteEdit::SetUrl {
                name,
                url,
                push: false,
            }),
            false,
        );
    }

    /// 把输入框里的 token 存进系统密钥链（FR-RMT-04）。
    pub fn save_token(&mut self) {
        let Some(host) = self.target_host() else {
            self.show_toast(text::tr("当前远端不是 HTTPS 地址，无法保存 token"));
            return;
        };
        let secret = self.token_input.trim().to_owned();
        if secret.is_empty() {
            self.show_toast(text::tr("请先填写 token"));
            return;
        }
        let username = self.selected_remote_username();
        self.submit_network(
            NetworkRequest::SaveToken {
                host,
                username,
                secret,
            },
            false,
        );
        self.token_input.clear();
        self.dirty.remotes = true;
    }

    /// 删除当前主机保存的 token。
    pub fn forget_token(&mut self) {
        let Some(host) = self.target_host() else {
            self.show_toast(text::tr("当前远端不是 HTTPS 地址，没有可删除的 token"));
            return;
        };
        self.submit_network(NetworkRequest::ForgetToken { host }, false);
    }

    /// 获取更新：`all` 为真时拉取全部远端。
    pub fn request_fetch(&mut self, all: bool) {
        let remote = if all {
            None
        } else {
            let Some(remote) = self.default_remote() else {
                self.show_toast(text::tr("该仓库没有配置远端，请先在「远端」里添加"));
                return;
            };
            Some(remote)
        };
        self.submit_network(
            NetworkRequest::Fetch(FetchRequest {
                remote,
                // 默认清理远端已删除的跟踪分支（与 IDEA 的 Update 行为一致）。
                prune: true,
                tags: false,
            }),
            false,
        );
    }

    /// 拉取并整合（FR-RMT-02）。
    pub fn request_update(&mut self) {
        let Some(remote) = self.default_remote() else {
            self.show_toast(text::tr("该仓库没有配置远端，请先在「远端」里添加"));
            return;
        };
        let branch = self.head.as_ref().and_then(|head| head.branch.clone());
        self.submit_network(
            NetworkRequest::Pull(PullRequest {
                remote,
                branch,
                strategy: self.pull_strategy(),
            }),
            false,
        );
    }

    /// 请求推送预览（先看再推，FR-RMT-03）。
    pub fn request_push(&mut self) {
        let Some(remote) = self.default_remote() else {
            self.show_toast(text::tr("该仓库没有配置远端，请先在「远端」里添加"));
            return;
        };
        let branch = self.head.as_ref().and_then(|head| head.branch.clone());
        self.awaiting_preview = true;
        self.submit_network(
            NetworkRequest::Preview(PushPreviewRequest {
                remote,
                branch,
                protected_patterns: self.protected_patterns.clone(),
            }),
            false,
        );
    }

    /// 关闭推送预览。
    pub fn push_cancel(&mut self) {
        self.push_open = false;
        self.awaiting_preview = false;
        self.dirty.push = true;
    }

    /// 切换强制推送选项。
    pub fn toggle_push_force(&mut self, on: bool) {
        self.push_force = on;
        self.refresh_push_hint();
        self.dirty.push = true;
    }

    /// 切换"设置上游"。
    pub fn toggle_push_upstream(&mut self, on: bool) {
        self.push_set_upstream = on;
        self.dirty.push = true;
    }

    /// 切换更新策略（0 合并 / 1 变基 / 2 仅快进）。
    pub fn set_pull_strategy(&mut self, index: i32) {
        self.pull_strategy = index.clamp(0, 2);
        self.dirty.remotes = true;
    }

    /// 确认推送（强制推送先走二次确认）。
    pub fn push_confirm(&mut self) {
        let Some(preview) = self.push_preview.clone() else {
            return;
        };
        // 受保护分支即使勾了强制也不允许（FR-RMT-03），这里再拦一次。
        let force = self.push_force && preview.protection.is_none();
        if preview.is_empty() && !force {
            self.show_toast(text::tr("没有需要推送的提交"));
            return;
        }
        if force {
            self.push_open = false;
            self.dirty.push = true;
            self.confirm = Some(Confirm::ForcePush {
                remote: preview.remote.clone(),
                branch: preview.remote_branch.clone(),
                commits: preview.ahead.max(preview.commits.len()),
            });
            return;
        }
        self.submit_push(&preview, false);
    }

    /// 取消正在进行的网络操作（FR-RMT-05）。
    pub fn cancel_network(&mut self) {
        if self.handle.cancel_network() {
            self.show_toast(text::tr("已请求取消，正在终止 git 子进程…"));
        } else {
            self.show_toast(text::tr("当前没有进行中的网络操作"));
        }
        self.dirty.network = true;
    }

    /// 关闭远端失败提示条。
    pub fn dismiss_network_error(&mut self) {
        self.network_failure = None;
        self.dirty.network = true;
    }

    /// 取走待写回配置的事件。
    pub fn take_config_events(&mut self) -> Vec<ConfigEvent> {
        std::mem::take(&mut self.config_events)
    }

    /// 最近一次网络失败（供测试与状态栏使用）。
    #[must_use]
    pub fn network_failure(&self) -> Option<&NetworkFailure> {
        self.network_failure.as_ref()
    }

    /// 推送预览（供测试与状态栏使用）。
    #[must_use]
    pub fn push_preview(&self) -> Option<&gt_backend::PushPreview> {
        self.push_preview.as_ref()
    }

    fn selected_remote_name(&self) -> Option<String> {
        self.selected_remote
            .and_then(|index| self.remotes.get(index))
            .map(|remote| remote.name.clone())
    }

    /// 凭据操作的目标远端：优先用户显式选中的，其次默认远端。
    ///
    /// 不能让"没点过列表"就导致 token 按钮全灰：只有单一远端的仓库是最常见的
    /// 情形，那时默认远端就是用户心里的那个。
    fn target_remote(&self) -> Option<&RemoteInfo> {
        if let Some(remote) = self
            .selected_remote
            .and_then(|index| self.remotes.get(index))
        {
            return Some(remote);
        }
        let name = self.default_remote()?;
        self.remotes.iter().find(|remote| remote.name == name)
    }

    /// 当前目标的 HTTPS 主机名（用于密钥链存取）。
    fn target_host(&self) -> Option<String> {
        gt_backend::host_of(self.target_remote()?.display_url())
    }

    /// 从 URL 的 userinfo 段取用户名（为空时由服务层补占位符）。
    fn selected_remote_username(&self) -> String {
        let Some(url) = self.target_remote().map(RemoteInfo::display_url) else {
            return String::new();
        };
        let Some((_, rest)) = url.split_once("://") else {
            return String::new();
        };
        let authority = rest.split('/').next().unwrap_or(rest);
        let Some((userinfo, _)) = authority.rsplit_once('@') else {
            return String::new();
        };
        let user = userinfo.split(':').next().unwrap_or_default();
        if user == "***" {
            // URL 里的口令被脱敏后用户名可能整体变成 `***`：此时不猜。
            String::new()
        } else {
            user.to_owned()
        }
    }

    /// 投递一次推送。
    fn submit_push(&mut self, preview: &gt_backend::PushPreview, force: bool) {
        self.push_open = false;
        self.dirty.push = true;
        self.submit_network(
            NetworkRequest::Push {
                request: PushRequest {
                    remote: preview.remote.clone(),
                    targets: vec![PushTarget {
                        local_branch: preview.local_branch.clone(),
                        remote_branch: preview.remote_branch.clone(),
                    }],
                    force_with_lease: force,
                    set_upstream: self.push_set_upstream,
                    tags: false,
                },
                protected_patterns: self.protected_patterns.clone(),
            },
            false,
        );
    }

    /// 投递一次网络请求。
    ///
    /// `silent` 为真时不弹"正在…"提示：用于首屏的静默预取。
    fn submit_network(&mut self, request: NetworkRequest, silent: bool) {
        let label = request.label();
        match self.service.request(
            self.handle.id(),
            RequestKind::Network(Box::new(request)),
            self.sender.clone(),
        ) {
            Ok(_) => {
                self.network = self.handle.network_snapshot();
                self.dirty.network = true;
                if !silent {
                    self.show_toast(text::trf("正在{}…", &[label]));
                }
            }
            Err(err) => {
                // 预览请求失败时不能把 awaiting 留在真：否则下次用户点 Push
                // 会莫名其妙弹出一个旧预览。
                self.awaiting_preview = false;
                self.show_toast(err.to_string());
            }
        }
    }

    /// 重算推送提示行（选项变化时）。
    fn refresh_push_hint(&mut self) {
        self.push_hint = self
            .push_preview
            .as_ref()
            .map_or_else(String::new, |preview| push_hints(preview, self.push_force));
    }

    /// 应用一次网络结果。
    fn apply_network(&mut self, outcome: NetworkOutcome) -> bool {
        self.network = self.handle.network_snapshot();
        self.dirty.network = true;
        match outcome {
            NetworkOutcome::Remotes(listing) => {
                self.remotes.clone_from(&listing.remotes);
                if self
                    .selected_remote
                    .is_some_and(|index| index >= self.remotes.len())
                {
                    self.selected_remote = None;
                }
                self.remote_help = credential_help(&listing, &self.stored_hosts);
                self.remote_listing = Some(*listing);
                self.dirty.remotes = true;
                true
            }
            NetworkOutcome::Preview(preview) => {
                self.push_summary = push_preview_text(&preview);
                self.push_rows = build_push_rows(&preview, now_unix());
                // 已有上游时默认不勾"设置上游"；新分支默认勾上（否则下次推送
                // 仍然没有跟踪关系，ahead/behind 永远是 0）。
                self.push_set_upstream = preview.upstream.is_none();
                self.push_preview = Some(*preview);
                self.refresh_push_hint();
                if self.awaiting_preview {
                    self.awaiting_preview = false;
                    self.push_open = true;
                }
                self.dirty.push = true;
                true
            }
            NetworkOutcome::Done(result) => {
                self.network_failure = None;
                self.show_toast(result.summary.clone());
                // 引用可能已变：整体刷新（概览 / 状态 / 日志）。
                self.refresh();
                true
            }
            NetworkOutcome::Token(token) => {
                self.show_toast(token.message.clone());
                self.config_events.push(if token.saved {
                    ConfigEvent::TokenSaved(token.host.clone())
                } else {
                    ConfigEvent::TokenForgotten(token.host.clone())
                });
                self.request_remotes();
                true
            }
            NetworkOutcome::Failed(failure) => {
                self.awaiting_preview = false;
                self.push_open = false;
                self.show_toast(
                    failure
                        .summary
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .to_owned(),
                );
                self.network_failure = Some(*failure);
                self.dirty.network = true;
                self.dirty.push = true;
                true
            }
        }
    }

    // ---------------------------------------------------------------- 确认

    /// 确认当前待确认操作。
    pub fn confirm_yes(&mut self) {
        let Some(pending) = self.confirm.take() else {
            return;
        };
        let request = match pending {
            Confirm::DiscardChanges(paths) => WriteRequest::Discard(paths),
            Confirm::DeleteUntracked(paths) => WriteRequest::DeleteUntracked(paths),
            Confirm::DiscardSelection { .. } => {
                // 选择集在确认期间可能已被改动：重新取一次当前选择。
                if self.diff_selection.is_empty() {
                    self.show_toast(text::tr("选择已失效，请重新选择"));
                    return;
                }
                let Some(diff) = self.diff.as_ref() else {
                    return;
                };
                let selections = self.selections_from_diff(diff);
                WriteRequest::Selection {
                    path: self.diff_path.clone().unwrap_or_default(),
                    left: self.diff_sides.0.clone(),
                    right: self.diff_sides.1.clone(),
                    selections,
                    expected_fingerprint: diff.fingerprint(),
                    mode: SelectionMode::Discard,
                }
            }
            Confirm::DeleteBranch { name, force } => {
                WriteRequest::Branch(gt_backend::BranchOp::Delete { name, force })
            }
            Confirm::CheckoutDetached { revision } => {
                WriteRequest::Branch(gt_backend::BranchOp::CheckoutDetached { revision })
            }
            Confirm::Reset { target, mode } => WriteRequest::Reset(ResetRequest { target, mode }),
            Confirm::TakeSide { path, stage } => WriteRequest::TakeSide { path, stage },
            Confirm::StashDrop { index, .. } => WriteRequest::Stash(StashOp::Drop { index }),
            Confirm::RebaseSkip => WriteRequest::RebaseControl(RebaseControl::Skip),
            Confirm::AbortInProgress { state } => abort_request(state),
            Confirm::ForcePush { .. } => {
                // 强制推送是网络操作，不走写通道。
                if let Some(preview) = self.push_preview.clone() {
                    self.push_force = true;
                    self.submit_push(&preview, true);
                }
                return;
            }
        };
        self.submit_write(request, PendingWrite::Other);
    }

    /// 取消当前待确认操作。
    pub fn confirm_no(&mut self) {
        self.confirm = None;
        self.dirty.diff = true;
        self.dirty.changes = true;
    }

    /// 是否有待确认的操作。
    #[must_use]
    pub const fn has_pending_confirm(&self) -> bool {
        self.confirm.is_some()
    }

    // ---------------------------------------------------------------- 其它

    /// 导出 Console 审计文本。
    #[must_use]
    pub fn export_console(&self) -> String {
        self.console.export_text()
    }

    /// 清空 Console。
    pub fn clear_console(&mut self) {
        self.console.clear();
        self.dirty.console = true;
    }

    /// 已加载的提交数（供状态栏与诊断使用）。
    #[must_use]
    pub fn loaded_commits(&self) -> usize {
        self.log.nodes.len()
    }

    /// 当前选中的提交下标。
    #[must_use]
    pub const fn selected_log(&self) -> Option<usize> {
        self.selected_log
    }

    /// 当前选中的本地更改下标。
    #[must_use]
    pub const fn selected_change(&self) -> Option<usize> {
        self.selected_change
    }

    /// 是否只读。
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        self.read_only.is_some()
    }

    /// 各通道是否仍有在飞请求（诊断与测试用）。
    #[must_use]
    pub const fn in_flight(&self) -> [bool; 5] {
        [
            self.overview_in_flight,
            self.status_in_flight,
            self.log_in_flight,
            self.diff_in_flight,
            self.write_in_flight,
        ]
    }

    /// 无仓库时也允许打开 Console 标签（导出诊断信息）。
    pub fn render_empty(&mut self, window: &MainWindow, recent: &[gt_config::RecentRepo]) {
        window.set_has_repo(false);
        window.set_recent(rc_model(crate::view_model::build_recent_rows(recent)));
        window.set_git_text(
            match self.service.capabilities().git_version.as_deref() {
                Some(version) => format!("git {version}"),
                None => text::tr("未找到 git（只读模式）").to_owned(),
            }
            .into(),
        );
        match self.service.capabilities().cli_unavailable_reason() {
            Some(reason) => {
                window.set_cli_missing(true);
                window.set_cli_missing_reason(reason.into());
            }
            None => window.set_cli_missing(false),
        }
        window.set_read_only(false);
    }

    // -------------------------------------------------------------- 内部实现

    fn show_toast(&mut self, text: impl Into<String>) {
        self.toast = Some((text.into(), TOAST_TICKS));
    }

    /// 投递一次写请求。
    ///
    /// 写通道不参与代次作废，因此这里不需要"先清在飞标记"那套逻辑；
    /// 只读仓库会在服务层被直接拒绝并返回原因。
    fn submit_write(&mut self, request: WriteRequest, kind: PendingWrite) {
        if self.write_in_flight {
            self.show_toast(text::tr("上一个操作还未完成，请稍候"));
            return;
        }
        let label = request.label();
        match self.service.request(
            self.handle.id(),
            RequestKind::Write(Box::new(request)),
            self.sender.clone(),
        ) {
            Ok(_) => {
                self.write_in_flight = true;
                self.pending_write = Some(kind);
                self.show_toast(text::trf("正在{}…", &[label]));
            }
            Err(err) => {
                self.show_toast(err.to_string());
            }
        }
    }

    fn request_overview(&mut self) {
        if self.overview_in_flight {
            return;
        }
        self.overview_in_flight = self
            .service
            .request(self.handle.id(), RequestKind::Overview, self.sender.clone())
            .is_ok();
    }

    fn request_status(&mut self) {
        if self.status_in_flight {
            return;
        }
        self.status_in_flight = self
            .service
            .request(self.handle.id(), RequestKind::Status, self.sender.clone())
            .is_ok();
    }

    fn request_log_page(&mut self, offset: usize) {
        if self.log_in_flight || self.log.exhausted {
            return;
        }
        let mut request = first_log_page();
        request.offset = offset;
        request.limit = LOG_PAGE_SIZE;
        request.graph = self.graph;
        self.log_in_flight = self
            .service
            .request(
                self.handle.id(),
                RequestKind::LogPage(request),
                self.sender.clone(),
            )
            .is_ok();
        if self.log_in_flight {
            self.dirty.header = true;
        }
    }

    fn request_diff(&mut self, sides: (DiffSide, DiffSide), path: Option<String>) {
        if self.diff_in_flight {
            return;
        }
        let Some(path) = path.or_else(|| self.diff_path.clone()) else {
            self.dirty.diff = true;
            return;
        };
        self.diff_in_flight = self
            .service
            .request(
                self.handle.id(),
                RequestKind::FileDiff {
                    left: sides.0,
                    right: sides.1,
                    path,
                },
                self.sender.clone(),
            )
            .is_ok();
        if !self.diff_in_flight {
            self.dirty.diff = true;
        }
    }

    /// 重新请求当前差异（写操作后刷新）。
    fn reload_diff(&mut self) {
        if self.diff_path.is_none() && self.diff_sides.1 == DiffSide::Worktree {
            return;
        }
        self.diff_in_flight = false;
        self.diff_selection.clear();
        self.request_diff(self.diff_sides.clone(), None);
    }

    /// 清空差异状态（切换选中项时）。
    fn clear_diff_state(&mut self) {
        self.diff = None;
        self.diff_rows.clear();
        self.diff_selection.clear();
        self.diff_selectable = false;
        self.dirty.diff = true;
    }

    /// 重建差异行模型（选择状态变化后调用）。
    fn rebuild_diff_rows(&mut self) {
        self.diff_rows = self
            .diff
            .as_ref()
            .map_or_else(Vec::new, |diff| build_diff_rows(diff, &self.diff_selection));
        self.dirty.diff = true;
    }

    /// 合并一条结果；返回是否改变了视图状态。
    ///
    /// 这是一个按 `Outcome` 平铺的分派表，长度即"结果类型有多少种"。
    /// 拆成多函数只会把"每种结果由谁消费"藏进调用链里。
    #[allow(clippy::too_many_lines)]
    fn apply(&mut self, delivery: Delivery) -> bool {
        let lane = delivery.kind.lane();
        match delivery.outcome {
            Outcome::Overview(overview) => {
                self.overview_in_flight = false;
                self.head = Some(overview.head.clone());
                self.refs.clone_from(&overview.refs);
                self.state = overview.state;
                self.user_email.clone_from(&overview.user_email);
                self.read_only.clone_from(&overview.read_only);
                self.rebase.clone_from(&overview.rebase);
                self.dirty.header = true;
                self.dirty.refs = true;
                self.dirty.commit = true;
                true
            }
            Outcome::Status(changes) => {
                self.status_in_flight = false;
                self.changes = (*changes).clone();
                // 选中项可能已不存在（文件被提交或丢弃）。
                if self
                    .selected_change
                    .is_some_and(|index| index >= self.changes.entries.len())
                {
                    self.selected_change = None;
                }
                self.dirty.changes = true;
                self.dirty.commit = true;
                true
            }
            Outcome::LogPage(page) => {
                self.log_in_flight = false;
                self.log.total = page.total;
                self.log.exhausted = page.page.exhausted;
                self.log.note.clone_from(&page.note);
                let start = page.page.offset;
                append_log_page(&mut self.log, start, page.page.nodes);
                self.dirty.log = true;
                true
            }
            Outcome::TreeChanges(changes) => {
                self.diff_in_flight = false;
                // 这里的 TreeChanges 只服务"两提交比较 / 快照浏览"（FR-LOG-08）：
                // 若当前没有比较视图在等结果，就当作无人认领的响应丢弃。
                if let Some(compare) = self.compare.as_mut() {
                    compare.files = changes;
                    compare.selected = None;
                    self.dirty.compare = true;
                }
                true
            }
            Outcome::ImageDiff(result) => {
                self.diff_in_flight = false;
                self.diff = None;
                self.diff_rows.clear();
                self.diff_selection.clear();
                self.diff_selectable = false;
                self.image_diff = Some(*result);
                self.dirty.diff = true;
                true
            }
            Outcome::Blame(result) => {
                self.blame = Some(BlameState {
                    path: result.path.clone(),
                    spec: result.spec.clone(),
                    rows: result.rows.clone(),
                    selected: None,
                    truncated: result.truncated,
                });
                self.dirty.blame = true;
                // 链式导航的滚动锚点由 apply_blame_panel 消费。
                if let Some(anchor) = result.anchor {
                    self.blame_pending_anchor = Some(anchor);
                }
                true
            }
            Outcome::FileHistory(result) => {
                // 若遮罩已关（用户不等了），丢弃过期结果。
                if let Some(state) = self.file_history.as_mut() {
                    state.rows.clone_from(&result.rows);
                    state.all_refs = result.all_refs;
                    state.selected = None;
                    state.truncated = result.truncated;
                    self.dirty.history = true;
                }
                true
            }
            Outcome::FileDiff(diff) => {
                self.diff_in_flight = false;
                self.image_diff = None;
                self.diff_selection.clear();
                // 能否按块落库取决于两侧来源：补丁只能作用在"索引有该文件"的路径上，
                // 新增/未跟踪文件必须整文件暂存（补丁头需要 new file mode，而
                // `gt-diff` 不生成那种头 —— 这是刻意的，整文件场景走文件级操作更稳）。
                self.diff_selectable = diff_side_supports_selection(&self.diff_sides)
                    && gt_diff::is_hunk_selectable(&diff);
                self.diff = Some(*diff);
                self.rebuild_diff_rows();
                true
            }
            Outcome::Search(result) => {
                self.log_in_flight = false;
                self.replace_log_with_search(result.nodes, &result.query, result.timed_out);
                true
            }
            Outcome::Write(outcome) => {
                self.write_in_flight = false;
                let pending = self.pending_write.take();
                self.show_toast(format!(
                    "{}{}",
                    outcome.summary,
                    if outcome.detail.is_empty() {
                        String::new()
                    } else {
                        format!("（{}）", first_line(&outcome.detail))
                    }
                ));
                if pending == Some(PendingWrite::Commit) {
                    self.commit_message.clear();
                    self.commit_amend = false;
                    self.dirty.commit = true;
                }
                // 写后统一刷新：状态、概览、日志都重新取一次。
                self.refresh();
                true
            }
            Outcome::Network(outcome) => self.apply_network(*outcome),
            Outcome::Advanced(result) => {
                self.write_in_flight = false;
                self.pending_write = None;
                self.apply_advanced_result(&result);
                true
            }
            Outcome::Conflicts(entries) => {
                // 记住当前正在编辑的是哪个文件：刷新后列表可能变短（刚解决掉一个），
                // 若只按下标沿用，会悄悄把用户切到另一个文件上。
                let previous = self
                    .conflict_selected
                    .and_then(|index| self.conflicts.get(index))
                    .map(|entry| entry.path.clone());
                self.conflicts = entries;

                if self.conflicts.is_empty() {
                    // 冲突全部解决：遮罩自动收起，不再显示已过期的文件内容。
                    self.conflict_selected = None;
                    self.conflict_file = None;
                    self.conflict_resolutions.clear();
                    self.conflict_block = None;
                    self.conflict_manual.clear();
                } else {
                    let same = previous.as_ref().and_then(|path| {
                        self.conflicts.iter().position(|entry| entry.path == *path)
                    });
                    let index = same.unwrap_or(0);
                    self.conflict_selected = Some(index);
                    if same.is_none() {
                        // 原文件已解决（或首次打开）：载入第一个仍冲突的文件。
                        self.conflict_file = None;
                        self.conflict_resolutions.clear();
                        self.conflict_block = None;
                    }
                    let path = self.conflicts[index].path.clone();
                    self.request_conflict_file(&path);
                }
                self.dirty.cfl = true;
                true
            }
            Outcome::ConflictFile(file) => {
                let mut resolutions = vec![None; file.hunk_count()];
                // 两侧完全相同的块自动取一侧：这是唯一不会误伤的自动动作，
                // 也是 IDEA 的默认行为（用户不需要为"双方写成一样"再点一次）。
                for (index, hunk) in file.hunks().enumerate() {
                    if hunk.identical_sides() {
                        resolutions[index] = Some(gt_diff::Resolution::Ours);
                    }
                }
                self.conflict_resolutions = resolutions;
                self.conflict_block = None;
                self.conflict_manual.clear();
                self.conflict_file = Some(*file);
                self.dirty.cfl = true;
                true
            }
            Outcome::RebasePlan(plan) => {
                self.plan_selected = None;
                self.plan_message.clear();
                self.plan = Some(*plan);
                self.dirty.iplan = true;
                true
            }
            Outcome::StashList(list) => {
                self.stashes = list;
                self.dirty.stash = true;
                true
            }
            Outcome::Failed(message) => {
                // 失败也要把在飞标记清掉，否则界面会永远显示"加载中"。
                if lane == gt_service::RequestLane::Write {
                    self.write_in_flight = false;
                    self.pending_write = None;
                } else {
                    self.clear_in_flight();
                }
                tracing::warn!(kind = delivery.kind.label(), %message, "{}", text::tr("请求失败"));
                self.show_toast(text::trf("{}失败：{}", &[delivery.kind.label(), &message]));
                if self.diff.is_none() {
                    self.diff_title = text::trf("读取失败：{}", &[&message]);
                    self.dirty.diff = true;
                }
                true
            }
        }
    }

    /// 用搜索结果替换日志列表。
    fn replace_log_with_search(&mut self, nodes: Vec<CommitMeta>, query: &str, timed_out: bool) {
        self.log = LogState::default();
        self.selected_log = None;
        self.log.total = nodes.len();
        self.log.exhausted = true;
        self.log.note = timed_out
            .then(|| text::trf("搜索「{}」超过 1.5 秒已中止；大仓库可等待后续版本的历史索引", &[query]));
        append_log_page(&mut self.log, 0, nodes);
        self.diff_title = text::trf("搜索：{}", &[query]);
        self.clear_diff_state();
        self.dirty.log = true;
    }

    /// 滚动接近末尾时预取下一页。
    fn maybe_prefetch(&mut self) -> bool {
        if self.search_active || self.log.exhausted || self.log_in_flight {
            return false;
        }
        let loaded = self.log.nodes.len();
        if loaded == 0 || self.log.total <= loaded {
            return false;
        }
        if self.log.total - loaded <= PREFETCH_MARGIN {
            self.request_log_page(loaded);
            return true;
        }
        false
    }

    fn log_summary(&self) -> String {
        let base = text::trf(
            text::tr("已加载 {} / {} 条"),
            &[&self.log.nodes.len().to_string(), &self.log.total.to_string()],
        );
        match &self.log.note {
            Some(note) => format!("{base} · {note}"),
            None => base,
        }
    }

    fn change_summary(&self) -> String {
        let parts = self.changes.partition();
        text::trf(
            text::tr("(冲突 {} · 未跟踪 {} · 已暂存 {} · 已修改 {})"),
            &[
                &parts.conflicted.len().to_string(),
                &parts.untracked.len().to_string(),
                &parts.staged.len().to_string(),
                &parts.modified.len().to_string(),
            ],
        )
    }

    fn diff_stats(&self) -> String {
        self.diff.as_ref().map_or_else(String::new, |diff| {
            if diff.stats.binary {
                text::tr("二进制").to_owned()
            } else {
                format!("+{} -{}", diff.stats.added, diff.stats.removed)
            }
        })
    }
}

/// 更新策略 → 界面下标（0 合并 / 1 变基 / 2 仅快进）。
#[must_use]
fn strategy_index(strategy: PullStrategy) -> i32 {
    match strategy {
        PullStrategy::Merge => 0,
        PullStrategy::Rebase => 1,
        PullStrategy::FastForwardOnly => 2,
    }
}

/// 界面下标 → 更新策略。
#[must_use]
fn strategy_from_index(index: i32) -> PullStrategy {
    match index {
        1 => PullStrategy::Rebase,
        2 => PullStrategy::FastForwardOnly,
        _ => PullStrategy::Merge,
    }
}

/// 该差异两侧的来源是否支持按块落库。
///
/// - `索引 vs 工作区`：可以暂存 / 丢弃（补丁作用于索引或工作区）；
/// - `修订 vs 索引`：可以反暂存（补丁反向作用于索引）；
/// - 其余组合（空树 vs 工作区、空树 vs 索引）都不行 —— 那意味着文件还没进索引，
///   补丁头需要 `new file mode`，交给文件级操作。
fn diff_side_supports_selection(sides: &(DiffSide, DiffSide)) -> bool {
    matches!(
        (&sides.0, &sides.1),
        (DiffSide::Index, DiffSide::Worktree) | (DiffSide::Rev(_), DiffSide::Index)
    )
}

/// 取文本首行（用于把多行 git 输出塞进提示条）。
fn first_line(text: &str) -> String {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .map_or_else(String::new, |line| line.trim().to_owned())
}

/// 把一页提交追加到 Log 状态里，并推进泳道布局。
///
/// **只支持顺序追加**：服务层的日志页永远按 offset 递增请求，因此
/// 乱序到达（例如用户来回点击不同仓库）时会直接丢弃 —— 错位的泳道图
/// 比暂时少几行难排查得多。
fn append_log_page(state: &mut LogState, offset: usize, nodes: Vec<CommitMeta>) {
    if offset != state.nodes.len() {
        tracing::debug!(
            offset,
            loaded = state.nodes.len(),
            "{}",
            text::tr("日志页不是顺序追加，已丢弃")
        );
        return;
    }

    for node in &nodes {
        let log_node = LogNode {
            id: node.id,
            parents: node.parents.clone(),
        };
        state.layout.push(&log_node);
    }
    state.nodes.extend(nodes);
    state.refs_text.resize(state.nodes.len(), String::new());
}

/// 当前 Unix 秒。
/// 把图片差异写入窗口属性（FR-DIF-04）。
///
/// 单侧缺失 / 不可解码 / 解码受限都降级为"只显示元信息"，绝不显示空白面板。
fn apply_image_diff_panel(window: &MainWindow, result: Option<&gt_service::ImageDiffResult>) {
    let Some(result) = result else {
        window.set_diff_image_visible(false);
        return;
    };
    window.set_diff_image_visible(true);
    window.set_diff_img_equal(result.equal);

    let side_image = |side: Option<&gt_service::ImageSide>| -> slint::Image {
        match side {
            Some(side) if side.decodable && !side.rgba.is_empty() => {
                let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                    &side.rgba, side.width, side.height,
                );
                slint::Image::from_rgba8(buffer)
            }
            _ => slint::Image::default(),
        }
    };
    let side_present =
        |side: Option<&gt_service::ImageSide>| side.is_some_and(|s| s.decodable && !s.rgba.is_empty());
    let side_meta = |side: Option<&gt_service::ImageSide>, empty_label: String| -> String {
        match side {
            None => empty_label,
            Some(side) if !side.decodable => format!(
                "{}（{} KB）",
                text::tr("无法按图片解码"),
                side.file_bytes / 1024
            ),
            Some(side) if side.limited => format!(
                "{}×{} · {} KB（{}）",
                side.width,
                side.height,
                side.file_bytes / 1024,
                text::tr("过大，仅显示尺寸")
            ),
            Some(side) => format!(
                "{}×{} · {} KB",
                side.width,
                side.height,
                side.file_bytes / 1024
            ),
        }
    };

    window.set_diff_img_old(side_image(result.old.as_ref()));
    window.set_diff_img_new(side_image(result.new.as_ref()));
    window.set_diff_img_old_present(side_present(result.old.as_ref()));
    window.set_diff_img_new_present(side_present(result.new.as_ref()));
    window.set_diff_img_meta_old(
        side_meta(result.old.as_ref(), text::tr("（旧侧不存在：新增文件）").to_owned()).into(),
    );
    window.set_diff_img_meta_new(
        side_meta(result.new.as_ref(), text::tr("（新侧不存在：已删除）").to_owned()).into(),
    );
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

// ==================================================================== M4 高级操作
//
// 这一段的共同结构：界面回调 → 会话更新状态并标脏 → 投递写请求 →
// `Outcome::Advanced` 回来时统一提示 + 刷新 + （如有冲突）打开三路编辑器。
// 所有方法都不做 I/O：读走 `RequestKind`，写走 `WriteRequest`。

impl Session {
    // ---------------------------------------------------------- 提交上下文菜单

    /// 打开提交上下文菜单（右键）。
    pub fn open_commit_menu(&mut self, index: usize, x: f32, y: f32) {
        let Some(node) = self.log.nodes.get(index) else {
            return;
        };
        let is_head = self
            .head
            .as_ref()
            .and_then(|head| head.target)
            .is_some_and(|target| target == node.id);
        self.ctx = Some(CtxTarget {
            id: node.id.to_hex(),
            short: node.short_id(),
            index,
            is_head,
            parent: node.parents.first().map(ToString::to_string),
        });
        self.ctx_pos = (x, y);
        self.dirty.ctx = true;
    }

    /// 关闭上下文菜单。
    pub fn close_commit_menu(&mut self) {
        if self.ctx.take().is_some() {
            self.dirty.ctx = true;
        }
    }

    /// 执行上下文菜单里的一个动作。
    pub fn commit_action(&mut self, action: &str) {
        let Some(ctx) = self.ctx.clone() else {
            return;
        };
        self.ctx = None;
        self.dirty.ctx = true;

        match action {
            "merge" => self.open_op(OpDialog::merge(Some(ctx.short))),
            "rebase" => self.open_op(OpDialog::rebase(Some(ctx.short))),
            "iplan" => self.start_plan(&ctx),
            "cherry-pick" => self.open_op(OpDialog::cherry_pick(ctx.id, &ctx.short)),
            "revert" => self.submit_write(
                WriteRequest::Revert(RevertRequest {
                    revs: vec![ctx.id],
                    no_commit: false,
                }),
                PendingWrite::Other,
            ),
            "reset" => self.open_op(OpDialog::reset(Some(ctx.short))),
            "undo" => {
                self.submit_write(WriteRequest::UndoCommit { id: ctx.id }, PendingWrite::Other);
            }
            "patch" => {
                let output = self.default_patch_path(&ctx);
                self.open_op(OpDialog::patch(ctx.id, &ctx.short, output));
            }
            "apply-patch" => self.open_op(OpDialog::apply_patch()),
            "tag" => self.open_op(OpDialog::tag(ctx.id)),
            "compare" => self.open_compare(&ctx, false),
            "browse" => self.open_compare(&ctx, true),
            "copy" => self.copy_hash(&ctx),
            "checkout" => {
                self.confirm = Some(Confirm::CheckoutDetached { revision: ctx.id });
            }
            other => tracing::warn!(action = other, "{}", text::tr("未知的提交菜单动作")),
        }
    }

    /// 默认补丁输出路径：仓库根下 `knotwork-<缩写>.patch`。
    fn default_patch_path(&self, ctx: &CtxTarget) -> String {
        let location = self.handle.location();
        let dir = location
            .work_dir
            .clone()
            .unwrap_or_else(|| location.git_dir.clone());
        dir.join(format!("knotwork-{}.patch", ctx.short))
            .display()
            .to_string()
    }

    /// 把哈希放进系统剪贴板（FR-LOG-07）。
    fn copy_hash(&mut self, ctx: &CtxTarget) {
        match arboard::Clipboard::new().and_then(|mut clipboard| clipboard.set_text(ctx.id.clone()))
        {
            Ok(()) => self.show_toast(text::trf("已复制哈希 {}", &[&ctx.short])),
            Err(err) => self.show_toast(text::trf("复制失败：{}（哈希：{}）", &[&err.to_string(), &ctx.id])),
        }
    }

    // ------------------------------------------------------ 比较 / 快照浏览

    /// 打开"两提交比较"或"只读浏览某版本"（FR-LOG-08）。
    fn open_compare(&mut self, ctx: &CtxTarget, browse: bool) {
        let (old, new, title) = if browse {
            // 浏览某个版本 = 看"它相对父提交带来了什么"，并明确标注只读。
            (
                ctx.parent.clone(),
                ctx.id.clone(),
                text::trf("浏览版本 {}（{}）", &[&ctx.short, text::tr("只读")]),
            )
        } else {
            (
                Some(ctx.id.clone()),
                "HEAD".to_owned(),
                text::trf("{} 与 HEAD 比较", &[&ctx.short]),
            )
        };

        let request = RequestKind::TreeChanges {
            old: old.clone(),
            new: new.clone(),
        };
        match self
            .service
            .request(self.handle.id(), request, self.sender.clone())
        {
            Ok(_) => {
                self.compare = Some(CompareState {
                    title,
                    old,
                    new,
                    browse,
                    files: Vec::new(),
                    selected: None,
                });
                self.dirty.compare = true;
            }
            Err(err) => self.show_toast(err.to_string()),
        }
    }

    /// 选中比较视图里的一个文件，把它的差异送进 Diff 面板。
    pub fn compare_select(&mut self, index: usize) {
        let Some(compare) = self.compare.clone() else {
            return;
        };
        let Some(file) = compare.files.get(index).cloned() else {
            return;
        };
        if let Some(state) = self.compare.as_mut() {
            state.selected = Some(index);
        }
        self.dirty.compare = true;

        self.diff_sides = (
            compare.old.map_or(DiffSide::Empty, DiffSide::Rev),
            DiffSide::Rev(compare.new.clone()),
        );
        self.diff_path = Some(file.path.clone());
        self.diff_title = format!("{} · {}", compare.title, file.path);
        self.diff_staged_view = false;
        self.diff_in_flight = false;
        self.request_diff(self.diff_sides.clone(), Some(file.path));
    }

    /// 关闭比较视图。
    pub fn compare_close(&mut self) {
        if self.compare.take().is_some() {
            self.dirty.compare = true;
        }
    }

    // ---------------------------------------------------------- Blame 与文件历史（FR-BLM，M5）

    /// 对当前选中的文件打开 Blame（FR-BLM-02）。
    ///
    /// 入口是本地更改工具栏的"Blame"按钮；路径取当前差异路径
    /// （选中更改行时已写入）。
    pub fn request_blame_selected(&mut self) {
        let Some(path) = self.diff_path.clone() else {
            self.show_toast(text::tr("先在本地更改中选择一个文件").to_owned());
            return;
        };
        self.blame = Some(BlameState {
            path: path.clone(),
            spec: None,
            rows: Vec::new(),
            selected: None,
            truncated: false,
        });
        self.dirty.blame = true;
        self.send_blame(&path, None);
    }

    /// 从 Blame 视图打开文件历史（FR-BLM-01）。
    pub fn blame_open_history(&mut self) {
        let Some(state) = self.blame.clone() else {
            return;
        };
        self.open_file_history(&state.path);
    }

    /// 对当前选中的文件打开文件历史（FR-BLM-01）。
    pub fn request_file_history_selected(&mut self) {
        let Some(path) = self.diff_path.clone() else {
            self.show_toast(text::tr("先在本地更改中选择一个文件").to_owned());
            return;
        };
        self.open_file_history(&path);
    }

    fn open_file_history(&mut self, path: &str) {
        self.file_history = Some(FileHistoryState {
            path: path.to_owned(),
            all_refs: false,
            rows: Vec::new(),
            selected: None,
            truncated: false,
        });
        self.dirty.history = true;
        self.send_file_history(path, false);
    }

    fn send_blame(&mut self, path: &str, spec: Option<String>) {
        let _ = self.service.request(
            self.handle.id(),
            RequestKind::Blame {
                spec,
                path: path.to_owned(),
            },
            self.sender.clone(),
        );
    }

    fn send_file_history(&mut self, path: &str, all_refs: bool) {
        let _ = self.service.request(
            self.handle.id(),
            RequestKind::FileHistory {
                all_refs,
                path: path.to_owned(),
            },
            self.sender.clone(),
        );
    }

    /// 点击 blame 行：跳转"该提交详情"——父提交到该提交对**这个文件**的差异
    /// （FR-BLM-02）。根提交由服务线程识别（旧侧取空树）。
    pub fn blame_select(&mut self, index: usize) {
        let Some(state) = self.blame.clone() else {
            return;
        };
        let Some(row) = state.rows.get(index) else {
            return;
        };
        if row.commit.bytes().all(|b| b == b'0') {
            self.show_toast(text::tr("该行尚未提交，没有可跳转的提交").to_owned());
            return;
        }
        self.blame = None;
        self.dirty.blame = true;
        self.diff_path = Some(state.path.clone());
        self.diff_title = format!("{} · {}", text::tr("提交详情 · {}"), state.path);
        self.diff_staged_view = false;
        self.diff_in_flight = false;
        self.diff_in_flight = self
            .service
            .request(
                self.handle.id(),
                RequestKind::BlameDetail {
                    commit: row.commit.clone(),
                    path: state.path.clone(),
                },
                self.sender.clone(),
            )
            .is_ok();
        self.dirty.diff = true;
    }

    /// 关闭 Blame 视图，回到差异面板。
    pub fn blame_close(&mut self) {
        if self.blame.take().is_some() {
            self.dirty.blame = true;
        }
    }

    /// 链式导航（FR-BLM-03）：上溯选中行的来源。行号映射与新 blame 都在
    /// 服务线程完成，结果一次性带回（含滚动锚点）。
    pub fn blame_chain(&mut self) {
        let Some(state) = self.blame.clone() else {
            return;
        };
        let Some(selected) = state.selected else {
            self.show_toast(text::tr("先选中一行再上溯").to_owned());
            return;
        };
        let Some(row) = state.rows.get(selected) else {
            return;
        };
        if row.boundary && row.prev_commit.is_none() {
            self.show_toast(text::tr("该行已追溯到文件最初的来源").to_owned());
            return;
        }
        let _ = self.service.request(
            self.handle.id(),
            RequestKind::BlameChain {
                commit: row.commit.clone(),
                prev_commit: row.prev_commit.clone(),
                prev_path: row.prev_path.clone(),
                path: state.path.clone(),
                from_spec: state.spec.clone(),
                line: row.final_line,
            },
            self.sender.clone(),
        );
    }

    /// 切换文件历史的分支过滤（0 = 当前分支，1 = 全部引用）。
    pub fn history_set_filter(&mut self, filter: i32) {
        let Some(state) = self.file_history.as_mut() else {
            return;
        };
        let all_refs = filter == 1;
        if state.all_refs == all_refs {
            return;
        }
        state.all_refs = all_refs;
        self.dirty.history = true;
        let path = state.path.clone();
        self.send_file_history(&path, all_refs);
    }

    /// 选中文件历史中的一条提交：关闭列表并在差异面板预览该提交对此
    /// 文件的改动（FR-BLM-01"选中提交即时 diff 预览"）。
    pub fn history_select(&mut self, index: usize) {
        let Some(state) = self.file_history.clone() else {
            return;
        };
        let Some(row) = state.rows.get(index) else {
            return;
        };
        self.file_history = None;
        self.dirty.history = true;
        self.diff_path = Some(state.path.clone());
        self.diff_title = format!("{} · {}", row.short, state.path);
        self.diff_staged_view = false;
        self.diff_in_flight = false;
        self.diff_in_flight = self
            .service
            .request(
                self.handle.id(),
                RequestKind::BlameDetail {
                    commit: row.commit.clone(),
                    path: state.path.clone(),
                },
                self.sender.clone(),
            )
            .is_ok();
        self.dirty.diff = true;
    }

    /// 关闭文件历史。
    pub fn history_close(&mut self) {
        if self.file_history.take().is_some() {
            self.dirty.history = true;
        }
    }

    // ---------------------------------------------------------- 通用操作对话框

    fn open_op(&mut self, dialog: OpDialog) {
        self.op = Some(dialog);
        self.dirty.op = true;
    }

    /// 设置对话框第一个输入框。
    pub fn set_op_input(&mut self, text: &str) {
        if let Some(op) = self.op.as_mut() {
            text.clone_into(&mut op.input);
        }
    }

    /// 设置对话框第二个输入框（区间起点 / 附注消息）。
    pub fn set_op_input2(&mut self, text: &str) {
        if let Some(op) = self.op.as_mut() {
            text.clone_into(&mut op.input2);
        }
    }

    /// 切换对话框的模式（策略 / 重置档位 / 补丁语义 / 三方合并）。
    pub fn set_op_mode(&mut self, mode: i32) {
        let Some(op) = self.op.as_mut() else {
            return;
        };
        op.mode = mode.clamp(
            0,
            i32::try_from(op.modes.len()).unwrap_or(1).saturating_sub(1),
        );
        // 每个模式下"危险"与"第二个输入框"的语义不同，集中在这里判定，
        // 避免界面自己写一套 if。
        op.danger = match op.kind {
            3 => op.mode == 2, // 硬重置
            _ => false,
        };
        op.show_input2 = match op.kind {
            4 => op.mode == 1, // 区间差异需要起点
            6 => true,         // 标签的附注消息
            _ => false,
        };
        self.dirty.op = true;
    }

    /// 取消对话框。
    pub fn op_cancel(&mut self) {
        if self.op.take().is_some() {
            self.dirty.op = true;
        }
    }

    /// 确认对话框：按 kind + mode 构造具体请求。
    #[allow(clippy::too_many_lines)]
    pub fn op_confirm(&mut self) {
        let Some(op) = self.op.take() else {
            return;
        };
        self.dirty.op = true;

        let input = op.input.trim().to_owned();
        if input.is_empty() {
            self.show_toast(text::tr("请先填写必填项"));
            self.op = Some(op);
            self.dirty.op = true;
            return;
        }

        match op.kind {
            1 => {
                let strategy = match op.mode {
                    1 => MergeStrategy::NoFastForward,
                    2 => MergeStrategy::FastForwardOnly,
                    3 => MergeStrategy::Squash,
                    _ => MergeStrategy::Default,
                };
                self.submit_write(
                    WriteRequest::Merge(MergeRequest {
                        rev: input,
                        strategy,
                        message: None,
                    }),
                    PendingWrite::Other,
                );
            }
            2 => {
                self.submit_write(
                    WriteRequest::Rebase(RebaseRequest { onto: input }),
                    PendingWrite::Other,
                );
            }
            3 => {
                let mode = match op.mode {
                    1 => ResetMode::Mixed,
                    2 => ResetMode::Hard,
                    _ => ResetMode::Soft,
                };
                if mode.is_destructive() {
                    // 硬重置必须先过确认遮罩（D4：强确认，且文案包含三要素）。
                    self.confirm = Some(Confirm::Reset {
                        target: input,
                        mode,
                    });
                } else {
                    self.submit_write(
                        WriteRequest::Reset(ResetRequest {
                            target: input,
                            mode,
                        }),
                        PendingWrite::Other,
                    );
                }
            }
            4 => {
                let source = match op.mode {
                    1 => {
                        if op.input2.trim().is_empty() {
                            self.show_toast(text::tr("区间差异需要填写起点"));
                            self.op = Some(op);
                            self.dirty.op = true;
                            return;
                        }
                        PatchSource::Range {
                            from: op.input2.trim().to_owned(),
                            to: op.commit.clone().unwrap_or_else(|| "HEAD".to_owned()),
                        }
                    }
                    2 => PatchSource::Worktree,
                    _ => PatchSource::Commit {
                        rev: op.commit.clone().unwrap_or_else(|| "HEAD".to_owned()),
                    },
                };
                self.submit_write(
                    WriteRequest::CreatePatch {
                        source,
                        output: PathBuf::from(input),
                    },
                    PendingWrite::Other,
                );
            }
            5 => {
                self.submit_write(
                    WriteRequest::ApplyPatch(PatchApplyRequest {
                        file: PathBuf::from(input),
                        three_way: op.mode == 1,
                        reverse: false,
                    }),
                    PendingWrite::Other,
                );
            }
            6 => {
                let message = op.input2.trim();
                self.submit_write(
                    WriteRequest::Tag(TagOp::Create {
                        name: input,
                        message: (!message.is_empty()).then(|| message.to_owned()),
                        target: op.commit,
                    }),
                    PendingWrite::Other,
                );
            }
            7 => {
                self.submit_write(
                    WriteRequest::CherryPick(CherryPickRequest {
                        revs: vec![input],
                        record_source: op.mode == 1,
                        no_commit: false,
                    }),
                    PendingWrite::Other,
                );
            }
            other => tracing::warn!(kind = other, "{}", text::tr("未知的操作对话框类型")),
        }
    }

    // ---------------------------------------------------------- 交互式变基

    /// 请求生成变基计划（FR-MRG-03）。
    fn start_plan(&mut self, ctx: &CtxTarget) {
        let Some(parent) = ctx.parent.clone() else {
            self.show_toast(text::tr("根提交没有父提交，无法从此处开始交互式变基"));
            return;
        };
        let request = RequestKind::RebasePlan {
            onto: parent.clone(),
            base: parent,
        };
        match self
            .service
            .request(self.handle.id(), request, self.sender.clone())
        {
            Ok(_) => self.show_toast(text::tr("正在生成变基计划…")),
            Err(err) => self.show_toast(err.to_string()),
        }
    }

    /// 选中计划里的一步（把它的消息装进编辑框）。
    pub fn iplan_select(&mut self, index: usize) {
        let Some(plan) = self.plan.as_ref() else {
            return;
        };
        let Some(step) = plan.steps.get(index) else {
            return;
        };
        self.plan_selected = Some(index);
        self.plan_message = step.message.clone().unwrap_or_default();
        self.dirty.iplan = true;
    }

    /// 设置某一步的动作。
    pub fn iplan_action(&mut self, index: usize, action: i32) {
        let Some(action) = rebase_action_from_code(action) else {
            return;
        };
        // 第一条不能"压缩/吞并"：它没有可并入的上一条。与其等到确认时报错，
        // 不如在点下去的瞬间就说明白。
        if index == 0 && action.is_squash_like() {
            self.show_toast(text::tr("第一个提交没有上一条可并入：请改用「保留」或「改写」"));
            return;
        }
        let Some(plan) = self.plan.as_mut() else {
            return;
        };
        let Some(step) = plan.steps.get_mut(index) else {
            return;
        };
        step.action = action;
        if action == RebaseAction::Reword && step.message.is_none() {
            // 改写消息的默认值就是原消息：用户通常在原文上改几个字，
            // 让空消息框启动等于强迫用户重写一遍。
            step.message = Some(step.subject.clone());
        }
        if self.plan_selected == Some(index) {
            self.plan_message = plan
                .steps
                .get(index)
                .and_then(|step| step.message.clone())
                .unwrap_or_default();
        }
        self.dirty.iplan = true;
    }

    /// 编辑所选步骤的新消息。
    pub fn iplan_set_message(&mut self, text: &str) {
        text.clone_into(&mut self.plan_message);
        if let (Some(plan), Some(index)) = (self.plan.as_mut(), self.plan_selected) {
            if let Some(step) = plan.steps.get_mut(index) {
                step.message = Some(text.to_owned());
            }
        }
        self.dirty.iplan = true;
    }

    /// 执行计划。
    pub fn iplan_confirm(&mut self) {
        let Some(plan) = self.plan.clone() else {
            return;
        };
        if let Err(error) = plan.validate() {
            self.show_toast(error.to_string());
            return;
        }
        self.plan = None;
        self.plan_selected = None;
        self.plan_message.clear();
        self.dirty.iplan = true;
        self.submit_write(
            WriteRequest::InteractiveRebase(Box::new(plan)),
            PendingWrite::Other,
        );
    }

    /// 放弃编辑。
    pub fn iplan_cancel(&mut self) {
        self.handle.cancel_lane(gt_service::RequestLane::Advanced);
        self.plan = None;
        self.plan_selected = None;
        self.plan_message.clear();
        self.dirty.iplan = true;
    }

    // ---------------------------------------------------------- 冲突解决

    /// 请求冲突文件清单（FR-CFL-02）。
    pub fn request_conflicts(&mut self) {
        match self.service.request(
            self.handle.id(),
            RequestKind::Conflicts,
            self.sender.clone(),
        ) {
            Ok(_) => {}
            Err(err) => self.show_toast(err.to_string()),
        }
    }

    fn request_conflict_file(&mut self, path: &str) {
        if let Err(err) = self.service.request(
            self.handle.id(),
            RequestKind::ConflictFile {
                path: path.to_owned(),
            },
            self.sender.clone(),
        ) {
            self.show_toast(err.to_string());
        }
    }

    /// 选中一个冲突文件并加载它的冲突块。
    pub fn cfl_select(&mut self, index: usize) {
        if index >= self.conflicts.len() {
            return;
        }
        self.conflict_selected = Some(index);
        self.conflict_file = None;
        self.conflict_resolutions.clear();
        self.conflict_block = None;
        self.conflict_manual.clear();
        let path = self.conflicts[index].path.clone();
        self.request_conflict_file(&path);
        self.dirty.cfl = true;
    }

    /// 决定某一处冲突的解法。
    pub fn cfl_resolve(&mut self, block: i32, code: i32) {
        let Some(resolution) = resolution_from_code(code) else {
            return;
        };
        let Ok(index) = usize::try_from(block) else {
            return;
        };
        if let Some(slot) = self.conflict_resolutions.get_mut(index) {
            *slot = Some(resolution);
            self.dirty.cfl = true;
        }
    }

    /// 全部冲突块取同一种解法（FR-CFL-03 的"按标记解析"快捷入口）。
    pub fn cfl_resolve_all(&mut self, code: i32) {
        let Some(resolution) = resolution_from_code(code) else {
            return;
        };
        for slot in &mut self.conflict_resolutions {
            *slot = Some(resolution.clone());
        }
        self.dirty.cfl = true;
    }

    /// 选中某处冲突进行手动编辑（把手写框填上当前结果）。
    pub fn cfl_pick_block(&mut self, block: i32) {
        let Ok(index) = usize::try_from(block) else {
            return;
        };
        let Some(file) = self.conflict_file.as_ref() else {
            return;
        };
        let Some(hunk) = file.hunks().nth(index) else {
            return;
        };
        let current = match self
            .conflict_resolutions
            .get(index)
            .and_then(Option::as_ref)
        {
            Some(gt_diff::Resolution::Ours) | None => hunk.ours.join("\n"),
            Some(gt_diff::Resolution::Theirs) => hunk.theirs.join("\n"),
            Some(gt_diff::Resolution::Both) => {
                format!("{}\n{}", hunk.ours.join("\n"), hunk.theirs.join("\n"))
            }
            Some(gt_diff::Resolution::Base) => hunk.base.as_deref().unwrap_or(&[]).join("\n"),
            Some(gt_diff::Resolution::Manual(lines)) => lines.join("\n"),
        };
        self.conflict_block = Some(index);
        self.conflict_manual = current;
        self.dirty.cfl = true;
    }

    /// 编辑手写结果。
    pub fn cfl_set_manual(&mut self, text: &str) {
        text.clone_into(&mut self.conflict_manual);
    }

    /// 把手写结果应用到当前冲突块。
    pub fn cfl_apply_manual(&mut self) {
        let Some(block) = self.conflict_block else {
            self.show_toast(text::tr("请先在某一处冲突上点「手动编辑」"));
            return;
        };
        let lines: Vec<String> = if self.conflict_manual.is_empty() {
            Vec::new()
        } else {
            self.conflict_manual
                .split('\n')
                .map(str::to_owned)
                .collect()
        };
        if let Some(slot) = self.conflict_resolutions.get_mut(block) {
            *slot = Some(gt_diff::Resolution::Manual(lines));
            self.dirty.cfl = true;
        }
    }

    /// 全部冲突块已决定时，写回文件并 `git add`。
    pub fn cfl_mark_resolved(&mut self) {
        let Some(file) = self.conflict_file.as_ref() else {
            self.show_toast(text::tr("请先选择一个冲突文件"));
            return;
        };
        if file.hunk_count() == 0 {
            // 没有标记就无法重建内容（二进制冲突、`merge.conflictStyle=union`）。
            // 这时只能整体取一侧，硬写会把文件写坏。
            self.show_toast(text::tr("该文件没有冲突标记：请用「整文件取当前分支 / 合入分支」"));
            return;
        }
        let undecided = self
            .conflict_resolutions
            .iter()
            .filter(|slot| slot.is_none())
            .count();
        if undecided > 0 {
            self.show_toast(text::trf("还有 {} 处冲突未决定", &[&undecided.to_string()]));
            return;
        }
        let resolutions: Vec<gt_diff::Resolution> = self
            .conflict_resolutions
            .iter()
            .filter_map(Clone::clone)
            .collect();
        let content = match file.solve(&resolutions) {
            Ok(content) => content,
            Err(error) => {
                self.show_toast(error.to_string());
                return;
            }
        };
        let path = file.path.clone();
        self.submit_write(
            WriteRequest::ResolveConflict {
                path,
                content: content.into_bytes(),
            },
            PendingWrite::Other,
        );
    }

    /// 整文件用某一侧解决（stage 2 = 当前分支，3 = 合入分支）。
    pub fn cfl_take_side(&mut self, stage: u8) {
        let Some(index) = self.conflict_selected else {
            return;
        };
        let Some(entry) = self.conflicts.get(index) else {
            return;
        };
        self.confirm = Some(Confirm::TakeSide {
            path: entry.path.clone(),
            stage,
        });
    }

    /// 关闭冲突编辑器。
    ///
    /// 同时作废"高级操作"通道上的在飞读取：否则刚关掉的遮罩会被迟到的
    /// 冲突清单重新弹回来（用户会以为关不掉）。
    pub fn cfl_close(&mut self) {
        self.handle.cancel_lane(gt_service::RequestLane::Advanced);
        self.conflicts.clear();
        self.conflict_selected = None;
        self.conflict_file = None;
        self.conflict_resolutions.clear();
        self.conflict_block = None;
        self.conflict_manual.clear();
        self.dirty.cfl = true;
    }

    // ---------------------------------------------------------- 贮藏

    /// 请求贮藏列表。
    pub fn request_stash_list(&mut self) {
        if let Err(err) = self.service.request(
            self.handle.id(),
            RequestKind::StashList,
            self.sender.clone(),
        ) {
            self.show_toast(err.to_string());
        }
    }

    /// 创建贮藏。
    pub fn stash_create(&mut self) {
        let message = self.stash_message.clone();
        self.stash_message.clear();
        self.dirty.stash = true;
        self.submit_write(
            WriteRequest::Stash(StashOp::Create {
                message,
                include_untracked: self.stash_untracked,
            }),
            PendingWrite::Other,
        );
    }

    /// 设置贮藏消息。
    pub fn set_stash_message(&mut self, text: &str) {
        text.clone_into(&mut self.stash_message);
    }

    /// 切换"包含未跟踪文件"。
    pub fn toggle_stash_untracked(&mut self, on: bool) {
        self.stash_untracked = on;
        self.dirty.stash = true;
    }

    /// 应用贮藏（保留该条）。
    pub fn stash_apply(&mut self, index: i32) {
        if let Ok(index) = u32::try_from(index) {
            self.submit_write(
                WriteRequest::Stash(StashOp::Apply { index }),
                PendingWrite::Other,
            );
        }
    }

    /// 弹出贮藏。
    pub fn stash_pop(&mut self, index: i32) {
        if let Ok(index) = u32::try_from(index) {
            self.submit_write(
                WriteRequest::Stash(StashOp::Pop { index }),
                PendingWrite::Other,
            );
        }
    }

    /// 删除贮藏（先确认）。
    pub fn stash_drop(&mut self, index: i32) {
        let Ok(index) = u32::try_from(index) else {
            return;
        };
        let selector = self
            .stashes
            .iter()
            .find(|entry| entry.index == index)
            .map_or_else(
                || format!("stash@{{{index}}}"),
                |entry| entry.selector.clone(),
            );
        self.confirm = Some(Confirm::StashDrop { selector, index });
    }

    /// 从贮藏创建分支（分支名取侧栏输入框）。
    pub fn stash_branch(&mut self, index: i32) {
        let name = self.branch_input.trim().to_owned();
        if name.is_empty() {
            self.show_toast(text::tr("请先在侧栏输入新分支名"));
            return;
        }
        if let Ok(index) = u32::try_from(index) {
            self.submit_write(
                WriteRequest::Stash(StashOp::Branch { index, name }),
                PendingWrite::Other,
            );
        }
    }

    // ---------------------------------------------------------- 中断恢复

    /// 继续进行中的操作（合并提交 / 变基 continue / 拣选 continue）。
    pub fn resume_continue(&mut self) {
        if !self.state.is_in_progress() {
            self.show_toast(text::tr("当前没有进行中的操作"));
            return;
        }
        self.submit_write(continue_request(self.state), PendingWrite::Other);
    }

    /// 跳过变基中的当前提交（会丢弃该提交的改动，需确认）。
    pub fn resume_skip(&mut self) {
        if !self.state.is_rebase() {
            self.show_toast(text::tr("当前操作不支持跳过"));
            return;
        }
        self.confirm = Some(Confirm::RebaseSkip);
    }

    /// 中止进行中的操作（需确认）。
    pub fn resume_abort(&mut self) {
        if !self.state.is_in_progress() {
            self.show_toast(text::tr("当前没有进行中的操作"));
            return;
        }
        self.confirm = Some(Confirm::AbortInProgress { state: self.state });
    }

    // ---------------------------------------------------------- 结果落地

    /// 高级操作结果落地：提示 + （有冲突则）打开三路编辑器 + 整体刷新。
    fn apply_advanced_result(&mut self, result: &AdvancedResult) {
        self.advanced_label = self.handle.advanced_label();
        self.show_toast(first_line(&result.summary));

        let conflicted = result.is_conflicted();
        if !conflicted {
            self.conflicts.clear();
            self.conflict_selected = None;
            self.conflict_file = None;
            self.conflict_resolutions.clear();
            self.conflict_block = None;
            self.conflict_manual.clear();
            self.dirty.cfl = true;
        }

        // 写操作后统一刷新：状态 / 概览 / 日志 / 差异。
        self.refresh();

        if conflicted {
            self.request_conflicts();
        }
        // 贮藏面板打开时就顺手刷新一次列表：用户刚做完 stash 操作，
        // 最想看到的是"列表变了没有"。
        self.request_stash_list();
    }

    // ---------------------------------------------------------- 属性写回

    fn apply_ctx_menu(&self, window: &MainWindow) {
        window.set_ctx_visible(self.ctx.is_some());
        if let Some(ctx) = &self.ctx {
            window.set_ctx_x(self.ctx_pos.0);
            window.set_ctx_y(self.ctx_pos.1);
            window.set_ctx_subject(ctx.short.clone().into());
            window.set_ctx_is_head(ctx.is_head);
        }
    }

    fn apply_op_dialog(&self, window: &MainWindow) {
        let Some(op) = &self.op else {
            window.set_op_visible(false);
            return;
        };
        window.set_op_visible(true);
        window.set_op_kind(op.kind);
        window.set_op_title(op.title.clone().into());
        window.set_op_hint(op.hint.clone().into());
        window.set_op_input(op.input.clone().into());
        window.set_op_placeholder(op.placeholder.clone().into());
        window.set_op_input2(op.input2.clone().into());
        window.set_op_placeholder2(op.placeholder2.clone().into());
        window.set_op_show_input2(op.show_input2);
        window.set_op_modes(rc_model(
            op.modes
                .iter()
                .map(|label| slint::SharedString::from(label.as_str()))
                .collect(),
        ));
        window.set_op_mode(op.mode);
        window.set_op_danger(op.danger);
    }

    fn apply_rebase_plan(&self, window: &MainWindow) {
        let Some(plan) = &self.plan else {
            window.set_iplan_visible(false);
            return;
        };
        window.set_iplan_visible(true);
        window.set_iplan_rows(rc_model(build_rebase_step_rows(plan, self.plan_selected)));
        window.set_iplan_title(text::trf("交互式变基：{} → {}", &[&plan.base, "HEAD"]).into());
        let kept = plan.kept();
        let merges = if plan.merges_dropped > 0 {
            text::trf(
                text::tr(" ⚠ 区间内还有 {} 个合并提交：交互式变基会线性化历史，这些合并提交不会保留。"),
                &[&plan.merges_dropped.to_string()],
            )
        } else {
            String::new()
        };
        window.set_iplan_summary(
            text::trf(
                text::tr("共 {} 个提交，其中 {} 个会保留。压缩/吞并会把该提交并入上一条；改写会先重放再改消息。{}"),
                &[
                    &plan.steps.len().to_string(),
                    &kept.to_string(),
                    &merges,
                ],
            )
            .into(),
        );
        window.set_iplan_selected(
            self.plan_selected
                .and_then(|index| i32::try_from(index).ok())
                .unwrap_or(-1),
        );
        window.set_iplan_message(self.plan_message.clone().into());
        window.set_iplan_message_visible(self.plan_selected.is_some());
    }

    fn apply_conflict_editor(&self, window: &MainWindow) {
        let open = !self.conflicts.is_empty() || self.conflict_file.is_some();
        window.set_cfl_visible(open);
        if !open {
            return;
        }
        window.set_cfl_files(rc_model(build_conflict_file_rows(
            &self.conflicts,
            self.conflict_selected,
        )));
        let Some(file) = &self.conflict_file else {
            window.set_cfl_rows(rc_model(Vec::new()));
            window.set_cfl_title(text::tr("冲突解决（正在读取文件…）").into());
            window.set_cfl_summary(text::trf("还有 {} 个文件待解决", &[&self.conflicts.len().to_string()]).into());
            return;
        };
        window.set_cfl_rows(rc_model(build_conflict_rows(
            file,
            &self.conflict_resolutions,
        )));
        window.set_cfl_title(
            text::trf(
                text::tr("冲突解决：{}（还有 {} 个文件待解决）"),
                &[&file.path, &self.conflicts.len().to_string()],
            )
            .into(),
        );
        window.set_cfl_summary(conflict_summary(file, &self.conflict_resolutions).into());
        window.set_cfl_manual(self.conflict_manual.clone().into());
        window.set_cfl_manual_visible(self.conflict_block.is_some());
        window.set_cfl_block(
            self.conflict_block
                .and_then(|index| i32::try_from(index).ok())
                .unwrap_or(-1),
        );
    }

    fn apply_stash_panel(&self, window: &MainWindow) {
        window.set_stash_rows(rc_model(build_stash_rows(&self.stashes)));
        window.set_stash_summary(format!("({})", self.stashes.len()).into());
        window.set_stash_message(self.stash_message.clone().into());
        window.set_stash_untracked(self.stash_untracked);
    }

    fn apply_compare_panel(&self, window: &MainWindow) {
        let Some(compare) = &self.compare else {
            window.set_compare_visible(false);
            return;
        };
        window.set_compare_visible(true);
        window.set_compare_title(compare.title.clone().into());
        window.set_compare_browse(compare.browse);
        window.set_compare_rows(rc_model(build_compare_rows(
            &compare.files,
            compare.selected,
        )));
    }

    /// Blame 面板（FR-BLM-02/03）。
    fn apply_blame_panel(&mut self, window: &MainWindow) {
        let Some(state) = &self.blame else {
            window.set_blame_visible(false);
            return;
        };
        window.set_blame_visible(true);
        window.set_blame_title(state.path.clone().into());
        // 修订说明：工作区 / 短哈希（链式导航后会变）。
        let spec_label = match &state.spec {
            None => text::tr("工作区").to_owned(),
            Some(rev) => format!("{} {}", text::tr("版本"), &rev[..rev.len().min(8)]),
        };
        window.set_blame_spec_label(spec_label.into());
        window.set_blame_rows(rc_model(build_blame_rows(
            &state.rows,
            state.selected,
            now_unix(),
        )));
        let can_chain = state
            .selected
            .and_then(|index| state.rows.get(index))
            .is_some_and(|row| !row.boundary || row.prev_commit.is_some());
        window.set_blame_can_chain(can_chain);
        window.set_blame_truncated(if state.truncated {
            text::tr("（行数过多，已截断）").into()
        } else {
            "".into()
        });
        // 滚动锚点：链式导航一次。取走后界面复位置 -1。
        if let Some(anchor) = self.blame_pending_anchor {
            window.set_blame_anchor(i32::try_from(anchor).unwrap_or(-1));
            self.blame_pending_anchor = None;
        }
    }

    /// 文件历史面板（FR-BLM-01）。
    fn apply_history_panel(&self, window: &MainWindow) {
        let Some(state) = &self.file_history else {
            window.set_history_visible(false);
            return;
        };
        window.set_history_visible(true);
        window.set_history_title(
            format!("{} · {}", text::tr("文件历史"), state.path).into(),
        );
        window.set_history_truncated(state.truncated);
        window.set_history_filter(i32::from(state.all_refs));
        window.set_history_rows(rc_model(build_history_rows(
            &state.rows,
            state.selected,
            now_unix(),
        )));
    }

    fn apply_advanced_header(&self, window: &MainWindow) {
        window.set_advanced_label(self.advanced_label.clone().unwrap_or_default().into());
        window.set_resume_visible(self.state.is_in_progress());
        window.set_resume_can_skip(self.state.is_rebase());
    }
}

#[cfg(test)]
mod tests {
    use super::{append_log_page, Confirm, LogState, LOG_PAGE_SIZE, PREFETCH_MARGIN};
    use gt_core::{CommitMeta, Oid};

    fn commit(byte: u8, parents: Vec<Oid>) -> CommitMeta {
        CommitMeta {
            id: Oid::from_bytes(&[byte; 20]).expect("Oid"),
            parents,
            summary: format!("提交 {byte}"),
            author_name: "张三".to_owned(),
            author_email: "z@example.com".to_owned(),
            author_time: 1_700_000_000,
            committer_time: 1_700_000_000,
        }
    }

    #[test]
    fn pages_append_in_order_and_advance_the_lane_layout() {
        let mut state = LogState::default();
        let parent = Oid::from_bytes(&[9; 20]).expect("Oid");
        append_log_page(&mut state, 0, vec![commit(1, vec![parent])]);
        append_log_page(&mut state, 1, vec![commit(2, Vec::new())]);

        assert_eq!(state.nodes.len(), 2);
        assert_eq!(state.layout.rows().len(), 2);
        assert_eq!(state.refs_text.len(), 2, "引用标注长度必须与节点对齐");
    }

    /// 造一个只含一个提交的临时仓库；git 不可用时返回 `None`。
    fn tmp_repo(label: &str) -> Option<std::path::PathBuf> {
        let cli = gt_backend::GitCli::detect()?;
        let root = std::env::temp_dir().join(format!(
            "knotwork-ui-session-{}-{label}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("建临时目录");

        let run = |args: &[&str]| {
            let output = cli
                .run(
                    Some(&root),
                    args,
                    &gt_backend::NoopObserver,
                    gt_backend::LOCAL_TIMEOUT,
                )
                .expect("执行 git");
            assert!(output.is_success(), "git {args:?} 失败：{}", output.stderr);
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "ui@knotwork.invalid"]);
        run(&["config", "user.name", "Knotwork UI"]);
        run(&["config", "core.autocrlf", "false"]);
        std::fs::write(root.join("a.txt"), "hello\n").expect("写文件");
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "初始提交"]);
        Some(root)
    }

    #[test]
    fn refresh_reissues_requests_after_invalidating_in_flight_ones() {
        // 回归防线：刷新会作废在飞请求，而作废的结果永远不会回传。
        // 若不在刷新时清掉"在飞"标记，重新发起会被自己的标记挡住，
        // 各通道代次不再推进 —— 界面就永久停在"加载中"。
        let Some(root) = tmp_repo("refresh") else {
            return;
        };
        let service = std::sync::Arc::new(
            gt_service::GitService::new(gt_service::silent_observer()).expect("建服务"),
        );
        let handle = service.open_repo(&root).expect("打开仓库");
        let console = gt_service::ConsoleBuffer::shared(8);
        let mut session =
            super::Session::new(std::sync::Arc::clone(&service), handle.clone(), console, 8);

        session.request_initial();
        assert!(
            session.in_flight().iter().any(|flag| *flag),
            "首屏请求应处于在飞状态"
        );

        let lanes = [
            gt_service::RequestLane::Overview,
            gt_service::RequestLane::Status,
            gt_service::RequestLane::Log,
        ];
        let before: Vec<_> = lanes.iter().map(|lane| handle.generation(*lane)).collect();

        session.refresh();

        let after: Vec<_> = lanes.iter().map(|lane| handle.generation(*lane)).collect();
        assert!(
            before.iter().zip(&after).all(|(early, late)| early < late),
            "刷新必须重新发号（清了在飞标记才会真的重发）：{before:?} -> {after:?}"
        );

        // 收尾：让服务工作线程退出，再清理临时目录。
        drop(session);
        drop(handle);
        let mut service = std::sync::Arc::try_unwrap(service)
            .unwrap_or_else(|_| panic!("服务不应还有其它强引用"));
        service.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn out_of_order_pages_are_dropped_instead_of_misaligning_the_graph() {
        let mut state = LogState::default();
        append_log_page(&mut state, 5, vec![commit(1, Vec::new())]);
        assert!(state.nodes.is_empty(), "乱序页必须丢弃");
        assert!(state.layout.rows().is_empty());

        append_log_page(&mut state, 0, vec![commit(2, Vec::new())]);
        assert_eq!(state.nodes.len(), 1);
    }

    #[test]
    fn force_push_confirmation_states_loss_and_alternative() {
        let confirm = Confirm::ForcePush {
            remote: "origin".to_owned(),
            branch: "main".to_owned(),
            commits: 3,
        };
        let (title, detail, danger) = confirm.text();
        assert!(title.contains("origin/main"), "{title}");
        assert!(danger, "强制推送必须是危险色");
        assert!(detail.contains("消失"), "必须说清失去什么：{detail}");
        assert!(detail.contains("分叉"), "必须说清不可逆的后果：{detail}");
        assert!(detail.contains("替代做法"), "必须给出替代做法：{detail}");
        assert!(detail.contains('3'), "必须给出待推提交数：{detail}");
    }

    #[test]
    fn pull_strategy_indices_round_trip() {
        for index in 0..=2 {
            let strategy = super::strategy_from_index(index);
            assert_eq!(
                super::strategy_index(strategy),
                index,
                "策略与界面下标必须一一对应"
            );
        }
        // 越界值归到合并，而不是 panic 或静默变成别的策略。
        assert_eq!(
            super::strategy_from_index(99),
            gt_backend::PullStrategy::Merge
        );
    }

    #[test]
    fn push_without_remotes_is_refused_with_a_toast_instead_of_a_request() {
        let Some(root) = tmp_repo("no-remote") else {
            return;
        };
        let service = std::sync::Arc::new(
            gt_service::GitService::new(gt_service::silent_observer()).expect("建服务"),
        );
        let handle = service.open_repo(&root).expect("打开仓库");
        let console = gt_service::ConsoleBuffer::shared(8);
        let mut session =
            super::Session::new(std::sync::Arc::clone(&service), handle.clone(), console, 8);

        assert!(session.default_remote().is_none(), "临时仓库没有远端");
        session.request_push();
        assert!(
            session.push_preview().is_none(),
            "没有远端时不应产生预览，更不能起子进程"
        );
        assert!(!handle.network_snapshot().busy);

        drop(session);
        drop(handle);
        let mut service = std::sync::Arc::try_unwrap(service).expect("服务不应还有其它强引用");
        service.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn destructive_operations_require_confirmation() {
        // 业务规则：这些动作必须经确认遮罩（需求文档第 9 节 L5 清单）。
        let paths = vec!["a.txt".to_owned()];
        for confirm in [
            Confirm::DiscardChanges(paths.clone()),
            Confirm::DeleteUntracked(paths.clone()),
            Confirm::DiscardSelection {
                path: "a.txt".to_owned(),
                lines: 2,
            },
            Confirm::DeleteBranch {
                name: "wip".to_owned(),
                force: true,
            },
        ] {
            let (_, detail, _) = confirm.text();
            assert!(!detail.is_empty(), "{confirm:?} 必须给出说明");
        }

        // 检出 detached 不是不可逆操作，用普通色。
        let (_, _, danger) = Confirm::CheckoutDetached {
            revision: "abc1234".to_owned(),
        }
        .text();
        assert!(!danger);
    }

    /// 预取窗口应当远小于单页，否则会在第一页就立刻再请求一页。
    const _: () = assert!(PREFETCH_MARGIN < LOG_PAGE_SIZE);
}
