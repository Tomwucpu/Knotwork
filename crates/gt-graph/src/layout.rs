//! 泳道布局的核心实现。
//!
//! 算法与 M0 POC-4 的单步分配一致（`pocs/poc-4-graph`），差别在于：
//! POC-4 追求离线计时，本实现追求**可流式追加**并把渲染所需信息显式化。

use std::collections::HashSet;

use gt_core::{LogNode, Oid};

/// 单行的泳道布局结果。
///
/// 渲染一条提交行的顺序为：
/// 1. 对本行的 [`lanes_before`](Self::lanes_before) ∩ [`lanes_after`](Self::lanes_after)
///    画纵向贯穿线；
/// 2. 在 [`node_lane`](Self::node_lane) 处画节点圆点；
/// 3. 对 [`parent_lanes`](Self::parent_lanes) 中每个泳道画一条自
///    [`node_lane`](Self::node_lane) 出发的转接曲线（同泳道时即直线延伸）。
///
/// 之所以同时给出 `lanes_before` 与 `lanes_after` 而非只给并集：本行可能是某条
/// 泳道的**起点**（刚分叉，纵向线只在下半段）或**终点**（刚汇合，纵向线只在上半段），
/// UI 需要据此绘制半段线。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RowLayout {
    /// 本提交占用的泳道下标。
    pub node_lane: u16,
    /// 处理本行**之前**处于等待状态的泳道（升序）。
    pub lanes_before: Vec<u16>,
    /// 处理本行**之后**处于等待状态的泳道（升序）。
    pub lanes_after: Vec<u16>,
    /// 本提交各父提交所在的泳道，顺序与 `node.parents` 一致。
    ///
    /// 第一父项恒为 [`node_lane`](Self::node_lane)（同一泳道向下延伸）；
    /// 其余父项为各自等待泳道（绘制斜向转接）。
    pub parent_lanes: Vec<u16>,
}

impl RowLayout {
    /// 本行是否为合并行（父多于一个）。
    #[must_use]
    pub fn is_merge(&self) -> bool {
        self.parent_lanes.len() > 1
    }

    /// 本行是否为分支的起点（有父项在本行之前不存在、需要新开泳道）。
    ///
    /// 判定依据：`lanes_after` 中存在不属于 `lanes_before` 的泳道。
    #[must_use]
    pub fn opens_lane(&self) -> bool {
        self.lanes_after
            .iter()
            .any(|lane| !self.lanes_before.contains(lane))
    }

    /// 本行是否为某条泳道的终点（有泳道在本行之后不再活跃）。
    #[must_use]
    pub fn closes_lane(&self) -> bool {
        self.lanes_before
            .iter()
            .any(|lane| !self.lanes_after.contains(lane))
    }
}

/// 泳道布局器：流式追加提交，逐行产出 [`RowLayout`]。
///
/// 使用方式（对应 `gt-service` 的分页加载）：
///
/// ```ignore
/// let mut layout = LaneLayout::new();
/// for page in pages {
///     layout.extend(&page.nodes.iter().map(CommitMeta::as_log_node).collect::<Vec<_>>());
/// }
/// ```
#[derive(Debug, Default)]
pub struct LaneLayout {
    /// 每行的布局结果，下标即行号。
    rows: Vec<RowLayout>,
    /// 泳道状态：`lanes[k] == Some(id)` 表示第 k 条泳道正在等待提交 `id` 落位。
    lanes: Vec<Option<Oid>>,
    /// 当前活跃泳道数量的上界（含历史上的空洞，用于 UI 预留宽度）。
    lane_count: u16,
    /// 过程峰值泳道数。
    max_lanes: u16,
    /// 是否开启拓扑序校验。
    validate_topology: bool,
    /// 已处理提交的集合，仅在校验开启时维护。
    seen: HashSet<Oid>,
    /// 拓扑序违规计数。
    violations: u32,
    /// 已处理的提交总数（用于违规信息展示）。
    processed: usize,
}

impl LaneLayout {
    /// 构造布局器，默认**关闭**拓扑序校验。
    ///
    /// 校验需要 `HashSet<Oid>`，百万级仓库下约 30MB 常驻；生产路径依赖
    /// 上游（`gt-backend` 的 revwalk）保证拓扑序，只在测试与断言构建中开启。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 构造开启拓扑序校验的布局器（测试与调试用）。
    #[must_use]
    pub fn with_validation() -> Self {
        Self {
            validate_topology: true,
            ..Self::default()
        }
    }

