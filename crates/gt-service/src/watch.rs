//! 仓库文件变更监听（FR-STA-03）。
//!
//! # 为什么"过滤"比"监听"更重要
//!
//! `.git/objects` 一次 `fetch` 能产生上万条事件，`.git/index.lock` 每次
//! `git add` 都会出现又消失。这些事件对界面毫无价值，却能在几秒内把
//! 工作线程灌满。因此本模块的核心是 [`classify`] —— 一个**纯函数**，
//! 决定某个路径的变化是否值得刷新界面；它不依赖 `notify`，可以被穷举测试。
//!
//! # 两级过滤
//!
//! 1. **监听根的选择**：`.git` 在仓库内时只注册工作区根（递归），
//!    靠分类函数挡住 `.git` 内部流量；`.git` 在工作区外（worktree /
//!    submodule）时额外注册 git 目录本身与 `refs/`。
//! 2. **路径分类**：只有工作区文件与"反映 git 状态"的 `.git` 条目
//!    （index / HEAD / refs / packed-refs / 操作标记）会触发刷新。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{new_debouncer, DebounceEventResult, Debouncer, RecommendedCache};

/// 去抖窗口下限（FR-STA-03：200–300ms 合并）。
pub const DEBOUNCE_MIN: Duration = Duration::from_millis(200);
/// 去抖窗口上限。
pub const DEBOUNCE_MAX: Duration = Duration::from_millis(300);
/// 默认去抖窗口。
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(250);

/// 一次变更是否值得刷新界面，以及属于哪一侧。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relevance {
    /// 与界面无关，直接丢弃。
    Ignore,
    /// 工作区文件变化 —— 影响 Local Changes 与 Diff。
    Worktree,
    /// git 自身状态变化（索引、引用、HEAD、进行中的操作标记）——
    /// 影响 Log、分支列表与状态分区。
    GitState,
}

impl Relevance {
    /// 是否需要刷新。
    #[must_use]
    pub const fn is_relevant(self) -> bool {
        !matches!(self, Self::Ignore)
    }

    /// 是否影响工作区视图。
    #[must_use]
    pub const fn touches_worktree(self) -> bool {
        matches!(self, Self::Worktree | Self::GitState)
    }

    /// 是否影响 git 状态视图。
    #[must_use]
    pub const fn touches_git_state(self) -> bool {
        matches!(self, Self::GitState)
    }
}

/// 仓库的物理路径，用于分类事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoPaths {
    /// 工作区根；裸仓库为 [`None`]。
    pub work_dir: Option<PathBuf>,
    /// `.git` 目录。
    pub git_dir: PathBuf,
}

/// `.git` 下必须忽略的目录名（事件风暴与纯内部记账）。
const IGNORED_GIT_DIRS: &[&str] = &["objects", "logs", "lfs", "modules", "hooks", "info"];

/// `.git` 下会改变"仓库状态"的顶层条目。
const GIT_STATE_TOP_LEVEL: &[&str] = &[
    "HEAD",
    "ORIG_HEAD",
    "FETCH_HEAD",
    "MERGE_HEAD",
    "MERGE_MSG",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "BISECT_LOG",
    "index",
    "packed-refs",
    "config",
    "COMMIT_EDITMSG",
    "rebase-merge",
    "rebase-apply",
];

/// 判断一个变更路径是否需要刷新界面。
///
/// 规则（按优先级）：
///
/// 1. 位于 `.git` 下：
///    - 命中 [`GIT_STATE_TOP_LEVEL`] 或位于 `refs/` / `rebase-*` 下 → [`Relevance::GitState`]；
///    - 位于 [`IGNORED_GIT_DIRS`] 下、或以 `.lock` 结尾 → [`Relevance::Ignore`]；
///    - 其余 `.git` 内部条目 → [`Relevance::Ignore`]（保守：不确定用途就别刷新）。
/// 2. 位于工作区下 → [`Relevance::Worktree`]。
/// 3. 其它 → [`Relevance::Ignore`]。
#[must_use]
pub fn classify(path: &Path, paths: &RepoPaths) -> Relevance {
    if let Some(relative) = relative_under(path, &paths.git_dir) {
        return classify_git_internal(&relative);
    }
    if let Some(work_dir) = paths.work_dir.as_deref() {
        if relative_under(path, work_dir).is_some() {
            return Relevance::Worktree;
        }
    }
    Relevance::Ignore
}

