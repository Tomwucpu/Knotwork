//! 差异的数据模型（与算法实现解耦，便于 UI 与测试直接构造）。

/// 单侧文件超过该字节数即不计算差异，只显示占位（架构规划 5.5）。
pub const DEFAULT_MAX_BYTES: usize = 2 * 1024 * 1024;

/// hunk 上下文的默认行数，与 `git diff -U3` 一致。
pub const DEFAULT_CONTEXT_LINES: usize = 3;

/// 二进制嗅探读取的字节数，与 `git` 的 `buffer_is_binary` 一致（前 8000 字节）。
pub const BINARY_SNIFF_LEN: usize = 8000;

/// 差异计算选项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffOptions {
    /// hunk 上下文行数。
    pub context_lines: usize,
    /// 是否忽略行首尾空白后再比较（`git diff -w` 的部分语义：不做行内规范化，
    /// 只把参与比较的行 token 替换为其裁剪后的形式）。
    pub ignore_whitespace: bool,
    /// 单侧超过该字节数即降级为占位。
    pub max_bytes: usize,
    /// 是否计算行内词级高亮。
    pub word_level: bool,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self {
            context_lines: DEFAULT_CONTEXT_LINES,
            ignore_whitespace: false,
            max_bytes: DEFAULT_MAX_BYTES,
            word_level: true,
        }
    }
}

impl DiffOptions {
    /// 关闭词级高亮（用于大 hunk 的性能兜底）。
    #[must_use]
    pub const fn without_word_level(mut self) -> Self {
        self.word_level = false;
        self
    }

    /// 设置上下文行数。
    #[must_use]
    pub const fn with_context_lines(mut self, lines: usize) -> Self {
        self.context_lines = lines;
        self
    }
}

/// 单行在差异中的角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LineKind {
    /// 上下文行（两侧都有）。
    Context,
    /// 新增行（仅新侧）。
    Added,
    /// 删除行（仅旧侧）。
    Removed,
}

impl LineKind {
    /// unified 视图的行首字符（上下文为空格）。
    #[must_use]
    pub const fn prefix(self) -> char {
        match self {
            Self::Context => ' ',
            Self::Added => '+',
            Self::Removed => '-',
        }
    }

    /// 该行是否属于变更。
    #[must_use]
    pub const fn is_change(self) -> bool {
        matches!(self, Self::Added | Self::Removed)
    }
}

/// 行内的一段文本及其是否属于变更部分（词级高亮用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineSpan {
    /// 该段是否被识别为变更。
    pub changed: bool,
    /// 片段文本（已去除行尾换行）。
    pub text: String,
}

impl LineSpan {
    /// 构造片段。
    #[must_use]
    pub fn new(changed: bool, text: impl Into<String>) -> Self {
        Self {
            changed,
            text: text.into(),
        }
    }
}

/// 差异中的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    /// 行角色。
    pub kind: LineKind,
    /// 旧文件行号（1 起）；新增行与上下文行分别见字段说明。
    pub old_lineno: Option<u32>,
    /// 新文件行号（1 起）。
    pub new_lineno: Option<u32>,
    /// 行文本（不含行尾换行符）。
    pub text: String,
    /// 词级高亮片段；未计算或整行变更时为空。
    pub spans: Vec<LineSpan>,
}

impl DiffLine {
    /// 构造不带词级片段的行。
    #[must_use]
    pub fn new(
        kind: LineKind,
        old_lineno: Option<u32>,
        new_lineno: Option<u32>,
        text: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            old_lineno,
            new_lineno,
            text: text.into(),
            spans: Vec::new(),
        }
    }

    /// 输出 unified 风格的一行（含行首前缀）。
    #[must_use]
    pub fn unified_line(&self) -> String {
        let mut out = String::with_capacity(self.text.len() + 1);
        out.push(self.kind.prefix());
        out.push_str(&self.text);
        out
    }
}

/// 一个变更块。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffHunk {
    /// 旧文件起始行号。
    pub old_start: u32,
    /// 旧文件行数。
    pub old_lines: u32,
    /// 新文件起始行号。
    pub new_start: u32,
    /// 新文件行数。
    pub new_lines: u32,
    /// 块内行（含上下文）。
    pub lines: Vec<DiffLine>,
}