    /// 已产出的行数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// 是否尚未产出任何行。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// 已分配的泳道槽位上界（含已释放的空洞）。
    ///
    /// UI 可据此预留横向宽度；它只会单调增长，不会因泳道释放而回收。
    #[must_use]
    pub fn lane_count(&self) -> u16 {
        self.lane_count
    }

    /// 过程**峰值同时等待**的泳道数，即提交图的视觉宽度上界。
    #[must_use]
    pub fn max_lanes(&self) -> u16 {
        self.max_lanes
    }

    /// 已产出的全部行。
    #[must_use]
    pub fn rows(&self) -> &[RowLayout] {
        &self.rows
    }

    /// 取某一行。
    #[must_use]
    pub fn row(&self, index: usize) -> Option<&RowLayout> {
        self.rows.get(index)
    }

    /// 拓扑序违规次数。
    ///
    /// **非零即表示输入违反了本模块的契约**（子提交必须先于父提交），
    /// 此时的布局结果无意义。校验关闭时恒为 0。
    #[must_use]
    pub fn violations(&self) -> u32 {
        self.violations
    }

    /// 已处理的提交数。
    #[must_use]
    pub fn processed(&self) -> usize {
        self.processed
    }

    /// 清空全部状态，可复用实例处理新仓库（避免反复分配）。
    pub fn clear(&mut self) {
        self.rows.clear();
        self.lanes.clear();
        self.lane_count = 0;
        self.max_lanes = 0;
        self.seen.clear();
        self.violations = 0;
        self.processed = 0;
    }

    /// 追加一批提交，返回新增行的起始下标。
    ///
    /// 入参顺序必须是**拓扑序**（子在前、父在后），与 `git rev-list` 输出一致。
    pub fn extend(&mut self, nodes: &[LogNode]) -> usize {
        let start = self.rows.len();
        self.rows.reserve(nodes.len());
        for node in nodes {
            self.push(node);
        }
        start
    }

    /// 追加单个提交，返回该行的布局。
    pub fn push(&mut self, node: &LogNode) -> RowLayout {
        if self.validate_topology {
            self.check_topology(node);
        }
        self.processed += 1;

        let lanes_before = self.active_lanes();

        // 1) 汇合：所有正在等待本提交的泳道合并到最早一条，其余立即释放。
        let mut node_lane: Option<usize> = None;
        for index in 0..self.lanes.len() {
            if self.lanes[index] == Some(node.id) {
                match node_lane {
                    None => node_lane = Some(index),
                    Some(_) => self.lanes[index] = None,
                }
            }
        }

        // 2) 定位本提交占用的泳道：复用等待它的泳道，否则取首个空泳道 / 新开。
        let node_lane = node_lane.unwrap_or_else(|| self.alloc_lane());

        // 3) 推进泳道状态并记录父提交所在泳道。
        let mut parent_lanes = Vec::with_capacity(node.parents.len());
        match node.parents.split_first() {
            None => {
                // 根提交：泳道到此结束。
                self.lanes[node_lane] = None;
            }
            Some((first, rest)) => {
                self.lanes[node_lane] = Some(*first);
                parent_lanes.push(index_to_lane(node_lane));
                for parent in rest {
                    let lane = self.lane_waiting_for(*parent).unwrap_or_else(|| {
                        let lane = self.alloc_lane();
                        self.lanes[lane] = Some(*parent);
                        lane
                    });
                    parent_lanes.push(index_to_lane(lane));
                }
            }
        }

        self.lane_count = self.lane_count.max(self.lanes.len() as u16);
        let active = self.active_lanes();
        self.max_lanes = self.max_lanes.max(active.len() as u16);

        let row = RowLayout {
            node_lane: index_to_lane(node_lane),
            lanes_before,
            lanes_after: active,
            parent_lanes,
        };
        self.rows.push(row.clone());
        row
    }

    /// 记录拓扑序违规（子提交尚未出现，其父提交却已被处理过）。
    fn check_topology(&mut self, node: &LogNode) {
        if node.parents.iter().any(|parent| self.seen.contains(parent)) {
            self.violations += 1;
        }
        self.seen.insert(node.id);
    }

    /// 当前处于等待状态的泳道（升序）。
    fn active_lanes(&self) -> Vec<u16> {
        self.lanes
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.is_some())
            .map(|(index, _)| index_to_lane(index))
            .collect()
    }

    /// 查找正在等待指定提交的泳道。
    fn lane_waiting_for(&self, id: Oid) -> Option<usize> {
        self.lanes.iter().position(|slot| *slot == Some(id))
    }

    /// 分配泳道：优先复用空洞，否则追加新泳道。
    fn alloc_lane(&mut self) -> usize {
        if let Some(index) = self.lanes.iter().position(Option::is_none) {
            return index;
        }
        self.lanes.push(None);
        self.lanes.len() - 1
    }
}

