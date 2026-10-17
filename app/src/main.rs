//! Knotwork 应用入口。
//!
//! # 用法
//!
//! ```text
//! knotwork                       # 打开启动页（最近仓库列表）
//! knotwork <路径> [路径...]       # 每个路径一个窗口（FR-MR-03）
//! knotwork --isolate <路径>       # 跳过单实例守卫，强制起独立进程（FR-MR-05）
//! knotwork --probe 3             # 阶段 0 的多窗口内存探针（M0 基线采集入口）
//! knotwork --askpass <提示>       # GIT_ASKPASS 回调（由 git 调用，不面向用户）
//! SLINT_BACKEND=winit-femtovg knotwork   # 切换 GPU 后端做内存 A/B 对比
//! ```
//!
//! # 启动顺序
//!
//! 1. 选默认渲染后端（软件渲染，架构规划 2.3）→ 初始化日志；
//! 2. 单实例守卫（FR-MR-05）：有主实例时把路径转发过去并立即退出；
//! 3. 建命令审计缓冲与 `GitService`（其中包含 **git CLI 探测**，FR-STA-04）；
//! 4. 读配置（含最近仓库列表）；
//! 5. 为每个命令行路径开一个窗口；没有路径时开启动页；
//! 6. 进事件循环；期间由定时器取出转发来的路径并开新窗口；
//! 7. 退出后保存配置、释放写锁并注销全部仓库。
//!
//! UI 线程在整个过程中**不读仓库文件、不起子进程**：打开仓库这一步的
//! `discover` 与配置解析是启动期一次性成本，之后所有数据都经由
//! `gt-service` 的工作线程送达。

mod ipc;
mod logging;
mod windows;

use std::cell::RefCell;
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::Arc;

use gt_config::UserConfig;
use gt_service::{ConsoleBuffer, GitService};
use slint::ComponentHandle;

use ipc::Startup;
use windows::{WindowManager, CONSOLE_CAPACITY};

/// Slint 根据 `ui/*.slint` 生成的探针窗口绑定代码。
///
/// 生成代码不携带文档注释，且不受本项目代码风格约束，因此在该模块内
/// 关闭 `missing_docs` 与 clippy 检查，避免污染工作区级别的严格 lint 配置。
#[allow(missing_docs, clippy::all, clippy::pedantic)]
mod generated {
    slint::include_modules!();
}

use generated::ProbeWindow;

/// `--askpass` 模式：把环境变量里的口令打到 stdout（FR-RMT-04）。
///
/// 这条路径对应 [`gt_backend::ASKPASS_SECRET_ENV`] 的注入方案：口令只活在
/// git 子进程的环境里，本程序读出来应答，不落盘、不进命令行参数。
/// 不是 `--askpass` 时返回 [`None`]，由正常启动流程接管。
fn askpass_mode(mut args: impl Iterator<Item = String>) -> Option<ExitCode> {
    if args.next().as_deref() != Some("--askpass") {
        return None;
    }
    let prompt = args.next().unwrap_or_default();
    match gt_backend::askpass_from_env(&prompt) {
        Some(answer) => {
            println!("{answer}");
            Some(ExitCode::SUCCESS)
        }
        // 没有环境变量时以非零码退出：git 会按"取不到凭据"处理，
        // 于是失败原因仍然是正常的认证错误，而不是挂住。
        None => Some(ExitCode::FAILURE),
    }
}

/// 探针模式的默认窗口数，对应架构规划 4.2 的 3 窗口验收场景。
const DEFAULT_PROBE_WINDOWS: usize = 3;

/// 探针窗口数上限，避免误传参数导致创建大量窗口。
const MAX_PROBE_WINDOWS: usize = 32;

/// 渲染后端选择所用的环境变量名（Slint 约定）。
const BACKEND_ENV: &str = "SLINT_BACKEND";

/// 软件渲染后端标识 —— 低内存路线的关键前提（架构规划 2.3）。
const SOFTWARE_BACKEND: &str = "winit-software";

