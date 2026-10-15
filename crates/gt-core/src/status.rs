//! 工作区状态条目，以及与 `git status --porcelain=v1` 的精确映射。
//!
//! 本模块承载 M0 待办 #6：把 `gix` 的状态模型固化为 porcelain 字母映射并加断言。
//! 映射表（`git status --porcelain=v1`，`XY PATH`）：
//!
//! | 字母 | 含义 | 对应 [`StatusFlag`] |
//! |---|---|---|
//! | ` ` | 未修改 | [`StatusFlag::Unmodified`] |
//! | `M` | 修改 | [`StatusFlag::Modified`] |
//! | `A` | 新增（已暂存） | [`StatusFlag::Added`] |
//! | `D` | 删除 | [`StatusFlag::Deleted`] |
//! | `R` | 重命名 | [`StatusFlag::Renamed`] |
//! | `C` | 复制 | [`StatusFlag::Copied`] |
//! | `T` | 类型变更 | [`StatusFlag::TypeChanged`] |
//! | `?` | 未跟踪（`??`） | [`StatusFlag::Untracked`] |
//! | `!` | 已忽略（`!!`） | [`StatusFlag::Ignored`] |
//!
//! 冲突状态不使用单字母标志，而由 [`ConflictKind`] 给出 `DD/AU/UD/UA/DU/AA/UU`
//! 七种精确组合。
//!
//! # 路径的 C 风格引号
//!
//! porcelain v1 是**行式**格式（状态、空格、路径），因此路径需要时可被引号包住。
//! 规则与 git 的 `quote_path(..., QUOTE_PATH_QUOTE_SP)` 一致（见 [`PorcelainFormat`]）：
//!
//! - 含双引号 `"`、反斜杠 `\`、控制字符（`< 0x20`）、DEL（`0x7f`）时**必须**加引号；
//!   其中 `"` 与 `\` 转义为 `\"` / `\\`，控制字符转义为三位八进制（如 `\177`）；
//! - 含**空格**时强制加引号（`a b.txt` → `"a b.txt"`），否则行式格式无法解析；
//! - 非 ASCII 字节是否转义由 `core.quotepath` 决定（见 [`PorcelainFormat::GIT_DEFAULT`]）。

/// 路径渲染口径。
///
/// 对应 git 的 `core.quotepath` 配置项；`quotepath` 只影响**非 ASCII 字节**
/// 是否按八进制转义，不影响"何时需要引号"（后者由内容决定，见模块文档）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PorcelainFormat {
    /// 是否把非 ASCII 字节（`>= 0x80`）转义成三位八进制。
    ///
    /// git 出厂配置为开启（[`Self::GIT_DEFAULT`]）。
    pub quote_non_ascii: bool,
}

impl PorcelainFormat {
    /// git 出厂配置：`core.quotepath=true`，非 ASCII 被转义为 `\344\270\255`。
    pub const GIT_DEFAULT: Self = Self {
        quote_non_ascii: true,
    };

    /// `core.quotepath=false`：非 ASCII 原样输出，可读性更好。
    ///
    /// 这也是 UI / Console 默认使用的口径 —— 把中文路径显示成
    /// `\344\270\255\346\226\207.txt` 对使用者毫无意义。
    pub const RAW_NON_ASCII: Self = Self {
        quote_non_ascii: false,
    };

    /// 由仓库配置的 `core.quotepath` 取值构造。
    #[must_use]
    pub const fn from_quotepath(quote_non_ascii: bool) -> Self {
        Self { quote_non_ascii }
    }
}

/// 单侧（索引侧 `X` 或工作区侧 `Y`）的状态标志。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum StatusFlag {
    /// ` ` 未修改。
    Unmodified,
    /// `M` 修改。
    Modified,
    /// `A` 新增。
    Added,
    /// `D` 删除。
    Deleted,
    /// `R` 重命名。
    Renamed,
    /// `C` 复制。
    Copied,
    /// `T` 类型变更。
    TypeChanged,
    /// `?` 未跟踪；仅在 `X`/`Y` 同时为该值时可解析为 porcelain 的 `??`。
    Untracked,
    /// `!` 已忽略；仅在 `X`/`Y` 同时为该值时可解析为 porcelain 的 `!!`。
    Ignored,
}

