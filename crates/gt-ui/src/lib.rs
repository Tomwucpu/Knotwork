//! `gt-ui` —— UI 组件层。
//!
//! 承载 Slint 组件与各窗口视图：Local Changes、Log + 分支图、Diff、
//! 三路合并编辑器、设置界面（见架构规划 4.1 的分层图）。
//!
//! # 设计约束
//!
//! - **UI 线程零 I/O**：所有数据经 `gt-service` 异步送达，本 crate 只做渲染与交互
//!   （架构规划 4.2）；[`Session`] 里没有一处文件读写或进程启动；
//! - **列表全虚拟化**：Log 与 Diff 都用 Slint 的 `ListView`，只创建视口内的行
//!   （架构规划 5.1）；
//! - **模型按需重建**：界面每 100ms 轮询一次结果，但只有数据真的变了才重建模型
//!   （[`Session`] 内部的脏标记）；
//! - **不做文本测量**：词级高亮以"分段文本 + 是否变更"的平行数组传给界面，
//!   由 Slint 逐段着色，省掉每帧的排版计算（架构规划 5.7）。
//!
//! # 模块分工
//!
//! | 模块 | 职责 |
//! |---|---|
//! | [`view_model`] | 纯转换：领域模型 → Slint 行结构（可脱离 Slint 运行时单测） |
//! | [`session`] | 每个窗口一个：合并异步结果、维护视图状态、发起按需请求 |
//! | [`generated`] | Slint 编译产物（`ui/main-window.slint`） |
//!
//! # 用法
//!
//! ```no_run
//! # fn demo(service: std::sync::Arc<gt_service::GitService>) -> Result<(), Box<dyn std::error::Error>> {
//! use slint::ComponentHandle;
//!
//! let handle = service.open_repo(std::path::Path::new("."))?;
//! let console = gt_service::ConsoleBuffer::shared(512);
//! let mut session = gt_ui::Session::new(service, handle, console, 512);
//! let window = gt_ui::MainWindow::new()?;
//!
//! session.request_initial();
//! session.apply_to(&window);
//! window.show()?;
//! # Ok(())
//! # }
//! ```

pub mod session;
pub mod view_model;

/// Slint 根据 `ui/main-window.slint` 生成的界面绑定代码。
///
/// 生成代码不携带文档注释，且不受本项目代码风格约束，因此在该模块内
/// 关闭 `missing_docs` 与 clippy 检查，避免污染工作区级别的严格 lint 配置。
#[allow(missing_docs, clippy::all, clippy::pedantic)]
pub mod generated {
    slint::include_modules!();
}

pub use generated::MainWindow;

/// 构造最近仓库的 Slint 模型。
///
/// 应用层的"未打开仓库"状态直接把它塞进窗口，因此这里提供一个不必
/// 暴露内部模型构造细节的入口。
#[must_use]
pub fn recent_model(recent: &[gt_config::RecentRepo]) -> slint::ModelRc<generated::RecentRow> {
    view_model::rc_model(view_model::build_recent_rows(recent))
}
pub use session::{Session, LOG_PAGE_SIZE};
pub use view_model::{
    branch_text, build_change_rows, build_console_rows, build_diff_rows, build_graph_cells,
    build_log_rows, build_recent_rows, build_ref_rows, diff_selection_hint, discard_confirmation,
    discard_selection_confirmation, head_text, lane_color, ordered_refs, relative_time,
    state_banner, GraphKind,
};