fn main() -> ExitCode {
    // `GIT_ASKPASS` 模式必须最先处理：git 会反向调用本程序取口令，
    // 此时任何初始化（日志、配置、单实例守卫）都是多余的，
    // 而 stdout 上除了口令**不能出现任何东西** —— 多一行日志就等于错误的口令。
    if let Some(code) = askpass_mode(std::env::args().skip(1)) {
        return code;
    }

    configure_default_backend();
    logging::init();

    let command = match Command::parse(std::env::args().skip(1)) {
        Ok(command) => command,
        Err(message) => {
            eprintln!(
                "{message}\n\n用法：\n  knotwork [路径...]\n  \
                 knotwork --isolate [路径...]\n  knotwork --probe [窗口数]"
            );
            return ExitCode::FAILURE;
        }
    };

    match command {
        Command::Probe(count) => run_probe(count),
        Command::Open { paths, isolate } => run_app(&paths, isolate),
    }
}

/// 命令行意图。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    /// 阶段 0 的多窗口内存探针。
    Probe(usize),
    /// 打开若干仓库路径（空表示启动页）。
    Open {
        /// 仓库路径。
        paths: Vec<String>,
        /// 是否跳过单实例守卫（`--isolate`）。
        isolate: bool,
    },
}

impl Command {
    /// 解析参数。
    ///
    /// # Errors
    /// `--probe` 的取值无法解析时返回错误文本。
    fn parse(args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut paths = Vec::new();
        let mut isolate = false;
        let mut args = args.peekable();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--probe" => {
                    let count = match args.peek() {
                        Some(value) if !value.starts_with('-') => {
                            let value = args.next().unwrap_or_default();
                            value
                                .parse::<usize>()
                                .map_err(|_| format!("窗口数无法解析：{value}"))?
                        }
                        _ => DEFAULT_PROBE_WINDOWS,
                    };
                    return Ok(Self::Probe(count.clamp(1, MAX_PROBE_WINDOWS)));
                }
                "--isolate" => isolate = true,
                "--help" | "-h" => {
                    return Err("Knotwork —— 轻量级 Git 管理工具".to_owned());
                }
                other if other.starts_with('-') => {
                    return Err(format!("未知参数：{other}"));
                }
                other => paths.push(other.to_owned()),
            }
        }

        Ok(Self::Open { paths, isolate })
    }
}

/// 以真实仓库打开应用。
fn run_app(paths: &[String], isolate: bool) -> ExitCode {
    // 单实例守卫必须在建服务之前：若是转发，本进程不应占用任何仓库写锁，
    // 也不该白建一个线程池（FR-MR-05 的"进程数不变"验收点）。
    let server = match ipc::start(paths, isolate) {
        Startup::Forwarded => {
            tracing::info!("打开请求已交给运行中的实例，本进程退出");
            return ExitCode::SUCCESS;
        }
        Startup::Primary(server) => Some(server),
        Startup::Standalone(reason) => {
            tracing::info!(%reason, "以独立实例启动");
            None
        }
    };

    let console = ConsoleBuffer::shared(CONSOLE_CAPACITY);
    let service = match GitService::new(ConsoleBuffer::observer(&console)) {
        Ok(service) => Arc::new(service),
        Err(err) => {
            tracing::error!(%err, "工作线程池创建失败");
            return ExitCode::FAILURE;
        }
    };

    let capabilities = service.capabilities().clone();
    tracing::info!(
        git = capabilities
            .git_version
            .as_deref()
            .unwrap_or("<缺失，只读降级>"),
        threads = service.pool().thread_count(),
        "启动 Knotwork"
    );

    let (config, config_path) = load_config();
    let config_path = config_path.unwrap_or_else(|| std::path::PathBuf::from("config.toml"));
    // 管理器放进 `Rc<RefCell<..>>`：转发请求的定时器需要拿到它来开新窗口，
    // 而 Slint 的定时器回调要求 `'static`。
    let manager = Rc::new(RefCell::new(WindowManager::new(
        Arc::clone(&service),
        Arc::clone(&console),
        config,
        config_path,
    )));

    {
        let mut guard = manager.borrow_mut();
        if paths.is_empty() {
            if let Err(err) = guard.open_empty() {
                tracing::error!(%err, "启动页创建失败");
                return ExitCode::FAILURE;
            }
        } else {
            for path in paths {
                let candidate = std::path::Path::new(path);
                match guard.open_repo(candidate) {
                    Ok(()) => tracing::info!(%path, "已打开仓库"),
                    Err(err) => {
                        // 单个路径打不开不该让整次启动失败：报告并继续开下一个。
                        tracing::error!(%path, %err, "打开仓库失败");
                        eprintln!("无法打开 {path}：{err}");
                    }
                }
            }
            if guard.window_count() == 0 {
                tracing::error!("没有成功打开任何仓库");
                return ExitCode::FAILURE;
            }
        }

        if let Err(err) = guard.show_all() {
            tracing::error!(%err, "窗口显示失败");
            return ExitCode::FAILURE;
        }
    }

    // 转发请求的落地：每 300ms 取一次待打开路径。
    // 定时器必须在事件循环期间存活，因此绑定到本地变量而不是临时值。
    let ipc_timer = make_ipc_timer(&manager);

    tracing::info!(
        windows = manager.borrow().window_count(),
        forwarded = server.is_some(),
        "进入事件循环"
    );
    let outcome = match slint::run_event_loop() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(%err, "事件循环异常退出");
            ExitCode::FAILURE
        }
    };

    // 关闭全部窗口后：保存配置、停止监听、释放写锁、注销仓库，然后按顺序释放。
    drop(ipc_timer);
    manager.borrow_mut().shutdown();
    if let Some(server) = &server {
        server.shutdown();
    }
    drop(server);
    drop(manager);
    drop(console);
    drop(service);
    outcome
}

