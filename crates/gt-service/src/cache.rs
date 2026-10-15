//! 硬上限 LRU 缓存（架构规划 5.4）。
//!
//! 全局统一预算，分三段配额：提交图节点 / diff hunk / blob 文本。
//! **超限按 LRU 淘汰，绝不自动扩容** —— 这是"打开大仓库不会把内存吃光"的
//! 最后一道防线，因此预算不是建议值而是硬上限。
//!
//! # 键的要求
//!
//! [`LruCache`] 同时要求 `K: Hash + Ord + Clone`：`HashMap` 负责取值，
//! `BTreeMap<访问序号, K>` 负责 O(log n) 找到最久未用的一项。
//! 本项目实际使用的键（路径、`Oid`、偏移量、它们的元组）都满足这个约束。

use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;

use gt_core::RepoId;

/// 默认总预算，单位 MiB。
pub const DEFAULT_TOTAL_MIB: u32 = 32;

/// 三段配额在总预算中的占比（百分比，合计 100）。
pub const COMMIT_GRAPH_PERCENT: u32 = 40;
/// `gt-diff` 的 hunk 缓存占比。
pub const DIFF_HUNKS_PERCENT: u32 = 35;
/// blob 文本缓存占比（余数归此段，避免整数除法丢字节）。
pub const BLOB_TEXT_PERCENT: u32 = 25;

/// 缓存分段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CacheSegment {
    /// 提交图节点与泳道布局（`gt-graph`）。
    CommitGraph,
    /// 差异 hunk（`gt-diff`）。
    DiffHunks,
    /// blob 文本（文件内容、行文本）。
    BlobText,
}

impl CacheSegment {
    /// 全部分段（按配额顺序）。
    pub const ALL: [Self; 3] = [Self::CommitGraph, Self::DiffHunks, Self::BlobText];

    /// 占比（百分比）。
    #[must_use]
    pub const fn percent(self) -> u32 {
        match self {
            Self::CommitGraph => COMMIT_GRAPH_PERCENT,
            Self::DiffHunks => DIFF_HUNKS_PERCENT,
            Self::BlobText => BLOB_TEXT_PERCENT,
        }
    }

    /// 中文标签，供设置界面展示。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::CommitGraph => "提交图",
            Self::DiffHunks => "差异块",
            Self::BlobText => "文件文本",
        }
    }
}

/// 缓存预算：总预算与各段配额。
///
/// 各段配额之和**恰好等于**总预算（余数归 [`CacheSegment::BlobText`]），
/// 因此不存在"配额加起来比预算大"的隐式超支。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheBudget {
    total_bytes: usize,
    segments: [usize; 3],
}

impl Default for CacheBudget {
    fn default() -> Self {
        Self::from_mib(DEFAULT_TOTAL_MIB)
    }
}

impl CacheBudget {
    /// 由总预算（MiB）构造。
    #[must_use]
    pub fn from_mib(mib: u32) -> Self {
        Self::from_bytes(mib as usize * 1024 * 1024)
    }

    /// 由总字节数构造。
    #[must_use]
    pub fn from_bytes(total_bytes: usize) -> Self {
        let graph = total_bytes * COMMIT_GRAPH_PERCENT as usize / 100;
        let diff = total_bytes * DIFF_HUNKS_PERCENT as usize / 100;
        // 余数归 blob 段：这样三段之和严格等于总预算。
        let blob = total_bytes.saturating_sub(graph + diff);
        Self {
            total_bytes,
            segments: [graph, diff, blob],
        }
    }

    /// 总预算（字节）。
    #[must_use]
    pub const fn total_bytes(self) -> usize {
        self.total_bytes
    }

    /// 总预算（MiB，向下取整）。
    #[must_use]
    pub const fn total_mib(self) -> u32 {
        (self.total_bytes / (1024 * 1024)) as u32
    }

    /// 某一段的配额（字节）。
    #[must_use]
    pub const fn segment_bytes(self, segment: CacheSegment) -> usize {
        self.segments[segment as usize]
    }
}

/// 一段缓存的运行统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// 当前条目数。
    pub entries: usize,
    /// 当前占用字节。
    pub used_bytes: usize,
    /// 预算字节。
    pub budget_bytes: usize,
    /// 累计命中次数。
    pub hits: u64,
    /// 累计未命中次数。
    pub misses: u64,
    /// 累计淘汰条目数。
    pub evictions: u64,
    /// 因单条超预算而被拒绝的插入次数。
    pub rejected: u64,
}

