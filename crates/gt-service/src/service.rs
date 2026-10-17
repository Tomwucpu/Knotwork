//! 服务调度层：仓库注册表、请求派发、结果代次校验、只读降级。
//!
//! # 一个请求的完整生命周期（架构规划 4.3）
//!
//! 1. UI 线程调 [`GitService::request`]：从注册表拿到仓库句柄、推进该仓库的
//!    代次闸门、把任务投进有界线程池 —— **全部是非阻塞操作**；
//! 2. 工作线程取到任务后：用共享句柄生成本线程的 `GitRepo`，在分片边界检查
//!    取消令牌，执行读取；
//! 3. 结果回传前用闸门校验代次：过期就丢弃（连消息都不发），
//!    过期的任务不会用陈旧数据污染界面；
//! 4. UI 线程从通道里取 [`Delivery`] 合并到视图模型。
//!
//! # 任务队列满了怎么办
//!
//! 不做等待、不做丢弃静默：[`GitService::request`] 直接返回
//! [`RequestError::QueueFull`]，由 UI 决定是提示用户还是取消上一个请求。
//! 让 UI 线程阻塞等一个"可能会很久"的槽位，正是这类工具卡顿的典型成因。
//!
//! # git 可执行文件缺失（FR-STA-04）
//!
//! 启动时探测一次，结果固化在 [`Capabilities`] 里。缺失时只读的内置引擎路径
//! （状态、日志、diff）照常工作，依赖 CLI 的能力被置灰并在界面上说明原因 ——
//! 而不是让用户在点了按钮之后才收到一句"找不到 git"。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crossbeam_channel::Sender;
use gt_backend::{
    noop_observer, BackendError, GitCli, GitRepo, LockAttempt, RepoLocation, RepoWriter,
    SharedGitRepo, SharedObserver, UpstreamRow, WriteLock,
};
use gt_core::{BranchScope, GraphOptions, LogPageRequest, Oid, RepoId};

use crate::cancel::CancellationToken;
use crate::generation::{Generation, RequestGate};
use crate::pool::WorkerPool;
use crate::task::{
    Delivery, DiffSide, LogPageResult, Outcome, RepoOverview, RequestError, RequestKind,
    RequestLane, SearchResult, SelectionMode, WriteRequest,
};
use crate::watch::{ChangeBatch, ChangeSink, RepoPaths, RepoWatcher, DEFAULT_DEBOUNCE};

/// blob 读取上限：超过即返回占位而不是内容（架构规划 5.5 的 2MB 阈值）。
pub const MAX_BLOB_BYTES: usize = 2 * 1024 * 1024;

/// 启动探测得到的可用能力。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// 探测到的 git 可执行文件版本；[`None`] 表示不可用（只读降级）。
    pub git_version: Option<String>,
}

impl Capabilities {
    /// git CLI 是否可用。
    #[must_use]
    pub const fn cli_available(&self) -> bool {
        self.git_version.is_some()
    }

    /// 依赖 CLI 的能力被禁用时的原因说明（供界面置灰提示）。
    ///
    /// 验收口径（FR-STA-04）：缺失时**明确降级**并把原因讲清楚，
    /// 而不是让操作失败后再报错。
    #[must_use]
    pub fn cli_unavailable_reason(&self) -> Option<String> {
        if self.cli_available() {
            None
        } else {
            Some(
                "未找到 git 可执行文件：只读的仓库浏览可用；\
                 远端操作、hook、rebase 等依赖 git 的功能已禁用。\
                 可通过环境变量 KNOTWORK_GIT 指定 git 路径。"
                    .to_owned(),
            )
        }
    }
}

/// 已打开仓库的句柄。
pub struct RepoHandle {
    id: RepoId,
    repo: SharedGitRepo,
    /// 每个视图通道一把闸门：Log 的新请求只作废旧的 Log 请求。
    gates: Mutex<GateSet>,
    log_index: Mutex<LogIndex>,
    watcher: Mutex<Option<RepoWatcher>>,
    /// 只读原因（FR-MR-04）；[`None`] 表示读写可用。
    read_only: Option<String>,
    /// 持有的写锁；窗口关闭时随句柄一起释放。
    write_lock: Mutex<Option<WriteLock>>,
    /// 启动时探测到的 git 句柄；缺失时写操作会给出明确降级说明。
    cli: Option<GitCli>,
}

/// 视图通道的闸门集合。
#[derive(Debug, Default)]
struct GateSet {
    overview: RequestGate,
    status: RequestGate,
    log: RequestGate,
    diff: RequestGate,
    /// 写通道的闸门：只用来发号与取取消令牌，**从不作废**。
    write: RequestGate,
}

impl GateSet {
    fn get(&self, lane: RequestLane) -> &RequestGate {
        match lane {
            RequestLane::Overview => &self.overview,
            RequestLane::Status => &self.status,
            RequestLane::Log => &self.log,
            RequestLane::Diff => &self.diff,
            RequestLane::Write => &self.write,
        }
    }
}

impl std::fmt::Debug for RepoHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepoHandle")
            .field("id", &self.id)
            .field("location", self.repo.location())
            .field(
                "log_index",
                &self.log_index.lock().map_or(0, |index| index.ids.len()),
            )
            .finish_non_exhaustive()
    }
}

impl RepoHandle {
    /// 仓库 id。
    #[must_use]
    pub const fn id(&self) -> RepoId {
        self.id
    }