impl DiffHunk {
    /// `@@ -a,b +c,d @@` 形式的块头。
    ///
    /// 与 `git` 一致：行数为 1 时省略 `,1`（例如 `@@ -1,0 +2 @@`）。
    #[must_use]
    pub fn header(&self) -> String {
        format!(
            "@@ -{} +{} @@",
            format_range(self.old_start, self.old_lines),
            format_range(self.new_start, self.new_lines)
        )
    }

    /// 块内新增行数。
    #[must_use]
    pub fn added(&self) -> u32 {
        self.lines
            .iter()
            .filter(|line| line.kind == LineKind::Added)
            .count() as u32
    }

    /// 块内删除行数。
    #[must_use]
    pub fn removed(&self) -> u32 {
        self.lines
            .iter()
            .filter(|line| line.kind == LineKind::Removed)
            .count() as u32
    }

    /// 简化的正文（不含块头），便于测试与日志比对。
    #[must_use]
    pub fn body(&self) -> String {
        let mut out = self
            .lines
            .iter()
            .map(DiffLine::unified_line)
            .collect::<Vec<_>>()
            .join("\n");
        out.push('\n');
        out
    }
}

/// 格式化 unified diff 的行号区间，规则与 `git` 一致。
fn format_range(start: u32, count: u32) -> String {
    if count == 1 {
        start.to_string()
    } else {
        format!("{start},{count}")
    }
}

/// 差异结果的载荷。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffPayload {
    /// 文本差异（可能为空表示两侧相同）。
    Text(Vec<DiffHunk>),
    /// 检测为二进制。
    Binary,
    /// 单侧超过阈值。
    TooLarge {
        /// 旧侧字节数。
        old_bytes: usize,
        /// 新侧字节数。
        new_bytes: usize,
    },
    /// 该文件只存在于一侧且内容为空（例如空文件新增）。
    Empty,
}

impl DiffPayload {
    /// 是否可展示行级内容。
    #[must_use]
    pub fn hunks(&self) -> Option<&[DiffHunk]> {
        match self {
            Self::Text(hunks) => Some(hunks),
            Self::Binary | Self::TooLarge { .. } | Self::Empty => None,
        }
    }

    /// 是否为占位载荷（不展示内容）。
    #[must_use]
    pub const fn is_placeholder(&self) -> bool {
        !matches!(self, Self::Text(_))
    }
}

/// 与 `git diff --numstat` 对应的统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiffStats {
    /// 新增行数。
    pub added: u32,
    /// 删除行数。
    pub removed: u32,
    /// 是否为二进制（numstat 输出 `-`）。
    pub binary: bool,
}

impl DiffStats {
    /// 是否为无变化的空统计。
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.added == 0 && self.removed == 0
    }

    /// `git diff --numstat` 的统计列（`added\tremoved`，二进制为 `-\t-`）。
    #[must_use]
    pub fn numstat_columns(&self) -> String {
        if self.binary {
            "-\t-".to_owned()
        } else {
            format!("{}\t{}", self.added, self.removed)
        }
    }
}

/// 单个文件的差异结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// 旧路径。
    pub old_path: String,
    /// 新路径。
    pub new_path: String,
    /// 差异载荷。
    pub payload: DiffPayload,
    /// 增删统计。
    pub stats: DiffStats,
}

impl FileDiff {
    /// 构造差异结果。
    #[must_use]
    pub fn new(
        old_path: impl Into<String>,
        new_path: impl Into<String>,
        payload: DiffPayload,
        stats: DiffStats,
    ) -> Self {
        Self {
            old_path: old_path.into(),
            new_path: new_path.into(),
            payload,
            stats,
        }
    }

    /// 是否为二进制文件。
    #[must_use]
    pub fn is_binary(&self) -> bool {
        matches!(self.payload, DiffPayload::Binary)
    }

    /// 是否为文件过大占位。
    #[must_use]
    pub fn is_too_large(&self) -> bool {
        matches!(self.payload, DiffPayload::TooLarge { .. })
    }

    /// 是否没有任何差异。
    #[must_use]
    pub fn is_identical(&self) -> bool {
        self.stats.is_empty() && !self.payload.is_placeholder()
    }

