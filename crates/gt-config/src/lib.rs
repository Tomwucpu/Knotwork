//! `gt-config` —— 配置子系统。
//!
//! 以 TOML 持久化用户配置，并提供仓库级覆盖（仓库级 > 全局级）。
//! 需求为 FR-SET-01/02 与 FR-MR-02，设置项清单见
//! 《轻量级Git工具-功能需求设计文档.md》5.14 节。
//!
//! # 分工
//!
//! - [`model`]：配置模型。**不含 I/O**，可自由单测与克隆。
//! - [`recent`]：最近仓库列表（FR-MR-02）的存取与排序规则。
//! - [`store`]：跨平台路径解析、容错加载、原子保存。
//!
//! # M1 覆盖范围
//!
//! | 能力 | 需求 |
//! |---|---|
//! | 配置模型与默认值 | FR-SET-02 |
//! | 最近仓库列表 | FR-MR-02 |
//! | 跨平台配置路径 + 原子保存 | FR-SET-01 的持久化部分 |
//! | 容错加载（未知键只告警） | FR-STA 的降级取向 |
//!
//! 可视化设置窗口在 M2 任务 2-11 交付；本 crate 只保证模型与持久化可用。

pub mod error;
pub mod model;
pub mod recent;
pub mod store;

pub use error::{ConfigError, Result};
pub use model::{
    AppearanceConfig, BranchProtectionConfig, CacheBudget, DiagnosticsConfig, ExternalToolConfig,
    GitBehaviorConfig, Language, LayoutConfig, LogLevel, RemoteSettings, Theme, UpdateStrategy,
    UserConfig,
};
pub use recent::{RecentRepo, RecentRepos, MAX_RECENT_REPOS};
pub use store::{
    config_dir, config_file, load, load_from, load_or_default, save, save_to, LoadOutcome,
    CONFIG_DIR_ENV, CONFIG_FILE_NAME,
};

/// 默认的配置模板文本。
///
/// 首次启动时若配置文件不存在，用它落一份带注释的初始配置 ——
/// 用户直接打开就有可参照的键名与取值范围，不用去翻文档。
#[must_use]
pub fn default_template() -> String {
    let body = UserConfig::default()
        .to_toml()
        .unwrap_or_else(|_| String::new());

    format!(
        "\
# Knotwork 配置文件
# 全部键均可省略；省略的键使用默认值。
# 未知键不会导致启动失败，只会在日志中告警。
#
# 路径解析顺序：KNOTWORK_CONFIG_DIR > 平台默认配置目录。

{body}"
    )
}

#[cfg(test)]
mod tests {
    use super::{default_template, UserConfig};

    #[test]
    fn template_is_a_valid_and_complete_config() {
        let text = default_template();
        assert!(text.starts_with("# Knotwork 配置文件"));

        // 模板必须能被自己解析回来，且与默认配置等价 ——
        // 否则"首次启动落一份模板"就成了制造坏配置的途径。
        let parsed = UserConfig::from_toml(&text).expect("模板应可解析");
        assert_eq!(parsed, UserConfig::default());
    }
}