impl StatusFlag {
    /// porcelain 单字母编码。
    #[must_use]
    pub const fn letter(self) -> char {
        match self {
            Self::Unmodified => ' ',
            Self::Modified => 'M',
            Self::Added => 'A',
            Self::Deleted => 'D',
            Self::Renamed => 'R',
            Self::Copied => 'C',
            Self::TypeChanged => 'T',
            Self::Untracked => '?',
            Self::Ignored => '!',
        }
    }

    /// 是否表示"相对 HEAD 有变化"。
    #[must_use]
    pub const fn is_changed(self) -> bool {
        !matches!(self, Self::Unmodified | Self::Ignored)
    }

    /// 中文标签，供 UI 直接展示。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Unmodified => "未修改",
            Self::Modified => "已修改",
            Self::Added => "已新增",
            Self::Deleted => "已删除",
            Self::Renamed => "已重命名",
            Self::Copied => "已复制",
            Self::TypeChanged => "类型变更",
            Self::Untracked => "未跟踪",
            Self::Ignored => "已忽略",
        }
    }
}

/// 冲突类型，对应 porcelain 的七种 `XY` 组合。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ConflictKind {
    /// `DD` 双方都删除。
    BothDeleted,
    /// `AU` 我们新增、他们修改。
    AddedByUs,
    /// `UD` 我们修改、他们删除。
    DeletedByThem,
    /// `UA` 他们新增、我们修改。
    AddedByThem,
    /// `DU` 我们删除、他们修改。
    DeletedByUs,
    /// `AA` 双方都新增。
    BothAdded,
    /// `UU` 双方都修改。
    BothModified,
}

impl ConflictKind {
    /// porcelain 的两个字母。
    #[must_use]
    pub const fn code(self) -> (char, char) {
        match self {
            Self::BothDeleted => ('D', 'D'),
            Self::AddedByUs => ('A', 'U'),
            Self::DeletedByThem => ('U', 'D'),
            Self::AddedByThem => ('U', 'A'),
            Self::DeletedByUs => ('D', 'U'),
            Self::BothAdded => ('A', 'A'),
            Self::BothModified => ('U', 'U'),
        }
    }

    /// 由两个字母反查冲突类型；不是合法冲突组合时返回 [`None`]。
    #[must_use]
    pub const fn from_code(x: char, y: char) -> Option<Self> {
        match (x, y) {
            ('D', 'D') => Some(Self::BothDeleted),
            ('A', 'U') => Some(Self::AddedByUs),
            ('U', 'D') => Some(Self::DeletedByThem),
            ('U', 'A') => Some(Self::AddedByThem),
            ('D', 'U') => Some(Self::DeletedByUs),
            ('A', 'A') => Some(Self::BothAdded),
            ('U', 'U') => Some(Self::BothModified),
            _ => None,
        }
    }

    /// 中文标签，供 UI 直接展示。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::BothDeleted => "双方删除",
            Self::AddedByUs => "我们新增 / 他们修改",
            Self::DeletedByThem => "我们修改 / 他们删除",
            Self::AddedByThem => "他们新增 / 我们修改",
            Self::DeletedByUs => "我们删除 / 他们修改",
            Self::BothAdded => "双方新增",
            Self::BothModified => "双方修改",
        }
    }
}

/// 一条工作区状态条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    /// 路径（相对仓库根，`/` 分隔）；重命名时为**新**路径。
    pub path: String,
    /// 重命名 / 复制时的**旧**路径。
    pub old_path: Option<String>,
    /// 索引侧状态（porcelain 的 `X`）。
    pub index: StatusFlag,
    /// 工作区侧状态（porcelain 的 `Y`）。
    pub worktree: StatusFlag,
    /// 冲突类型；非冲突条目为 [`None`]。
    pub conflict: Option<ConflictKind>,
}