    /// `git diff --numstat` 的单行输出（`added\tremoved\tpath`）。
    ///
    /// 重命名 / 复制时路径按 git 的惯例缩写（见 [`abbreviate_rename`]），
    /// 例如 `dir/a.txt` 与 `dir/b.txt` 输出为 `dir/{a.txt => b.txt}`。
    ///
    /// 注：本函数不做 porcelain 的路径引号（`gt-diff` 不感知 `core.quotepath`）；
    /// 需要与 CLI 逐字节一致的展示请由上层对路径套用引号规则。
    #[must_use]
    pub fn numstat_line(&self) -> String {
        let path = if self.old_path == self.new_path {
            self.new_path.clone()
        } else {
            abbreviate_rename(&self.old_path, &self.new_path)
        };
        format!("{}\t{path}", self.stats.numstat_columns())
    }

    /// 本次差异内容的指纹（FNV-1a，64 位）。
    ///
    /// 用途见 `gt-service` 的 hunk 落库校验：界面把"它看到的那份差异"的指纹
    /// 一并带上来，落库前重新计算并比对。文件在用户选择期间被外部改动时，
    /// 旧下标可能指向错误的位置 —— 这时必须拒绝，而不是"尽量打上去"。
    ///
    /// 不追求密码学强度：这是同机同进程的乐观校验，不是防篡改。
    #[must_use]
    pub fn fingerprint(&self) -> u64 {
        const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const PRIME: u64 = 0x0000_0100_0000_01b3;

        let mut hash = OFFSET;
        {
            let mut feed = |bytes: &[u8]| {
                for byte in bytes {
                    hash ^= u64::from(*byte);
                    hash = hash.wrapping_mul(PRIME);
                }
            };

            feed(self.old_path.as_bytes());
            feed(&[0]);
            feed(self.new_path.as_bytes());
            match &self.payload {
                DiffPayload::Text(hunks) => {
                    for hunk in hunks {
                        feed(hunk.header().as_bytes());
                        for line in &hunk.lines {
                            feed(&[line.kind.prefix() as u8]);
                            feed(line.text.as_bytes());
                            feed(b"\n");
                        }
                    }
                }
                DiffPayload::Binary => feed(b"binary"),
                DiffPayload::TooLarge {
                    old_bytes,
                    new_bytes,
                } => feed(format!("too-large:{old_bytes}:{new_bytes}").as_bytes()),
                DiffPayload::Empty => feed(b"empty"),
            }
        }
        hash
    }

    /// unified 视图的完整文本（占位载荷返回说明行）。
    #[must_use]
    pub fn unified_text(&self) -> String {
        match &self.payload {
            DiffPayload::Text(hunks) => {
                let mut out = String::new();
                for hunk in hunks {
                    out.push_str(&hunk.header());
                    out.push('\n');
                    out.push_str(&hunk.body());
                }
                out
            }
            DiffPayload::Binary => "二进制文件不展示差异\n".to_owned(),
            DiffPayload::TooLarge { .. } => "文件过大，未计算差异\n".to_owned(),
            DiffPayload::Empty => String::new(),
        }
    }
}

