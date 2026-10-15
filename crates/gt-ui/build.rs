//! 构建脚本：编译本 crate 的 Slint 界面文件。
//!
//! 界面入口是 `ui/main-window.slint`；组件拆分通过在同一个入口里
//! `import` 其它 `.slint` 文件完成。Slint 的"一个入口文件"约束来自
//! `slint_build::compile` 的语义 —— 它编译整棵 import 树并生成一个 Rust 模块。

fn main() {
    if let Err(err) = slint_build::compile("ui/main-window.slint") {
        eprintln!("Slint 界面编译失败: {err}");
        std::process::exit(1);
    }
}
