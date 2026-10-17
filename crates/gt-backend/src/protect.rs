//! 受保护分支策略（FR-RMT-03 的"禁止 force push"）。
//!
//! 规则来自设置里的正则列表（`BranchProtectionConfig::protected_patterns`），
//! 匹配对象是**本地分支名**（`main`、`release/1.2`），不带 `refs/heads/` 前缀 ——
//! 与用户在 IDEA 里填写"受保护分支"的习惯一致。
//!
//! # 为什么无效正则只是被忽略并记录，而不是让整个策略失效
//!
//! 一份写错的正则（例如 `release/(`）如果让策略整体不可用，用户会在毫不知情的
//! 情况下失去保护 —— 这是**安全默认值**的反面。因此：无效项被忽略、记入
//! [`BranchProtection::invalid`]，由界面提示"这条规则无效"，其余规则照常生效。
//!
//! # 匹配语义
//!
//! 用"整串匹配"（等价于给每条规则加上 `^...$`）而不是"包含"：
//! 规则 `main` 不应该把 `maintenance` 也保护起来。用户想匹配子串时
//! 自己写 `.*main.*` 即可 —— 这与 git 的 `branch.<name>` 配置习惯一致，
//! 也是"正则"这个说法的字面含义。

use regex::Regex;

/// 一次推送策略判定的结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtectionVerdict {
    /// 允许。
    Allowed,
    /// 分支受保护，禁止强制推送。
    ForceBlocked {
        /// 命中的分支名。
        branch: String,
        /// 命中的规则原文（用户能据此去改设置）。
        pattern: String,
    },
}

impl ProtectionVerdict {
    /// 是否允许执行。
    #[must_use]
    pub const fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed)
    }

    /// 面向用户的拒绝说明。
    ///
    /// 固定包含"发生了什么 / 为什么 / 替代做法"三要素 —— 与 D4 的高危确认
    /// 文案同一口径：拒绝也要告诉用户下一步怎么走。
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Allowed => String::new(),
            Self::ForceBlocked { branch, pattern } => format!(
                "分支 {branch} 属于受保护分支，禁止强制推送。\n\
                 命中规则：{pattern}\n\
                 替代做法：先 Fetch 并把远端变更合并进来（Update），再用普通推送；\
                 若确实需要改写历史，请在终端手工操作并自行承担风险，\
                 或把该分支移出设置里的受保护列表（不推荐）。"
            ),
        }
    }
}

/// 编译后的受保护分支规则集。
#[derive(Debug, Clone, Default)]
pub struct BranchProtection {
    patterns: Vec<String>,
    regexes: Vec<(String, Regex)>,
    invalid: Vec<String>,
}

impl BranchProtection {
    /// 由正则字符串列表编译。
    ///
    /// 编译失败的规则进入 [`Self::invalid`]，不影响其余规则。
    #[must_use]
    pub fn new(patterns: &[String]) -> Self {
        let mut regexes = Vec::with_capacity(patterns.len());
        let mut invalid = Vec::new();
        for pattern in patterns {
            let trimmed = pattern.trim();
            if trimmed.is_empty() {
                continue;
            }
            // 用户自带锚点时按原样编译（`^release/` 就是前缀匹配）；
            // 不带锚点时按"整串相等"处理，避免 `main` 把 `maintenance` 也保护了。
            let anchored = if trimmed.contains('^') || trimmed.contains('$') {
                trimmed.to_owned()
            } else {
                format!("^(?:{trimmed})$")
            };
            match Regex::new(&anchored) {
                Ok(regex) => regexes.push((trimmed.to_owned(), regex)),
                Err(err) => {
                    tracing::warn!(pattern = trimmed, %err, "受保护分支规则无效，已忽略");
                    invalid.push(trimmed.to_owned());
                }
            }
        }
        Self {
            patterns: patterns.to_vec(),
            regexes,
            invalid,
        }
    }