/// 把 `旧 => 新` 缩写成 git 的写法。
///
/// 规则由 `git diff --numstat` 实测得出（`scripts/probe-git-porcelain.py`
/// 与本文件的用例共同固化）：
///
/// - 公共前缀只保留**到最后一个 `/`** 为止的部分，没有 `/` 就当作空前缀；
/// - 公共后缀必须从某个 `/` 开始才算数；
/// - 两侧都为空时退化为完整的 `旧 => 新`。
///
/// 实测对照：
///
/// | 旧 | 新 | 输出中段 |
/// |---|---|---|
/// | `renamed.txt` | `moved.txt` | `renamed.txt => moved.txt` |
/// | `dir/a.txt` | `dir/b.txt` | `dir/{a.txt => b.txt}` |
/// | `a.txt` | `a-old.txt` | `a.txt => a-old.txt` |
/// | `dir/sub/a.txt` | `dir/sub/b.txt` | `dir/sub/{a.txt => b.txt}` |
#[must_use]
pub fn abbreviate_rename(old: &str, new: &str) -> String {
    let old_bytes = old.as_bytes();
    let new_bytes = new.as_bytes();

    // 公共前缀：只认到最后一个 `/`（含）为止。
    let mut prefix_end = 0usize;
    let mut index = 0usize;
    while index < old_bytes.len() && index < new_bytes.len() && old_bytes[index] == new_bytes[index]
    {
        if old_bytes[index] == b'/' {
            prefix_end = index + 1;
        }
        index += 1;
    }

    // 公共后缀：从末尾往前比，只认从 `/` 开始的那一段。
    let mut suffix: Option<(usize, usize)> = None;
    let mut back = 0usize;
    loop {
        let old_index = old.len().checked_sub(back + 1);
        let new_index = new.len().checked_sub(back + 1);
        let (Some(old_index), Some(new_index)) = (old_index, new_index) else {
            break;
        };
        if old_index < prefix_end || new_index < prefix_end {
            break;
        }
        if old_bytes[old_index] != new_bytes[new_index] {
            break;
        }
        if old_bytes[old_index] == b'/' {
            suffix = Some((old_index, new_index));
        }
        back += 1;
    }

    let (old_mid, new_mid, tail) = match suffix {
        Some((old_index, new_index)) if old_index > prefix_end && new_index > prefix_end => (
            &old[prefix_end..old_index],
            &new[prefix_end..new_index],
            &old[old_index..],
        ),
        _ => (&old[prefix_end..], &new[prefix_end..], ""),
    };

    if prefix_end == 0 && tail.is_empty() {
        return format!("{old} => {new}");
    }
    // git 的花括号缩写：`前缀{旧中段 => 新中段}后缀`。
    format!("{}{{{old_mid} => {new_mid}}}{tail}", &old[..prefix_end])
}

#[cfg(test)]
mod tests {
    use super::{
        abbreviate_rename, DiffHunk, DiffLine, DiffOptions, DiffPayload, DiffStats, FileDiff,
        LineKind, LineSpan, DEFAULT_MAX_BYTES,
    };

    /// 与 `git diff --numstat` 实测输出逐条对齐。
    #[test]
    fn rename_abbreviation_matches_git() {
        for (old, new, expected) in [
            ("renamed.txt", "moved.txt", "renamed.txt => moved.txt"),
            ("dir/a.txt", "dir/b.txt", "dir/{a.txt => b.txt}"),
            ("dir/sub/a.txt", "dir/sub/b.txt", "dir/sub/{a.txt => b.txt}"),
            ("src/a.rs", "src/b.rs", "src/{a.rs => b.rs}"),
            ("a.txt", "a-old.txt", "a.txt => a-old.txt"),
            (
                "prefix-old.txt",
                "prefix-new.txt",
                "prefix-old.txt => prefix-new.txt",
            ),
        ] {
            assert_eq!(abbreviate_rename(old, new), expected, "{old} -> {new}");
        }
    }

    #[test]
    fn rename_abbreviation_keeps_a_slash_anchored_suffix() {
        // 只有后缀锚定在 `/` 上时才缩写中段。
        assert_eq!(
            abbreviate_rename("old-dir/x.txt", "new-dir/x.txt"),
            "{old-dir => new-dir}/x.txt"
        );
        // 符号不同则不缩写。
        assert_eq!(
            abbreviate_rename("a/x.txt", "a/y.txt"),
            "a/{x.txt => y.txt}"
        );
    }

    #[test]
    fn numstat_line_uses_the_abbreviated_rename_form() {
        let mut diff = FileDiff {
            old_path: "dir/a.txt".to_owned(),
            new_path: "dir/b.txt".to_owned(),
            payload: DiffPayload::Empty,
            stats: DiffStats::default(),
        };
        assert_eq!(diff.numstat_line(), "0\t0\tdir/{a.txt => b.txt}");

        // 非重命名（两侧同路径）时仍是普通形式。
        diff.new_path = "dir/a.txt".to_owned();
        assert_eq!(diff.numstat_line(), "0\t0\tdir/a.txt");

        // 二进制统计沿用 `-\t-`。
        diff.stats = DiffStats {
            binary: true,
            ..DiffStats::default()
        };
        assert_eq!(diff.numstat_line(), "-\t-\tdir/a.txt");
    }

    #[test]
    fn default_options_follow_git_conventions() {
        let options = DiffOptions::default();
        assert_eq!(options.context_lines, 3, "与 git diff -U3 一致");
        assert_eq!(options.max_bytes, DEFAULT_MAX_BYTES);
        assert!(options.word_level);
        assert!(!options.ignore_whitespace);
        assert!(!options.without_word_level().word_level);
        assert_eq!(options.with_context_lines(0).context_lines, 0);
    }