/// 构造"取出转发路径并开窗口"的定时器。
///
/// 为什么用定时器而不是 `invoke_from_event_loop`：新窗口必须在 UI 线程创建，
/// 而这里的回调本来就在 UI 线程上；定时器只是"轮到我了"的触发点，
/// 不引入额外的线程与消息泵。
fn make_ipc_timer(manager: &Rc<RefCell<WindowManager>>) -> slint::Timer {
    let timer = slint::Timer::default();
    let manager = Rc::clone(manager);
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(300),
        move || {
            let pending = ipc::take_pending();
            if pending.is_empty() {
                return;
            }
            let mut guard = manager.borrow_mut();
            let mut opened = 0usize;
            for path in &pending {
                match guard.open_repo(std::path::Path::new(path)) {
                    Ok(()) => opened += 1,
                    Err(err) => tracing::warn!(%path, %err, "转发的仓库打开失败"),
                }
            }
            if opened > 0 {
                if let Err(err) = guard.show_all() {
                    tracing::warn!(%err, "显示新窗口失败");
                }
                tracing::info!(opened, "已按转发请求打开窗口");
            }
        },
    );
    timer
}

/// 读取配置；失败时退回默认配置并把原因写进日志。
fn load_config() -> (UserConfig, Option<std::path::PathBuf>) {
    let outcome = gt_config::load_or_default();
    for warning in &outcome.warnings {
        tracing::warn!(%warning, "配置");
    }
    let path = outcome
        .path
        .clone()
        .or_else(|| gt_config::config_file().ok());
    (outcome.config, path)
}

/// 阶段 0 的多窗口内存探针（M0 基线采集入口，保留以便复测）。
fn run_probe(count: usize) -> ExitCode {
    let backend = active_backend();
    tracing::info!(windows = count, backend = %backend, "启动 Knotwork 探针");

    let Some(windows) = create_probe_windows(count, &backend) else {
        return ExitCode::FAILURE;
    };
    tracing::info!(created = windows.len(), "窗口已创建，进入事件循环");

    let outcome = match slint::run_event_loop() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(%err, "事件循环异常退出");
            ExitCode::FAILURE
        }
    };
    drop(windows);
    outcome
}

