//! 构建脚本：编译 Slint 界面文件。
//!
//! 阶段 0 只编译探针窗口 `ui/probe.slint`；正式界面自 M1 起按模块拆分。

fn main() {
    if let Err(err) = slint_build::compile("ui/probe.slint") {
        eprintln!("Slint 界面编译失败: {err}");
        std::process::exit(1);
    }
}