/// 分类 `.git` 目录内部的相对路径。
fn classify_git_internal(relative: &Path) -> Relevance {
    let mut components = relative.components();
    let Some(first) = components.next() else {
        // 变化的是 `.git` 目录自身（例如被删除）。
        return Relevance::GitState;
    };
    let first = first.as_os_str().to_string_lossy();

    // 索引锁、引用锁等临时文件：每次写操作都会出现，一律丢弃。
    if first.ends_with(".lock") || relative.extension().is_some_and(|ext| ext == "lock") {
        return Relevance::Ignore;
    }

    if first == "refs" || first.starts_with("rebase-") {
        return Relevance::GitState;
    }

    if IGNORED_GIT_DIRS
        .iter()
        .any(|ignored| first.eq_ignore_ascii_case(ignored))
    {
        return Relevance::Ignore;
    }

    if GIT_STATE_TOP_LEVEL
        .iter()
        .any(|known| first.eq_ignore_ascii_case(known))
    {
        return Relevance::GitState;
    }

    // 未列出的 `.git` 内部条目：宁可漏刷新（用户还能手动刷新），
    // 也不要因为不认识的文件名把刷新打成风暴。
    Relevance::Ignore
}

/// 分区/大小写宽松的 `strip_prefix`：返回相对路径的组成部分。
///
/// Windows 与 macOS 的文件系统大小写不敏感，`notify` 给出的路径大小写
/// 未必与注册监听时一致（盘符、目录名都可能不同），严格比较会漏掉全部事件。
fn relative_under(path: &Path, base: &Path) -> Option<PathBuf> {
    let mut path_components = path.components();
    let mut base_components = base.components();

    loop {
        match base_components.next() {
            None => break,
            Some(expected) => {
                let actual = path_components.next()?;
                if !component_eq(actual.as_os_str(), expected.as_os_str()) {
                    return None;
                }
            }
        }
    }

    let rest = path_components.as_path();
    if rest.as_os_str().is_empty() {
        // 路径就是 base 本身。
        return Some(PathBuf::new());
    }
    Some(rest.to_path_buf())
}

/// 单个路径分量的比较：大小写不敏感的平台放宽大小写。
fn component_eq(left: &std::ffi::OsStr, right: &std::ffi::OsStr) -> bool {
    if cfg!(any(windows, target_os = "macos")) {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    } else {
        left == right
    }
}

/// 需要注册监听的根与模式。
#[must_use]
pub fn watch_roots(paths: &RepoPaths) -> Vec<(PathBuf, RecursiveMode)> {
    let mut roots: Vec<(PathBuf, RecursiveMode)> = Vec::new();

    let git_inside_worktree = paths
        .work_dir
        .as_deref()
        .is_some_and(|work_dir| relative_under(&paths.git_dir, work_dir).is_some());

    if let Some(work_dir) = paths.work_dir.as_deref() {
        if work_dir.is_dir() {
            roots.push((work_dir.to_path_buf(), RecursiveMode::Recursive));
        }
    }

    if !git_inside_worktree {
        // `.git` 不在工作区内（worktree / submodule）：必须单独监听，
        // 否则索引与引用的变化完全看不到。
        if paths.git_dir.is_dir() {
            roots.push((paths.git_dir.clone(), RecursiveMode::NonRecursive));
        }
        let refs = paths.git_dir.join("refs");
        if refs.is_dir() {
            roots.push((refs, RecursiveMode::Recursive));
        }
    }

    roots
}

/// 一批合并后的变更。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeBatch {
    /// 触发刷新的路径（已过滤，且去重）。
    pub paths: Vec<PathBuf>,
    /// 是否包含工作区侧变化。
    pub worktree: bool,
    /// 是否包含 git 状态侧变化。
    pub git_state: bool,
    /// 合并窗口结束的时刻。
    pub at: Instant,
}

impl ChangeBatch {
    /// 是否为空批次（全部事件都被过滤掉）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.worktree && !self.git_state
    }

    /// 是否需要刷新工作区视图。
    #[must_use]
    pub const fn needs_worktree_refresh(&self) -> bool {
        self.worktree
    }

    /// 是否需要刷新 git 状态视图。
    #[must_use]
    pub const fn needs_git_state_refresh(&self) -> bool {
        self.git_state
    }
}

/// 变更批次的接收端。
pub trait ChangeSink: Send + Sync + 'static {
    /// 收到一批合并后的变更。
    fn on_change(&self, batch: ChangeBatch);
}