    /// 是否只能读（FR-MR-04）。
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        self.read_only.is_some()
    }

    /// 只读原因；可写时为 [`None`]。
    #[must_use]
    pub fn read_only_reason(&self) -> Option<&str> {
        self.read_only.as_deref()
    }

    /// 显式释放写锁（关闭窗口时调用；`Drop` 也会做，这里是为了确定性）。
    pub fn release_write_lock(&self) {
        let taken = self
            .write_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        drop(taken);
    }

    /// 仓库位置。
    #[must_use]
    pub fn location(&self) -> &RepoLocation {
        self.repo.location()
    }

    /// 用于窗口标题的显示名。
    #[must_use]
    pub fn display_name(&self) -> String {
        self.repo.location().display_name()
    }

    /// 文件监听涉及的路径。
    #[must_use]
    pub fn watch_paths(&self) -> RepoPaths {
        RepoPaths {
            work_dir: self.repo.location().work_dir.clone(),
            git_dir: self.repo.location().git_dir.clone(),
        }
    }

    /// 作废该仓库全部通道当前代次的在飞任务。
    ///
    /// 写通道被**刻意排除**：用户点"刷新"不该拦下一次正在执行的提交 ——
    /// 那会让仓库处于"改了但不知道成功没有"的状态。
    pub fn cancel_in_flight(&self) {
        let gates = self.gates();
        for lane in RequestLane::ALL {
            if lane.is_gated() {
                gates.get(lane).cancel_current();
            }
        }
    }

    /// 某条通道的当前代次。
    #[must_use]
    pub fn generation(&self, lane: RequestLane) -> Generation {
        self.gates().get(lane).current()
    }

    /// 推进某条通道的代次并发号。
    fn begin(&self, lane: RequestLane) -> (Generation, CancellationToken) {
        self.gates().get(lane).begin()
    }

    /// 某条通道的指定代次是否仍然有效。
    fn is_current(&self, lane: RequestLane, generation: Generation) -> bool {
        self.gates().get(lane).is_current(generation)
    }

    /// 丢弃日志 id 列表缓存（收到 git 状态变化后调用）。
    pub fn invalidate_log_index(&self) {
        *self.lock_log_index() = LogIndex::default();
    }

    /// 启动文件监听；重复调用会替换掉旧的监听器。
    ///
    /// # Errors
    /// 监听器创建失败时返回错误（调用方通常只记录告警并继续，不阻断打开仓库）。
    pub fn start_watching<S: ChangeSink>(
        &self,
        sink: Arc<S>,
        debounce: std::time::Duration,
    ) -> Result<(), crate::watch::WatchError> {
        let watcher = RepoWatcher::start_with_debounce(&self.watch_paths(), sink, debounce)?;
        let mut slot = self
            .watcher
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = Some(watcher);
        Ok(())
    }

    /// 是否正在监听文件变化。
    #[must_use]
    pub fn is_watching(&self) -> bool {
        self.watcher
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(RepoWatcher::is_active)
    }

    /// 停止文件监听（窗口关闭时调用）。
    pub fn stop_watching(&self) {
        let taken = self
            .watcher
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        drop(taken);
    }

    fn gates(&self) -> std::sync::MutexGuard<'_, GateSet> {
        self.gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_log_index(&self) -> std::sync::MutexGuard<'_, LogIndex> {
        self.log_index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// 日志 id 列表缓存（M0 POC-2：百万提交全量枚举 release 4.23s，
/// 因此完整枚举一次后按页切片，绝不为翻页重走历史）。
#[derive(Debug, Default)]
struct LogIndex {
    /// 缓存所对应的查询口径：分支范围 + 图选项。
    ///
    /// 图选项必须进缓存键：同一 `scope` 下"隐藏合并"与"不隐藏"是两个不同的
    /// id 列表，只按 scope 缓存会让切换选项后仍拿到旧列表（静默错误）。
    key: Option<(BranchScope, GraphOptions)>,
    /// 完整的提交 id 列表（revwalk 顺序）。
    ids: Vec<Oid>,
    /// 该列表的降级说明（例如拓扑排序因规模退回日期序）。
    note: Option<String>,
}

impl LogIndex {
    /// 确保缓存与查询口径匹配，返回 id 列表的引用与降级说明。
    ///
    /// # Errors
    /// 仓库读取失败时返回错误。
    fn ensure(
        &mut self,
        repo: &GitRepo,
        scope: BranchScope,
        graph: GraphOptions,
        token: &CancellationToken,
    ) -> Result<(&[Oid], bool, Option<String>), BackendError> {
        let key = (scope, graph);
        let from_cache = self.key == Some(key);
        if !from_cache {
            token
                .check()
                .map_err(|_| BackendError::Status("操作已取消".to_owned()))?;
            // 构建完整请求只为拿 scope/graph：offset/limit 在下游切片时用。
            let request = LogPageRequest {
                filter: gt_core::LogFilter {
                    scope,
                    ..gt_core::LogFilter::default()
                },
                offset: 0,
                limit: gt_core::DEFAULT_PAGE_SIZE,
                graph,
            };
            let (ids, note) = repo.walk_ids_with(&request, None)?;
            self.ids = ids;
            self.note = note;
            self.key = Some(key);
        }
        Ok((&self.ids, from_cache, self.note.clone()))
    }
}

/// 服务层入口。
pub struct GitService {
    pool: WorkerPool,
    cli: Option<GitCli>,
    caps: Capabilities,
    observer: SharedObserver,
    repos: Mutex<HashMap<RepoId, Arc<RepoHandle>>>,
    next_repo_id: Mutex<u32>,
}

impl std::fmt::Debug for GitService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitService")
            .field("pool", &self.pool)
            .field("capabilities", &self.caps)
            .field("repos", &self.repo_count())
            .finish_non_exhaustive()
    }
}

impl GitService {
    /// 建服务；探测 git CLI 并固化能力集。
    ///
    /// `observer` 通常传 [`crate::console::ConsoleBuffer`] 的观察者句柄；
    /// 不需要审计时传 [`noop_observer`]。
    ///
    /// # Errors
    /// 工作线程创建失败时返回错误。
    pub fn new(observer: SharedObserver) -> std::io::Result<Self> {
        let pool = WorkerPool::new()?;
        let cli = GitCli::detect();
        let caps = Capabilities {
            git_version: cli.as_ref().map(|cli| cli.version().to_owned()),
        };
        Ok(Self {
            pool,
            cli,
            caps,
            observer,
            repos: Mutex::new(HashMap::new()),
            next_repo_id: Mutex::new(1),
        })
    }

    /// 是否启用了审计观察者。
    #[must_use]
    pub fn observer(&self) -> &SharedObserver {
        &self.observer
    }

    /// 启动探测得到的能力集。
    #[must_use]
    pub const fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    /// 已探测到的 git 可执行文件句柄；CLI 缺失时为 [`None`]。
    ///
    /// M1 的读取路径全部走内置引擎，这里的句柄主要供界面展示 git 版本；
    /// M2 的写路径（暂存、提交）与 M3 的远端路径会用它起子进程。
    #[must_use]
    pub fn cli(&self) -> Option<&GitCli> {
        self.cli.as_ref()
    }

    /// 工作线程池句柄（诊断用）。
    #[must_use]
    pub const fn pool(&self) -> &WorkerPool {
        &self.pool
    }

    /// 已打开仓库数。
    #[must_use]
    pub fn repo_count(&self) -> usize {
        self.lock_repos().len()
    }

    /// 打开仓库并注册；返回新的仓库 id 与句柄。
    ///
    /// 同一路径重复打开会得到**新的 id**：同一仓库多开由上层（应用层）拒绝，
    /// 因为"是否允许重复打开"是产品策略而不是服务层的事实（FR-MR-04）。
    ///
    /// # Errors
    /// 路径不是仓库、或哈希算法不受支持时返回错误。
    pub fn open_repo(&self, path: &Path) -> Result<Arc<RepoHandle>, BackendError> {
        let repo = GitRepo::open(path, self.observer.clone())?;
        let location = repo.location().clone();

        // 写锁（FR-MR-04）：拿不到就以只读模式打开，并把原因固化在句柄上 ——
        // 界面据此禁用写操作、一次性说明原因（而不是让用户点了才报错）。
        let (read_only, lock) = match WriteLock::acquire(&location.git_dir)? {
            LockAttempt::Acquired(lock) => (None, Some(lock)),
            held @ LockAttempt::Held { .. } => (held.read_only_reason(), None),
        };
        if let Some(reason) = &read_only {
            tracing::warn!(path = %location.work_dir_or_git_dir().display(), %reason, "以只读模式打开仓库");
        }

        let id = {
            let mut counter = self
                .next_repo_id
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let id = RepoId::from_raw(*counter);
            *counter += 1;
            id
        };

        let handle = Arc::new(RepoHandle {
            id,
            repo: repo.into_shared(),
            gates: Mutex::new(GateSet::default()),
            log_index: Mutex::new(LogIndex::default()),
            watcher: Mutex::new(None),
            read_only,
            write_lock: Mutex::new(lock),
            cli: self.cli.clone(),
        });

        tracing::info!(repo = ?id, path = %location.work_dir_or_git_dir().display(), "已注册仓库");
        self.lock_repos().insert(id, Arc::clone(&handle));
        Ok(handle)
    }

