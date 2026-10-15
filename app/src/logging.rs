//! 日志基建（实施计划任务 0-3）。
//!
//! 过滤级别优先级：`KNOTWORK_LOG` 环境变量 > 默认 [`DEFAULT_FILTER`]。
//! 级别配置后续接入设置子系统的"诊断"分组（FR-SET-02）。
//!
//! 注意：应用侧不使用标准输出作为主要日志出口，M1 起改为按级别分流到
//! 环形缓冲（供 Console 面板与"导出诊断日志"使用）。

use tracing_subscriber::EnvFilter;

/// 未设置 `KNOTWORK_LOG` 时使用的默认过滤级别。
const DEFAULT_FILTER: &str = "info";

/// 初始化全局 tracing 订阅器。
///
/// 重复调用会被忽略（`set_global_default` 语义），不视为错误。
pub(crate) fn init() {
    let filter = std::env::var("KNOTWORK_LOG").unwrap_or_else(|_| DEFAULT_FILTER.to_owned());

    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(filter))
        .with_target(false)
        .finish();

    if tracing::subscriber::set_global_default(subscriber).is_err() {
        // 已初始化过，保持既有订阅器。
    }
}