/// 泳道下标转 `u16`；泳道数超过 `u16::MAX` 属于病态输入（拓扑序已被破坏），
/// 此处饱和处理以免在渲染层造成越界。
fn index_to_lane(index: usize) -> u16 {
    u16::try_from(index).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::{LaneLayout, RowLayout};
    use gt_core::{LogNode, Oid};

    fn oid(seed: u32) -> Oid {
        let mut bytes = [0u8; gt_core::OID_LEN];
        bytes[..4].copy_from_slice(&seed.to_be_bytes());
        Oid::from_bytes(&bytes).expect("长度正确")
    }

    fn node(seed: u32, parents: &[u32]) -> LogNode {
        LogNode {
            id: oid(seed),
            parents: parents.iter().copied().map(oid).collect(),
        }
    }

    /// 线性历史：泳道应恒为 0，且不产生分支起点 / 终点。
    #[test]
    fn linear_history_stays_in_lane_zero() {
        let nodes: Vec<LogNode> = (0..5).map(|i| node(i, &[i + 1])).collect();
        let mut layout = LaneLayout::new();
        layout.extend(&nodes);

        assert_eq!(layout.len(), 5);
        assert_eq!(layout.max_lanes(), 1);
        for row in layout.rows() {
            assert_eq!(row.node_lane, 0);
            assert_eq!(row.parent_lanes, vec![0]);
            assert!(!row.is_merge());
        }
    }

    /// 根提交应释放泳道，后续行不再有活跃泳道。
    #[test]
    fn root_commit_releases_lane() {
        let mut layout = LaneLayout::new();
        let row = layout.push(&node(0, &[]));
        assert_eq!(row.parent_lanes, Vec::<u16>::new());
        assert!(row.lanes_after.is_empty());
        assert_eq!(layout.lane_count(), 1, "泳道虽释放但上界保留");
    }

    /// 合并提交：第二父项应落到另一条泳道，并产生转接曲线。
    #[test]
    fn merge_commit_opens_second_lane() {
        let mut layout = LaneLayout::new();
        let row = layout.push(&node(0, &[1, 2]));
        assert!(row.is_merge());
        assert_eq!(row.node_lane, 0);
        assert_eq!(row.parent_lanes, vec![0, 1]);
        assert!(row.opens_lane());
        assert_eq!(row.lanes_after, vec![0, 1]);
    }

    /// 汇合点必须把两条等待泳道合并为一条 —— 这是 M0 修复的泳道泄漏缺陷。
    #[test]
    fn merge_point_converges_lanes_without_leaking() {
        let mut layout = LaneLayout::new();

        // 0 是合并提交：父 = 主干 2 与侧枝尖端 1 → 泳道 0 等 2、泳道 1 等 1。
        let row = layout.push(&node(0, &[2, 1]));
        assert!(row.is_merge());
        assert_eq!(row.parent_lanes, vec![0, 1]);
        assert_eq!(row.lanes_after, vec![0, 1]);

        // 侧枝尖端 1：在泳道 1 落位，随后该泳道也改为等待 2。
        let row = layout.push(&node(1, &[2]));
        assert_eq!(row.node_lane, 1, "只有泳道 1 在等待提交 1");
        assert_eq!(row.lanes_after, vec![0, 1], "两条泳道现在都等待提交 2");
        assert!(!row.closes_lane());

        // 消费 2：两条泳道同时等待它，必须汇合到最早一条并释放另一条。
        let row = layout.push(&node(2, &[3]));
        assert_eq!(row.node_lane, 0, "汇合到最早一条泳道");
        assert_eq!(
            row.lanes_after,
            vec![0],
            "另一条等待同一提交的泳道必须立即释放"
        );
        assert!(row.closes_lane());
        assert_eq!(layout.max_lanes(), 2, "峰值同时等待泳道数为 2");
    }

    /// 反复合并不应让泳道数增长（回归 M0 的泳道泄漏缺陷）。
    #[test]
    fn repeated_merges_keep_lane_count_bounded() {
        let mut layout = LaneLayout::new();
        // 构造：每轮一个合并提交，汇合点固定指向下一条主干提交。
        let mut nodes = Vec::new();
        for i in 0..50u32 {
            let main = i * 3;
            // 合并提交：父 = 主干下一条（main+2）与侧枝尖端（main+1）
            nodes.push(node(main, &[main + 2, main + 1]));
            nodes.push(node(main + 1, &[main + 2]));
            nodes.push(node(main + 2, &[main + 3]));
        }
        nodes.push(node(1000, &[]));

        for n in &nodes {
            layout.push(n);
        }
        assert_eq!(layout.max_lanes(), 2, "泳道数必须恒定，不得随合并数增长");
        assert_eq!(layout.violations(), 0);
    }

    /// 校验开启时，逆序（非拓扑序）输入必须被检出。
    #[test]
    fn reversed_input_is_flagged_when_validating() {
        // 拓扑序：0 -> 1 -> 2（0 是最新）
        let topological = vec![node(0, &[1]), node(1, &[2]), node(2, &[])];
        let mut good = LaneLayout::with_validation();
        good.extend(&topological);
        assert_eq!(good.violations(), 0);
        assert_eq!(good.max_lanes(), 1);

        // 逆序：父提交先出现，违反契约。
        let reversed = vec![node(2, &[]), node(1, &[2]), node(0, &[1])];
        let mut bad = LaneLayout::with_validation();
        bad.extend(&reversed);
        assert!(bad.violations() >= 2, "逆序输入应被检出多次违规");
    }

    /// 校验默认关闭，生产路径不承担 `HashSet` 开销。
    #[test]
    fn validation_is_opt_in() {
        let reversed = vec![node(2, &[]), node(1, &[2])];
        let mut layout = LaneLayout::new();
        layout.extend(&reversed);
        assert_eq!(layout.violations(), 0);
        assert_eq!(layout.processed(), 2);
    }

    /// 复现 M0 报告 6 节的病态退化：逆序输入下泳道数线性膨胀。
    #[test]
    fn reversed_input_explodes_lane_count_without_validation() {
        let count = 200u32;
        let reversed: Vec<LogNode> = (0..count).rev().map(|i| node(i, &[i + 1])).collect();
        let mut layout = LaneLayout::new();
        layout.extend(&reversed);
        assert!(
            layout.max_lanes() > count as u16 / 2,
            "非拓扑序输入下泳道数应显著膨胀（实测峰值 {}）",
            layout.max_lanes()
        );
    }

    /// `extend` 返回新增行的起始下标，供分页增量拼接使用。
    #[test]
    fn extend_reports_start_index() {
        let mut layout = LaneLayout::new();
        assert_eq!(layout.extend(&[node(0, &[1])]), 0);
        assert_eq!(layout.extend(&[node(1, &[2])]), 1);
        assert_eq!(layout.len(), 2);
        assert_eq!(layout.row(1).map(|r| r.node_lane), Some(0));
        assert_eq!(layout.row(2), None);
    }

    /// `clear` 后实例可复用。
    #[test]
    fn clear_resets_all_state() {
        let mut layout = LaneLayout::new();
        layout.extend(&[node(0, &[1, 2])]);
        layout.clear();
        assert!(layout.is_empty());
        assert_eq!(layout.lane_count(), 0);
        assert_eq!(layout.max_lanes(), 0);
        assert_eq!(layout.processed(), 0);
    }

    /// 空洞复用：泳道释放后应被后续分叉复用，而不是无限追加。
    #[test]
    fn freed_lanes_are_reused() {
        let mut layout = LaneLayout::new();

        // 泳道 0 等 1，泳道 1 等 2。
        layout.push(&node(0, &[1, 2]));
        assert_eq!(layout.lane_count(), 2);

        // 消费 2 → 泳道 1 改等 10；再消费 10（根提交）→ 泳道 1 释放。
        layout.push(&node(2, &[10]));
        let row = layout.push(&node(10, &[]));
        assert_eq!(row.node_lane, 1);
        assert!(!row.lanes_after.contains(&1), "泳道 1 应已释放");
        assert_eq!(layout.lane_count(), 2);

        // 新的合并应复用释放出的泳道 1，而不是追加泳道 2。
        let row = layout.push(&node(1, &[20, 30]));
        assert_eq!(row.node_lane, 0);
        assert_eq!(row.parent_lanes, vec![0, 1], "应复用空洞泳道 1");
        assert_eq!(layout.lane_count(), 2, "不得新增泳道");
    }

    /// 行布局的辅助判定。
    #[test]
    fn row_flags_describe_lane_events() {
        let row = RowLayout {
            node_lane: 0,
            lanes_before: vec![0],
            lanes_after: vec![0, 1],
            parent_lanes: vec![0, 1],
        };
        assert!(row.is_merge());
        assert!(row.opens_lane());
        assert!(!row.closes_lane());

        let row = RowLayout {
            node_lane: 0,
            lanes_before: vec![0, 1],
            lanes_after: vec![0],
            parent_lanes: vec![0],
        };
        assert!(!row.is_merge());
        assert!(!row.opens_lane());
        assert!(row.closes_lane());
    }
}