    /// 取仓库句柄。
    #[must_use]
    pub fn repo(&self, id: RepoId) -> Option<Arc<RepoHandle>> {
        self.lock_repos().get(&id).map(Arc::clone)
    }

    /// 注销仓库（关闭窗口）。返回是否存在过。
    pub fn close_repo(&self, id: RepoId) -> bool {
        let removed = self.lock_repos().remove(&id);
        match removed {
            Some(handle) => {
                handle.cancel_in_flight();
                handle.stop_watching();
                // 写锁必须在注销时立刻释放：用户关掉窗口后再开另一个实例，
                // 不该被一个"已经不再写任何东西"的锁挡住。
                handle.release_write_lock();
                true
            }
            None => false,
        }
    }

    /// 全部分配监听用的 `(仓库 id, 句柄)` 快照（注册表语义是只增不减的映射，
    /// 这里给启动页 / 窗口管理器用）。
    #[must_use]
    pub fn repos(&self) -> Vec<(RepoId, Arc<RepoHandle>)> {
        let mut list: Vec<_> = self
            .lock_repos()
            .iter()
            .map(|(id, handle)| (*id, Arc::clone(handle)))
            .collect();
        list.sort_by_key(|(id, _)| id.as_raw());
        list
    }

    /// 就一批文件变化作废相关缓存。
    ///
    /// - git 状态变化 → 日志 id 列表与状态都可能过期；
    /// - 工作区变化 → 只有状态过期。
    ///
    /// 返回"是否需要重新请求状态 / 概览"，由调用方决定是立刻重算还是等用户操作。
    #[must_use]
    pub fn apply_change_batch(&self, id: RepoId, batch: &ChangeBatch) -> ChangeImpact {
        let Some(handle) = self.repo(id) else {
            return ChangeImpact::default();
        };
        if batch.is_empty() {
            return ChangeImpact::default();
        }

        let mut impact = ChangeImpact {
            worktree_changed: batch.worktree,
            ..ChangeImpact::default()
        };

        if batch.git_state {
            // 引用 / 索引 / HEAD 变了：历史枚举的起点可能已经不同，
            // 缓存的 id 列表必须作废（否则新提交与切分支都不会反映到 Log）。
            handle.invalidate_log_index();
            impact.git_state_changed = true;
        }
        impact
    }

    /// 发起一次读取请求。
    ///
    /// **不阻塞**：入队成功立即返回该请求的代次，失败立即返回原因。
    ///
    /// # Errors
    /// 仓库未注册 → [`RequestError::UnknownRepo`]；
    /// 队列满 / 池已关闭 → [`RequestError::QueueFull`]。
    pub fn request(
        &self,
        id: RepoId,
        kind: RequestKind,
        sink: Sender<Delivery>,
    ) -> Result<Generation, RequestError> {
        let handle = self.repo(id).ok_or(RequestError::UnknownRepo(id))?;

        // 只读仓库的写请求在**入队前**就被拒：让用户立刻知道原因，
        // 而不是看着按钮转一会儿再报错。
        if matches!(kind, RequestKind::Write(_)) {
            if let Some(reason) = handle.read_only_reason() {
                return Err(RequestError::ReadOnly(reason.to_owned()));
            }
        }

        // 推进该通道的代次：这条通道上此前在飞的任务会在下一个分片边界退出，
        // 且它们的过期结果不会再被投递。其它通道的请求不受影响。
        let lane = kind.lane();
        let (generation, token) = handle.begin(lane);

        let job_handle = Arc::clone(&handle);
        let boxed: crate::pool::Job = Box::new(move || {
            run_request(&job_handle, &kind, generation, &token, &sink);
        });

        self.pool.submit(boxed).map_err(RequestError::from)?;
        Ok(generation)
    }
    /// 关闭服务：停止全部监听、注销仓库、等待工作线程收尾。
    pub fn shutdown(&mut self) {
        let handles: Vec<Arc<RepoHandle>> = {
            let mut repos = self.lock_repos();
            let handles = repos.values().map(Arc::clone).collect();
            repos.clear();
            handles
        };
        for handle in handles {
            handle.cancel_in_flight();
            handle.stop_watching();
        }
        self.pool.shutdown();
    }

    fn lock_repos(&self) -> std::sync::MutexGuard<'_, HashMap<RepoId, Arc<RepoHandle>>> {
        self.repos
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// 一批文件变化造成的影响。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChangeImpact {
    /// 工作区内容变了（状态与 diff 可能过期）。
    pub worktree_changed: bool,
    /// git 状态变了（引用 / 索引 / HEAD；概览与日志可能过期）。
    pub git_state_changed: bool,
}

impl ChangeImpact {
    /// 是否需要重新计算工作区状态。
    #[must_use]
    pub const fn needs_status_refresh(&self) -> bool {
        self.worktree_changed || self.git_state_changed
    }

    /// 是否需要重新计算仓库概览。
    #[must_use]
    pub const fn needs_overview_refresh(&self) -> bool {
        self.git_state_changed
    }
}

/// 工作线程侧：执行一次请求并把结果投递回 UI。
///
/// 这个函数是"工作线程上唯一允许做 I/O 的地方"，因此所有读取都收在这里，
/// 便于审查"UI 线程零 I/O"这条铁律。
fn run_request(
    handle: &RepoHandle,
    kind: &RequestKind,
    generation: Generation,
    token: &CancellationToken,
    sink: &Sender<Delivery>,
) {
    let began = std::time::Instant::now();
    let repo = handle.repo.to_thread_local();

    let outcome = match kind {
        RequestKind::Write(write) => match execute_write(handle, &repo, write, token) {
            Ok(outcome) => outcome,
            Err(message) => Outcome::Failed(message),
        },
        _ => match execute(&repo, kind, token, handle) {
            Ok(outcome) => outcome,
            Err(message) => Outcome::Failed(message),
        },
    };

    // 代次校验：过期结果连消息都不发（架构规划 4.3 第 6 点）。
    // 写通道例外 —— 写结果必须落地，否则用户无从知道操作是否生效。
    if kind.lane().is_gated() && !handle.is_current(kind.lane(), generation) {
        tracing::debug!(
            repo = ?handle.id,
            lane = kind.lane().label(),
            %generation,
            kind = kind.label(),
            "结果代次已过期，丢弃"
        );
        return;
    }

    tracing::trace!(
        repo = ?handle.id,
        %generation,
        kind = kind.label(),
        elapsed_ms = began.elapsed().as_millis(),
        "请求完成"
    );

    // 接收端已关闭（窗口销毁中）不是错误。
    let _ = sink.send(Delivery {
        generation,
        kind: kind.clone(),
        outcome,
    });
}

