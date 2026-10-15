//! 窗口管理：一窗口一仓库（FR-MR-03）、关闭即释放（架构规划 5.9）、
//! 最近仓库列表（FR-MR-02）。
//!
//! # 生命周期
//!
//! Slint 的回调要求 `'static`，且全部在 UI 线程上执行，因此窗口状态用
//! `Rc<RefCell<..>>` 共享 —— 这里**刻意不用 `Arc<Mutex<..>>`**：
//! 加锁只会让"UI 线程零 I/O、零阻塞"这条纪律看起来像可以松动。
//!
//! # 关闭窗口为什么必须显式注销仓库
//!
//! `Session` 持有 `Arc<RepoHandle>`，但服务层的注册表还持有一份。只 drop
//! 会话的话，仓库的文件监听与缓存分区会一直挂着 —— 这正是"关掉窗口内存
//! 不降"的典型成因。因此关闭时走 `GitService::close_repo`（它会停止监听
//! 并作废在飞任务），再丢掉会话。

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use crossbeam_channel::Receiver;
use gt_backend::RepoLocation;
use gt_config::{RecentRepo, UserConfig};
use gt_service::{ChangeBatch, ConsoleBuffer, GitService};
use gt_ui::{MainWindow, Session};
use slint::ComponentHandle;

/// Console 环形缓冲容量（FR-CON-01：可审计最近若干条操作）。
pub const CONSOLE_CAPACITY: usize = 512;

/// 一个窗口槽位。
struct Slot {
    window: MainWindow,
    state: Rc<RefCell<SlotState>>,
}

/// 窗口的运行状态。
struct SlotState {
    service: Arc<GitService>,
    /// 会话；窗口关闭后置空以释放仓库相关资源。
    session: Option<Session>,
    /// 文件变化批次（由监听线程投递，UI 线程轮询）。
    changes: Receiver<ChangeBatch>,
    /// 打开的仓库位置（用于最近仓库列表与窗口标题）。
    location: Option<RepoLocation>,
    /// 未打开仓库时的空状态（显示最近仓库）。
    empty: bool,
    /// 与监听线程保持连接：`Sender` 一旦全部丢弃，批次就送不进来了。
    _watch_sender: Option<crossbeam_channel::Sender<ChangeBatch>>,
}

/// 窗口管理器。
pub struct WindowManager {
    service: Arc<GitService>,
    console: Arc<ConsoleBuffer>,
    config: UserConfig,
    config_path: PathBuf,
    slots: Vec<Slot>,
}

impl WindowManager {
    /// 建管理器。
    ///
    /// `config` 与 `config_path` 一起传入：配置的读写都发生在本模块，
    /// 但路径由调用方决定（测试要给临时目录，正式运行给平台默认位置）。
    #[must_use]
    pub fn new(
        service: Arc<GitService>,
        console: Arc<ConsoleBuffer>,
        config: UserConfig,
        config_path: PathBuf,
    ) -> Self {
        Self {
            service,
            console,
            config,
            config_path,
            slots: Vec::new(),
        }
    }

