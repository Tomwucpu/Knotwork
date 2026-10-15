//! Knotwork 应用入口。
//!
//! 阶段 0 职责（实施计划任务 0-4）：验证 Slint 软件渲染后端与"单进程多窗口"
//! 路径可用，并作为启动内存基线的采集入口。
//!
//! 用法：
//!
//! ```text
//! knotwork                      # 默认开 3 个探针窗口
//! knotwork --windows 6          # 指定窗口数（1..=32）
//! SLINT_BACKEND=winit-femtovg knotwork   # 切换 GPU 后端做内存 A/B 对比
//! ```

mod logging;

use std::process::ExitCode;

use slint::ComponentHandle;

/// Slint 根据 `ui/*.slint` 生成的界面绑定代码。
///
/// 生成代码不携带文档注释，且不受本项目代码风格约束，因此在该模块内
/// 关闭 `missing_docs` 与 clippy 检查，避免污染工作区级别的严格 lint 配置。
#[allow(missing_docs, clippy::all, clippy::pedantic)]
mod generated {
    slint::include_modules!();
}

use generated::ProbeWindow;

/// 默认窗口数，对应架构规划 4.2 的 3 窗口验收场景。
const DEFAULT_WINDOWS: usize = 3;

/// 窗口数上限，避免误传参数导致创建大量窗口。
const MAX_WINDOWS: usize = 32;

/// 渲染后端选择所用的环境变量名（Slint 约定）。
const BACKEND_ENV: &str = "SLINT_BACKEND";

/// 软件渲染后端标识 —— 低内存路线的关键前提（架构规划 2.3）。
const SOFTWARE_BACKEND: &str = "winit-software";

fn main() -> ExitCode {
    configure_default_backend();
    logging::init();

    let window_count = window_count_from_args();
    let backend = active_backend();

    tracing::info!(windows = window_count, backend = %backend, "启动 Knotwork 探针");

    let Some(windows) = create_windows(window_count, &backend) else {
        return ExitCode::FAILURE;
    };

    tracing::info!(
        created = windows.len(),
        "窗口已创建，进入事件循环（关闭全部窗口即退出）"
    );

    match slint::run_event_loop() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(%err, "事件循环异常退出");
            ExitCode::FAILURE
        }
    }
}

/// 若用户未显式指定后端，则默认选择软件渲染。
///
/// 之所以在进程内设置而非要求用户配置环境变量：产品定位是"双击即用"的低内存
/// 客户端，默认后端必须由程序自身保证；环境变量保留为对比测试的覆盖入口。
fn configure_default_backend() {
    if std::env::var_os(BACKEND_ENV).is_none() {
        std::env::set_var(BACKEND_ENV, SOFTWARE_BACKEND);
    }
}

/// 返回当前生效的渲染后端标识，用于日志与窗口信息展示。
fn active_backend() -> String {
    std::env::var(BACKEND_ENV).unwrap_or_else(|_| SOFTWARE_BACKEND.to_owned())
}

/// 解析 `--windows N` / `-w N`；非法或缺失时回退到 [`DEFAULT_WINDOWS`]。
fn window_count_from_args() -> usize {
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        if arg != "--windows" && arg != "-w" {
            continue;
        }

        let Some(value) = args.next() else {
            tracing::warn!("--windows 缺少取值，回退默认值");
            return DEFAULT_WINDOWS;
        };

        return value.parse::<usize>().map_or_else(
            |_| {
                tracing::warn!(value = %value, "窗口数无法解析，回退默认值");
                DEFAULT_WINDOWS
            },
            |parsed| parsed.clamp(1, MAX_WINDOWS),
        );
    }

    DEFAULT_WINDOWS
}

/// 创建并显示 `count` 个探针窗口；任一窗口创建或显示失败则返回 [`None`]。
///
/// 返回值必须被持有至事件循环结束 —— 窗口句柄是 Slint 窗口的生命周期持有者，
/// 提前 drop 会关闭窗口。
fn create_windows(count: usize, backend: &str) -> Option<Vec<ProbeWindow>> {
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
