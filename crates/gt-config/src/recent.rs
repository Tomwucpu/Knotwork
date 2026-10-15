//! 最近打开的仓库列表（FR-MR-02）。
//!
//! 只负责"记什么、怎么排序、上限多少"，不做仓库有效性校验 ——
//! 路径是否仍是仓库由 `gt-backend` 的打开流程判定，本模块只在启动页
//! 需要时提供一次廉价的"路径是否还存在"过滤（[`RecentRepos::retain_existing`]）。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 最近仓库列表的长度上限。
///
/// 超限时丢弃最旧的条目：列表只是"最近用过"的便利设施，不是归档。
pub const MAX_RECENT_REPOS: usize = 20;

/// 一条最近仓库记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecentRepo {
    /// 仓库路径（打开时用户给出的路径，不强制规范化：规范化结果对用户可能不可读）。
    pub path: PathBuf,
    /// 用户给这个仓库起的别名；未设置时为 [`None`]，界面回退显示目录名。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// 最近一次打开的时间（Unix 秒）。
    pub opened_at: i64,
}

impl RecentRepo {
    /// 构造一条记录。
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, opened_at: i64) -> Self {
        Self {
            path: path.into(),
            alias: None,
            opened_at,
        }
    }

    /// 界面展示名：优先别名，其次目录名，最后退回完整路径。
    #[must_use]
    pub fn display_name(&self) -> String {
        if let Some(alias) = self.alias.as_deref().filter(|alias| !alias.is_empty()) {
            return alias.to_owned();
        }
        self.path.file_name().map_or_else(
            || self.path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        )
    }

    /// 路径当前是否仍然存在。
    #[must_use]
    pub fn exists(&self) -> bool {
        self.path.exists()
    }
}

/// 最近仓库列表（新的在前）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RecentRepos {
    /// 条目，按 `opened_at` 从新到旧。
    pub entries: Vec<RecentRepo>,
}

impl RecentRepos {
    /// 空列表。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 条目数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 记录一次打开：同路径的旧记录被移到最前并刷新时间，别名以新值覆盖。
    ///
    /// 路径比较用规范化结果（失败时退回原路径），以免同一仓库因
    /// `./repo`、`repo/`、大小写差异等写法重复占位。
    pub fn touch(&mut self, path: &Path, alias: Option<&str>, now: i64) {
        let key = normalize(path);
        self.entries.retain(|entry| normalize(&entry.path) != key);

        self.entries.insert(
            0,
            RecentRepo {
                path: path.to_path_buf(),
                alias: alias.map(str::to_owned),
                opened_at: now,
            },
        );
        self.entries.truncate(MAX_RECENT_REPOS);
    }

    /// 移除某条记录。
    pub fn remove(&mut self, path: &Path) {
        let key = normalize(path);
        self.entries.retain(|entry| normalize(&entry.path) != key);
    }

    /// 设置别名（把条目移到最前）。
    ///
    /// 路径不在列表中时是空操作 —— 别名只对已记录过的仓库有意义。
    pub fn set_alias(&mut self, path: &Path, alias: Option<&str>) {
        let key = normalize(path);
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| normalize(&entry.path) == key)
        {
            let mut entry = self.entries.remove(index);
            entry.alias = alias.map(str::to_owned);
            self.entries.insert(0, entry);
        }
    }

    /// 丢弃路径已不存在的条目，返回被丢弃的数量。
    ///
    /// 供启动页在展示前调用；`git` 仓库有效性由打开流程另行判定
    /// （FR-MR-02 验收：路径失效时给提示且不崩溃）。
    pub fn retain_existing(&mut self) -> usize {
        let before = self.entries.len();
        self.entries.retain(RecentRepo::exists);
        before - self.entries.len()
    }

    /// 按 `opened_at` 降序规范化顺序（对外部写入的配置做一次兜底排序）。
    pub fn sort_by_recency(&mut self) {
        self.entries.sort_by(|a, b| b.opened_at.cmp(&a.opened_at));
        self.entries.truncate(MAX_RECENT_REPOS);
    }
}

