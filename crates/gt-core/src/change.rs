//! 树差异条目（两个修订之间的文件级变更）。

/// 单个文件的变更类型，语义对齐 `git diff --name-status` 的状态字母。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ChangeKind {
    /// `A` 新增。
    Added,
    /// `D` 删除。
    Deleted,
    /// `M` 内容修改或模式变更。
    Modified,
    /// `R` 重命名。
    Renamed,
    /// `C` 复制。
    Copied,
    /// `T` 类型变更（普通文件 ↔ 符号链接 ↔ 子模块）。
    TypeChanged,
}

impl ChangeKind {
    /// `git diff --name-status` 使用的单字母状态码。
    #[must_use]
    pub const fn letter(self) -> char {
        match self {
            Self::Added => 'A',
            Self::Deleted => 'D',
            Self::Modified => 'M',
            Self::Renamed => 'R',
            Self::Copied => 'C',
            Self::TypeChanged => 'T',
        }
    }

    /// 该状态是否携带"旧路径"信息。
    #[must_use]
    pub const fn has_old_path(self) -> bool {
        matches!(self, Self::Renamed | Self::Copied)
    }

    /// 中文标签，供 UI 直接展示。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Added => "新增",
            Self::Deleted => "删除",
            Self::Modified => "修改",
            Self::Renamed => "重命名",
            Self::Copied => "复制",
            Self::TypeChanged => "类型变更",
        }
    }
}

/// 一个文件的树差异条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// 新路径（相对仓库根，使用 `/` 分隔）。
    pub path: String,
    /// 旧路径；仅重命名 / 复制时存在。
    pub old_path: Option<String>,
    /// 变更类型。
    pub kind: ChangeKind,
    /// 重命名 / 复制的相似度（0–100）；其他类型为 [`None`]。
    pub similarity: Option<u8>,
}

impl FileChange {
    /// 构造一个非重命名类变更。
    #[must_use]
    pub fn new(path: impl Into<String>, kind: ChangeKind) -> Self {
        Self {
            path: path.into(),
            old_path: None,
            kind,
            similarity: None,
        }
    }

    /// 构造一个重命名 / 复制类变更。
    #[must_use]
    pub fn with_old_path(
        path: impl Into<String>,
        old_path: impl Into<String>,
        kind: ChangeKind,
    ) -> Self {
        Self {
            path: path.into(),
            old_path: Some(old_path.into()),
            kind,
            similarity: None,
        }
    }

    /// 用于展示的路径（重命名时显示 `旧 → 新`）。
    #[must_use]
    pub fn display_path(&self) -> String {
        match &self.old_path {
            None => self.path.clone(),
            Some(old) => format!("{old} → {}", self.path),
        }
    }

    /// 输出与 `git diff --name-status` 一致的一行。
    ///
    /// 分隔符与 CLI 一致使用 TAB（而非空格）：单路径为 `M<TAB>path`，
    /// 重命名 / 复制为 `R<score><TAB>旧<TAB>新`。相似度按 CLI 的惯例
    /// **补零到三位**（`R050`、`R100`）；未知时按内容完全相同处理（100）。
    #[must_use]
    pub fn name_status_line(&self) -> String {
        match &self.old_path {
            None => format!("{}\t{}", self.kind.letter(), self.path),
            Some(old) => format!(
                "{}{:03}\t{old}\t{}",
                self.kind.letter(),
                self.similarity.unwrap_or(100),
                self.path
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ChangeKind, FileChange};

    #[test]
    fn letters_match_git_status_codes() {
        assert_eq!(ChangeKind::Added.letter(), 'A');
        assert_eq!(ChangeKind::Deleted.letter(), 'D');
        assert_eq!(ChangeKind::Modified.letter(), 'M');
        assert_eq!(ChangeKind::Renamed.letter(), 'R');
        assert_eq!(ChangeKind::Copied.letter(), 'C');
        assert_eq!(ChangeKind::TypeChanged.letter(), 'T');
        assert!(ChangeKind::Renamed.has_old_path());
        assert!(!ChangeKind::Modified.has_old_path());
    }

    #[test]
    fn name_status_line_matches_cli_format() {
        // CLI 用 TAB 分隔状态与路径（`git diff --name-status`）。
        assert_eq!(
            FileChange::new("src/main.rs", ChangeKind::Modified).name_status_line(),
            "M\tsrc/main.rs"
        );
        // 重命名带相似度分数，按 CLI 惯例补零到三位；未知时按内容相同处理（100）。
        assert_eq!(
            FileChange::with_old_path("b.rs", "a.rs", ChangeKind::Renamed).name_status_line(),
            "R100\ta.rs\tb.rs"
        );
        let mut scored = FileChange::with_old_path("b.rs", "a.rs", ChangeKind::Renamed);
        scored.similarity = Some(87);
        assert_eq!(scored.name_status_line(), "R087\ta.rs\tb.rs");
    }

    #[test]
    fn display_path_shows_transition() {
        assert_eq!(
            FileChange::new("a.rs", ChangeKind::Added).display_path(),
            "a.rs"
        );
        assert_eq!(
            FileChange::with_old_path("b.rs", "a.rs", ChangeKind::Renamed).display_path(),
            "a.rs → b.rs"
        );
    }
}
