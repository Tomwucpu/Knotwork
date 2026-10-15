//! 配置的持久化：路径解析、加载、原子保存。
//!
//! # 路径
//!
//! 解析顺序：
//!
//! 1. `KNOTWORK_CONFIG_DIR` 环境变量（**测试与便携模式用**；设了就完全接管，
//!    不会再去看系统目录）；
//! 2. Windows：`%APPDATA%\Knotwork`；
//! 3. macOS：`$HOME/Library/Application Support/Knotwork`；
//! 4. 其它（Linux 等）：`$XDG_CONFIG_HOME/knotwork`，回退 `$HOME/.config/knotwork`。
//!
//! # 写入
//!
//! 先写同目录下的临时文件再 `rename` 覆盖（同分区内 `rename` 是原子操作）。
//! 直接截断重写的话，进程在写入中途崩溃会留下一份半截的配置，
//! 而用户手写的配置往往很长，重新写一遍成本不低。
//!
//! # 未知字段
//!
//! 解析不拒绝未知键：手写配置打错一个键名只会得到一条告警（[`LoadOutcome::warnings`]）
//! 而不是整份配置不可用；用新版本写出的配置被旧版本读取时也不会直接报错。
//! 告警通过把解析结果与"默认配置序列化后的键集合"做差集得出。

use std::path::{Path, PathBuf};

use crate::error::{ConfigError, Result};
use crate::model::UserConfig;
use crate::recent::RecentRepo;

/// 配置目录覆盖点。
pub const CONFIG_DIR_ENV: &str = "KNOTWORK_CONFIG_DIR";

/// 配置目录名（Windows / macOS 使用；Linux 下按约定用小写）。
const APP_DIR_NAME: &str = "Knotwork";

/// 配置文件名。
pub const CONFIG_FILE_NAME: &str = "config.toml";

/// 一次加载的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadOutcome {
    /// 解析出的配置。
    pub config: UserConfig,
    /// 配置来源路径；文件不存在时为 [`None`]（此时返回的是默认配置）。
    pub path: Option<PathBuf>,
    /// 解析期间的告警（当前仅"未知配置项"）。
    pub warnings: Vec<String>,
}

impl LoadOutcome {
    /// 是否来自实际存在的配置文件（否则为默认配置）。
    #[must_use]
    pub fn is_from_file(&self) -> bool {
        self.path.is_some()
    }
}

/// 配置目录。
///
/// # Errors
/// 环境变量缺失且无法从用户目录推出路径时返回 [`ConfigError::NoConfigDir`]。
pub fn config_dir() -> Result<PathBuf> {
    if let Some(custom) = std::env::var_os(CONFIG_DIR_ENV) {
        if !custom.is_empty() {
            return Ok(PathBuf::from(custom));
        }
    }

    if cfg!(windows) {
        let app_data = std::env::var_os("APPDATA")
            .or_else(|| std::env::var_os("LOCALAPPDATA"))
            .ok_or_else(|| ConfigError::NoConfigDir("未设置 APPDATA / LOCALAPPDATA".to_owned()))?;
        return Ok(PathBuf::from(app_data).join(APP_DIR_NAME));
    }

    if cfg!(target_os = "macos") {
        let home = home_dir()?;
        return Ok(home
            .join("Library")
            .join("Application Support")
            .join(APP_DIR_NAME));
    }

    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Ok(PathBuf::from(xdg).join("knotwork"));
        }
    }
    Ok(home_dir()?.join(".config").join("knotwork"))
}

/// 配置文件路径。
///
/// # Errors
/// 同 [`config_dir`]。
pub fn config_file() -> Result<PathBuf> {
    Ok(config_dir()?.join(CONFIG_FILE_NAME))
}

/// 从默认位置加载配置。
///
/// 文件不存在时返回默认配置（**不是错误** —— 首次启动就是这种状态）。
///
/// # Errors
/// 路径无法确定、文件读取失败或 TOML 解析失败时返回错误。
pub fn load() -> Result<LoadOutcome> {
    let path = config_file()?;
    load_from(&path)
}

/// 从指定路径加载配置。
///
/// # Errors
/// 同 [`load`]。
pub fn load_from(path: &Path) -> Result<LoadOutcome> {
    if !path.exists() {
        return Ok(LoadOutcome {
            config: UserConfig::default(),
            path: None,
            warnings: Vec::new(),
        });
    }

    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;

    let config = UserConfig::from_toml(&text)?;
    let warnings = unknown_key_warnings(&text);
    Ok(LoadOutcome {
        config,
        path: Some(path.to_path_buf()),
        warnings,
    })
}