impl ChangeSink for crossbeam_channel::Sender<ChangeBatch> {
    fn on_change(&self, batch: ChangeBatch) {
        // 接收端已关闭（窗口正在销毁）时静默丢弃：这不是错误。
        if self.try_send(batch).is_err() {
            tracing::trace!("变更批次无人接收，已丢弃（窗口可能正在关闭）");
        }
    }
}

/// 一个仓库的文件监听器。
///
/// 析构时自动停止监听 —— 这是"关闭窗口即释放"（架构规划 5.9）里
/// 最容易漏掉的一环：监听器线程若持有强引用，仓库的内存就永远回不去。
pub struct RepoWatcher {
    /// `Debouncer::stop` 会消费自身，因此用 `Option` 表达"已停止"。
    debouncer: Option<Debouncer<RecommendedWatcher, RecommendedCache>>,
    roots: Vec<PathBuf>,
    debounce: Duration,
}

impl std::fmt::Debug for RepoWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepoWatcher")
            .field("roots", &self.roots)
            .field("debounce", &self.debounce)
            .finish_non_exhaustive()
    }
}

impl RepoWatcher {
    /// 在 `paths` 上注册监听，去抖窗口取默认值（250ms）。
    ///
    /// # Errors
    /// 平台监听器创建失败、或没有任何可监听的根时返回错误。
    pub fn start<S: ChangeSink>(paths: &RepoPaths, sink: Arc<S>) -> Result<Self, WatchError> {
        Self::start_with_debounce(paths, sink, DEFAULT_DEBOUNCE)
    }

    /// 指定去抖窗口的版本；窗口会被夹到 [`DEBOUNCE_MIN`]..=[`DEBOUNCE_MAX`]。
    ///
    /// # Errors
    /// 同 [`Self::start`]。
    pub fn start_with_debounce<S: ChangeSink>(
        paths: &RepoPaths,
        sink: Arc<S>,
        debounce: Duration,
    ) -> Result<Self, WatchError> {
        let debounce = debounce.clamp(DEBOUNCE_MIN, DEBOUNCE_MAX);
        let roots = watch_roots(paths);
        if roots.is_empty() {
            return Err(WatchError::NoWatchableRoot);
        }

        let owned = paths.clone();
        let mut debouncer = new_debouncer(debounce, None, move |result: DebounceEventResult| {
            match result {
                Ok(events) => {
                    // 一个 `notify::Event` 可能携带多个路径（重命名会给出旧、新两个）。
                    let touched = events.iter().flat_map(|event| event.event.paths.clone());
                    if let Some(batch) = merge_events(touched, &owned) {
                        sink.on_change(batch);
                    }
                }
                Err(errors) => {
                    // 单个路径的监听失败不应中断整体监听（例如权限不足的子目录）。
                    for error in errors {
                        tracing::warn!(%error, "文件监听错误");
                    }
                }
            }
        })
        .map_err(WatchError::Notify)?;

        let mut registered = Vec::with_capacity(roots.len());
        for (root, mode) in roots {
            debouncer
                .watch(&root, mode)
                .map_err(|err| WatchError::Register {
                    path: root.clone(),
                    source: err,
                })?;
            registered.push(root);
        }

        tracing::debug!(?registered, ?debounce, "仓库文件监听已启动");
        Ok(Self {
            debouncer: Some(debouncer),
            roots: registered,
            debounce,
        })
    }

    /// 已注册监听的根。
    #[must_use]
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// 生效的去抖窗口。
    #[must_use]
    pub const fn debounce(&self) -> Duration {
        self.debounce
    }

    /// 是否仍在监听。
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.debouncer.is_some()
    }

    /// 停止监听（幂等）。
    ///
    /// 用于"窗口关闭但 `RepoWatcher` 仍被别处持有"的场景 —— 显式停掉可以
    /// 让监听线程立刻退出，而不是等最后一个 `Arc` 归零。
    pub fn stop(&mut self) {
        if let Some(debouncer) = self.debouncer.take() {
            debouncer.stop();
        }
    }
}

impl Drop for RepoWatcher {
    fn drop(&mut self) {
        // 显式停掉是为了让"关闭窗口即释放监听"在代码里可见。
        // 监听线程若被别处强引用，仓库的内存就永远回不去（架构规划 5.9）。
        self.stop();
    }
}