impl CacheStats {
    /// 占用率（0.0–1.0）。
    ///
    /// 供设置界面与诊断面板展示；精度差异在这个量级上无关紧要。
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn utilization(&self) -> f64 {
        if self.budget_bytes == 0 {
            return 0.0;
        }
        self.used_bytes as f64 / self.budget_bytes as f64
    }

    /// 命中率（无访问时返回 0.0）。
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn hit_ratio(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            return 0.0;
        }
        self.hits as f64 / total as f64
    }
}

/// 插入结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InsertOutcome<V> {
    /// 已插入。
    Inserted {
        /// 同键被覆盖的旧值。
        replaced: Option<V>,
        /// 为腾出空间而淘汰的条目数。
        evicted: usize,
    },
    /// 单条代价超过整段预算，未插入；值被原样退回（调用方仍可直接使用）。
    TooLarge(V),
}

/// 按字节计量的 LRU 缓存。
#[derive(Debug)]
pub struct LruCache<K, V> {
    budget: usize,
    used: usize,
    tick: u64,
    entries: HashMap<K, Slot<V>>,
    /// 访问序号 → 键；首个即最久未用。
    order: BTreeMap<u64, K>,
    hits: u64,
    misses: u64,
    evictions: u64,
    rejected: u64,
}

#[derive(Debug)]
struct Slot<V> {
    value: V,
    cost: usize,
    tick: u64,
}

impl<K, V> LruCache<K, V>
where
    K: Eq + Hash + Ord + Clone,
{
    /// 新建一段缓存。`budget_bytes` 为 0 时该段等效关闭（所有插入被拒）。
    #[must_use]
    pub fn new(budget_bytes: usize) -> Self {
        Self {
            budget: budget_bytes,
            used: 0,
            tick: 0,
            entries: HashMap::new(),
            order: BTreeMap::new(),
            hits: 0,
            misses: 0,
            evictions: 0,
            rejected: 0,
        }
    }

    /// 预算（字节）。
    #[must_use]
    pub const fn budget(&self) -> usize {
        self.budget
    }

    /// 当前占用（字节）。
    #[must_use]
    pub const fn used_bytes(&self) -> usize {
        self.used
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

    /// 查看而不更新访问新鲜度。
    #[must_use]
    pub fn peek(&self, key: &K) -> Option<&V> {
        self.entries.get(key).map(|slot| &slot.value)
    }

    /// 取值并更新访问新鲜度。
    pub fn get(&mut self, key: &K) -> Option<&V> {
        let Some(tick) = self.entries.get(key).map(|slot| slot.tick) else {
            self.misses += 1;
            return None;
        };
        self.hits += 1;
        self.touch(key, tick);
        self.entries.get(key).map(|slot| &slot.value)
    }

    /// 是否包含某键（不更新新鲜度）。
    #[must_use]
    pub fn contains_key(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }

    /// 插入一条，必要时按 LRU 淘汰。
    pub fn insert(&mut self, key: K, value: V, cost: usize) -> InsertOutcome<V> {
        if cost > self.budget {
            self.rejected += 1;
            return InsertOutcome::TooLarge(value);
        }

        let replaced = self.remove(&key);

        let mut evicted = 0usize;
        while self.used + cost > self.budget {
            if !self.evict_oldest() {
                // 预算被清空后仍放不下 —— 只可能是 cost > budget，已经在上面挡掉了。
                break;
            }
            evicted += 1;
        }

        self.tick += 1;
        self.used += cost;
        self.order.insert(self.tick, key.clone());
        self.entries.insert(
            key,
            Slot {
                value,
                cost,
                tick: self.tick,
            },
        );

        InsertOutcome::Inserted { replaced, evicted }
    }

    /// 移除一条。
    pub fn remove(&mut self, key: &K) -> Option<V> {
        let slot = self.entries.remove(key)?;
        self.order.remove(&slot.tick);
        self.used = self.used.saturating_sub(slot.cost);
        Some(slot.value)
    }

    /// 清空。
    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.used = 0;
    }

    /// 调整预算；缩小时立即按 LRU 淘汰超出部分。
    ///
    /// 返回被淘汰的条目数。
    pub fn set_budget(&mut self, budget_bytes: usize) -> usize {
        self.budget = budget_bytes;
        let mut evicted = 0usize;
        while self.used > self.budget && self.evict_oldest() {
            evicted += 1;
        }
        evicted
    }

    /// 运行统计。
    #[must_use]
    pub fn stats(&self) -> CacheStats {
        CacheStats {
            entries: self.entries.len(),
            used_bytes: self.used,
            budget_bytes: self.budget,
            hits: self.hits,
            misses: self.misses,
            evictions: self.evictions,
            rejected: self.rejected,
        }
    }

    /// 把某键标记为最近使用。
    fn touch(&mut self, key: &K, old_tick: u64) {
        self.order.remove(&old_tick);
        self.tick += 1;
        self.order.insert(self.tick, key.clone());
        if let Some(slot) = self.entries.get_mut(key) {
            slot.tick = self.tick;
        }
    }

    /// 淘汰最久未用的一条；缓存为空时返回 `false`。
    fn evict_oldest(&mut self) -> bool {
        let Some((&tick, key)) = self.order.iter().next().map(|(t, k)| (t, k.clone())) else {
            return false;
        };
        self.order.remove(&tick);
        if let Some(slot) = self.entries.remove(&key) {
            self.used = self.used.saturating_sub(slot.cost);
            self.evictions += 1;
            return true;
        }
        // 顺序表与条目表不一致（不应发生）：继续尝试下一条而不是死循环。
        self.evict_oldest()
    }
}