/// 按请求类别执行读取。返回的错误文本已经面向用户可读。
fn execute(
    repo: &GitRepo,
    kind: &RequestKind,
    token: &CancellationToken,
    handle: &RepoHandle,
) -> Result<Outcome, String> {
    match kind {
        RequestKind::Overview => {
            let head = repo.head().map_err(user_message)?;
            token.check().map_err(user_message)?;
            let mut refs = repo.refs().map_err(user_message)?;
            token.check().map_err(user_message)?;
            // ahead/behind（FR-BR-01）：一次 `for-each-ref` 全部取回。
            // CLI 缺失时保持 0 —— 这属于"计数不可用"，不是读失败。
            if let Some(cli) = handle.cli.as_ref() {
                let writer = RepoWriter::new(repo.location(), cli.clone(), repo.observer());
                match writer.upstream_tracking() {
                    Ok(rows) => apply_tracking(&mut refs, &rows),
                    Err(err) => tracing::debug!(%err, "读取上游跟踪信息失败，ahead/behind 置 0"),
                }
            }
            let state = repo.repo_state();
            Ok(Outcome::Overview(Box::new(RepoOverview {
                head,
                refs,
                state,
                location: repo.location().clone(),
                read_only: handle.read_only_reason().map(str::to_owned),
                user_email: repo.user_email(),
            })))
        }
        RequestKind::Status => {
            let changes = repo.status().map_err(user_message)?;
            Ok(Outcome::Status(Box::new(changes)))
        }
        RequestKind::LogPage(request) => {
            let scope = request.filter.scope;
            let (ids, from_cache, note) = {
                let mut index = handle.lock_log_index();
                let (ids, from_cache, note) = index
                    .ensure(repo, scope, request.graph, token)
                    .map_err(user_message)?;
                (ids.to_vec(), from_cache, note)
            };
            token.check().map_err(user_message)?;

            let total = ids.len();
            // 下界取 1：分页大小由调用方决定（首次加载与滚动预取可能不同），
            // 只对"异常大的一页"设上界，避免一次物化过多提交元数据。
            let limit = request.limit.clamp(1, gt_core::MAX_PAGE_SIZE);
            let end = request.offset.saturating_add(limit).min(total);
            let slice: Vec<Oid> = if request.offset < total {
                ids[request.offset..end].to_vec()
            } else {
                Vec::new()
            };
            token.check().map_err(user_message)?;

            let nodes = repo.commit_metas(&slice, request).map_err(user_message)?;
            let exhausted = end >= total;

            Ok(Outcome::LogPage(Box::new(LogPageResult {
                page: gt_core::LogNodePage {
                    nodes,
                    offset: request.offset,
                    exhausted,
                },
                total,
                from_cache,
                note,
            })))
        }
        RequestKind::TreeChanges { old, new } => {
            let changes = repo
                .tree_changes(old.as_deref(), new)
                .map_err(user_message)?;
            Ok(Outcome::TreeChanges(changes))
        }
        RequestKind::FileDiff { left, right, path } => {
            let diff = compute_file_diff(repo, left, right, path)?;
            Ok(Outcome::FileDiff(Box::new(diff)))
        }
        RequestKind::Search(request) => execute_search(repo, handle, request, token),
        RequestKind::Write(_) => Err("写请求不应进入读取路径".to_owned()),
    }
}

/// 执行一次提交消息搜索（FR-LOG-03）。
///
/// 抽成独立函数有两个理由：`execute` 已经接近行数上限；搜索的超时语义
/// （"超时不是失败"）需要注释说清楚，混在一个大 match 里很容易被后来者改错。
fn execute_search(
    repo: &GitRepo,
    handle: &RepoHandle,
    request: &crate::task::SearchRequest,
    token: &CancellationToken,
) -> Result<Outcome, String> {
    let Some(cli) = handle.cli.as_ref() else {
        return Err("提交搜索依赖 git 可执行文件（当前未检测到 git）".to_owned());
    };
    let writer = RepoWriter::new(repo.location(), cli.clone(), repo.observer());
    let (ids, timed_out) = match writer.search_commits(
        &request.query,
        request.regex,
        request.case_sensitive,
        request.all_refs,
        request.limit,
        Some(&TokenProbe(token)),
    ) {
        Ok(ids) => (ids, false),
        // 超时是"建议构建索引"的信号，不是失败：返回空结果并标记。
        Err(BackendError::ProcessTimeout { .. }) => (Vec::new(), true),
        Err(err) => return Err(user_message(err)),
    };
    token.check().map_err(user_message)?;

    let oids: Vec<Oid> = ids
        .iter()
        .filter_map(|text| Oid::from_prefix_hex(text))
        .collect();
    let request_model = LogPageRequest {
        limit: oids.len().max(1),
        ..LogPageRequest::default()
    };
    let nodes = repo
        .commit_metas(&oids, &request_model)
        .map_err(user_message)?;

    Ok(Outcome::Search(Box::new(SearchResult {
        nodes,
        query: request.query.clone(),
        timed_out,
    })))
}

/// 执行一次写操作。
///
/// 写后**不做**任何读取重算：状态与日志由界面收到结果后主动重新请求。
/// 理由是这样才能保持"一次用户操作 = 一条 Console 记录 + 一次明确的刷新"，
/// 而不是在写路径里悄悄夹带一串读请求（那些读请求的值也无人在意）。
fn execute_write(
    handle: &RepoHandle,
    repo: &GitRepo,
    request: &WriteRequest,
    token: &CancellationToken,
) -> Result<Outcome, String> {
    if let Some(reason) = handle.read_only_reason() {
        return Err(reason.to_owned());
    }
    let Some(cli) = handle.cli.as_ref() else {
        return Err(format!(
            "「{}」需要 git 可执行文件支持（当前未检测到 git）",
            request.label()
        ));
    };
    let writer = RepoWriter::new(repo.location(), cli.clone(), repo.observer());

    let outcome = match request {
        WriteRequest::Stage(paths) => writer.stage_paths(paths),
        WriteRequest::Unstage(paths) => writer.unstage_paths(paths),
        WriteRequest::Discard(paths) => writer.discard_tracked(paths),
        WriteRequest::DeleteUntracked(paths) => writer.delete_untracked(paths),
        WriteRequest::Commit(commit) => writer.commit(commit),
        WriteRequest::Branch(op) => writer.branch(op),
        WriteRequest::Ignore { target, path } => writer.append_ignore(*target, path),
        WriteRequest::Selection {
            path,
            left,
            right,
            selections,
            expected_fingerprint,
            mode,
        } => apply_selection(
            repo,
            &writer,
            path,
            left,
            right,
            selections,
            *expected_fingerprint,
            *mode,
        ),
    };

    token.check().map_err(user_message)?;

    match outcome {
        Ok(result) => {
            // 写成功意味着引用 / 索引 / HEAD 至少有一处变了：缓存必须作废，
            // 否则下一次翻页会拿到写之前的 id 列表。
            handle.invalidate_log_index();
            Ok(Outcome::Write(Box::new(result)))
        }
        Err(err) => Err(user_message(err)),
    }
}

