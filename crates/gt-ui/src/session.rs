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
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender};
use gt_core::{ChangeSet, CommitCheck, CommitCheckInput, CommitMeta, HeadInfo, LogNode, RepoState};
use gt_diff::{FileDiff, HunkSelection};
use gt_graph::LaneLayout;
use gt_service::{
    first_log_page, ConsoleBuffer, Delivery, DiffSide, GitService, Outcome, RepoHandle,
    RequestKind, SearchRequest, SelectionMode, WriteRequest,
};

use crate::generated::{DiffRow, MainWindow};
use crate::view_model::{
    branch_text, build_change_rows, build_console_rows, build_diff_rows, build_log_rows,
    build_ref_rows, diff_selection_hint, discard_confirmation, discard_selection_confirmation,
    head_text, ordered_refs, rc_model, state_banner,
};

/// 距离已加载末尾多少行时预取下一页。
const PREFETCH_MARGIN: usize = 50;

/// 单页提交数。
pub const LOG_PAGE_SIZE: usize = 200;

/// 提示条的显示时长（以轮询次数计，100ms/次 → 约 6 秒）。
const TOAST_TICKS: u8 = 60;

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
                    format!("强制删除分支 {name}？")
                } else {
                    format!("删除分支 {name}？")
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
                format!("检出修订 {revision}？"),
                "检出提交会让 HEAD 处于分离状态（不属于任何分支）。\
                 在此状态下的新提交不会被任何分支引用，切换分支后可能丢失。\n\n\
                 替代做法：若要在此提交上继续开发，请先「新建」分支。"
                    .to_owned(),
                false,
            ),
        }
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

    // ---- 交互 ----
    confirm: Option<Confirm>,
    toast: Option<(String, u8)>,
    pending_write: Option<PendingWrite>,

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
            diff_title: "差异".to_owned(),
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
            confirm: None,
            toast: None,
            pending_write: None,
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
        changed |= self.maybe_prefetch();
        changed
    }

    /// 把当前会话状态写进窗口。
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
            match state_banner(self.state) {
                Some(text) => {
                    window.set_state_text(text.into());
                    window.set_state_banner_visible(true);
                }
                None => window.set_state_banner_visible(false),
            }
            let git_text = match self.service.capabilities().git_version.as_deref() {
                Some(version) => format!("git {version} · 内置引擎读取"),
                None => "未找到 git（只读浏览）".to_owned(),
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

        self.apply_overlays(window);
    }

    /// 写 Diff 面板（行模型 / 标题 / 统计 / 按块操作的可用性）。
    fn apply_diff_panel(&self, window: &MainWindow) {
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
                "已暂存"
            } else {
                "工作区"
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
            self.show_toast(format!("无法提交：{reason}"));
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
            return "暂存区为空：先暂存要提交的内容".to_owned();
        }
        format!(
            "将提交 {} 个已暂存路径",
            partition.staged.len() + partition.conflicted.len()
        )
    }

    // ------------------------------------------------------------ 文件级写操作

    /// 暂存当前选中的更改。
    pub fn stage_selected_change(&mut self) {
        let Some(paths) = self.selected_paths() else {
            self.show_toast("请先选择一个文件");
            return;
        };
        self.submit_write(WriteRequest::Stage(paths), PendingWrite::Other);
    }

    /// 反暂存当前选中的更改。
    pub fn unstage_selected_change(&mut self) {
        let Some(paths) = self.selected_paths() else {
            self.show_toast("请先选择一个文件");
            return;
        };
        self.submit_write(WriteRequest::Unstage(paths), PendingWrite::Other);
    }

    /// 请求丢弃当前选中更改的修改（需强确认）。
    pub fn request_discard_change(&mut self) {
        let Some(paths) = self.selected_paths() else {
            self.show_toast("请先选择一个文件");
            return;
        };
        self.confirm = Some(Confirm::DiscardChanges(paths));
        self.dirty.diff = true;
    }

    /// 请求删除选中的未跟踪路径（需强确认）。
    pub fn request_delete_untracked(&mut self) {
        let Some(paths) = self.selected_paths() else {
            self.show_toast("请先选择一个未跟踪路径");
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
            self.show_toast("请先选择一个路径");
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
            self.show_toast("请先在差异视图中选择 hunk 或行");
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
            self.show_toast("当前差异不支持按块操作；新增文件请用「暂存」整文件操作");
            return;
        }
        if self.diff_selection.is_empty() {
            self.show_toast("请先在差异视图中选择 hunk 或行");
            return;
        }
        let Some(diff) = self.diff.as_ref() else {
            return;
        };
        let Some(path) = self.diff_path.clone() else {
            self.show_toast("当前差异不支持按块操作");
            return;
        };
        let selections = self.selections_from_diff(diff);
        if selections.is_empty() {
            self.show_toast("所选内容不包含可落库的变更");
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
            self.show_toast("请先在输入框填写新分支名");
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
            self.show_toast("请先在侧栏选择一个分支");
            return;
        };
        if ordered.kind != gt_core::RefKind::LocalBranch {
            // 远程分支与企业惯例：先建本地跟踪分支再检出；M2 只给出明确说明。
            self.show_toast("只能直接检出本地分支；远程分支请先「新建」本地分支");
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
            self.show_toast("请先在侧栏选择一个分支");
            return;
        };
        let to = self.branch_input.trim().to_owned();
        if to.is_empty() || to == entry.name {
            self.show_toast("请在输入框填写新的分支名");
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
            self.show_toast("请先在侧栏选择一个分支");
            return;
        };
        if entry.is_head {
            self.show_toast("不能删除当前所在的分支");
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
            self.show_toast("请先在侧栏选择一个分支");
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
            self.show_toast("请先在提交列表中选择一条提交");
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
            self.show_toast("请输入搜索内容");
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
                    self.show_toast("选择已失效，请重新选择");
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
                None => "未找到 git（只读模式）".to_owned(),
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
            self.show_toast("上一个操作还未完成，请稍候");
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
                self.show_toast(format!("正在{label}…"));
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
                let _ = changes;
                true
            }
            Outcome::FileDiff(diff) => {
                self.diff_in_flight = false;
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
            Outcome::Failed(message) => {
                // 失败也要把在飞标记清掉，否则界面会永远显示"加载中"。
                if lane == gt_service::RequestLane::Write {
                    self.write_in_flight = false;
                    self.pending_write = None;
                } else {
                    self.clear_in_flight();
                }
                tracing::warn!(kind = delivery.kind.label(), %message, "请求失败");
                self.show_toast(format!("{}失败：{message}", delivery.kind.label()));
                if self.diff.is_none() {
                    self.diff_title = format!("读取失败：{message}");
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
            .then(|| format!("搜索「{query}」超过 1.5 秒已中止；大仓库可等待后续版本的历史索引"));
        append_log_page(&mut self.log, 0, nodes);
        self.diff_title = format!("搜索：{query}");
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
        let base = format!("已加载 {} / {} 条", self.log.nodes.len(), self.log.total);
        match &self.log.note {
            Some(note) => format!("{base} · {note}"),
            None => base,
        }
    }

    fn change_summary(&self) -> String {
        let parts = self.changes.partition();
        format!(
            "(冲突 {} · 未跟踪 {} · 已暂存 {} · 已修改 {})",
            parts.conflicted.len(),
            parts.untracked.len(),
            parts.staged.len(),
            parts.modified.len()
        )
    }

    fn diff_stats(&self) -> String {
        self.diff.as_ref().map_or_else(String::new, |diff| {
            if diff.stats.binary {
                "二进制".to_owned()
            } else {
                format!("+{} -{}", diff.stats.added, diff.stats.removed)
            }
        })
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
            "日志页不是顺序追加，已丢弃"
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
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
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