impl StatusEntry {
    /// 构造普通条目。
    #[must_use]
    pub fn new(path: impl Into<String>, index: StatusFlag, worktree: StatusFlag) -> Self {
        Self {
            path: path.into(),
            old_path: None,
            index,
            worktree,
            conflict: None,
        }
    }

    /// 构造未跟踪条目（`??`）。
    #[must_use]
    pub fn untracked(path: impl Into<String>) -> Self {
        Self::new(path, StatusFlag::Untracked, StatusFlag::Untracked)
    }

    /// 构造已忽略条目（`!!`）。
    #[must_use]
    pub fn ignored(path: impl Into<String>) -> Self {
        Self::new(path, StatusFlag::Ignored, StatusFlag::Ignored)
    }

    /// 构造冲突条目。
    #[must_use]
    pub fn conflicted(path: impl Into<String>, kind: ConflictKind) -> Self {
        Self {
            path: path.into(),
            old_path: None,
            index: StatusFlag::Unmodified,
            worktree: StatusFlag::Unmodified,
            conflict: Some(kind),
        }
    }

    /// 是否为冲突条目。
    #[must_use]
    pub const fn is_conflicted(&self) -> bool {
        self.conflict.is_some()
    }

    /// 是否有已暂存的内容（索引侧相对 HEAD 有变化）。
    #[must_use]
    pub const fn is_staged(&self) -> bool {
        !self.is_conflicted()
            && !matches!(
                self.index,
                StatusFlag::Unmodified | StatusFlag::Untracked | StatusFlag::Ignored
            )
    }

    /// 是否有未暂存的内容（工作区侧相对索引有变化）。
    #[must_use]
    pub const fn has_unstaged(&self) -> bool {
        !self.is_conflicted()
            && !matches!(
                self.worktree,
                StatusFlag::Unmodified | StatusFlag::Untracked | StatusFlag::Ignored
            )
    }

    /// 所属分区。
    #[must_use]
    pub const fn bucket(&self) -> StatusBucket {
        if self.is_conflicted() {
            return StatusBucket::Conflicted;
        }
        if matches!(self.index, StatusFlag::Ignored) {
            return StatusBucket::Ignored;
        }
        if matches!(self.index, StatusFlag::Untracked) {
            return StatusBucket::Untracked;
        }
        if self.is_staged() {
            return StatusBucket::Staged;
        }
        StatusBucket::Modified
    }

    /// 用于 porcelain 输出的路径片段：重命名显示为 `旧 -> 新`。
    #[must_use]
    pub fn porcelain_path(&self) -> String {
        self.porcelain_path_with(PorcelainFormat::RAW_NON_ASCII)
    }

    /// [`Self::porcelain_path`] 的指定口径版本。
    #[must_use]
    pub fn porcelain_path_with(&self, format: PorcelainFormat) -> String {
        match &self.old_path {
            None => quote_path(&self.path, format),
            Some(old) => format!(
                "{} -> {}",
                quote_path(old, format),
                quote_path(&self.path, format)
            ),
        }
    }

    /// 与 `git status --porcelain=v1` 单行一致的输出。
    ///
    /// 默认按 [`PorcelainFormat::RAW_NON_ASCII`] 渲染非 ASCII 路径（可读性优先）；
    /// 需要与 git 出厂配置逐字节一致时用 [`Self::porcelain_with`]。
    #[must_use]
    pub fn porcelain(&self) -> String {
        self.porcelain_with(PorcelainFormat::RAW_NON_ASCII)
    }

    /// [`Self::porcelain`] 的指定口径版本。
    #[must_use]
    pub fn porcelain_with(&self, format: PorcelainFormat) -> String {
        match self.conflict {
            Some(kind) => {
                let (x, y) = kind.code();
                format!("{x}{y} {}", self.porcelain_path_with(format))
            }
            None => format!(
                "{}{} {}",
                self.index.letter(),
                self.worktree.letter(),
                self.porcelain_path_with(format)
            ),
        }
    }
}