/// 规范化路径用于去重比较；失败时退回原路径。
fn normalize(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::{RecentRepo, RecentRepos, MAX_RECENT_REPOS};
    use std::path::{Path, PathBuf};

    fn repo(path: &str) -> PathBuf {
        PathBuf::from(path)
    }

    #[test]
    fn touch_moves_existing_entry_to_front_and_refreshes_time() {
        let mut list = RecentRepos::new();
        list.touch(&repo("/tmp/a"), None, 100);
        list.touch(&repo("/tmp/b"), None, 200);
        list.touch(&repo("/tmp/a"), Some("别名 A"), 300);

        assert_eq!(list.len(), 2, "同一路径不应重复占位");
        assert_eq!(list.entries[0].path, repo("/tmp/a"));
        assert_eq!(list.entries[0].opened_at, 300);
        assert_eq!(list.entries[0].alias.as_deref(), Some("别名 A"));
        assert_eq!(list.entries[1].path, repo("/tmp/b"));
    }

    #[test]
    fn list_is_capped_and_drops_the_oldest() {
        let total = i64::try_from(MAX_RECENT_REPOS).expect("上限可放入 i64") + 5;
        let mut list = RecentRepos::new();
        for index in 0..total {
            list.touch(&repo(&format!("/tmp/repo-{index}")), None, index);
        }
        assert_eq!(list.len(), MAX_RECENT_REPOS);
        // 最新的在最前，最旧的 5 条已被丢弃。
        assert_eq!(
            list.entries[0].path,
            repo(&format!("/tmp/repo-{}", total - 1))
        );
        assert!(list
            .entries
            .iter()
            .all(|entry| entry.path != repo("/tmp/repo-0")));
    }

    #[test]
    fn remove_and_alias_updates() {
        let mut list = RecentRepos::new();
        list.touch(&repo("/tmp/a"), None, 1);
        list.touch(&repo("/tmp/b"), None, 2);

        list.set_alias(&repo("/tmp/a"), Some("前端"));
        assert_eq!(list.entries[0].path, repo("/tmp/a"), "设别名应提到最前");
        assert_eq!(list.entries[0].display_name(), "前端");

        // 未记录过的路径设别名是空操作。
        list.set_alias(&repo("/tmp/unknown"), Some("x"));
        assert_eq!(list.len(), 2);

        list.remove(&repo("/tmp/a"));
        assert_eq!(list.len(), 1);
        assert_eq!(list.entries[0].path, repo("/tmp/b"));
    }

    #[test]
    fn display_name_falls_back_to_directory_name() {
        let plain = RecentRepo::new("/tmp/some-project", 0);
        assert_eq!(plain.display_name(), "some-project");

        let mut aliased = RecentRepo::new("/tmp/some-project", 0);
        aliased.alias = Some(String::new());
        assert_eq!(aliased.display_name(), "some-project", "空别名应被忽略");
    }

    #[test]
    fn retain_existing_drops_missing_paths() {
        let dir = std::env::temp_dir();
        let mut list = RecentRepos::new();
        list.touch(&dir, None, 2);
        list.touch(Path::new("/definitely/not/here/knotwork"), None, 1);

        assert_eq!(list.retain_existing(), 1);
        assert_eq!(list.len(), 1);
        assert_eq!(list.entries[0].path, dir);
    }

    #[test]
    fn sort_by_recency_handles_externally_edited_config() {
        let mut list = RecentRepos {
            entries: vec![
                RecentRepo::new("/tmp/old", 10),
                RecentRepo::new("/tmp/new", 30),
                RecentRepo::new("/tmp/mid", 20),
            ],
        };
        list.sort_by_recency();
        assert_eq!(
            list.entries.iter().map(|e| e.opened_at).collect::<Vec<_>>(),
            vec![30, 20, 10]
        );
    }
}
