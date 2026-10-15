//! Knotwork 应用入口。
//!
//! # 用法
//!
//! ```text
//! knotwork                       # 打开启动页（最近仓库列表）
//! knotwork <路径> [路径...]       # 每个路径一个窗口（FR-MR-03）
//! knotwork --probe 3             # 阶段 0 的多窗口内存探针（M0 基线采集入口）
//! SLINT_BACKEND=winit-femtovg knotwork   # 切换 GPU 后端做内存 A/B 对比
//! ```
//!
//! # 启动顺序
//!
//! 1. 选默认渲染后端（软件渲染，架构规划 2.3）→ 初始化日志；
//! 2. 建命令审计缓冲与 `GitService`（其中包含 **git CLI 探测**，FR-STA-04）；
//! 3. 读配置（含最近仓库列表）；
//! 4. 为每个命令行路径开一个窗口；没有路径时开启动页；
//! 5. 进事件循环；退出后保存配置并释放全部仓库。
//!
//! UI 线程在整个过程中**不读仓库文件、不起子进程**：打开仓库这一步的
//! `discover` 与配置解析是启动期一次性成本，之后所有数据都经由
//! `gt-service` 的工作线程送达。

mod logging;
mod windows;

use std::process::ExitCode;
use std::sync::Arc;

use gt_config::UserConfig;
use gt_service::{ConsoleBuffer, GitService};
use slint::ComponentHandle;

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

/// 探针模式的默认窗口数，对应架构规划 4.2 的 3 窗口验收场景。
const DEFAULT_PROBE_WINDOWS: usize = 3;

/// 探针窗口数上限，避免误传参数导致创建大量窗口。
const MAX_PROBE_WINDOWS: usize = 32;

/// 渲染后端选择所用的环境变量名（Slint 约定）。
const BACKEND_ENV: &str = "SLINT_BACKEND";

/// 软件渲染后端标识 —— 低内存路线的关键前提（架构规划 2.3）。
const SOFTWARE_BACKEND: &str = "winit-software";

fn main() -> ExitCode {
    configure_default_backend();
    logging::init();

    let command = match Command::parse(std::env::args().skip(1)) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("{message}\n\n用法：\n  knotwork [路径...]\n  knotwork --probe [窗口数]");
            return ExitCode::FAILURE;
        }
    };

    match command {
        Command::Probe(count) => run_probe(count),
        Command::Open(paths) => run_app(&paths),
    }
}

/// 命令行意图。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    /// 阶段 0 的多窗口内存探针。
    Probe(usize),
    /// 打开若干仓库路径（空表示启动页）。
    Open(Vec<String>),
}

impl Command {
    /// 解析参数。
    ///
    /// # Errors
    /// `--probe` 的取值无法解析时返回错误文本。
    fn parse(args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut paths = Vec::new();
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
                "--help" | "-h" => {
                    return Err("Knotwork —— 轻量级 Git 管理工具".to_owned());
                }
                other if other.starts_with('-') => {
                    return Err(format!("未知参数：{other}"));
                }
                other => paths.push(other.to_owned()),
            }
        }

        Ok(Self::Open(paths))
    }
}

/// 以真实仓库打开应用。
fn run_app(paths: &[String]) -> ExitCode {
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
    let mut manager = WindowManager::new(
        Arc::clone(&service),
        Arc::clone(&console),
        config,
        config_path,
    );

    if paths.is_empty() {
        if let Err(err) = manager.open_empty() {
            tracing::error!(%err, "启动页创建失败");
            return ExitCode::FAILURE;
        }
    } else {
        for path in paths {
            let candidate = std::path::Path::new(path);
            match manager.open_repo(candidate) {
                Ok(()) => tracing::info!(%path, "已打开仓库"),
                Err(err) => {
                    // 单个路径打不开不该让整次启动失败：报告并继续开下一个。
                    tracing::error!(%path, %err, "打开仓库失败");
                    eprintln!("无法打开 {path}：{err}");
                }
            }
        }
        if manager.window_count() == 0 {
            tracing::error!("没有成功打开任何仓库");
            return ExitCode::FAILURE;
        }
    }

    if let Err(err) = manager.show_all() {
        tracing::error!(%err, "窗口显示失败");
        return ExitCode::FAILURE;
    }

    tracing::info!(windows = manager.window_count(), "进入事件循环");
    let outcome = match slint::run_event_loop() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(%err, "事件循环异常退出");
            ExitCode::FAILURE
        }
    };

    // 关闭全部窗口后：保存配置、停止监听、注销仓库，然后按顺序释放。
    manager.shutdown();
    drop(manager);
    drop(console);
    drop(service);
    outcome
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
        assert_eq!(parse(&[]).expect("解析应成功"), Command::Open(Vec::new()));
    }

    #[test]
    fn paths_become_windows() {
        assert_eq!(
            parse(&["/tmp/a", "/tmp/b"]).expect("解析应成功"),
            Command::Open(vec!["/tmp/a".to_owned(), "/tmp/b".to_owned()])
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