/// 容错加载：任何失败都退回默认配置，并把原因放进 `warnings`。
///
/// 用于启动路径 —— 配置坏了不应该导致应用起不来（FR-STA 的降级取向）。
#[must_use]
pub fn load_or_default() -> LoadOutcome {
    match load() {
        Ok(outcome) => outcome,
        Err(err) => {
            let path = config_file().ok();
            LoadOutcome {
                config: UserConfig::default(),
                path: None,
                warnings: vec![format!("配置加载失败，已使用默认配置：{err}")],
            }
            .with_path_hint(path)
        }
    }
}

impl LoadOutcome {
    /// 补上"本来想读哪个文件"的信息，便于界面提示。
    fn with_path_hint(mut self, path: Option<PathBuf>) -> Self {
        if let Some(path) = path {
            self.warnings.push(format!("预期位置：{}", path.display()));
        }
        self
    }
}

/// 保存配置到默认位置。
///
/// # Errors
/// 路径无法确定、目录创建失败或写入失败时返回错误。
pub fn save(config: &UserConfig) -> Result<PathBuf> {
    let path = config_file()?;
    save_to(config, &path)?;
    Ok(path)
}

/// 保存配置到指定路径（原子写入）。
///
/// # Errors
/// 目录创建失败或写入失败时返回错误。
pub fn save_to(config: &UserConfig, path: &Path) -> Result<()> {
    let text = config.to_toml()?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    // 临时文件必须与目标同目录：跨分区的改名不是原子操作。
    let temp = path.with_extension("toml.tmp");
    std::fs::write(&temp, text.as_bytes()).map_err(|source| ConfigError::Write {
        path: temp.clone(),
        source,
    })?;

    std::fs::rename(&temp, path).map_err(|source| {
        // 改名失败就别留下垃圾文件。
        let _ = std::fs::remove_file(&temp);
        ConfigError::Write {
            path: path.to_path_buf(),
            source,
        }
    })
}

/// 找出配置文本里"模型不认识"的键路径。
///
/// 做法是把一份**填满的样本配置**序列化成 TOML，再逐层比对键集合 ——
/// 这样键清单永远跟随模型，不需要额外维护一份手写列表。
fn unknown_key_warnings(text: &str) -> Vec<String> {
    let Ok(parsed) = text.parse::<toml::Value>() else {
        // 语法错误已经由 `UserConfig::from_toml` 报出，这里不重复。
        return Vec::new();
    };
    let Ok(known) = toml::Value::try_from(key_exemplar()) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    collect_unknown_keys(&parsed, &known, "", &mut out);
    out
}

/// 供未知键比对使用的样本配置：把所有 `Option` 字段填成 `Some`、
/// 每个数组至少留一个元素。
///
/// TOML 无法表示"空值"，序列化 `None` 时整个键会被省略；数组为空时
/// 元素里的键也无从比对。用默认配置做样本会让 `font_family`、
/// `tools.diff_command`、`recent[].alias` 这些键在比对集合里缺席，
/// 从而被误报为"未知配置项"。
///
/// 模型新增字段时这里通常不需要改（新字段只要不是 `Option`/空数组就会
/// 出现在默认序列化结果里）；万一漏改，`unknown_keys_are_reported_by_path`
/// 一类的用例会立刻暴露。
fn key_exemplar() -> UserConfig {
    let mut config = UserConfig::default();
    config.appearance.font_family = Some(String::new());
    config.tools.diff_command = Some(String::new());
    config.tools.merge_command = Some(String::new());
    config.recent.entries.push(RecentRepo {
        path: PathBuf::new(),
        alias: Some(String::new()),
        opened_at: 0,
    });
    config
}

/// 递归收集未知键路径。
///
/// 表逐键比对；数组按"第一个已知元素"作为模板比对每个实际元素 ——
/// 这样 `[[recent]]` 条目里打错的键也能被发现。
fn collect_unknown_keys(
    actual: &toml::Value,
    known: &toml::Value,
    prefix: &str,
    out: &mut Vec<String>,
) {
    match (actual, known) {
        (toml::Value::Table(actual), toml::Value::Table(known)) => {
            for (key, value) in actual {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };

                match known.get(key) {
                    None => out.push(format!("未知配置项 `{path}`，已忽略")),
                    Some(expected) => collect_unknown_keys(value, expected, &path, out),
                }
            }
        }
        (toml::Value::Array(actual), toml::Value::Array(known)) => {
            let Some(template) = known.first() else {
                return;
            };
            for (index, item) in actual.iter().enumerate() {
                collect_unknown_keys(item, template, &format!("{prefix}[{index}]"), out);
            }
        }
        _ => {}
    }
}