/// 单个路径的 porcelain 渲染，对齐 git 的 `quote_path(..., QUOTE_PATH_QUOTE_SP)`。
///
/// 含空格时**强制**加引号（行式格式的解析需要），否则只在内容需要转义时加引号。
#[must_use]
pub fn quote_path(path: &str, format: PorcelainFormat) -> String {
    let force_quotes = path.contains(' ');
    let mut out: Vec<u8> = Vec::with_capacity(path.len() + 2);
    if force_quotes {
        out.push(b'"');
    }
    escape_c_style(path.as_bytes(), format, force_quotes, &mut out);
    if force_quotes {
        out.push(b'"');
    }
    // 逐字节构造（git 自身也是字节级算法）。结果恒为合法 UTF-8：
    // `quotepath` 开启时全部非 ASCII 已被转义成 ASCII；关闭时非 ASCII 字节
    // 从不参与转义，因此总是成段原样拷贝，边界始终落在字符边界上。
    String::from_utf8_lossy(&out).into_owned()
}

/// 按 git 的 `quote_c_style_counted` 语义转义。
///
/// `suppress_quotes` 为真时只做转义、不自行补引号（调用方已补过）。
fn escape_c_style(bytes: &[u8], format: PorcelainFormat, suppress_quotes: bool, out: &mut Vec<u8>) {
    let mut index = 0usize;
    let mut run_start = 0usize;
    let mut quoted = false;

    while index < bytes.len() {
        if !needs_escape(bytes[index], format) {
            index += 1;
            continue;
        }

        // 与 git 一致：开引号只补在"第一次遇到需转义字节"的那次循环里。
        if !suppress_quotes && run_start == 0 {
            out.push(b'"');
        }
        out.extend_from_slice(&bytes[run_start..index]);
        push_escaped(bytes[index], out);
        quoted = true;
        index += 1;
        run_start = index;
    }

    out.extend_from_slice(&bytes[run_start..]);
    if quoted && !suppress_quotes {
        out.push(b'"');
    }
}

/// 该字节是否必须转义 —— 对应 git 的 `cq_must_quote`。
///
/// git 的查表结果与 `quote_path_fully`（即 `core.quotepath`）相加后取正，
/// 因此控制字符 / `"` / `\` 无论如何都必须转义，而高位字节只在
/// `quotepath` 开启时转义。
const fn needs_escape(byte: u8, format: PorcelainFormat) -> bool {
    match byte {
        // 控制字符与 DEL：git 的查表值为 1，与 `quotepath` 无关，恒需转义；
        // `"` 与 `\`：查表值就是字符本身（>= `' '`），同样恒需转义。
        // 两者的**转义形式**不同（八进制 vs `\"` / `\\`），见 `push_escaped`。
        0x00..=0x1f | 0x7f | b'"' | b'\\' => true,
        // 查表值为 0，仅当 `core.quotepath` 开启时才必须转义。
        0x80..=0xff => format.quote_non_ascii,
        _ => false,
    }
}

/// 写出一个字节的转义形式（调用方保证该字节需要转义）。
fn push_escaped(byte: u8, out: &mut Vec<u8>) {
    out.push(b'\\');
    match byte {
        0x07 => out.push(b'a'),
        0x08 => out.push(b'b'),
        0x09 => out.push(b't'),
        0x0a => out.push(b'n'),
        0x0b => out.push(b'v'),
        0x0c => out.push(b'f'),
        0x0d => out.push(b'r'),
        b'"' => out.push(b'"'),
        b'\\' => out.push(b'\\'),
        _ => {
            // 三位八进制，与 git 的 `((ch >> 6) & 03) / ((ch >> 3) & 07) / (ch & 07)` 一致。
            out.push(b'0' + ((byte >> 6) & 0o3));
            out.push(b'0' + ((byte >> 3) & 0o7));
            out.push(b'0' + (byte & 0o7));
        }
    }
}

