//! POC-1：Slint 软件渲染多窗口内存基线。
//!
//! 对应实施计划 M0 节 POC-1 与需求 NFR-MEM-01/02：
//!   - 单窗口空闲 RSS < 40MB；
//!   - 3 窗口总 RSS < 90MB。
//!
//! 为让内存画像接近真实 Log 视图，每个窗口承载一个 1 万行的虚拟列表
//! （含泳道图列与提交文本列），而非空窗口。
//!
//! 用法：
//!   poc1-memory [--windows N] [--rows M] [--hold SECS] [--gpu]
//!
//! `--hold` 到点后自动退出（默认 60s），便于脚本化采样；
//! `--gpu` 切换 femtovg 后端做 A/B 对比。

use std::time::Duration;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

slint::include_modules!();

const DEFAULT_WINDOWS: usize = 3;
const DEFAULT_ROWS: usize = 10_000;
const DEFAULT_HOLD_SECS: u64 = 60;

/// 解析 `--key value` 形式的整数参数。
fn int_arg(flag: &str) -> Option<u64> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == flag {
            return args.next().and_then(|v| v.parse().ok());
        }
    }
    None
}

fn has_flag(flag: &str) -> bool {
    std::env::args().any(|a| a == flag)
}

/// 生成一行泳道占位符（等宽），模拟真实分支图的绘制成本。
fn graph_symbol(row: usize) -> String {
    const WIDTH: usize = 6;
    let lane = row % WIDTH;
    let mut s = String::with_capacity(WIDTH * 2);
    for c in 0..WIDTH {
        s.push(if c == lane { '●' } else { '│' });
        s.push(' ');
    }
    s
}

fn main() {
    if std::env::var_os("SLINT_BACKEND").is_none() {
        let backend = if has_flag("--gpu") {
            "winit-femtovg"
        } else {
            "winit-software"
        };
        std::env::set_var("SLINT_BACKEND", backend);
    }

    let windows_n = int_arg("--windows").unwrap_or(DEFAULT_WINDOWS as u64) as usize;
    let rows = int_arg("--rows").unwrap_or(DEFAULT_ROWS as u64) as usize;
    let hold = int_arg("--hold").unwrap_or(DEFAULT_HOLD_SECS);
    let backend = std::env::var("SLINT_BACKEND").unwrap_or_default();

    println!(
        "POC-1 启动：windows={windows_n} rows={rows} hold={hold}s backend={backend}"
    );

    let mut windows = Vec::with_capacity(windows_n);
    for i in 0..windows_n {
        let win = Poc1Window::new().expect("创建窗口失败");

        let graph: Vec<SharedString> = (0..rows).map(|r| graph_symbol(r).into()).collect();
        let texts: Vec<SharedString> = (0..rows)
            .map(|r| {
                let hash = r.wrapping_mul(2_654_435_761) & 0x0fff_ffff;
                format!(
                    "{hash:07x}  ·  author{:02}  ·  提交 #{r}  ·  feat(lane): 合成提交信息占位文本",
                    r % 8
                )
                .into()
            })
            .collect();

        win.set_graph(ModelRc::new(VecModel::from(graph)));
        win.set_rows(ModelRc::new(VecModel::from(texts)));
        win.set_heading(format!("POC-1 · 窗口 {i}").into());
        win.set_info(format!("虚拟列表 {rows} 行 · 后端 {backend}").into());
        win.show().expect("显示窗口失败");

        windows.push(win);
    }

    println!("READY windows={windows_n} rows={rows} backend={backend}");

    let _timer = slint::Timer::single_shot(Duration::from_secs(hold), || {
        let _ = slint::quit_event_loop();
    });

    slint::run_event_loop().expect("事件循环异常");
    println!("EXIT");
}
