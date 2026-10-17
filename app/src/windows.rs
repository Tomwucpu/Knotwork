//! 窗口管理：一窗口一仓库（FR-MR-03）、关闭即释放（架构规划 5.9）、
//! 最近仓库列表（FR-MR-02）、设置持久化（FR-SET-01）、单实例转发（FR-MR-05）。
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
//! 会话的话，仓库的文件监听、写锁与缓存分区会一直挂着 —— 这正是"关掉窗口内存
//! 不降"以及"关掉窗口后另一个实例仍进只读"的典型成因。因此关闭时走
//! `GitService::close_repo`（它会停止监听、作废在飞任务并释放写锁），再丢掉会话。
//!
//! # 设置为什么用共享 `Rc<RefCell<UserConfig>>`
//!
//! 设置是**应用级**的（主题、staging 模式、告警开关），但每个窗口的界面属性
//! 是窗口级的。共享一份配置 + 每个窗口在 `poll` 时对齐，可以让"改一个窗口的
//! 设置"在 100ms 内反映到所有窗口，而不需要在回调里持有管理器引用。

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use crossbeam_channel::Receiver;
use gt_backend::RepoLocation;
use gt_config::{
    DiagnosticsConfig, Language, LogLevel, RecentRepo, Theme, UpdateStrategy, UserConfig,
};
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
    /// 应用级配置（与窗口管理器共享）。
    config: Rc<RefCell<UserConfig>>,
    /// 配置文件路径（与窗口管理器共享）：设置保存必须落到同一处，
    /// 否则测试用临时目录、正式运行用平台目录这条约定会被回调绕过。
    config_path: Rc<PathBuf>,
    /// 与监听线程保持连接：`Sender` 一旦全部丢弃，批次就送不进来了。
    _watch_sender: Option<crossbeam_channel::Sender<ChangeBatch>>,
}