/// 把 hunk / 行级选择落库。
///
/// 指纹校验是这里最重要的一步：用户在界面上看到的是**某一份**差异，
/// 若在他点"暂存"之前文件又被改动（外部编辑器、hook、另一个终端），
/// 用旧下标去打补丁可能命中错误的位置。宁可拒绝并要求刷新。
#[allow(clippy::too_many_arguments)]
fn apply_selection(
    repo: &GitRepo,
    writer: &RepoWriter,
    path: &str,
    left: &DiffSide,
    right: &DiffSide,
    selections: &[gt_diff::HunkSelection],
    expected_fingerprint: u64,
    mode: SelectionMode,
) -> Result<gt_backend::WriteOutcome, BackendError> {
    let diff =
        compute_file_diff(repo, left, right, path).map_err(|message| BackendError::Rejected {
            summary: message,
            detail: String::new(),
        })?;

    if diff.fingerprint() != expected_fingerprint {
        return Err(BackendError::Rejected {
            summary: "文件内容已变化，已取消本次操作，请刷新后重试".to_owned(),
            detail: String::new(),
        });
    }

    let Some(patch_text) = gt_diff::build_patch(&diff, gt_diff::PatchPaths::same(path), selections)
    else {
        return Ok(gt_backend::WriteOutcome::new("没有选中任何变更"));
    };

    match mode {
        SelectionMode::Stage => writer.apply_patch_cached(&patch_text, false),
        SelectionMode::Unstage => writer.apply_patch_cached(&patch_text, true),
        SelectionMode::Discard => writer.apply_patch_worktree(&patch_text),
    }
}

/// 把上游计数写回引用列表。
fn apply_tracking(refs: &mut [gt_core::RefInfo], rows: &[UpstreamRow]) {
    for reference in refs.iter_mut() {
        if reference.kind != gt_core::RefKind::LocalBranch {
            continue;
        }
        if let Some((_, upstream, ahead, behind)) =
            rows.iter().find(|(name, _, _, _)| name == &reference.name)
        {
            reference.upstream = upstream.clone();
            reference.ahead = *ahead;
            reference.behind = *behind;
        }
    }
}

/// 把取消令牌适配成后端的取消探针（保持依赖方向 gt-service → gt-backend）。
struct TokenProbe<'a>(&'a CancellationToken);

impl gt_backend::CancelProbe for TokenProbe<'_> {
    fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }
}

/// 计算单文件差异（组合 `gt-diff` 的引擎）。
fn compute_file_diff(
    repo: &GitRepo,
    left: &DiffSide,
    right: &DiffSide,
    path: &str,
) -> Result<gt_diff::FileDiff, String> {
    let options = gt_diff::DiffOptions::default();
    let old_bytes = read_side(repo, left, path)?;
    let new_bytes = read_side(repo, right, path)?;

    Ok(gt_diff::diff_bytes(
        path,
        path,
        old_bytes.as_deref(),
        new_bytes.as_deref(),
        &options,
    ))
}

/// 读取差异某一侧的内容。
fn read_side(repo: &GitRepo, side: &DiffSide, path: &str) -> Result<Option<Vec<u8>>, String> {
    match side {
        DiffSide::Empty => Ok(None),
        DiffSide::Rev(spec) => repo
            .blob_at(spec, path, MAX_BLOB_BYTES)
            .map_err(user_message),
        DiffSide::Index => repo.index_blob(path, MAX_BLOB_BYTES).map_err(user_message),
        DiffSide::Worktree => repo
            .worktree_blob(path, MAX_BLOB_BYTES)
            .map_err(user_message),
    }
}

/// 把后端错误转成面向用户的说明。
///
/// 保留原始错误文本（用户要把这句话贴进 issue），但去掉内部的
/// `Debug` 包装噪声。
fn user_message(error: impl std::fmt::Display) -> String {
    error.to_string()
}

/// 未启用审计时的默认观察者，避免调用方重复写 `noop_observer()`。
#[must_use]
pub fn silent_observer() -> SharedObserver {
    noop_observer()
}

/// 默认去抖窗口（透出给应用层，避免它自己去 import `watch`）。
#[must_use]
pub const fn default_debounce() -> std::time::Duration {
    DEFAULT_DEBOUNCE
}

/// 默认只读范围（D2 口径）。
#[must_use]
pub const fn default_branch_scope() -> BranchScope {
    BranchScope::LocalAndUpstream
}

/// 便捷：构造一个默认日志页请求（从第 0 条开始，一页默认大小）。
#[must_use]
pub fn first_log_page() -> LogPageRequest {
    LogPageRequest::default()
}

