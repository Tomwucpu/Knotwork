//! `gt-config` —— 配置子系统。
//!
//! 以 TOML 持久化用户配置，并提供仓库级覆盖（仓库级 > 全局级）。
//! 需求为 FR-SET-01/02，设置项清单见《轻量级Git工具-功能需求设计文档.md》5.14 节。
//!
//! 阶段 0 先落地配置模型与序列化往返测试；设置界面在 M2 任务 2-11 交付。

use serde::{Deserialize, Serialize};

/// 界面主题。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Theme {
    /// 浅色主题。
    Light,
    /// 深色主题。
    Dark,
    /// 跟随操作系统。
    #[default]
    System,
}

/// 缓存预算（对应架构规划 5.4 的硬上限 LRU 缓存）。
///
/// 超出预算按 LRU 淘汰，**绝不自动扩容**。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CacheBudget {
    /// 内存缓存上限，单位 MiB。
    pub memory_mib: u32,
}

impl Default for CacheBudget {
    fn default() -> Self {
        Self { memory_mib: 32 }
    }
}

/// 用户级配置根。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UserConfig {
    /// 界面主题。
    pub theme: Theme,
    /// 缓存预算。
    pub cache: CacheBudget,
}

impl UserConfig {
    /// 从 TOML 文本解析配置；空文本返回默认配置。
    ///
    /// # Errors
    /// 文本不是合法 TOML，或包含未知字段时返回错误。
    pub fn from_toml(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    /// 序列化为 TOML 文本。
    ///
    /// # Errors
    /// 序列化失败时返回错误（当前模型不会发生，保留以兼容后续字段类型）。
    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }
}

#[cfg(test)]
mod tests {
    use super::{CacheBudget, Theme, UserConfig};

    #[test]
    fn empty_toml_yields_defaults() {
        let config = UserConfig::from_toml("").expect("空配置应解析为默认值");
        assert_eq!(config.theme, Theme::System);
        assert_eq!(config.cache, CacheBudget { memory_mib: 32 });
    }

    #[test]
    fn config_round_trips_through_toml() {
        let original = UserConfig {
            theme: Theme::Dark,
            cache: CacheBudget { memory_mib: 64 },
        };
        let text = original.to_toml().expect("序列化应成功");
        let parsed = UserConfig::from_toml(&text).expect("反序列化应成功");
        assert_eq!(parsed, original);
    }

    #[test]
    fn unknown_field_is_rejected() {
        assert!(UserConfig::from_toml("not-a-real-field = 1").is_err());
    }
}
