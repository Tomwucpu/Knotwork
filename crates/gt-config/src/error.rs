//! 配置错误类型。

use std::path::PathBuf;

use thiserror::Error;

/// `gt-config` 的统一错误类型。
#[derive(Debug, Error)]
pub enum ConfigError {
    /// 无法确定配置目录（既没有 `KNOTWORK_CONFIG_DIR`，也没有可用的用户目录环境变量）。
    #[error("无法确定配置目录：{0}")]
    NoConfigDir(String),

    /// 读取配置文件失败。
    #[error("读取配置失败：{path}（{source}）")]
    Read {
        /// 被读取的路径。
        path: PathBuf,
        /// 底层 I/O 错误。
        #[source]
        source: std::io::Error,
    },

    /// 写入配置文件失败。
    #[error("写入配置失败：{path}（{source}）")]
    Write {
        /// 目标路径。
        path: PathBuf,
        /// 底层 I/O 错误。
        #[source]
        source: std::io::Error,
    },

    /// TOML 语法或类型错误。
    #[error("配置格式错误：{0}")]
    Parse(#[from] toml::de::Error),

    /// 序列化失败。
    #[error("配置序列化失败：{0}")]
    Serialize(#[from] toml::ser::Error),
}

/// 便捷别名。
pub type Result<T, E = ConfigError> = std::result::Result<T, E>;