/// 状态条目所属的界面分区。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum StatusBucket {
    /// 冲突（需先解决，FR-CFL）。
    Conflicted,
    /// 未跟踪。
    Untracked,
    /// 已暂存。
    Staged,
    /// 已修改未暂存。
    Modified,
    /// 已忽略；默认不展示，仅"显示忽略文件"开启时出现。
    Ignored,
}

impl StatusBucket {
    /// 中文分区名，供 UI 直接展示（对应 FR-LC-01 的三分区 + 冲突区）。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Conflicted => "冲突",
            Self::Untracked => "未跟踪",
            Self::Staged => "已暂存",
            Self::Modified => "已修改",
            Self::Ignored => "已忽略",
        }
    }
}

/// 一次状态计算的完整结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangeSet {
    /// 全部条目，按路径排序。
    pub entries: Vec<StatusEntry>,
}

impl ChangeSet {
    /// 由条目列表构造（调用方负责排序）。
    #[must_use]
    pub fn new(entries: Vec<StatusEntry>) -> Self {
        Self { entries }
    }

    /// 工作区是否干净。
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.entries.is_empty()
    }

    /// 条目总数。
    #[must_use]
    pub fn total(&self) -> usize {
        self.entries.len()
    }

    /// 按分区拆分，供 FR-LC-01 的三分区视图直接消费。
    #[must_use]
    pub fn partition(&self) -> PartitionedChanges<'_> {
        let mut out = PartitionedChanges::default();
        for entry in &self.entries {
            match entry.bucket() {
                StatusBucket::Conflicted => out.conflicted.push(entry),
                StatusBucket::Untracked => out.untracked.push(entry),
                StatusBucket::Staged => out.staged.push(entry),
                StatusBucket::Modified => out.modified.push(entry),
                StatusBucket::Ignored => out.ignored.push(entry),
            }
        }
        out
    }

    /// 输出与 `git status --porcelain=v1` 一致的文本（按行排序后比较即可）。
    ///
    /// 按 [`PorcelainFormat::RAW_NON_ASCII`] 渲染非 ASCII 路径；
    /// 需要与 git 出厂配置逐字节一致时用 [`Self::to_porcelain_with`]。
    #[must_use]
    pub fn to_porcelain(&self) -> String {
        self.to_porcelain_with(PorcelainFormat::RAW_NON_ASCII)
    }

    /// [`Self::to_porcelain`] 的指定口径版本。
    #[must_use]
    pub fn to_porcelain_with(&self, format: PorcelainFormat) -> String {
        let mut lines: Vec<String> = self
            .entries
            .iter()
            .map(|entry| entry.porcelain_with(format))
            .collect();
        // git 的输出顺序跟随其内部状态遍历（索引侧变更在前、未跟踪在后），
        // 并非路径字典序；这里统一按行排序，让"内容一致"与"顺序一致"解耦。
        lines.sort();
        let mut out = lines.join("\n");
        if !out.is_empty() {
            out.push('\n');
        }
        out
    }
}

/// [`ChangeSet::partition`] 的结果。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PartitionedChanges<'a> {
    /// 冲突条目。
    pub conflicted: Vec<&'a StatusEntry>,
    /// 未跟踪条目。
    pub untracked: Vec<&'a StatusEntry>,
    /// 已暂存条目。
    pub staged: Vec<&'a StatusEntry>,
    /// 已修改未暂存条目。
    pub modified: Vec<&'a StatusEntry>,
    /// 已忽略条目（默认不展示）。
    pub ignored: Vec<&'a StatusEntry>,
}

#[cfg(test)]
mod tests {
    use super::{
        quote_path, ChangeSet, ConflictKind, PorcelainFormat, StatusBucket, StatusEntry, StatusFlag,
    };

