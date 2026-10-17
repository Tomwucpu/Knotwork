//! 配置模型（FR-SET-02）。
//!
//! 字段口径对齐《轻量级Git工具-功能需求设计文档.md》5.14 节的设置项清单。
//! 所有字段都有默认值，且**未知字段被忽略并告警**（见 [`crate::store`]）：
//! 用户手写配置时打错一个键名只会得到一条提示，而不会让整份配置不可用；
//! 反过来，用新版本写出的配置被旧版本读取时也不会直接报错。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::recent::RecentRepos;

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

/// 界面语言。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Language {
    /// 简体中文。
    #[default]
    ZhCn,
    /// 英文。
    En,
}

/// 拉取时的更新策略（FR-SET-02「Git 行为」组）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateStrategy {
    /// 合并（`git pull` 默认行为）。
    #[default]
    Merge,
    /// 变基（`git pull --rebase`）。
    Rebase,
    /// 仅快进（`git pull --ff-only`）：出现分叉即失败，绝不产生合并提交。
    FastForwardOnly,
}

/// 日志级别（对应 `tracing` 的过滤级别）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LogLevel {
    /// 仅错误。
    Error,
    /// 错误与警告。
    Warn,
    /// 常规信息。
    #[default]
    Info,
    /// 调试信息（含内核路由记录）。
    Debug,
    /// 全量追踪（性能影响明显，仅供排障）。
    Trace,
}

impl LogLevel {
    /// 交给 `tracing-subscriber` 的级别字符串。
    #[must_use]
    pub const fn as_filter(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

/// 外观设置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceConfig {
    /// 主题。
    pub theme: Theme,
    /// 界面语言。
    pub language: Language,
    /// 等宽字体族；[`None`] 表示使用平台默认。
    pub font_family: Option<String>,
    /// 字号（逻辑像素）。
    pub font_size: u16,
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            theme: Theme::default(),
            language: Language::default(),
            font_family: None,
            font_size: 13,
        }
    }
}

/// Git 行为设置（FR-SET-02「Git 行为」组）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GitBehaviorConfig {
    /// 是否启用 staging 模式（FR-LC-03 的分区暂存）。
    pub staging_mode: bool,
    /// 拉取更新策略。
    pub update_strategy: UpdateStrategy,
    /// 提交时检测到 CRLF/LF 混用是否警告。
    pub warn_crlf: bool,
    /// 在 detached HEAD 上提交是否警告。
    pub warn_detached_commit: bool,
}

impl Default for GitBehaviorConfig {
    fn default() -> Self {
        Self {
            staging_mode: true,
            update_strategy: UpdateStrategy::default(),
            warn_crlf: true,
            warn_detached_commit: true,
        }
    }
}

/// 受保护分支设置。
///
/// 存的是**正则字符串**，M3 实现 push 拦截时才会引入正则引擎去匹配 ——
/// 这里刻意不做语法校验，避免为了一个尚未被使用的检查引入依赖。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BranchProtectionConfig {
    /// 受保护分支的正则列表；匹配到的分支禁止 force push（FR-RMT-03）。
    pub protected_patterns: Vec<String>,
}

impl BranchProtectionConfig {
    /// 默认保护 `main` / `master` / `release/*`。
    #[must_use]
    pub fn recommended() -> Self {
        Self {
            protected_patterns: vec![
                "^main$".to_owned(),
                "^master$".to_owned(),
                "^release/".to_owned(),
            ],
        }
    }
}

/// 远端与凭据设置（M3）。
///
/// 只记"哪些主机存过 token"，**不记 token 本身**：真值在系统密钥链里
/// （NFR-SEC-01 要求明文不落盘，配置文件同样不能例外）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteSettings {
    /// 已把 token 存入系统密钥链的主机名。
    pub stored_token_hosts: Vec<String>,
}

impl RemoteSettings {
    /// 记录一个主机（幂等）。
    pub fn remember_host(&mut self, host: &str) {
        if host.is_empty() || self.stored_token_hosts.iter().any(|item| item == host) {
            return;
        }
        self.stored_token_hosts.push(host.to_owned());
    }

    /// 忘记一个主机。
    pub fn forget_host(&mut self, host: &str) {
        self.stored_token_hosts.retain(|item| item != host);
    }
}

/// 缓存预算（对应架构规划 5.4 的硬上限 LRU 缓存）。
///
/// 超出预算按 LRU 淘汰，**绝不自动扩容**。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheBudget {
    /// 内存缓存上限，单位 MiB。
    pub memory_mib: u32,
    /// 历史索引磁盘预算，单位 MiB（FR-LOG-03 的索引落地，M5）。
    pub log_index_disk_mib: u32,
}

impl Default for CacheBudget {
    fn default() -> Self {
        Self {
            memory_mib: 32,
            log_index_disk_mib: 256,
        }
    }
}