/// 合并一批事件：过滤、去重、归纳两侧变化。
///
/// 纯函数，便于直接对"事件风暴"做断言。
#[must_use]
pub fn merge_events(
    paths: impl IntoIterator<Item = PathBuf>,
    repo_paths: &RepoPaths,
) -> Option<ChangeBatch> {
    let mut kept: Vec<PathBuf> = Vec::new();
    let mut worktree = false;
    let mut git_state = false;

    for path in paths {
        match classify(&path, repo_paths) {
            Relevance::Ignore => {}
            Relevance::Worktree => {
                worktree = true;
                if !kept.contains(&path) {
                    kept.push(path);
                }
            }
            Relevance::GitState => {
                git_state = true;
                if !kept.contains(&path) {
                    kept.push(path);
                }
            }
        }
    }

    if !worktree && !git_state {
        return None;
    }

    Some(ChangeBatch {
        paths: kept,
        worktree,
        git_state,
        at: Instant::now(),
    })
}

/// 监听启动失败的原因。
#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    /// 平台监听器创建失败。
    #[error("创建文件监听器失败：{0}")]
    Notify(#[from] notify::Error),

    /// 注册某个根失败。
    #[error("监听路径失败：{path}（{source}）")]
    Register {
        /// 失败的路径。
        path: PathBuf,
        /// 底层错误。
        #[source]
        source: notify::Error,
    },

    /// 没有任何可监听的目录（例如裸仓库 + 不存在的 git 目录）。
    #[error("没有可监听的目录（仓库路径无效或权限不足）")]
    NoWatchableRoot,
}

#[cfg(test)]
mod tests {
    use super::{
        classify, merge_events, relative_under, watch_roots, Relevance, RepoPaths, DEBOUNCE_MAX,
        DEBOUNCE_MIN, DEFAULT_DEBOUNCE,
    };
    use notify::RecursiveMode;
    use std::path::{Path, PathBuf};

    fn paths() -> RepoPaths {
        RepoPaths {
            work_dir: Some(PathBuf::from("/repo")),
            git_dir: PathBuf::from("/repo/.git"),
        }
    }

    #[test]
    fn debounce_window_follows_the_requirement() {
        assert_eq!(DEBOUNCE_MIN, std::time::Duration::from_millis(200));
        assert_eq!(DEBOUNCE_MAX, std::time::Duration::from_millis(300));
        assert!(DEFAULT_DEBOUNCE >= DEBOUNCE_MIN && DEFAULT_DEBOUNCE <= DEBOUNCE_MAX);
    }

    #[test]
    fn worktree_files_are_relevant_git_internals_are_not() {
        let repo = paths();

        assert_eq!(
            classify(Path::new("/repo/src/main.rs"), &repo),
            Relevance::Worktree
        );
        assert_eq!(
            classify(Path::new("/repo/README.md"), &repo),
            Relevance::Worktree
        );
        assert_eq!(
            classify(Path::new("/elsewhere/x"), &repo),
            Relevance::Ignore
        );

        // 事件风暴源。
        assert_eq!(
            classify(Path::new("/repo/.git/objects/ab/cdef"), &repo),
            Relevance::Ignore
        );
        assert_eq!(
            classify(Path::new("/repo/.git/logs/HEAD"), &repo),
            Relevance::Ignore
        );
        assert_eq!(
            classify(Path::new("/repo/.git/index.lock"), &repo),
            Relevance::Ignore
        );
        assert_eq!(
            classify(Path::new("/repo/.git/refs/heads/main.lock"), &repo),
            Relevance::Ignore
        );
        // 未列出的内部条目保守忽略。
        assert_eq!(
            classify(Path::new("/repo/.git/COMMIT_EDITMSG.bak"), &repo),
            Relevance::Ignore
        );
    }

    #[test]
    fn git_state_paths_map_to_git_state() {
        let repo = paths();
        for path in [
            "/repo/.git/HEAD",
            "/repo/.git/index",
            "/repo/.git/packed-refs",
            "/repo/.git/MERGE_HEAD",
            "/repo/.git/rebase-merge/head-name",
            "/repo/.git/refs/heads/feature/x",
            "/repo/.git/config",
        ] {
            assert_eq!(
                classify(Path::new(path), &repo),
                Relevance::GitState,
                "{path} 应被判定为 git 状态变化"
            );
        }
    }