/// 某仓库某类数据的缓存键前缀片段。
///
/// 用 `(RepoId, 序号)` 组合成键，避免不同仓库的同名路径互相污染。
#[must_use]
pub fn repo_key<Id>(repo: RepoId, id: Id) -> (RepoId, Id) {
    (repo, id)
}

#[cfg(test)]
mod tests {
    use super::{CacheBudget, CacheSegment, InsertOutcome, LruCache};
    use gt_core::RepoId;

    fn cache(budget: usize) -> LruCache<String, String> {
        LruCache::new(budget)
    }

    #[test]
    fn budget_segments_sum_to_the_total() {
        for mib in [1u32, 8, 32, 100, 1024] {
            let budget = CacheBudget::from_mib(mib);
            let sum: usize = CacheSegment::ALL
                .iter()
                .map(|segment| budget.segment_bytes(*segment))
                .sum();
            assert_eq!(sum, budget.total_bytes(), "各段配额之和必须等于总预算");
            assert_eq!(budget.total_mib(), mib);
        }

        assert_eq!(CacheBudget::default(), CacheBudget::from_mib(32));
        // 0 预算不应 panic，各段也都是 0。
        let zero = CacheBudget::from_bytes(0);
        assert_eq!(zero.total_bytes(), 0);
        assert!(CacheSegment::ALL
            .iter()
            .all(|segment| zero.segment_bytes(*segment) == 0));
    }

    #[test]
    fn segment_labels_and_percents_are_stable() {
        assert_eq!(CacheSegment::CommitGraph.percent(), 40);
        assert_eq!(CacheSegment::DiffHunks.percent(), 35);
        assert_eq!(CacheSegment::BlobText.percent(), 25);
        assert_eq!(
            CacheSegment::ALL.iter().map(|s| s.percent()).sum::<u32>(),
            100
        );
        assert_eq!(CacheSegment::BlobText.label(), "文件文本");
    }