    /// git 的 `quote_path(..., QUOTE_PATH_QUOTE_SP)` 行为，逐条对齐实测输出。
    #[test]
    fn path_quoting_matches_git() {
        let raw = PorcelainFormat::RAW_NON_ASCII;

        // 无需引号。
        for path in [
            "plain.txt",
            "with#hash.txt",
            "with[bracket].txt",
            "with~tilde.txt",
            "-dash.txt",
            "中文.txt",
            "emoji-🎯.txt",
        ] {
            assert_eq!(quote_path(path, raw), path, "{path} 不应被加引号");
        }

        // 含空格：强制加引号（porcelain v1 是行式格式）。
        assert_eq!(quote_path("with space.txt", raw), "\"with space.txt\"");
        assert_eq!(quote_path("a  b.txt", raw), "\"a  b.txt\"");
        assert_eq!(quote_path("trail .txt", raw), "\"trail .txt\"");

        // `"` 与 `\` 转义，并整体加引号。
        assert_eq!(quote_path("q\".txt", raw), "\"q\\\".txt\"");
        assert_eq!(quote_path("back\\slash.txt", raw), "\"back\\\\slash.txt\"");

        // 控制字符与 DEL 转三位八进制。
        assert_eq!(quote_path("ctrl\u{1}.txt", raw), "\"ctrl\\001.txt\"");
        assert_eq!(quote_path("del\u{7f}.txt", raw), "\"del\\177.txt\"");
        assert_eq!(quote_path("nl\u{7}.txt", raw), "\"nl\\a.txt\"");

        // `core.quotepath` 只影响高位字节。
        assert_eq!(
            quote_path("中文.txt", PorcelainFormat::GIT_DEFAULT),
            "\"\\344\\270\\255\\346\\226\\207.txt\""
        );
        assert_eq!(
            quote_path("café.txt", PorcelainFormat::GIT_DEFAULT),
            "\"caf\\303\\251.txt\""
        );
        assert_eq!(quote_path("café.txt", raw), "café.txt");
        assert_eq!(
            PorcelainFormat::from_quotepath(true),
            PorcelainFormat::GIT_DEFAULT
        );
    }

    #[test]
    fn rename_path_quotes_each_side_independently() {
        let raw = PorcelainFormat::RAW_NON_ASCII;

        let mut entry = StatusEntry::new("b c.txt", StatusFlag::Renamed, StatusFlag::Unmodified);
        entry.old_path = Some("a.txt".to_owned());
        // git 对两侧分别渲染，再以 " -> " 连接（实测：`R  a.txt -> "b c.txt"`）。
        assert_eq!(entry.porcelain(), "R  a.txt -> \"b c.txt\"");

        let mut both = StatusEntry::new("c d.txt", StatusFlag::Renamed, StatusFlag::Unmodified);
        both.old_path = Some("a b.txt".to_owned());
        assert_eq!(both.porcelain(), "R  \"a b.txt\" -> \"c d.txt\"");

        let mut plain = StatusEntry::new("b.txt", StatusFlag::Renamed, StatusFlag::Unmodified);
        plain.old_path = Some("a.txt".to_owned());
        assert_eq!(plain.porcelain(), "R  a.txt -> b.txt");
        assert_eq!(plain.porcelain_path_with(raw), "a.txt -> b.txt");
    }

    #[test]
    fn letters_match_porcelain_encoding() {
        let cases = [
            (StatusFlag::Unmodified, ' '),
            (StatusFlag::Modified, 'M'),
            (StatusFlag::Added, 'A'),
            (StatusFlag::Deleted, 'D'),
            (StatusFlag::Renamed, 'R'),
            (StatusFlag::Copied, 'C'),
            (StatusFlag::TypeChanged, 'T'),
            (StatusFlag::Untracked, '?'),
            (StatusFlag::Ignored, '!'),
        ];
        for (flag, letter) in cases {
            assert_eq!(flag.letter(), letter, "{flag:?} 的 porcelain 字母应一致");
        }
    }