/// 便捷：把仓库路径转成显示字符串（供日志与诊断）。
#[must_use]
pub fn display_path(path: &Path) -> String {
    PathBuf::from(path).display().to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        default_branch_scope, display_path, Capabilities, GitService, RequestKind, MAX_BLOB_BYTES,
    };
    use crate::console::ConsoleBuffer;
    use crate::task::{DiffSide, Outcome, SelectionMode, WriteRequest};
    use crossbeam_channel::unbounded;
    use gt_core::BranchScope;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// 用 git CLI 造一个临时仓库；git 不可用时返回 `None`。
    fn tmp_repo(label: &str) -> Option<PathBuf> {
        let cli = gt_backend::GitCli::detect()?;
        let root =
            std::env::temp_dir().join(format!("knotwork-service-{}-{label}", std::process::id()));
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
        run(&["config", "user.email", "service@knotwork.invalid"]);
        run(&["config", "user.name", "Knotwork Service"]);
        run(&["config", "core.autocrlf", "false"]);
        std::fs::write(root.join("a.txt"), "hello\n").expect("写文件");
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "初始提交"]);
        Some(root)
    }

    fn service() -> (GitService, std::sync::Arc<ConsoleBuffer>) {
        let console = ConsoleBuffer::shared(64);
        let service = GitService::new(ConsoleBuffer::observer(&console)).expect("建服务应成功");
        (service, console)
    }

    /// 阻塞等待一条结果（仅测试用；UI 侧永远是非阻塞轮询）。
    fn wait_delivery(
        receiver: &crossbeam_channel::Receiver<crate::task::Delivery>,
        timeout: Duration,
    ) -> crate::task::Delivery {
        receiver.recv_timeout(timeout).expect("应在超时前收到结果")
    }

    #[test]
    fn capabilities_degrade_explicitly_when_git_is_missing() {
        let with_cli = Capabilities {
            git_version: Some("2.44.0".to_owned()),
        };
        assert!(with_cli.cli_available());
        assert_eq!(with_cli.cli_unavailable_reason(), None);

        let without = Capabilities { git_version: None };
        assert!(!without.cli_available());
        let reason = without.cli_unavailable_reason().expect("必须给出降级原因");
        assert!(
            reason.contains("KNOTWORK_GIT"),
            "应告知可用的覆盖方式：{reason}"
        );
    }

    #[test]
    fn overview_and_status_round_trip_through_the_worker_pool() {
        let Some(root) = tmp_repo("overview") else {
            return;
        };
        let (mut service, console) = service();
        let handle = service.open_repo(&root).expect("打开仓库");
        let (sender, receiver) = unbounded();

        service
            .request(handle.id(), RequestKind::Overview, sender.clone())
            .expect("投递概览");
        let delivery = wait_delivery(&receiver, Duration::from_secs(10));
        match delivery.outcome {
            Outcome::Overview(overview) => {
                assert_eq!(overview.head.branch.as_deref(), Some("main"));
                assert!(!overview.refs.is_empty(), "至少应有 main 分支");
                assert_eq!(overview.state, gt_core::RepoState::Clean);
                assert_eq!(overview.location.display_name(), handle.display_name());
            }
            other => panic!("概览结果类型不符：{other:?}"),
        }

        std::fs::write(root.join("a.txt"), "hello\nworld\n").expect("改文件");
        service
            .request(handle.id(), RequestKind::Status, sender.clone())
            .expect("投递状态");
        let delivery = wait_delivery(&receiver, Duration::from_secs(10));
        let Outcome::Status(changes) = delivery.outcome else {
            panic!("状态结果类型不符");
        };
        assert_eq!(changes.total(), 1);
        assert_eq!(changes.entries[0].path, "a.txt");

        // 审计记录应已落到 Console：内置引擎路由也要被记录（FR-CON-01）。
        assert!(!console.is_empty(), "内置引擎操作也应产生审计记录");
        assert!(console
            .snapshot()
            .iter()
            .any(|record| record.command_line.contains("git status")));

        service.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn log_page_uses_the_cached_id_list_and_slices_it() {
        let Some(root) = tmp_repo("log") else {
            return;
        };
        let (mut service, _console) = service();
        let handle = service.open_repo(&root).expect("打开仓库");
        let (sender, receiver) = unbounded();

        let mut request = crate::task::RequestKind::LogPage(super::first_log_page());
        if let RequestKind::LogPage(page) = &mut request {
            page.limit = 1;
        }

        service
            .request(handle.id(), request.clone(), sender.clone())
            .expect("投递日志页");
        let first = wait_delivery(&receiver, Duration::from_secs(10));
        let Outcome::LogPage(page) = first.outcome else {
            panic!("日志结果类型不符");
        };
        assert_eq!(page.total, 1, "只有一个提交");
        assert_eq!(page.page.nodes.len(), 1);
        assert!(page.page.exhausted);
        assert!(!page.from_cache, "首次请求必然未命中缓存");

        // 第二次请求应命中缓存（M0 POC-2 的结论：绝不为翻页重走历史）。
        service
            .request(handle.id(), request, sender.clone())
            .expect("再投递日志页");
        let second = wait_delivery(&receiver, Duration::from_secs(10));
        let Outcome::LogPage(page) = second.outcome else {
            panic!("日志结果类型不符");
        };
        assert!(page.from_cache, "同口径的后续请求应复用 id 列表");

        service.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unknown_repo_is_rejected_without_touching_the_queue() {
        let (service, _console) = service();
        let (sender, _receiver) = unbounded();
        let err = service
            .request(gt_core::RepoId::from_raw(999), RequestKind::Status, sender)
            .expect_err("未注册仓库必须被拒");
        assert!(matches!(err, crate::task::RequestError::UnknownRepo(_)));
        assert_eq!(service.pool().pending(), 0);
    }

    #[test]
    fn independent_lanes_do_not_invalidate_each_other() {
        let Some(root) = tmp_repo("lanes") else {
            return;
        };
        let (mut service, _console) = service();
        let handle = service.open_repo(&root).expect("打开仓库");
        let (sender, receiver) = unbounded();

        // 首屏会连发这三个请求。若三者共用一把代次闸门，前两个会被第三个
        // 立刻作废，窗口永远拿不到完整的首屏。
        let requested = [
            RequestKind::Overview,
            RequestKind::Status,
            RequestKind::LogPage(super::first_log_page()),
        ];
        for kind in &requested {
            service
                .request(handle.id(), kind.clone(), sender.clone())
                .expect("投递首屏请求");
        }

        let mut seen = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while seen.len() < requested.len() && Instant::now() < deadline {
            if let Ok(delivery) = receiver.recv_timeout(Duration::from_millis(200)) {
                assert!(
                    delivery.outcome.is_ok(),
                    "首屏请求不应失败：{:?}",
                    delivery.outcome
                );
                seen.push(delivery.kind);
            }
        }

        assert_eq!(
            seen.len(),
            requested.len(),
            "三条独立通道的请求都应落地，实际只收到 {seen:?}"
        );

        service.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn stale_generation_results_are_dropped() {
        let Some(root) = tmp_repo("stale") else {
            return;
        };
        let (mut service, _console) = service();
        let handle = service.open_repo(&root).expect("打开仓库");
        let (sender, receiver) = unbounded();

        // 连续投递同一仓库的多个请求：只有最后一代的结果会被采纳。
        let mut last = crate::generation::Generation::INITIAL;
        for _ in 0..8 {
            last = service
                .request(handle.id(), RequestKind::Status, sender.clone())
                .expect("投递");
        }

        let mut accepted = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            match receiver.recv_timeout(Duration::from_millis(200)) {
                Ok(delivery) => accepted.push(delivery.generation),
                Err(_) => break,
            }
        }

        assert!(!accepted.is_empty(), "至少应有一条结果被采纳");
        assert!(
            accepted.iter().all(|generation| *generation == last),
            "只应保留最新代次的结果，实际：{accepted:?}（最新为 {last}）"
        );

        service.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn file_diff_reads_both_revisions_for_a_commit_pair() {
        let Some(root) = tmp_repo("diff") else {
            return;
        };
        // 第二轮提交：改一行、加一行。
        std::fs::write(root.join("a.txt"), "hello\nworld\n").expect("改文件");
        let cli = gt_backend::GitCli::detect().expect("git");
        let output = cli
            .run(
                Some(&root),
                &["commit", "-q", "-a", "-m", "第二轮"],
                &gt_backend::NoopObserver,
                gt_backend::LOCAL_TIMEOUT,
            )
            .expect("提交");
        assert!(output.is_success());

        let (mut service, _console) = service();
        let handle = service.open_repo(&root).expect("打开仓库");
        let (sender, receiver) = unbounded();

        service
            .request(
                handle.id(),
                RequestKind::FileDiff {
                    left: DiffSide::Rev("HEAD~1".to_owned()),
                    right: DiffSide::Rev("HEAD".to_owned()),
                    path: "a.txt".to_owned(),
                },
                sender,
            )
            .expect("投递差异");

        let delivery = wait_delivery(&receiver, Duration::from_secs(10));
        let Outcome::FileDiff(diff) = delivery.outcome else {
            panic!("差异结果类型不符：{:?}", delivery.outcome);
        };
        assert_eq!(diff.stats.added, 1);
        assert_eq!(diff.stats.removed, 0);
        assert!(
            diff.unified_text().contains("+world"),
            "统一视图应包含新增行：\n{}",
            diff.unified_text()
        );

        service.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn worktree_diff_reads_the_working_copy() {
        let Some(root) = tmp_repo("worktree-diff") else {
            return;
        };
        std::fs::write(root.join("a.txt"), "hello\nworking copy\n").expect("改文件");

        let (mut service, _console) = service();
        let handle = service.open_repo(&root).expect("打开仓库");
        let (sender, receiver) = unbounded();

        service
            .request(
                handle.id(),
                RequestKind::FileDiff {
                    left: DiffSide::Rev("HEAD".to_owned()),
                    right: DiffSide::Worktree,
                    path: "a.txt".to_owned(),
                },
                sender,
            )
            .expect("投递差异");

        let delivery = wait_delivery(&receiver, Duration::from_secs(10));
        let Outcome::FileDiff(diff) = delivery.outcome else {
            panic!("差异结果类型不符");
        };
        assert_eq!(diff.stats.added, 1);
        assert!(diff.unified_text().contains("+working copy"));

        service.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn error_message_mentions_the_problem_for_non_repositories() {
        let root = std::env::temp_dir().join(format!(
            "knotwork-service-not-a-repo-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("建目录");

        let (mut service, _console) = service();
        let err = service.open_repo(&root).expect_err("非仓库应失败");
        assert!(matches!(
            err,
            gt_backend::BackendError::NotARepository { .. }
        ));

        service.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn closing_a_repo_stops_watching_and_rejects_new_requests() {
        let Some(root) = tmp_repo("close") else {
            return;
        };
        let (mut service, _console) = service();
        let handle = service.open_repo(&root).expect("打开仓库");
        let id = handle.id();
        assert!(handle
            .start_watching(
                Arc::new(crossbeam_channel::unbounded().0),
                Duration::from_millis(250)
            )
            .is_ok());
        assert!(handle.is_watching());

        assert!(service.close_repo(id));
        assert!(
            !handle.is_watching(),
            "关闭仓库必须停止监听（关闭窗口即释放）"
        );
        assert_eq!(service.repo_count(), 0);

        let (sender, _receiver) = unbounded();
        assert!(service.request(id, RequestKind::Status, sender).is_err());

        service.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn small_helpers_expose_expected_defaults() {
        assert_eq!(MAX_BLOB_BYTES, 2 * 1024 * 1024);
        assert_eq!(default_branch_scope(), BranchScope::LocalAndUpstream);
        assert_eq!(super::first_log_page().limit, gt_core::DEFAULT_PAGE_SIZE);
        assert_eq!(display_path(Path::new("/tmp/x")), "/tmp/x");
        assert_eq!(super::default_debounce(), crate::watch::DEFAULT_DEBOUNCE);
        // 静默观察者必须可用（无 Console 场景下不 panic）。
        super::silent_observer().observe(&gt_backend::CommandRecord::internal(
            gt_backend::CommandRoute::Gix,
            None,
            "noop",
            std::time::Duration::ZERO,
        ));
    }

    // ------------------------------------------------------------ M2 写路径

    /// 造一个 20 行的文件并提交，返回仓库根。
    fn tmp_repo_with_lines(label: &str) -> Option<PathBuf> {
        let root = tmp_repo(label)?;
        let baseline = (1..=20).fold(String::new(), |mut acc, n| {
            use std::fmt::Write as _;
            let _ = writeln!(acc, "line{n}");
            acc
        });
        std::fs::write(root.join("a.txt"), baseline).expect("写文件");
        let cli = gt_backend::GitCli::detect()?;
        let output = cli
            .run(
                Some(&root),
                &["commit", "-q", "-am", "多行基线"],
                &gt_backend::NoopObserver,
                gt_backend::LOCAL_TIMEOUT,
            )
            .expect("提交");
        assert!(output.is_success(), "{}", output.stderr);
        Some(root)
    }

    /// 等待一条指定类别的结果（跳过其它通道的更新）。
    fn wait_for_outcome(
        receiver: &crossbeam_channel::Receiver<crate::task::Delivery>,
        predicate: impl Fn(&Outcome) -> bool,
    ) -> Outcome {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if let Ok(delivery) = receiver.recv_timeout(Duration::from_millis(200)) {
                if predicate(&delivery.outcome) {
                    return delivery.outcome;
                }
            }
        }
        panic!("在超时前未收到期望的结果");
    }

    #[test]
    fn staging_through_the_service_updates_the_index() {
        let Some(root) = tmp_repo("write-stage") else {
            return;
        };
        std::fs::write(root.join("a.txt"), "hello\nstaged\n").expect("改文件");

        let (mut service, console) = service();
        let handle = service.open_repo(&root).expect("打开仓库");
        assert!(!handle.is_read_only(), "单实例应拿到写锁");
        let (sender, receiver) = unbounded();

        service
            .request(
                handle.id(),
                RequestKind::Write(Box::new(WriteRequest::Stage(vec!["a.txt".to_owned()]))),
                sender,
            )
            .expect("投递暂存");

        let outcome = wait_for_outcome(&receiver, Outcome::is_write);
        let Outcome::Write(result) = outcome else {
            panic!("写结果类型不符");
        };
        assert!(result.summary.contains("已暂存"), "{result:?}");

        // 与 CLI 等价的验收：索引里应有这条修改。
        let cli = gt_backend::GitCli::detect().expect("git");
        let output = cli
            .run(
                Some(&root),
                &["diff", "--cached", "--name-only"],
                &gt_backend::NoopObserver,
                gt_backend::LOCAL_TIMEOUT,
            )
            .expect("执行 git");
        assert!(output.stdout.contains("a.txt"), "{}", output.stdout);

        // Console 里应能看到这次写操作的真实命令行（FR-CON-01）。
        assert!(console
            .snapshot()
            .iter()
            .any(|record| record.command_line.contains("git add")));

        service.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn hunk_selection_applies_only_the_selected_lines() {
        let Some(root) = tmp_repo_with_lines("write-hunk") else {
            return;
        };
        // 两处相隔足够远（第 2 行与第 18 行）→ 必然是两个 hunk。
        let mut content: String = (1..=20)
            .map(|n| match n {
                2 => "CHANGED2\n".to_owned(),
                18 => "CHANGED18\n".to_owned(),
                other => format!("line{other}\n"),
            })
            .collect();
        if !content.ends_with('\n') {
            content.push('\n');
        }
        std::fs::write(root.join("a.txt"), content).expect("改文件");

        let (mut service, _console) = service();
        let handle = service.open_repo(&root).expect("打开仓库");
        let (sender, receiver) = unbounded();

        // 先取差异，拿到指纹与 hunk 结构（与界面看到的完全一致）。
        service
            .request(
                handle.id(),
                RequestKind::FileDiff {
                    left: DiffSide::Index,
                    right: DiffSide::Worktree,
                    path: "a.txt".to_owned(),
                },
                sender.clone(),
            )
            .expect("投递差异");
        let Outcome::FileDiff(diff) =
            wait_for_outcome(&receiver, |outcome| matches!(outcome, Outcome::FileDiff(_)))
        else {
            panic!("差异结果类型不符");
        };
        let hunks = diff.payload.hunks().expect("应有文本差异");
        assert!(hunks.len() >= 2, "应有两个独立 hunk：{}", hunks.len());

        service
            .request(
                handle.id(),
                RequestKind::Write(Box::new(WriteRequest::Selection {
                    path: "a.txt".to_owned(),
                    left: DiffSide::Index,
                    right: DiffSide::Worktree,
                    selections: vec![gt_diff::HunkSelection::whole(0)],
                    expected_fingerprint: diff.fingerprint(),
                    mode: SelectionMode::Stage,
                })),
                sender,
            )
            .expect("投递 hunk 暂存");
        let outcome = wait_for_outcome(&receiver, Outcome::is_write);
        assert!(outcome.is_ok(), "{:?}", outcome.error());

        // 索引里应只有第一个 hunk 的变化：CHANGED2 进入暂存，CHANGED7 没有。
        let cli = gt_backend::GitCli::detect().expect("git");
        let cached = cli
            .run(
                Some(&root),
                &["diff", "--cached", "-U0"],
                &gt_backend::NoopObserver,
                gt_backend::LOCAL_TIMEOUT,
            )
            .expect("执行 git");
        assert!(cached.stdout.contains("+CHANGED2"), "{}", cached.stdout);
        assert!(
            !cached.stdout.contains("+CHANGED18"),
            "未选中的 hunk 不得被暂存：{}",
            cached.stdout
        );

        service.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_stale_fingerprint_blocks_the_patch() {
        let Some(root) = tmp_repo_with_lines("write-stale") else {
            return;
        };
        std::fs::write(root.join("a.txt"), "line1\nfirst\nline3\n").expect("改文件");

        let (mut service, _console) = service();
        let handle = service.open_repo(&root).expect("打开仓库");
        let (sender, receiver) = unbounded();

        service
            .request(
                handle.id(),
                RequestKind::Write(Box::new(WriteRequest::Selection {
                    path: "a.txt".to_owned(),
                    left: DiffSide::Index,
                    right: DiffSide::Worktree,
                    selections: vec![gt_diff::HunkSelection::whole(0)],
                    // 故意给一个不可能的指纹：模拟"界面上的差异已过期"。
                    expected_fingerprint: 0,
                    mode: SelectionMode::Stage,
                })),
                sender,
            )
            .expect("投递");

        let outcome = wait_for_outcome(&receiver, |outcome| {
            outcome.is_write() || matches!(outcome, Outcome::Failed(_))
        });
        let message = outcome.error().unwrap_or_default();
        assert!(message.contains("已变化"), "必须拒绝过期选择：{message}");

        service.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_second_handle_in_the_same_process_is_read_only() {
        let Some(root) = tmp_repo("write-lock") else {
            return;
        };
        let (mut service, _console) = service();
        let first = service.open_repo(&root).expect("第一次打开");
        let second = service.open_repo(&root).expect("第二次打开");

        assert!(!first.is_read_only());
        assert!(second.is_read_only(), "第二个句柄必须是只读");
        assert!(second
            .read_only_reason()
            .is_some_and(|reason| reason.contains("只读")));

        let (sender, _receiver) = unbounded();
        let err = service
            .request(
                second.id(),
                RequestKind::Write(Box::new(WriteRequest::Stage(vec!["a.txt".to_owned()]))),
                sender,
            )
            .expect_err("只读仓库的写请求必须在入队前被拒");
        assert!(matches!(err, crate::task::RequestError::ReadOnly(_)));

        // 释放第一个句柄的写锁后，新的打开应该恢复可写。
        service.close_repo(first.id());
        let third = service.open_repo(&root).expect("第三次打开");
        assert!(
            !third.is_read_only(),
            "前者退出后应恢复读写（FR-MR-04 验收）"
        );

        service.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn commit_through_the_service_lands_in_the_log() {
        let Some(root) = tmp_repo("write-commit") else {
            return;
        };
        std::fs::write(root.join("a.txt"), "hello\ncommitted\n").expect("改文件");

        let (mut service, _console) = service();
        let handle = service.open_repo(&root).expect("打开仓库");
        let (sender, receiver) = unbounded();

        service
            .request(
                handle.id(),
                RequestKind::Write(Box::new(WriteRequest::Stage(vec!["a.txt".to_owned()]))),
                sender.clone(),
            )
            .expect("暂存");
        let _ = wait_for_outcome(&receiver, Outcome::is_write);

        service
            .request(
                handle.id(),
                RequestKind::Write(Box::new(WriteRequest::Commit(gt_backend::CommitRequest {
                    message: "feat: 通过服务层提交".to_owned(),
                    ..gt_backend::CommitRequest::default()
                }))),
                sender.clone(),
            )
            .expect("提交");
        let outcome = wait_for_outcome(&receiver, Outcome::is_write);
        assert!(outcome.is_ok(), "{:?}", outcome.error());

        // 日志页应能看到这条新提交（写后缓存已作废）。
        service
            .request(
                handle.id(),
                RequestKind::LogPage(super::first_log_page()),
                sender,
            )
            .expect("日志");
        let outcome = wait_for_outcome(&receiver, |outcome| matches!(outcome, Outcome::LogPage(_)));
        let Outcome::LogPage(page) = outcome else {
            panic!("日志结果类型不符");
        };
        assert_eq!(
            page.page.nodes[0].summary, "feat: 通过服务层提交",
            "最新提交应排在首位"
        );

        service.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn search_returns_matching_commits() {
        let Some(root) = tmp_repo("search") else {
            return;
        };
        let cli = gt_backend::GitCli::detect().expect("git");
        let _ = cli.run(
            Some(&root),
            &[
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "feat: 可搜索的关键字",
            ],
            &gt_backend::NoopObserver,
            gt_backend::LOCAL_TIMEOUT,
        );

        let (mut service, _console) = service();
        let handle = service.open_repo(&root).expect("打开仓库");
        let (sender, receiver) = unbounded();

        service
            .request(
                handle.id(),
                RequestKind::Search(crate::task::SearchRequest {
                    query: "关键字".to_owned(),
                    ..crate::task::SearchRequest::default()
                }),
                sender,
            )
            .expect("搜索");
        let outcome = wait_for_outcome(&receiver, |outcome| matches!(outcome, Outcome::Search(_)));
        let Outcome::Search(result) = outcome else {
            panic!("搜索结果类型不符");
        };
        assert_eq!(result.nodes.len(), 1, "{:?}", result.nodes);
        assert_eq!(result.query, "关键字");
        assert!(!result.timed_out);

        service.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }
}
