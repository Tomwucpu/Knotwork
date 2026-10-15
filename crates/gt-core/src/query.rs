//! Log 视图的筛选 / 排序条件与分页模型。

use crate::commit::CommitMeta;

/// 分页默认页大小。
///
/// 架构规划 5.2 建议每批 500–2000；取 500 使首屏延迟最小，
/// 同时保证后续分页的往返次数可接受（百万级仓库 2000 次分页）。
pub const DEFAULT_PAGE_SIZE: usize = 500;

/// 分页大小的允许区间。
pub const MIN_PAGE_SIZE: usize = 64;
/// 分页大小的允许区间。
pub const MAX_PAGE_SIZE: usize = 4096;

/// 分支筛选范围（D2 决策，2026-10-15 已确认）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum BranchScope {
    /// 默认：本地分支 + 当前跟踪分支。
    ///
    /// 大仓库下 revwalk 规模可控，是本产品的默认口径（FR-LOG-02）。
    #[default]
    LocalAndUpstream,
    /// 显式选项：全部本地与远程分支。
    ///
    /// 由用户主动勾选，自行承担 revwalk 规模成本。
    All,
}

impl BranchScope {
    /// 中文标签，供筛选器直接展示。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::LocalAndUpstream => "本地分支 + 当前跟踪分支",
            Self::All => "全部本地与远程分支",
        }
    }
}

/// 提交排序方式（FR-LOG-05）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum LogSort {
    /// 按提交日期降序（等价 `git log` 默认）。
    #[default]
    Date,
    /// 拓扑排序（IntelliSort：入边提交紧贴合并提交下方）。
    Topological,
}

impl LogSort {
    /// 中文标签。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Date => "按提交日期",
            Self::Topological => "拓扑排序",
        }
    }
}

/// Log 视图的筛选条件。
///
/// 条件的任何变更都构成一次**新的请求代次**，旧代次结果必须作废
/// （架构规划 4.3）——因此本结构实现 `PartialEq` 以便代次判定复用。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogFilter {
    /// 分支筛选范围。
    pub scope: BranchScope,
    /// 作者筛选（子串匹配，大小写不敏感由后端决定）。
    pub author: Option<String>,
    /// 起始时间（Unix 秒，含）。
    pub since: Option<i64>,
    /// 结束时间（Unix 秒，含）。
    pub until: Option<i64>,
    /// 路径筛选（相对仓库根的路径或其前缀）。
    pub path: Option<String>,
    /// 提交信息关键字或正则（FR-LOG-03）。
    pub grep: Option<String>,
}

impl LogFilter {
    /// 是否为空条件（等价无条件 `git log`）。
    #[must_use]
    pub fn is_unfiltered(&self) -> bool {
        self.author.is_none()
            && self.since.is_none()
            && self.until.is_none()
            && self.path.is_none()
            && self.grep.is_none()
    }

    /// 用当前条件与页大小构造一次分页请求。
    #[must_use]
    pub fn page(&self, offset: usize, limit: usize) -> LogPageRequest {
        LogPageRequest {
            filter: self.clone(),
            offset,
            limit: limit.clamp(MIN_PAGE_SIZE, MAX_PAGE_SIZE),
            sort: LogSort::default(),
        }
    }
}

/// 一次分页请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogPageRequest {
    /// 筛选条件。
    pub filter: LogFilter,
    /// 跳过的提交数。
    pub offset: usize,
    /// 期望返回的提交数（会被裁剪到 `MIN..=MAX`）。
    pub limit: usize,
    /// 排序方式。
    pub sort: LogSort,
}

impl Default for LogPageRequest {
    /// 默认语义是"从第 0 条开始要一页默认大小"——不能靠 derive，
    /// 因为 `usize::default()` 是 0，会让默认请求要不到任何提交。
    fn default() -> Self {
        Self {
            filter: LogFilter::default(),
            offset: 0,
            limit: DEFAULT_PAGE_SIZE,
            sort: LogSort::default(),
        }
    }
}

/// 一页提交数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogNodePage {
    /// 本页提交，顺序即展示顺序（父提交在后）。
    pub nodes: Vec<CommitMeta>,
    /// 本页在完整结果中的起始下标。
    pub offset: usize,
    /// 是否已到达历史末尾。
    pub exhausted: bool,
}

impl LogNodePage {
    /// 空页。
    #[must_use]
    pub const fn empty(offset: usize) -> Self {
        Self {
            nodes: Vec::new(),
            offset,
            exhausted: true,
        }
    }

    /// 本页条目数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// 本页是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// 下一页的起始下标（即本页末尾之后）。
    #[must_use]
    pub fn next_offset(&self) -> usize {
        self.offset + self.nodes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BranchScope, LogFilter, LogNodePage, LogPageRequest, LogSort, DEFAULT_PAGE_SIZE,
        MAX_PAGE_SIZE, MIN_PAGE_SIZE,
    };

    #[test]
    fn default_scope_is_local_and_upstream() {
        // D2 决策口径：默认不是全部分支。
        assert_eq!(BranchScope::default(), BranchScope::LocalAndUpstream);
        assert_eq!(LogSort::default(), LogSort::Date);
        assert!(LogFilter::default().is_unfiltered());
    }

    #[test]
    fn page_size_is_clamped() {
        let request: LogPageRequest = LogFilter::default().page(0, 10);
        assert_eq!(request.limit, MIN_PAGE_SIZE);
        let request = LogFilter::default().page(0, usize::MAX);
        assert_eq!(request.limit, MAX_PAGE_SIZE);
        let request = LogFilter::default().page(0, DEFAULT_PAGE_SIZE);
        assert_eq!(request.limit, DEFAULT_PAGE_SIZE);
        assert_eq!(request.offset, 0);
    }

    #[test]
    fn filter_change_breaks_equality() {
        let base = LogFilter::default();
        let mut changed = base.clone();
        changed.author = Some("alice".to_owned());
        assert_ne!(base, changed, "筛选条件变化必须触发新请求代次");
        assert!(!changed.is_unfiltered());
    }

    #[test]
    fn page_tracks_offsets_and_exhaustion() {
        let empty = LogNodePage::empty(500);
        assert!(empty.is_empty());
        assert!(empty.exhausted);
        assert_eq!(empty.next_offset(), 500);
        assert_eq!(empty.len(), 0);
    }
}