/// 用户主目录（跨平台，不引入额外依赖）。
fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| ConfigError::NoConfigDir("未设置 HOME / USERPROFILE".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::{unknown_key_warnings, LoadOutcome};
    use crate::model::{LogLevel, Theme, UserConfig};
    use crate::recent::RecentRepo;

    #[test]
    fn unknown_keys_are_reported_by_path() {
        let warnings = unknown_key_warnings(
            r#"
            [appearance]
            theme = "dark"
            font_sizee = 14

            [nope]
            value = 1
            "#,
        );
        assert_eq!(warnings.len(), 2, "实际：{warnings:#?}");
        assert!(warnings[0].contains("appearance.font_sizee"));
        assert!(warnings[1].contains("nope"));
    }

    #[test]
    fn optional_and_array_keys_are_recognized() {
        // `Option` 字段在 TOML 里会被整条省略，数组元素也需要被递归比对 ——
        // 这两类键曾经被误报为"未知配置项"。
        assert!(
            unknown_key_warnings(
                r#"
                [appearance]
                font_family = "JetBrains Mono"

                [tools]
                diff_command = "meld"
                merge_command = "meld"
                "#
            )
            .is_empty(),
            "Option 字段的键应被识别"
        );

        let warnings = unknown_key_warnings(
            r#"
            [[recent]]
            path = "/tmp/repo"
            opened_at = 1
            alias = "别名"
            typpo = 1
            "#,
        );
        assert_eq!(warnings.len(), 1, "实际：{warnings:#?}");
        assert!(
            warnings[0].contains("recent[0].typpo"),
            "数组元素的未知键应带下标：{}",
            warnings[0]
        );
    }

    #[test]
    fn sample_config_serializes_every_optional_key() {
        // 防止有人给模型加 `Option` 字段却忘了同步样本配置。
        let text = super::key_exemplar().to_toml().expect("样本应可序列化");
        for key in ["font_family", "diff_command", "merge_command", "alias"] {
            assert!(text.contains(key), "样本缺少可选键 {key}：\n{text}");
        }
    }

    #[test]
    fn recent_round_trips_through_toml() {
        let mut config = UserConfig::default();
        config.recent.entries.push(RecentRepo {
            path: std::path::PathBuf::from("/tmp/demo"),
            alias: Some("演示".to_owned()),
            opened_at: 1_700_000_000,
        });
        let text = config.to_toml().expect("序列化应成功");
        let parsed = UserConfig::from_toml(&text).expect("反序列化应成功");
        assert_eq!(parsed, config);
        assert!(unknown_key_warnings(&text).is_empty());
    }

    #[test]
    fn known_keys_produce_no_warnings() {
        let warnings = unknown_key_warnings(
            r#"
            [appearance]
            theme = "dark"
            language = "en"
            font_family = "JetBrains Mono"
            font_size = 14

            [git]
            staging_mode = false
            update_strategy = "rebase"
            warn_crlf = false
            warn_detached_commit = true

            [cache]
            memory_mib = 64
            log_index_disk_mib = 512

            [diagnostics]
            log_level = "debug"
            log_to_file = true

            [[recent]]
            path = "/tmp/repo"
            opened_at = 1
            "#,
        );
        assert!(warnings.is_empty(), "实际：{warnings:#?}");
    }

    #[test]
    fn tolerantly_parses_a_config_with_typos() {
        // 打错键名不应让整份配置不可用 —— 只是那项回退默认值。
        let config = UserConfig::from_toml(
            r#"
            [appearance]
            theme = "dark"
            font_sizee = 14
            "#,
        )
        .expect("未知键应被忽略");
        assert_eq!(config.appearance.theme, Theme::Dark);
        assert_eq!(config.appearance.font_size, 13, "打错的项回退默认值");
        assert_eq!(config.diagnostics.log_level, LogLevel::Info);
    }

    #[test]
    fn load_reports_missing_file_as_defaults_not_error() {
        let dir =
            std::env::temp_dir().join(format!("knotwork-config-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.toml");

        let outcome = super::load_from(&path).expect("文件不存在不应报错");
        assert_eq!(outcome.config, UserConfig::default());
        assert!(!outcome.is_from_file());
        assert!(outcome.warnings.is_empty());

        // 保存后再读回，内容与来源都应对得上。
        super::save_to(&outcome.config, &path).expect("保存应成功");
        let reloaded = super::load_from(&path).expect("读回应成功");
        assert_eq!(reloaded.config, UserConfig::default());
        assert!(reloaded.is_from_file());
        assert_eq!(reloaded, reloaded.clone());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_is_atomic_and_leaves_no_temp_file() {
        let dir =
            std::env::temp_dir().join(format!("knotwork-config-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.toml");

        let config = UserConfig::default();
        super::save_to(&config, &path).expect("首次保存应成功");
        super::save_to(&config, &path).expect("覆盖保存应成功");

        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("列出配置目录")
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "config.toml")
            .collect();
        assert!(leftovers.is_empty(), "不应留下临时文件：{leftovers:#?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_outcome_distinguishes_default_from_file() {
        let default = LoadOutcome {
            config: UserConfig::default(),
            path: None,
            warnings: vec!["x".to_owned()],
        };
        assert!(!default.is_from_file());
    }
}