    #[test]
    fn conflict_codes_round_trip() {
        for kind in [
            ConflictKind::BothDeleted,
            ConflictKind::AddedByUs,
            ConflictKind::DeletedByThem,
            ConflictKind::AddedByThem,
            ConflictKind::DeletedByUs,
            ConflictKind::BothAdded,
            ConflictKind::BothModified,
        ] {
            let (x, y) = kind.code();
            assert_eq!(ConflictKind::from_code(x, y), Some(kind));
        }
        assert_eq!(ConflictKind::from_code('M', 'M'), None);
    }

    #[test]
    fn porcelain_lines_match_git_output() {
        assert_eq!(
            StatusEntry::new("a.txt", StatusFlag::Modified, StatusFlag::Unmodified).porcelain(),
            "M  a.txt"
        );
        assert_eq!(
            StatusEntry::new("b.txt", StatusFlag::Unmodified, StatusFlag::Modified).porcelain(),
            " M b.txt"
        );
        assert_eq!(StatusEntry::untracked("u.txt").porcelain(), "?? u.txt");
        assert_eq!(StatusEntry::ignored("i.log").porcelain(), "!! i.log");
        assert_eq!(
            StatusEntry::conflicted("c.txt", ConflictKind::BothModified).porcelain(),
            "UU c.txt"
        );
        assert_eq!(
            StatusEntry::conflicted("c.txt", ConflictKind::BothAdded).porcelain(),
            "AA c.txt"
        );
    }

    #[test]
    fn rename_shows_arrow_form() {
        let mut entry = StatusEntry::new("b.rs", StatusFlag::Renamed, StatusFlag::Unmodified);
        entry.old_path = Some("a.rs".to_owned());
        assert_eq!(entry.porcelain(), "R  a.rs -> b.rs");
    }

    #[test]
    fn buckets_follow_precedence() {
        assert_eq!(
            StatusEntry::conflicted("c", ConflictKind::BothModified).bucket(),
            StatusBucket::Conflicted
        );
        assert_eq!(
            StatusEntry::untracked("u").bucket(),
            StatusBucket::Untracked
        );
        assert_eq!(
            StatusEntry::new("s", StatusFlag::Added, StatusFlag::Unmodified).bucket(),
            StatusBucket::Staged
        );
        assert_eq!(
            StatusEntry::new("m", StatusFlag::Unmodified, StatusFlag::Modified).bucket(),
            StatusBucket::Modified
        );
    }

    #[test]
    fn staged_and_unstaged_can_coexist() {
        let entry = StatusEntry::new("x", StatusFlag::Modified, StatusFlag::Modified);
        assert!(entry.is_staged());
        assert!(entry.has_unstaged());
        // 分区归属以"已暂存"优先（IDEA 亦将两侧都变的文件放在已暂存组）。
        assert_eq!(entry.bucket(), StatusBucket::Staged);
    }

    #[test]
    fn changeset_partitions_and_renders_sorted_porcelain() {
        let set = ChangeSet::new(vec![
            StatusEntry::new("z.txt", StatusFlag::Modified, StatusFlag::Unmodified),
            StatusEntry::untracked("a.txt"),
            StatusEntry::conflicted("m.txt", ConflictKind::BothModified),
        ]);
        assert!(!set.is_clean());
        assert_eq!(set.total(), 3);

        let parts = set.partition();
        assert_eq!(parts.conflicted.len(), 1);
        assert_eq!(parts.untracked.len(), 1);
        assert_eq!(parts.staged.len(), 1);
        assert!(parts.modified.is_empty());

        assert_eq!(set.to_porcelain(), "?? a.txt\nM  z.txt\nUU m.txt\n");
        assert!(ChangeSet::default().is_clean());
        assert_eq!(ChangeSet::default().to_porcelain(), "");
    }

    #[test]
    fn ignored_entries_are_not_staged() {
        let entry = StatusEntry::ignored("build/out.o");
        assert!(!entry.is_staged());
        assert!(!entry.has_unstaged());
        assert_eq!(entry.bucket(), StatusBucket::Ignored);
    }
}