    /// 实际存活的窗口数（已关闭的槽位会在下一次回收时消失）。
    #[must_use]
    pub fn window_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| {
                let guard = slot.state.borrow();
                guard.session.is_some() || guard.empty
            })
            .count()
    }

    /// 丢弃已关闭窗口的槽位。
    ///
    /// Slint 不提供"窗口是否还开着"的查询，因此以会话状态为准：
    /// 关闭回调会把会话置空，这里据此回收句柄。
    fn reap(&mut self) {
        self.slots.retain(|slot| {
            let guard = slot.state.borrow();
            guard.session.is_some() || guard.empty
        });
    }

    /// 最近仓库列表（只读）。
    #[must_use]
    pub fn recent(&self) -> &[RecentRepo] {
        &self.config.recent.entries
    }

    /// 打开一个仓库窗口。
    ///
    /// 同一路径重复打开会被拒绝（FR-MR-04：同一仓库不允许重复多开）。
    ///
    /// # Errors
    /// 路径不是仓库、或该仓库已打开时返回可展示的错误文本。
    pub fn open_repo(&mut self, path: &Path) -> Result<(), String> {
        self.reap();
        if let Some(existing) = self
            .slots
            .iter()
            .filter_map(|slot| slot.state.borrow().location.clone())
            .find(|location| same_repo(location, path))
        {
            return Err(format!(
                "仓库已在另一个窗口中打开：{}",
                existing.work_dir_or_git_dir().display()
            ));
        }

        let handle = self
            .service
            .open_repo(path)
            .map_err(|err| err.to_string())?;
        let location = handle.location().clone();

        // 文件监听：批次先收进通道，由 UI 线程轮询后再决定要不要重算。
        let (sender, changes) = crossbeam_channel::unbounded();
        let watch_sender = Arc::new(sender.clone());
        if let Err(err) =
            handle.start_watching(Arc::clone(&watch_sender), gt_service::default_debounce())
        {
            // 监听失败不阻断打开：状态仍然是好的，只是不会自动刷新。
            tracing::warn!(%err, "文件监听启动失败，自动刷新不可用");
        }

        let session = Session::new(
            Arc::clone(&self.service),
            Arc::clone(&handle),
            Arc::clone(&self.console),
            CONSOLE_CAPACITY,
        );

        let state = Rc::new(RefCell::new(SlotState {
            service: Arc::clone(&self.service),
            session: Some(session),
            changes,
            location: Some(location.clone()),
            empty: false,
            _watch_sender: Some(sender),
        }));

        let window = MainWindow::new().map_err(|err| err.to_string())?;
        Self::wire(&window, &state);
        self.install(&window, &state);

        // 记录到最近仓库并落盘。
        self.config
            .recent
            .touch(location.work_dir_or_git_dir(), None, now_unix());
        self.persist();

        self.slots.push(Slot { window, state });
        Ok(())
    }

    /// 打开一个"未选择仓库"的窗口（启动页）。
    ///
    /// # Errors
    /// 窗口创建失败时返回错误文本。
    pub fn open_empty(&mut self) -> Result<(), String> {
        let (_sender, changes) = crossbeam_channel::unbounded();
        let state = Rc::new(RefCell::new(SlotState {
            service: Arc::clone(&self.service),
            session: None,
            changes,
            location: None,
            empty: true,
            _watch_sender: None,
        }));

        let window = MainWindow::new().map_err(|err| err.to_string())?;
        Self::wire(&window, &state);
        self.install(&window, &state);
        self.slots.push(Slot { window, state });
        Ok(())
    }

    /// 显示全部窗口。
    ///
    /// # Errors
    /// 任一次 `show` 失败即返回错误（此时已有窗口可能已显示）。
    pub fn show_all(&self) -> Result<(), String> {
        for slot in &self.slots {
            slot.window.show().map_err(|err| err.to_string())?;
        }
        Ok(())
    }

    /// 关闭全部窗口连接并保存配置。
    pub fn shutdown(&self) {
        self.persist();
        for slot in &self.slots {
            let state = slot.state.borrow();
            if let Some(session) = state.session.as_ref() {
                state.service.close_repo(session.handle().id());
            }
        }
    }

    /// 处理所有窗口的文件变化批次并触发必要的重算。
    ///
    /// 由每个窗口的 `poll` 回调顺带调用（见 [`Self::wire`]），这样
    /// 不需要额外的定时器与全局状态。
    fn apply_changes(state: &Rc<RefCell<SlotState>>, window: &MainWindow) {
        let batches: Vec<ChangeBatch> = state.borrow().changes.try_iter().collect();
        if batches.is_empty() {
            return;
        }

        let mut guard = state.borrow_mut();
        let SlotState {
            service,
            session: Some(session),
            ..
        } = &mut *guard
        else {
            return;
        };

        let mut needs_refresh = false;
        for batch in &batches {
            let impact = service.apply_change_batch(session.handle().id(), batch);
            needs_refresh |= impact.needs_status_refresh();
        }
        if needs_refresh {
            tracing::debug!(batches = batches.len(), "检测到文件变化，刷新视图");
            session.refresh();
        }
        session.apply_to(window);
    }

    /// 连接窗口回调。
    fn wire(window: &MainWindow, state: &Rc<RefCell<SlotState>>) {
        let poll_state = Rc::clone(state);
        let weak = window.as_weak();
        window.on_poll(move || {
            let Some(window) = weak.upgrade() else {
                return;
            };
            Self::apply_changes(&poll_state, &window);

            let mut guard = poll_state.borrow_mut();
            let Some(session) = guard.session.as_mut() else {
                return;
            };
            if session.drain() {
                session.apply_to(&window);
            }
        });

        let refresh_state = Rc::clone(state);
        let weak = window.as_weak();
        window.on_refresh(move || {
            let Some(window) = weak.upgrade() else {
                return;
            };
            let mut guard = refresh_state.borrow_mut();
            if let Some(session) = guard.session.as_mut() {
                session.refresh();
                session.apply_to(&window);
            }
        });

        let changed_state = Rc::clone(state);
        let weak = window.as_weak();
        window.on_select_log(move |index| {
            let Some(window) = weak.upgrade() else {
                return;
            };
            let mut guard = changed_state.borrow_mut();
            if let Some(session) = guard.session.as_mut() {
                session.select_log(usize::try_from(index).unwrap_or(0));
                session.apply_to(&window);
            }
        });

        let changed_state = Rc::clone(state);
        let weak = window.as_weak();
        window.on_select_change(move |index| {
            let Some(window) = weak.upgrade() else {
                return;
            };
            let mut guard = changed_state.borrow_mut();
            if let Some(session) = guard.session.as_mut() {
                session.select_change(usize::try_from(index).unwrap_or(0));
                session.apply_to(&window);
            }
        });

        let console_state = Rc::clone(state);
        let weak = window.as_weak();
        window.on_export_console(move || {
            let Some(window) = weak.upgrade() else {
                return;
            };
            let mut guard = console_state.borrow_mut();
            if let Some(session) = guard.session.as_mut() {
                let text = session.export_console();
                if let Err(err) = write_console_export(&text) {
                    tracing::warn!(%err, "导出 Console 失败");
                    window.set_diff_title(format!("导出失败：{err}").into());
                } else {
                    window.set_diff_title("Console 已导出到配置目录".into());
                }
            }
        });

        // 关闭请求挂在低层 `Window` 上：Slint 没有为 `Window` 元素生成
        // `on_close_requested` 访问器，必须走 `ComponentHandle::window()`。
        let close_state = Rc::clone(state);
        window.window().on_close_requested(move || {
            // 释放仓库相关资源：停止文件监听、作废在飞任务、丢弃会话。
            // 这一步漏掉的直接后果就是"关掉窗口内存不降"。
            let id = {
                let guard = close_state.borrow();
                guard.session.as_ref().map(|session| session.handle().id())
            };
            if let Some(id) = id {
                let mut guard = close_state.borrow_mut();
                if let Some(session) = guard.session.take() {
                    guard.service.close_repo(session.handle().id());
                }
                let _ = id;
            }
            slint::CloseRequestResponse::HideWindow
        });

        let console_state = Rc::clone(state);
        let weak = window.as_weak();
        window.on_clear_console(move || {
            let Some(window) = weak.upgrade() else {
                return;
            };
            let mut guard = console_state.borrow_mut();
            if let Some(session) = guard.session.as_mut() {
                session.clear_console();
                session.apply_to(&window);
            }
        });
    }

    /// 首次装载（把初始状态写进窗口并发起首屏请求）。
    fn install(&self, window: &MainWindow, state: &Rc<RefCell<SlotState>>) {
        let mut guard = state.borrow_mut();
        if guard.empty {
            // 空状态也需要一个会话来读服务能力与最近仓库；这里直接写窗口，
            // 不建会话（没有仓库时没有任何请求可发）。
            let recent = self.recent().to_vec();
            drop(guard);
            render_empty_state(window, &self.service, &recent);
            return;
        }
        if let Some(session) = guard.session.as_mut() {
            session.request_initial();
            session.apply_to(window);
        }
    }

    /// 保存配置（失败只告警：配置写不进去不该影响正在进行的会话）。
    fn persist(&self) {
        if let Err(err) = gt_config::save_to(&self.config, &self.config_path) {
            tracing::warn!(%err, path = %self.config_path.display(), "保存配置失败");
        }
    }
}