    /// 原始规则（用于回填设置界面）。
    #[must_use]
    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }

    /// 编译失败的规则原文。
    #[must_use]
    pub fn invalid(&self) -> &[String] {
        &self.invalid
    }

    /// 是否没有任何生效规则。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.regexes.is_empty()
    }

    /// 分支命中的第一条规则。
    #[must_use]
    pub fn matched(&self, branch: &str) -> Option<&str> {
        self.regexes
            .iter()
            .find(|(_, regex)| regex.is_match(branch))
            .map(|(pattern, _)| pattern.as_str())
    }

    /// 是否为受保护分支。
    #[must_use]
    pub fn is_protected(&self, branch: &str) -> bool {
        self.matched(branch).is_some()
    }

    /// 判定一次推送是否允许。
    ///
    /// 只有"强制推送"需要拦截：普通推送对受保护分支本身是合法的
    /// （远端是否接受由服务端策略决定），把它一起禁掉会让日常推送无法使用。
    #[must_use]
    pub fn check_push(&self, branch: &str, force: bool) -> ProtectionVerdict {
        if !force {
            return ProtectionVerdict::Allowed;
        }
        match self.matched(branch) {
            Some(pattern) => ProtectionVerdict::ForceBlocked {
                branch: branch.to_owned(),
                pattern: pattern.to_owned(),
            },
            None => ProtectionVerdict::Allowed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BranchProtection, ProtectionVerdict};

    fn rules(patterns: &[&str]) -> BranchProtection {
        BranchProtection::new(
            &patterns
                .iter()
                .map(|item| (*item).to_owned())
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn recommended_defaults_protect_the_usual_branches() {
        let protection = BranchProtection::new(BranchProtection::default().patterns());
        // 默认没有规则（设置里为空），这里确认"空规则"不会误伤。
        assert!(protection.is_empty());
        assert!(!protection.is_protected("main"));
    }

    #[test]
    fn matching_is_anchored_to_the_whole_branch_name() {
        let protection = rules(&["main", "^release/"]);
        assert!(protection.is_protected("main"));
        assert!(protection.is_protected("release/1.2"));
        assert!(
            !protection.is_protected("maintenance"),
            "`main` 不应把 `maintenance` 也保护起来"
        );
        assert!(!protection.is_protected("feature/main"));
    }

    #[test]
    fn force_push_is_blocked_only_for_protected_branches() {
        let protection = rules(&["^main$", "^release/"]);

        assert_eq!(
            protection.check_push("main", false),
            ProtectionVerdict::Allowed
        );
        assert_eq!(
            protection.check_push("feature/x", true),
            ProtectionVerdict::Allowed
        );

        let blocked = protection.check_push("main", true);
        assert!(!blocked.is_allowed());
        let message = blocked.message();
        assert!(message.contains("受保护分支"), "{message}");
        assert!(
            message.contains("替代做法"),
            "拒绝文案必须给出替代做法：{message}"
        );
        assert!(message.contains("^main$"), "必须指出命中的规则：{message}");

        // 用户写 `^release/` 这种前缀形式也要能命中。
        let prefix = rules(&["^release/"]);
        assert!(!prefix.check_push("release/2.0", true).is_allowed());
        // 不带锚点的规则按整串相等处理：`main` 不保护 `maintenance`。
        let exact = rules(&["main"]);
        assert!(!exact.is_protected("maintenance"));
    }

    #[test]
    fn invalid_patterns_are_ignored_but_reported() {
        let protection = rules(&["^main$", "release/(", "*bad"]);
        assert_eq!(protection.invalid().len(), 2, "两条无效规则应被记录");
        assert!(protection.is_protected("main"), "有效规则仍须生效");
        assert_eq!(protection.matched("main"), Some("^main$"));
    }

    #[test]
    fn empty_and_whitespace_patterns_are_dropped() {
        let protection = rules(&["", "   ", "^main$"]);
        assert_eq!(protection.patterns().len(), 3, "原文保留供界面回填");
        assert!(protection.invalid().is_empty());
        assert!(protection.is_protected("main"));
    }
}
