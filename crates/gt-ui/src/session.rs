//! 每个窗口一个的会话：把服务层的异步结果合并成界面状态。
//!
//! # 职责边界
//!
//! - **只做数据搬运与状态合并**：不打开仓库、不读文件、不起进程（架构规划 4.2
//!   的"UI 线程零 I/O"）；所有数据来自 `gt-service` 的 [`Delivery`]；
//! - **不重建模型除非真的变了**：界面每 100ms 轮询一次，若无条件重建 500 行的
//!   模型，滚动会明显掉帧。因此用脏标记区分"数据变了"与"只是又轮询了一次"；
//! - **滚动加载按需触发**：Log 视图只在接近已加载末尾时请求下一页，
//!   与架构规划 5.1 的"全虚拟化 + 增量推进"一致。

use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender};
use gt_core::{ChangeSet, CommitMeta, HeadInfo, LogNode, Oid, RepoState};
use gt_diff::FileDiff;
use gt_graph::LaneLayout;
use gt_service::{
    first_log_page, ConsoleBuffer, Delivery, GitService, Outcome, RepoHandle, RequestKind,
};

use crate::generated::MainWindow;
use crate::view_model::{
    branch_text, build_change_rows, build_console_rows, build_diff_rows, build_log_rows,
    build_ref_rows, head_text, rc_model, state_banner,
};

/// 距离已加载末尾多少行时预取下一页。
const PREFETCH_MARGIN: usize = 50;

/// 单页提交数。
pub const LOG_PAGE_SIZE: usize = 200;

/// 一个仓库窗口的会话。
///
/// 四个 `*_in_flight` 是**相互独立**的布尔量：日志加载中不影响 diff 计算，
/// 用位标志打包只会让"某个请求是否在飞"这种最常问的问题变得难读。
#[allow(clippy::struct_excessive_bools)]
pub struct Session {
    service: Arc<GitService>,
    handle: Arc<RepoHandle>,
    sender: Sender<Delivery>,
    receiver: Receiver<Delivery>,
    console: Arc<ConsoleBuffer>,
    console_capacity: usize,

    // ---- 视图状态 ----
    head: Option<HeadInfo>,
    state: RepoState,
    refs: Vec<gt_core::RefInfo>,
    changes: ChangeSet,
    log: LogState,
    diff: Option<FileDiff>,
    diff_title: String,
    /// 当前选中的提交下标（与 `log.nodes` 对齐）。
    selected_log: Option<usize>,
    /// 当前选中的本地更改下标（与 `changes.entries` 对齐）。
    selected_change: Option<usize>,

    // ---- 在飞标记（避免重复投递同一请求）----
    overview_in_flight: bool,
    status_in_flight: bool,
    log_in_flight: bool,
    diff_in_flight: bool,

    // ---- 脏标记（决定哪些模型需要重建）----
    dirty: Dirty,
}

/// Log 视图的状态。
struct LogState {
    layout: LaneLayout,
    nodes: Vec<CommitMeta>,
    /// 与 `nodes` 一一对应的引用标注（`HEAD -> main, v1.0`）。
    refs_text: Vec<String>,
    total: usize,
    exhausted: bool,
}