impl Drop for WindowManager {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// 两个路径是否指向同一个仓库。
fn same_repo(location: &RepoLocation, path: &Path) -> bool {
    let existing = location.work_dir_or_git_dir();
    normalize(existing) == normalize(path)
}

/// 路径规范化（失败时退回原值）。
fn normalize(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// 把"未打开仓库"的状态写进窗口。
fn render_empty_state(window: &MainWindow, service: &GitService, recent: &[RecentRepo]) {
    window.set_has_repo(false);
    window.set_repo_title("Knotwork".into());
    window.set_repo_path(String::new().into());
    window.set_branch_text("—".into());
    window.set_head_text("—".into());
    window.set_recent(gt_ui::recent_model(recent));
    window.set_git_text(
        match service.capabilities().git_version.as_deref() {
            Some(version) => format!("git {version}"),
            None => "未找到 git（只读模式）".to_owned(),
        }
        .into(),
    );
    match service.capabilities().cli_unavailable_reason() {
        Some(reason) => {
            window.set_cli_missing(true);
            window.set_cli_missing_reason(reason.into());
        }
        None => window.set_cli_missing(false),
    }
}

/// 当前 Unix 秒。
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

/// 把 Console 导出到配置目录（FR-CON-01 的导出入口）。
///
/// # Errors
/// 无法确定配置目录或写入失败时返回错误。
fn write_console_export(text: &str) -> Result<PathBuf, String> {
    let dir = gt_config::config_dir().map_err(|err| err.to_string())?;
    write_console_export_into(&dir, text)
}

/// 导出到指定目录（把目录作为参数是为了让单测能用临时目录，
/// 而不必去改进程级的环境变量 —— 工作区禁止 `unsafe`）。
fn write_console_export_into(dir: &Path, text: &str) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|err| err.to_string())?;
    let path = dir.join(format!("console-{}.log", now_unix()));
    std::fs::write(&path, text).map_err(|err| err.to_string())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::{normalize, same_repo, write_console_export_into};
    use gt_backend::RepoLocation;
    use std::path::PathBuf;