/// 若用户未显式指定后端，则默认选择软件渲染。
///
/// 之所以在进程内设置而非要求用户配置环境变量：产品定位是"双击即用"的低内存
/// 客户端，默认后端必须由程序自身保证；环境变量保留为对比测试的覆盖入口。
fn configure_default_backend() {
    if std::env::var_os(BACKEND_ENV).is_none() {
        // 在进程启动最早期、单线程阶段设置环境变量（edition 2021 下是安全调用）。
        std::env::set_var(BACKEND_ENV, SOFTWARE_BACKEND);
    }
}

/// 返回当前生效的渲染后端标识，用于日志与窗口信息展示。
fn active_backend() -> String {
    std::env::var(BACKEND_ENV).unwrap_or_else(|_| SOFTWARE_BACKEND.to_owned())
}

/// 创建并显示 `count` 个探针窗口；任一窗口创建或显示失败则返回 [`None`]。
///
/// 返回值必须被持有至事件循环结束 —— 窗口句柄是 Slint 窗口的生命周期持有者，
/// 提前 drop 会关闭窗口。
fn create_probe_windows(count: usize, backend: &str) -> Option<Vec<ProbeWindow>> {
    let mut windows = Vec::with_capacity(count);

    for index in 0..count {
        let window = match ProbeWindow::new() {
            Ok(window) => window,
            Err(err) => {
                tracing::error!(%err, index, "窗口创建失败");
                return None;
            }
        };

        window.set_heading(format!("Knotwork 探针 · 窗口 {index}").into());
        window.set_info(format!("渲染后端: {backend}").into());

        if let Err(err) = window.show() {
            tracing::error!(%err, index, "窗口显示失败");
            return None;
        }

        windows.push(window);
    }

    Some(windows)
}

#[cfg(test)]
mod tests {
    use super::{Command, DEFAULT_PROBE_WINDOWS, MAX_PROBE_WINDOWS};

    fn parse(args: &[&str]) -> Result<Command, String> {
        Command::parse(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn no_arguments_opens_the_start_page() {
        assert_eq!(
            parse(&[]).expect("解析应成功"),
            Command::Open {
                paths: Vec::new(),
                isolate: false
            }
        );
    }

    #[test]
    fn paths_become_windows() {
        assert_eq!(
            parse(&["/tmp/a", "/tmp/b"]).expect("解析应成功"),
            Command::Open {
                paths: vec!["/tmp/a".to_owned(), "/tmp/b".to_owned()],
                isolate: false
            }
        );
    }

    #[test]
    fn isolate_flag_is_parsed_without_being_treated_as_a_path() {
        assert_eq!(
            parse(&["--isolate", "/tmp/a"]).expect("解析应成功"),
            Command::Open {
                paths: vec!["/tmp/a".to_owned()],
                isolate: true
            }
        );
        // 只有一个开关、没有路径时也要能解析（启动页 + 强制独立进程）。
        assert_eq!(
            parse(&["--isolate"]).expect("解析应成功"),
            Command::Open {
                paths: Vec::new(),
                isolate: true
            }
        );
    }

    #[test]
    fn probe_accepts_an_optional_count_and_clamps_it() {
        assert_eq!(
            parse(&["--probe"]).expect("解析应成功"),
            Command::Probe(DEFAULT_PROBE_WINDOWS)
        );
        assert_eq!(
            parse(&["--probe", "6"]).expect("解析应成功"),
            Command::Probe(6)
        );
        // 越界取值被夹回合法区间，而不是照单全收。
        assert_eq!(
            parse(&["--probe", "999"]).expect("解析应成功"),
            Command::Probe(MAX_PROBE_WINDOWS)
        );
        assert_eq!(
            parse(&["--probe", "0"]).expect("解析应成功"),
            Command::Probe(1)
        );
        // 后面跟的是另一个开关时不吞掉它，而是退回默认窗口数。
        assert_eq!(
            parse(&["--probe", "--help"]).expect("解析应成功"),
            Command::Probe(DEFAULT_PROBE_WINDOWS)
        );
    }

    #[test]
    fn invalid_input_is_reported() {
        assert!(parse(&["--probe", "abc"]).is_err());
        assert!(parse(&["--unknown"]).is_err());
    }
}