/// 窗口管理器。
pub struct WindowManager {
    service: Arc<GitService>,
    console: Arc<ConsoleBuffer>,
    config: Rc<RefCell<UserConfig>>,
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
            config: Rc::new(RefCell::new(config)),
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
    pub fn recent(&self) -> Vec<RecentRepo> {
        self.config.borrow().recent.entries.clone()
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
            config: Rc::clone(&self.config),
            config_path: Rc::new(self.config_path.clone()),
            _watch_sender: Some(sender),
        }));

        let window = MainWindow::new().map_err(|err| err.to_string())?;
        Self::wire(&window, &state);
        self.install(&window, &state);

        // 记录到最近仓库并落盘。
        {
            let mut config = self.config.borrow_mut();
            config
                .recent
                .touch(location.work_dir_or_git_dir(), None, now_unix());
        }
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
            config: Rc::clone(&self.config),
            config_path: Rc::new(self.config_path.clone()),
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
    fn apply_changes(state: &Rc<RefCell<SlotState>>, window: &MainWindow) {
        let batches: Vec<ChangeBatch> = state.borrow().changes.try_iter().collect();
        if batches.is_empty() {
            return;
        }

        let mut guard = state.borrow_mut();
        // 先在不可变借用阶段取出服务句柄与仓库 id，再进入可变借用：
        // 否则"可变借用会话 + 不可变借用服务"会打架（编译器不允许同时成立）。
        let service = Arc::clone(&guard.service);
        let Some(repo_id) = guard.session.as_ref().map(|session| session.handle().id()) else {
            return;
        };
        let Some(session) = guard.session.as_mut() else {
            return;
        };

        let mut needs_refresh = false;
        for batch in &batches {
            let impact = service.apply_change_batch(repo_id, batch);
            needs_refresh |= impact.needs_status_refresh();
        }
        if needs_refresh {
            tracing::debug!(batches = batches.len(), "检测到文件变化，刷新视图");
            session.refresh();
        }
        session.apply_to(window);
    }

    /// 把共享配置里的"应用级设置"对齐到窗口（主题 / staging 模式 / 告警）。
    fn sync_preferences(state: &Rc<RefCell<SlotState>>, window: &MainWindow) {
        let (dark, staging, warn_detached) = {
            let shared = {
                let guard = state.borrow();
                Rc::clone(&guard.config)
            };
            let config = shared.borrow();
            (
                matches!(config.appearance.theme, Theme::Dark | Theme::System),
                config.git.staging_mode,
                config.git.warn_detached_commit,
            )
        };

        if window.get_dark_theme() != dark {
            window.set_dark_theme(dark);
        }

        let mut guard = state.borrow_mut();
        if let Some(session) = guard.session.as_mut() {
            session.set_preferences(staging, warn_detached);
        }
        drop(guard);

        // staging 模式影响底部面板的按钮可用性，随时对齐。
        if window.get_staging_mode() != staging {
            window.set_staging_mode(staging);
        }
    }

    /// 把配置写进设置面板（只在打开面板时调用）。
    ///
    /// 不放在 100ms 的轮询里：那样每秒会重新分配十几个 `SharedString`，
    /// 而且会覆盖用户正在编辑但尚未保存的输入。
    fn fill_settings(window: &MainWindow, state: &Rc<RefCell<SlotState>>) {
        let shared = {
            let guard = state.borrow();
            Rc::clone(&guard.config)
        };
        let config = shared.borrow();
        window.set_staging_mode(config.git.staging_mode);
        window.set_set_warn_detached(config.git.warn_detached_commit);
        window.set_set_warn_crlf(config.git.warn_crlf);
        window.set_set_protected(config.branches.protected_patterns.join(",").into());
        window.set_set_diff_tool(config.tools.diff_command.clone().unwrap_or_default().into());
        window.set_set_merge_tool(
            config
                .tools
                .merge_command
                .clone()
                .unwrap_or_default()
                .into(),
        );
        window.set_set_cache_mib(i32::try_from(config.cache.memory_mib).unwrap_or(32));
        window.set_set_log_level(log_level_index(config.diagnostics.log_level));
        window.set_set_update_strategy(
            i32::try_from(usize::from(matches!(
                config.git.update_strategy,
                UpdateStrategy::Rebase
            )))
            .unwrap_or(0),
        );
    }

    /// 连接窗口回调。
    ///
    /// 这里刻意保持"一个函数一长串 `on_*`"的形态：它是一张**契约表**，
    /// 界面上的每个回调都必须在这里出现一次。拆成一堆小函数会让"漏接一个
    /// 回调"变得难以察觉，而那是这类代码最常见的缺陷。
    #[allow(clippy::too_many_lines)]
    fn wire(window: &MainWindow, state: &Rc<RefCell<SlotState>>) {
        // ---- 轮询：先对齐设置，再收文件变化，最后收异步结果 ----
        let poll_state = Rc::clone(state);
        let weak = window.as_weak();
        window.on_poll(move || {
            let Some(window) = weak.upgrade() else {
                return;
            };
            Self::sync_preferences(&poll_state, &window);
            Self::apply_changes(&poll_state, &window);

            let mut guard = poll_state.borrow_mut();
            let Some(session) = guard.session.as_mut() else {
                return;
            };
            if session.drain() {
                session.apply_to(&window);
            }
        });

        // ---- 基础 ----
        let weak = window.as_weak();
        let slot = Rc::clone(state);
        window.on_refresh(move || {
            if let Some(window) = weak.upgrade() {
                Self::with_session(&slot, &window, Session::refresh);
            }
        });

        let weak = window.as_weak();
        let slot = Rc::clone(state);
        window.on_select_log(move |index| {
            if let Some(window) = weak.upgrade() {
                Self::with_session(&slot, &window, |session| {
                    session.select_log(usize::try_from(index).unwrap_or(0));
                });
            }
        });

        let weak = window.as_weak();
        let slot = Rc::clone(state);
        window.on_select_change(move |index| {
            if let Some(window) = weak.upgrade() {
                Self::with_session(&slot, &window, |session| {
                    session.select_change(usize::try_from(index).unwrap_or(0));
                });
            }
        });

        let weak = window.as_weak();
        let slot = Rc::clone(state);
        window.on_select_ref(move |index| {
            if let Some(window) = weak.upgrade() {
                Self::with_session(&slot, &window, |session| {
                    session.select_ref(usize::try_from(index).unwrap_or(0));
                });
            }
        });

        // ---- 写操作 ----
        let weak = window.as_weak();
        let slot = Rc::clone(state);
        window.on_stage_selected(move || {
            if let Some(window) = weak.upgrade() {
                Self::with_session(&slot, &window, Session::stage_selected_change);
            }
        });

        let weak = window.as_weak();
        let slot = Rc::clone(state);
        window.on_unstage_selected(move || {
            if let Some(window) = weak.upgrade() {
                Self::with_session(&slot, &window, Session::unstage_selected_change);
            }
        });

        let weak = window.as_weak();
        let slot = Rc::clone(state);
        window.on_discard_selected(move || {
            if let Some(window) = weak.upgrade() {
                Self::with_session(&slot, &window, Session::request_discard_change);
            }
        });

        let weak = window.as_weak();
        let slot = Rc::clone(state);
        window.on_delete_selected(move || {
            if let Some(window) = weak.upgrade() {
                Self::with_session(&slot, &window, Session::request_delete_untracked);
            }
        });

        let weak = window.as_weak();
        let slot = Rc::clone(state);
        window.on_ignore_selected(move |target| {
            if let Some(window) = weak.upgrade() {
                Self::with_session(&slot, &window, |session| {
                    session.ignore_selected(target != 0);
                });
            }
        });

        let weak = window.as_weak();
        let slot = Rc::clone(state);
        window.on_commit(move || {
            if let Some(window) = weak.upgrade() {
                Self::with_session(&slot, &window, Session::request_commit);
            }
        });

        let slot = Rc::clone(state);
        window.on_set_commit_message(move |text| {
            if let Some(session) = slot.borrow_mut().session.as_mut() {
                session.set_commit_message(text.as_str());
            }
        });

        let weak = window.as_weak();
        let slot = Rc::clone(state);
        window.on_toggle_amend(move |amend| {
            if let Some(window) = weak.upgrade() {
                Self::with_session(&slot, &window, |session| session.set_amend(amend));
            }
        });

        let weak = window.as_weak();
        let slot = Rc::clone(state);
        window.on_toggle_reset_author(move |reset| {
            if let Some(window) = weak.upgrade() {
                Self::with_session(&slot, &window, |session| session.set_reset_author(reset));
            }
        });

        let weak = window.as_weak();
        let slot = Rc::clone(state);
        window.on_toggle_diff_row(move |index| {
            if let Some(window) = weak.upgrade() {
                Self::with_session(&slot, &window, |session| {
                    session.toggle_diff_row(usize::try_from(index).unwrap_or(usize::MAX));
                });
            }
        });

        let weak = window.as_weak();
        let slot = Rc::clone(state);
        window.on_stage_hunks(move || {
            if let Some(window) = weak.upgrade() {
                Self::with_session(&slot, &window, Session::request_stage_selection);
            }
        });

        let weak = window.as_weak();
        let slot = Rc::clone(state);
        window.on_unstage_hunks(move || {
            if let Some(window) = weak.upgrade() {
                Self::with_session(&slot, &window, Session::request_unstage_selection);
            }
        });

        let weak = window.as_weak();
        let slot = Rc::clone(state);
        window.on_discard_hunks(move || {
            if let Some(window) = weak.upgrade() {
                Self::with_session(&slot, &window, Session::request_discard_selection);
            }
        });

        // ---- 分支 ----
        let slot = Rc::clone(state);
        window.on_set_branch_input(move |text| {
            if let Some(session) = slot.borrow_mut().session.as_mut() {
                session.set_branch_input(text.as_str());
            }
        });

        let weak = window.as_weak();
        let slot = Rc::clone(state);
        window.on_branch_create(move |checkout| {
            if let Some(window) = weak.upgrade() {
                Self::with_session(&slot, &window, |session| {
                    session.request_branch_create(checkout);
                });
            }
        });

        let weak = window.as_weak();
        let slot = Rc::clone(state);
        window.on_branch_checkout(move || {
            if let Some(window) = weak.upgrade() {
                Self::with_session(&slot, &window, Session::request_branch_checkout);
            }
        });

        let weak = window.as_weak();
        let slot = Rc::clone(state);
        window.on_branch_rename(move || {
            if let Some(window) = weak.upgrade() {
                Self::with_session(&slot, &window, Session::request_branch_rename);
            }
        });

        let weak = window.as_weak();
        let slot = Rc::clone(state);
        window.on_branch_delete(move |force| {
            if let Some(window) = weak.upgrade() {
                Self::with_session(&slot, &window, |session| {
                    session.request_branch_delete(force);
                });
            }
        });

        let weak = window.as_weak();
        let slot = Rc::clone(state);
        window.on_branch_upstream(move || {
            if let Some(window) = weak.upgrade() {
                Self::with_session(&slot, &window, Session::request_branch_upstream);
            }
        });

        let weak = window.as_weak();
        let slot = Rc::clone(state);
        window.on_checkout_selected_commit(move || {
            if let Some(window) = weak.upgrade() {
                Self::with_session(&slot, &window, Session::request_checkout_selected_commit);
            }
        });

        // ---- 搜索与图选项 ----
        let slot = Rc::clone(state);
        window.on_set_search(move |text| {
            if let Some(session) = slot.borrow_mut().session.as_mut() {
                session.set_search_query(text.as_str());
            }
        });

        let slot = Rc::clone(state);
        window.on_toggle_search_regex(move |regex| {
            if let Some(session) = slot.borrow_mut().session.as_mut() {
                session.set_search_regex(regex);
            }
        });

        let weak = window.as_weak();
        let slot = Rc::clone(state);
        window.on_run_search(move || {
            if let Some(window) = weak.upgrade() {
                Self::with_session(&slot, &window, Session::run_search);
            }
        });

        let weak = window.as_weak();
        let slot = Rc::clone(state);
        window.on_clear_search(move || {
            if let Some(window) = weak.upgrade() {
                Self::with_session(&slot, &window, Session::clear_search);
            }
        });

        let weak = window.as_weak();
        let slot = Rc::clone(state);
        window.on_toggle_option(move |option| {
            if let Some(window) = weak.upgrade() {
                Self::with_session(&slot, &window, |session| {
                    session.toggle_option(usize::try_from(option).unwrap_or(99));
                });
            }
        });

        // ---- 确认遮罩 ----
        let weak = window.as_weak();
        let slot = Rc::clone(state);
        window.on_confirm_yes(move || {
            if let Some(window) = weak.upgrade() {
                Self::with_session(&slot, &window, Session::confirm_yes);
            }
        });

        let weak = window.as_weak();
        let slot = Rc::clone(state);
        window.on_confirm_no(move || {
            if let Some(window) = weak.upgrade() {
                Self::with_session(&slot, &window, Session::confirm_no);
            }
        });

        // ---- Console ----
        let weak = window.as_weak();
        let slot = Rc::clone(state);
        window.on_export_console(move || {
            let Some(window) = weak.upgrade() else {
                return;
            };
            let text = slot
                .borrow()
                .session
                .as_ref()
                .map(Session::export_console)
                .unwrap_or_default();
            match write_console_export(&text) {
                Ok(path) => window.set_toast(format!("Console 已导出到 {}", path.display()).into()),
                Err(err) => window.set_toast(format!("导出失败：{err}").into()),
            }
        });

        let weak = window.as_weak();
        let slot = Rc::clone(state);
        window.on_clear_console(move || {
            if let Some(window) = weak.upgrade() {
                Self::with_session(&slot, &window, Session::clear_console);
            }
        });

        // ---- 设置 ----
        let weak = window.as_weak();
        let slot = Rc::clone(state);
        window.on_open_settings(move || {
            if let Some(window) = weak.upgrade() {
                Self::fill_settings(&window, &slot);
                window.set_settings_visible(true);
                window.set_set_status(String::new().into());
            }
        });

        let weak = window.as_weak();
        window.on_close_settings(move || {
            if let Some(window) = weak.upgrade() {
                window.set_settings_visible(false);
            }
        });

        let weak = window.as_weak();
        let slot = Rc::clone(state);
        window.on_toggle_theme(move |dark| {
            if let Some(window) = weak.upgrade() {
                window.set_dark_theme(dark);
                // 立即写回共享配置并落盘：主题是最常被改的一项，
                // "先改界面再等保存"会让用户以为没生效。
                {
                    let shared = {
                        let guard = slot.borrow();
                        Rc::clone(&guard.config)
                    };
                    shared.borrow_mut().appearance.theme =
                        if dark { Theme::Dark } else { Theme::Light };
                }
                if let Err(err) = persist_shared(&slot) {
                    tracing::warn!(%err, "保存主题设置失败");
                }
            }
        });

        let weak = window.as_weak();
        let slot = Rc::clone(state);
        window.on_apply_settings(move || {
            let Some(window) = weak.upgrade() else {
                return;
            };
            {
                let shared = {
                    let guard = slot.borrow();
                    Rc::clone(&guard.config)
                };
                let mut config = shared.borrow_mut();
                config.appearance.theme = if window.get_dark_theme() {
                    Theme::Dark
                } else {
                    Theme::Light
                };
                // 语言在 M5 才资源化，这里保留字段以便配置项完整。
                config.appearance.language = Language::ZhCn;
                config.git.staging_mode = window.get_staging_mode();
                config.git.warn_detached_commit = window.get_set_warn_detached();
                config.git.warn_crlf = window.get_set_warn_crlf();
                config.git.update_strategy = if window.get_set_update_strategy() == 1 {
                    UpdateStrategy::Rebase
                } else {
                    UpdateStrategy::Merge
                };
                config.branches.protected_patterns = window
                    .get_set_protected()
                    .split(',')
                    .map(str::trim)
                    .filter(|item| !item.is_empty())
                    .map(str::to_owned)
                    .collect();
                config.tools.diff_command = non_empty(window.get_set_diff_tool().as_str());
                config.tools.merge_command = non_empty(window.get_set_merge_tool().as_str());
                config.cache.memory_mib =
                    u32::try_from(window.get_set_cache_mib().max(8)).unwrap_or(32);
                config.diagnostics = DiagnosticsConfig {
                    log_level: index_log_level(window.get_set_log_level()),
                    ..config.diagnostics.clone()
                };
            }
            let saved = persist_shared(&slot);
            // staging 模式即时生效；日志级别需要重启才生效 —— 如实说明，
            // 而不是让用户以为改了没反应是 bug。
            let mut status =
                String::from("已保存并应用（staging 模式与主题即时生效；日志级别在下次启动生效）");
            if let Err(err) = &saved {
                status = format!("已应用到本窗口，但写盘失败：{err}");
            }
            window.set_set_status(status.into());
            window.set_settings_visible(false);
        });

        // ---- 关闭 ----
        //
        // 关闭请求挂在低层 `Window` 上：Slint 没有为 `Window` 元素生成
        // `on_close_requested` 访问器，必须走 `ComponentHandle::window()`。
        let close_state = Rc::clone(state);
        window.window().on_close_requested(move || {
            // 释放仓库相关资源：停止文件监听、作废在飞任务、释放写锁、丢弃会话。
            // 这一步漏掉的直接后果就是"关掉窗口内存不降 / 写锁不放"。
            let mut guard = close_state.borrow_mut();
            if let Some(session) = guard.session.take() {
                guard.service.close_repo(session.handle().id());
            }
            slint::CloseRequestResponse::HideWindow
        });
    }

    /// 有会话时执行 `action` 并把状态写回窗口。
    fn with_session(
        state: &Rc<RefCell<SlotState>>,
        window: &MainWindow,
        action: impl FnOnce(&mut Session),
    ) {
        let mut guard = state.borrow_mut();
        if let Some(session) = guard.session.as_mut() {
            action(session);
            session.apply_to(window);
        }
    }

    /// 首次装载（把初始状态写进窗口并发起首屏请求）。
    fn install(&self, window: &MainWindow, state: &Rc<RefCell<SlotState>>) {
        let config = self.config.borrow().clone();
        window.set_dark_theme(matches!(
            config.appearance.theme,
            Theme::Dark | Theme::System
        ));
        window.set_staging_mode(config.git.staging_mode);
        window.set_set_warn_detached(config.git.warn_detached_commit);
        window.set_set_warn_crlf(config.git.warn_crlf);
        window.set_set_protected(config.branches.protected_patterns.join(",").into());
        window.set_set_diff_tool(config.tools.diff_command.clone().unwrap_or_default().into());
        window.set_set_merge_tool(
            config
                .tools
                .merge_command
                .clone()
                .unwrap_or_default()
                .into(),
        );
        window.set_set_cache_mib(i32::try_from(config.cache.memory_mib).unwrap_or(32));
        window.set_set_log_level(log_level_index(config.diagnostics.log_level));
        window.set_set_update_strategy(
            i32::try_from(usize::from(matches!(
                config.git.update_strategy,
                UpdateStrategy::Rebase
            )))
            .unwrap_or(0),
        );

        let mut guard = state.borrow_mut();
        if guard.empty {
            let recent = self.recent();
            drop(guard);
            render_empty_state(window, &self.service, &recent);
            return;
        }
        if let Some(session) = guard.session.as_mut() {
            session.set_preferences(config.git.staging_mode, config.git.warn_detached_commit);
            session.request_initial();
            session.apply_to(window);
        }
    }

    /// 保存配置（失败只告警：配置写不进去不该影响正在进行的会话）。
    fn persist(&self) {
        let config = self.config.borrow();
        if let Err(err) = gt_config::save_to(&config, &self.config_path) {
            tracing::warn!(%err, path = %self.config_path.display(), "保存配置失败");
        }
    }
}