    fn location(work: Option<&str>, git: &str) -> RepoLocation {
        RepoLocation {
            git_dir: PathBuf::from(git),
            work_dir: work.map(PathBuf::from),
            is_bare: work.is_none(),
        }
    }

    #[test]
    fn same_repo_compares_the_worktree_when_present() {
        let repo = location(Some("/tmp/demo"), "/tmp/demo/.git");
        assert!(same_repo(&repo, std::path::Path::new("/tmp/demo")));
        assert!(!same_repo(&repo, std::path::Path::new("/tmp/other")));

        // 裸仓库没有工作区，比对 `.git` 目录本身。
        let bare = location(None, "/srv/demo.git");
        assert!(same_repo(&bare, std::path::Path::new("/srv/demo.git")));
        assert!(!same_repo(&bare, std::path::Path::new("/srv/demo")));
    }

    #[test]
    fn normalize_is_total() {
        // 不存在的路径也必须返回一个可比较的值，而不是 panic。
        let value = normalize(std::path::Path::new("/definitely/not/here/knotwork"));
        assert!(!value.as_os_str().is_empty());
    }

    #[test]
    fn console_export_lands_in_the_given_directory() {
        let dir =
            std::env::temp_dir().join(format!("knotwork-console-export-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let path = write_console_export_into(&dir, "审计内容").expect("导出应成功");
        assert!(path.starts_with(&dir));
        assert_eq!(
            std::fs::read_to_string(&path).expect("读回导出文件"),
            "审计内容"
        );

        // 目录不存在时应自动创建，而不是报错。
        let nested = dir.join("nested/deeper");
        assert!(write_console_export_into(&nested, "x").is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