impl Default for LogState {
    fn default() -> Self {
        Self {
            layout: LaneLayout::new(),
            nodes: Vec::new(),
            refs_text: Vec::new(),
            total: 0,
            exhausted: false,
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
        }
    }
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
            changes: ChangeSet::default(),
            log: LogState::default(),
            diff: None,
            diff_title: "差异".to_owned(),
            selected_log: None,
            selected_change: None,
            overview_in_flight: false,
            status_in_flight: false,
            log_in_flight: false,
            diff_in_flight: false,
            dirty: Dirty::all(),
        }
    }

    /// 仓库句柄。
    #[must_use]
    pub fn handle(&self) -> &Arc<RepoHandle> {
        &self.handle
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
        self.dirty.log = true;
        self.clear_in_flight();
        self.request_initial();
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
        // 触发一次按需加载：滚动到接近末尾时预取下一页。
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
                Some(version) => format!("内置引擎读取 · git {version}"),
                None => "内置引擎读取 · 未找到 git".to_owned(),
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
        }

        if dirty.refs {
            window.set_sidebar_refs(rc_model(build_ref_rows(&self.refs)));
        }

        if dirty.changes {
            let rows = build_change_rows(&self.changes, self.selected_change);
            window.set_change_rows(rc_model(rows));
            window.set_change_summary(self.change_summary().into());
        }

        if dirty.log {
            let rows = build_log_rows(
                &self.log.nodes,
                self.log.layout.rows(),
                &self.log.refs_text,
                self.selected_log,
                now_unix(),
            );
            window.set_log_rows(rc_model(rows));
            window.set_log_total(i32::try_from(self.log.total).unwrap_or(i32::MAX));
            window.set_log_summary(self.log_summary().into());
        }

        if dirty.diff {
            let rows = self.diff.as_ref().map_or_else(Vec::new, build_diff_rows);
            window.set_diff_rows(rc_model(rows));
            window.set_diff_title(self.diff_title.clone().into());
            window.set_diff_stats(self.diff_stats().into());
        }

        if dirty.console {
            let snapshot = self.console.snapshot();
            window.set_console_rows(rc_model(build_console_rows(&snapshot)));
            window.set_console_summary(
                format!("({}/{})", snapshot.len(), self.console_capacity).into(),
            );
        }

        // 加载指示。
        window.set_log_loading(self.log_in_flight);
        window.set_diff_loading(self.diff_in_flight);
    }

    /// 选中一条提交并请求它的文件级差异。
    pub fn select_log(&mut self, index: usize) {
        if index >= self.log.nodes.len() {
            return;
        }
        if self.selected_log == Some(index) {
            return;
        }
        self.selected_log = Some(index);
        self.dirty.log = true;

        let node = self.log.nodes[index].clone();
        self.diff_title = format!("{} — {}", node.short_id(), node.summary);
        self.diff = None;
        self.dirty.diff = true;

        // 与第一个父提交比较；根提交用空树口径。
        let old = node.first_parent().map(Oid::to_hex);
        self.request_diff(RequestKind::TreeChanges {
            old,
            new: node.id.to_hex(),
        });
    }

    /// 选中一条本地更改并请求它相对 HEAD 的差异。
    pub fn select_change(&mut self, index: usize) {
        if index >= self.changes.entries.len() {
            return;
        }
        self.selected_change = Some(index);
        self.dirty.changes = true;

        let entry = self.changes.entries[index].clone();
        self.diff_title = format!("{}（工作区）", entry.path);
        self.diff = None;
        self.dirty.diff = true;

        // 已删除的文件没有工作区侧内容；两侧都由引擎按需读取。
        let old = if entry.index == gt_core::StatusFlag::Added {
            None
        } else {
            Some("HEAD".to_owned())
        };
        self.request_diff(RequestKind::FileDiff {
            old,
            new: "HEAD".to_owned(),
            path: entry.path.clone(),
            worktree: true,
        });
    }

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

    /// 各通道是否仍有在飞请求（诊断与测试用）。
    #[must_use]
    pub const fn in_flight(&self) -> [bool; 4] {
        [
            self.overview_in_flight,
            self.status_in_flight,
            self.log_in_flight,
            self.diff_in_flight,
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
    }

    // -------------------------------------------------------------- 内部实现

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

    fn request_diff(&mut self, kind: RequestKind) {
        if self.diff_in_flight {
            return;
        }
        self.diff_in_flight = self
            .service
            .request(self.handle.id(), kind, self.sender.clone())
            .is_ok();
        // 请求可能被拒（队列满）：那就把"计算中"标记清掉，避免界面一直转圈。
        if !self.diff_in_flight {
            self.dirty.diff = true;
        }
    }

    /// 合并一条结果；返回是否改变了视图状态。
    fn apply(&mut self, delivery: Delivery) -> bool {
        match delivery.outcome {
            Outcome::Overview(overview) => {
                self.overview_in_flight = false;
                self.head = Some(overview.head.clone());
                self.refs.clone_from(&overview.refs);
                self.state = overview.state;
                self.dirty.header = true;
                self.dirty.refs = true;
                true
            }
            Outcome::Status(changes) => {
                self.status_in_flight = false;
                self.changes = (*changes).clone();
                self.dirty.changes = true;
                true
            }
            Outcome::LogPage(page) => {
                self.log_in_flight = false;
                self.log.total = page.total;
                self.log.exhausted = page.page.exhausted;
                let start = page.page.offset;
                append_log_page(&mut self.log, start, page.page.nodes);
                self.dirty.log = true;
                true
            }
            Outcome::TreeChanges(changes) => {
                self.diff_in_flight = false;
                // 提交级差异给出的是文件清单；取第一个文件的内容差异作为预览。
                // （多文件切换在 M2 的 Diff 视图里做。）
                if let Some(first) = changes.first() {
                    let new = self
                        .selected_log
                        .and_then(|index| self.log.nodes.get(index))
                        .map_or_else(|| "HEAD".to_owned(), |node| node.id.to_hex());
                    let old = self
                        .selected_log
                        .and_then(|index| self.log.nodes.get(index))
                        .and_then(CommitMeta::first_parent)
                        .map(Oid::to_hex);
                    self.diff_title = format!("{}（{}）", first.display_path(), first.kind.label());
                    self.dirty.diff = true;
                    self.request_diff(RequestKind::FileDiff {
                        old,
                        new,
                        path: first.path.clone(),
                        worktree: false,
                    });
                } else {
                    self.diff = None;
                    "该提交没有文件变更".clone_into(&mut self.diff_title);
                    self.dirty.diff = true;
                }
                true
            }
            Outcome::FileDiff(diff) => {
                self.diff_in_flight = false;
                self.diff = Some(*diff);
                self.dirty.diff = true;
                true
            }
            Outcome::Failed(message) => {
                // 失败也要把在飞标记清掉，否则界面会永远显示"加载中"。
                self.clear_in_flight();
                tracing::warn!(kind = delivery.kind.label(), %message, "请求失败");
                if self.diff.is_none() && self.diff_title == "差异" {
                    self.diff_title = format!("读取失败：{message}");
                    self.dirty.diff = true;
                }
                true
            }
        }
    }

    /// 滚动接近末尾时预取下一页。
    fn maybe_prefetch(&mut self) -> bool {
        if self.log.exhausted || self.log_in_flight {
            return false;
        }
        let loaded = self.log.nodes.len();
        if loaded == 0 || self.log.total <= loaded {
            return false;
        }
        // 界面没有滚动位置反馈（Slint ListView 不暴露首可见行），
        // 因此这里按"已加载窗口"预取：宁可早一点加载，也不要让用户滚到空白。
        if self.log.total - loaded <= PREFETCH_MARGIN {
            self.request_log_page(loaded);
            return true;
        }
        false
    }

    fn log_summary(&self) -> String {
        format!("已加载 {} / {} 条", self.log.nodes.len(), self.log.total)
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
    use super::{append_log_page, LogState, LOG_PAGE_SIZE, PREFETCH_MARGIN};
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

    /// 预取窗口应当远小于单页，否则会在第一页就立刻再请求一页。
    const _: () = assert!(PREFETCH_MARGIN < LOG_PAGE_SIZE);
}