    #[test]
    fn insert_replaces_same_key_without_double_counting() {
        let mut cache = cache(100);
        let first = cache.insert("a".to_owned(), "v1".to_owned(), 30);
        assert_eq!(
            first,
            InsertOutcome::Inserted {
                replaced: None,
                evicted: 0
            }
        );
        assert_eq!(cache.used_bytes(), 30);

        let second = cache.insert("a".to_owned(), "v2".to_owned(), 40);
        assert_eq!(
            second,
            InsertOutcome::Inserted {
                replaced: Some("v1".to_owned()),
                evicted: 0
            }
        );
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.used_bytes(), 40, "覆盖不应累计占用");
        assert_eq!(cache.peek(&"a".to_owned()).map(String::as_str), Some("v2"));
    }

    #[test]
    fn eviction_follows_least_recently_used_order() {
        let mut cache = cache(100);
        cache.insert("a".to_owned(), "1".to_owned(), 30);
        cache.insert("b".to_owned(), "2".to_owned(), 30);
        cache.insert("c".to_owned(), "3".to_owned(), 30);

        // 访问 a，使它变成最新；b 成为最久未用。
        assert_eq!(cache.get(&"a".to_owned()).map(String::as_str), Some("1"));

        // 再插一条 30 字节：必须淘汰 b。
        let outcome = cache.insert("d".to_owned(), "4".to_owned(), 30);
        assert!(matches!(
            outcome,
            InsertOutcome::Inserted { evicted: 1, .. }
        ));
        assert!(!cache.contains_key(&"b".to_owned()), "b 应被淘汰");
        assert!(cache.contains_key(&"a".to_owned()), "刚被访问过的 a 应保留");
        assert!(cache.contains_key(&"c".to_owned()));
        assert!(cache.contains_key(&"d".to_owned()));
        assert!(cache.used_bytes() <= cache.budget(), "占用不得超过预算");
    }

    #[test]
    fn oversized_single_entry_is_rejected_and_returned() {
        let mut cache = cache(100);
        let outcome = cache.insert("huge".to_owned(), "x".to_owned(), 101);
        assert_eq!(outcome, InsertOutcome::TooLarge("x".to_owned()));
        assert!(cache.is_empty());
        assert_eq!(cache.used_bytes(), 0);
        assert_eq!(cache.stats().rejected, 1);
    }

    #[test]
    fn zero_budget_rejects_everything() {
        let mut cache = cache(0);
        assert_eq!(
            cache.insert("a".to_owned(), "v".to_owned(), 1),
            InsertOutcome::TooLarge("v".to_owned())
        );
        assert!(cache.is_empty());
    }

    #[test]
    fn shrinking_the_budget_evicts_immediately() {
        let mut cache = cache(300);
        for key in ["a", "b", "c"] {
            cache.insert(key.to_owned(), key.to_owned(), 100);
        }
        assert_eq!(cache.used_bytes(), 300);

        let evicted = cache.set_budget(150);
        assert_eq!(evicted, 2);
        assert_eq!(cache.used_bytes(), 100);
        assert_eq!(cache.len(), 1);
        // 剩下的应当是最近使用的那条。
        assert!(cache.contains_key(&"c".to_owned()));
    }

    #[test]
    fn remove_and_clear_free_budget() {
        let mut cache = cache(200);
        cache.insert("a".to_owned(), "1".to_owned(), 50);
        cache.insert("b".to_owned(), "2".to_owned(), 50);

        assert_eq!(cache.remove(&"a".to_owned()), Some("1".to_owned()));
        assert_eq!(cache.used_bytes(), 50);
        assert_eq!(cache.remove(&"a".to_owned()), None);

        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.used_bytes(), 0);
    }

    #[test]
    #[allow(clippy::float_cmp)] // 这里比较的是"提前返回的字面量 0.0"，精确可比
    fn stats_track_hits_misses_and_utilization() {
        let mut cache = cache(100);
        cache.insert("a".to_owned(), "1".to_owned(), 50);

        assert!(cache.get(&"a".to_owned()).is_some());
        assert!(cache.get(&"missing".to_owned()).is_none());

        let stats = cache.stats();
        assert_eq!(stats.entries, 1);
        assert_eq!(stats.used_bytes, 50);
        assert_eq!(stats.budget_bytes, 100);
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
        assert!((stats.hit_ratio() - 0.5).abs() < f64::EPSILON);
        assert!((stats.utilization() - 0.5).abs() < f64::EPSILON);

        assert_eq!(super::CacheStats::default().hit_ratio(), 0.0);
        assert_eq!(super::CacheStats::default().utilization(), 0.0);
    }

    #[test]
    fn iterating_many_inserts_never_exceeds_the_budget() {
        // 回归防线：不论插入顺序与访问模式如何，占用都必须守住硬上限。
        let mut cache = cache(1024);
        for index in 0..500u32 {
            let key = format!("k{index}");
            assert!(cache.get(&key).is_none_or(|value| value == key.as_str()));
            cache.insert(key.clone(), key, 64);
            assert!(
                cache.used_bytes() <= cache.budget(),
                "第 {index} 次插入后占用 {} 超过预算",
                cache.used_bytes()
            );
        }
        assert!(cache.len() <= 1024 / 64);
        assert!(cache.stats().evictions > 0);
    }

    #[test]
    fn repo_key_composes_without_collision() {
        let first = super::repo_key(RepoId::from_raw(1), "src/main.rs");
        let second = super::repo_key(RepoId::from_raw(2), "src/main.rs");
        assert_ne!(first, second, "不同仓库的同名路径不得共享缓存槽");

        let mut cache: LruCache<(RepoId, &str), u32> = LruCache::new(1024);
        cache.insert(first, 1, 4);
        cache.insert(second, 2, 4);
        assert_eq!(cache.get(&first), Some(&1));
        assert_eq!(cache.get(&second), Some(&2));
    }
}