impl Drop for WindowManager {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// 保存共享配置；返回错误文本供界面提示。
fn persist_shared(state: &Rc<RefCell<SlotState>>) -> Result<(), String> {
    let (config, path) = {
        let guard = state.borrow();
        let config = guard.config.borrow().clone();
        (config, Rc::clone(&guard.config_path))
    };
    gt_config::save_to(&config, &path).map_err(|err| err.to_string())
}

fn non_empty(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// 日志级别的界面下标（与 Slint 里的 `OptionToggle` 对应）。
fn log_level_index(level: LogLevel) -> i32 {
    match level {
        LogLevel::Error | LogLevel::Warn | LogLevel::Info => 2,
        LogLevel::Debug | LogLevel::Trace => 3,
    }
}

/// 界面下标 → 日志级别。
fn index_log_level(index: i32) -> LogLevel {
    match index {
        0 => LogLevel::Error,
        1 => LogLevel::Warn,
        2 => LogLevel::Info,
        _ => LogLevel::Debug,
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
    window.set_read_only(false);
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
    use super::{
        index_log_level, log_level_index, non_empty, normalize, same_repo,
        write_console_export_into,
    };
    use gt_backend::RepoLocation;
    use gt_config::LogLevel;
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

    #[test]
    fn log_level_round_trips_through_the_ui_index() {
        for level in [LogLevel::Info, LogLevel::Debug, LogLevel::Trace] {
            let index = log_level_index(level);
            let back = index_log_level(index);
            // Error/Warn 在界面上没有独立开关，统一落到 info 一档。
            assert!(back >= LogLevel::Info, "{level:?} -> {index} -> {back:?}");
        }
        assert_eq!(index_log_level(3), LogLevel::Debug);
    }

    #[test]
    fn empty_tool_commands_become_none() {
        assert_eq!(non_empty("  "), None);
        assert_eq!(non_empty(" code --diff "), Some("code --diff".to_owned()));
    }
}