    #[test]
    fn git_dir_outside_worktree_is_still_classified() {
        // worktree / submodule 场景：git 目录在工作区之外。
        let repo = RepoPaths {
            work_dir: Some(PathBuf::from("/repo")),
            git_dir: PathBuf::from("/gitstore/repo.git"),
        };
        assert_eq!(
            classify(Path::new("/gitstore/repo.git/HEAD"), &repo),
            Relevance::GitState
        );
        assert_eq!(
            classify(Path::new("/gitstore/repo.git/objects/x"), &repo),
            Relevance::Ignore
        );
        assert_eq!(
            classify(Path::new("/repo/a.txt"), &repo),
            Relevance::Worktree
        );
    }

    #[test]
    fn bare_repository_only_tracks_git_state() {
        let bare = RepoPaths {
            work_dir: None,
            git_dir: PathBuf::from("/srv/repo.git"),
        };
        assert_eq!(
            classify(Path::new("/srv/repo.git/refs/heads/main"), &bare),
            Relevance::GitState
        );
        assert_eq!(
            classify(Path::new("/srv/repo.git/HEAD"), &bare),
            Relevance::GitState
        );
    }

    #[test]
    fn relative_under_is_case_insensitive_on_windows() {
        // Windows 上 `notify` 可能给出不同大小写的盘符或目录名。
        let base = Path::new("E:/project/Repo");
        let path = Path::new("e:/project/repo/src/main.rs");
        let relative = relative_under(path, base);
        if cfg!(any(windows, target_os = "macos")) {
            assert_eq!(relative.as_deref(), Some(Path::new("src/main.rs")));
        } else {
            assert!(relative.is_none());
        }

        assert!(relative_under(Path::new("/other/x"), Path::new("/repo")).is_none());
        assert_eq!(
            relative_under(Path::new("/repo"), Path::new("/repo")).as_deref(),
            Some(Path::new(""))
        );
    }

    #[test]
    fn merge_events_drops_storms_and_summarizes() {
        let repo = paths();
        let storm: Vec<PathBuf> = (0..500)
            .map(|index| PathBuf::from(format!("/repo/.git/objects/{index:02x}/abcdef")))
            .collect();
        assert!(
            merge_events(storm, &repo).is_none(),
            "纯内部事件风暴必须被完全丢弃"
        );

        let mixed = merge_events(
            vec![
                PathBuf::from("/repo/.git/objects/ab/cdef"),
                PathBuf::from("/repo/.git/index"),
                PathBuf::from("/repo/src/main.rs"),
                PathBuf::from("/repo/src/main.rs"),
                PathBuf::from("/repo/.git/logs/HEAD"),
                PathBuf::from("/outside/ignored.txt"),
            ],
            &repo,
        )
        .expect("应产出批次");

        assert!(mixed.worktree);
        assert!(mixed.git_state);
        assert!(mixed.needs_worktree_refresh());
        assert!(mixed.needs_git_state_refresh());
        assert_eq!(
            mixed.paths,
            vec![
                PathBuf::from("/repo/.git/index"),
                PathBuf::from("/repo/src/main.rs")
            ],
            "应去重且只保留相关路径"
        );
        assert!(!mixed.is_empty());
    }

    #[test]
    fn merge_events_allows_unrelated_worktree_change_only() {
        let repo = paths();
        let batch = merge_events(vec![PathBuf::from("/repo/a.txt")], &repo).expect("应产出批次");
        assert!(batch.worktree);
        assert!(!batch.git_state);
        assert!(!batch.needs_git_state_refresh());
    }

    #[test]
    fn watch_roots_cover_both_layouts() {
        // 常规仓库：只注册工作区根，`.git` 流量靠分类函数挡。
        let regular = watch_roots(&RepoPaths {
            work_dir: Some(std::env::temp_dir()),
            git_dir: std::env::temp_dir().join(".git"),
        });
        assert_eq!(regular.len(), 1);
        assert_eq!(regular[0].1, RecursiveMode::Recursive);

        // 裸仓库（工作区不存在）：只有 git 目录本身，会被 NonRecursive 注册。
        let temp = std::env::temp_dir();
        let bare = watch_roots(&RepoPaths {
            work_dir: None,
            git_dir: temp.clone(),
        });
        assert_eq!(bare.len(), 1);
        assert_eq!(bare[0].0, temp);
        assert_eq!(bare[0].1, RecursiveMode::NonRecursive);
    }
}