/// 外部工具绑定。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExternalToolConfig {
    /// 外部 diff 工具命令行；[`None`] 表示使用内置差异视图。
    pub diff_command: Option<String>,
    /// 外部 merge 工具命令行；[`None`] 表示使用内置三路编辑器。
    pub merge_command: Option<String>,
}

/// 诊断设置。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DiagnosticsConfig {
    /// 日志级别。
    pub log_level: LogLevel,
    /// 是否把日志同时写入文件（便于用户导出诊断信息）。
    pub log_to_file: bool,
}

/// 用户级配置根。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserConfig {
    /// 外观。
    pub appearance: AppearanceConfig,
    /// Git 行为。
    pub git: GitBehaviorConfig,
    /// 受保护分支。
    pub branches: BranchProtectionConfig,
    /// 远端与凭据。
    pub remote: RemoteSettings,
    /// 缓存预算。
    pub cache: CacheBudget,
    /// 外部工具。
    pub tools: ExternalToolConfig,
    /// 诊断。
    pub diagnostics: DiagnosticsConfig,
    /// 最近打开的仓库（FR-MR-02）。
    pub recent: RecentRepos,
    /// 快捷键覆盖（命令 id → 键序列）；完整快捷键系统在 M2 交付。
    pub keybindings: BTreeMap<String, String>,
}

impl UserConfig {
    /// 从 TOML 文本解析配置；空文本返回默认配置。
    ///
    /// **未知键被忽略**（不报错）：见本模块文档与 [`crate::store::load_from`]
    /// 的告警机制。语义或类型错误仍然报错。
    ///
    /// # Errors
    /// 文本不是合法 TOML，或字段类型不匹配时返回错误。
    pub fn from_toml(text: &str) -> std::result::Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    /// 序列化为 TOML 文本。
    ///
    /// # Errors
    /// 序列化失败时返回错误（当前模型不会发生，保留以兼容后续字段类型）。
    pub fn to_toml(&self) -> std::result::Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AppearanceConfig, BranchProtectionConfig, CacheBudget, DiagnosticsConfig,
        GitBehaviorConfig, LogLevel, RemoteSettings, UpdateStrategy, UserConfig,
    };

    #[test]
    fn defaults_match_the_settings_spec() {
        let config = UserConfig::default();
        assert_eq!(config.appearance.font_size, 13);
        assert!(config.git.staging_mode, "staging 模式默认开启");
        assert_eq!(config.git.update_strategy, UpdateStrategy::Merge);
        assert!(config.git.warn_crlf);
        assert_eq!(
            config.cache,
            CacheBudget {
                memory_mib: 32,
                log_index_disk_mib: 256
            }
        );
        assert_eq!(config.diagnostics.log_level, LogLevel::Info);
        assert!(config.branches.protected_patterns.is_empty());
        assert!(config.remote.stored_token_hosts.is_empty());
        assert!(config.recent.is_empty());
        assert!(config.keybindings.is_empty());
    }

    #[test]
    fn log_level_filters_are_ordered() {
        assert!(LogLevel::Error < LogLevel::Warn);
        assert!(LogLevel::Info < LogLevel::Debug);
        assert_eq!(LogLevel::Trace.as_filter(), "trace");
        assert_eq!(LogLevel::default().as_filter(), "info");
    }

    #[test]
    fn sub_configs_are_independently_defaultable() {
        // 每个子结构都应有对外的 `Default`，便于仓库级覆盖只写变更项。
        assert_eq!(AppearanceConfig::default().theme, super::Theme::System);
        assert_eq!(
            GitBehaviorConfig::default().update_strategy,
            UpdateStrategy::Merge
        );
        assert_eq!(DiagnosticsConfig::default().log_level, LogLevel::Info);
        assert_eq!(
            BranchProtectionConfig::recommended()
                .protected_patterns
                .len(),
            3
        );
    }

    #[test]
    fn remote_settings_track_hosts_not_secrets() {
        let mut settings = RemoteSettings::default();
        settings.remember_host("github.com");
        settings.remember_host("github.com");
        settings.remember_host("");
        assert_eq!(settings.stored_token_hosts, vec!["github.com".to_owned()]);

        settings.forget_host("github.com");
        assert!(settings.stored_token_hosts.is_empty());
    }

    #[test]
    fn update_strategy_round_trips_through_toml() {
        let mut config = UserConfig::default();
        config.git.update_strategy = UpdateStrategy::FastForwardOnly;
        let text = config.to_toml().expect("序列化");
        assert!(text.contains("fast-forward-only"), "{text}");
        let back = UserConfig::from_toml(&text).expect("反序列化");
        assert_eq!(
            back.git.update_strategy,
            UpdateStrategy::FastForwardOnly,
            "仅快进必须能被记住（FR-RMT-02）"
        );
    }
}