    #[test]
    fn line_kind_prefixes_match_unified_diff() {
        assert_eq!(LineKind::Context.prefix(), ' ');
        assert_eq!(LineKind::Added.prefix(), '+');
        assert_eq!(LineKind::Removed.prefix(), '-');
        assert!(LineKind::Added.is_change());
        assert!(!LineKind::Context.is_change());
    }

    #[test]
    fn hunk_header_matches_git_format() {
        let hunk = DiffHunk {
            old_start: 1,
            old_lines: 3,
            new_start: 1,
            new_lines: 4,
            lines: vec![],
        };
        assert_eq!(hunk.header(), "@@ -1,3 +1,4 @@");
    }

    #[test]
    fn hunk_header_omits_single_line_count() {
        // git 对行数为 1 的一侧省略 `,1`。
        let hunk = DiffHunk {
            old_start: 1,
            old_lines: 0,
            new_start: 2,
            new_lines: 1,
            lines: vec![],
        };
        assert_eq!(hunk.header(), "@@ -1,0 +2 @@");

        let hunk = DiffHunk {
            old_start: 7,
            old_lines: 1,
            new_start: 7,
            new_lines: 1,
            lines: vec![],
        };
        assert_eq!(hunk.header(), "@@ -7 +7 @@");
    }

    #[test]
    fn hunk_counts_changes_and_renders_body() {
        let hunk = DiffHunk {
            old_start: 1,
            old_lines: 2,
            new_start: 1,
            new_lines: 2,
            lines: vec![
                DiffLine::new(LineKind::Context, Some(1), Some(1), "same"),
                DiffLine::new(LineKind::Removed, Some(2), None, "old"),
                DiffLine::new(LineKind::Added, None, Some(2), "new"),
            ],
        };
        assert_eq!(hunk.added(), 1);
        assert_eq!(hunk.removed(), 1);
        assert_eq!(hunk.body(), " same\n-old\n+new\n");
    }

    #[test]
    fn numstat_columns_render_binary_as_dashes() {
        assert_eq!(
            DiffStats {
                added: 3,
                removed: 2,
                binary: false
            }
            .numstat_columns(),
            "3\t2"
        );
        assert_eq!(
            DiffStats {
                added: 0,
                removed: 0,
                binary: true
            }
            .numstat_columns(),
            "-\t-"
        );
        assert!(DiffStats::default().is_empty());
    }

    #[test]
    fn file_diff_reports_placeholder_states() {
        let binary = FileDiff::new(
            "img.png",
            "img.png",
            DiffPayload::Binary,
            DiffStats {
                binary: true,
                ..DiffStats::default()
            },
        );
        assert!(binary.is_binary());
        assert!(!binary.is_too_large());
        assert!(!binary.is_identical());
        assert_eq!(binary.numstat_line(), "-\t-\timg.png");

        let large = FileDiff::new(
            "big.bin",
            "big.bin",
            DiffPayload::TooLarge {
                old_bytes: 3_000_000,
                new_bytes: 3_100_000,
            },
            DiffStats::default(),
        );
        assert!(large.is_too_large());
        assert!(!large.is_binary());
        assert_eq!(large.unified_text(), "文件过大，未计算差异\n");

        let identical = FileDiff::new(
            "a.txt",
            "a.txt",
            DiffPayload::Text(Vec::new()),
            DiffStats::default(),
        );
        assert!(identical.is_identical());
        assert_eq!(identical.unified_text(), "");
    }

    #[test]
    fn payload_hunks_accessor() {
        let payload = DiffPayload::Text(vec![]);
        assert_eq!(payload.hunks(), Some(&[][..]));
        assert!(!payload.is_placeholder());
        assert!(DiffPayload::Binary.is_placeholder());
        assert!(DiffPayload::Empty.is_placeholder());
    }

    #[test]
    fn unified_line_includes_prefix() {
        let line = DiffLine::new(LineKind::Added, None, Some(7), "hello");
        assert_eq!(line.unified_line(), "+hello");
        let spans = [LineSpan::new(true, "a"), LineSpan::new(false, "b")];
        assert_eq!(spans[0].text, "a");
    }
}
