//! 视图模型：把领域模型转换成 Slint 可直接渲染的行数据结构。
//!
//! # 为什么不把领域模型直接塞给 Slint
//!
//! 三条原因：
//!
//! 1. **渲染需要的信息比领域模型多**（泳道颜色、行号槽、分段文本），
//!    而这些都要在 UI 线程上算；算在 Rust 侧才能被单测覆盖；
//! 2. **Slint 的 `ModelRc` 要求 `'static` + `Clone`**，借用式领域模型无法直接传递；
//! 3. **词级高亮不做文本测量**：把行拆成"分段文本 + 是否变更"的平行数组，
//!    界面只做逐段着色，省掉每帧的排版计算（架构规划 5.7）。
//!
//! 本模块**只做纯转换**（输入领域模型 → 输出行结构），不接触 I/O 与线程，
//! 因此可以脱离 Slint 运行时直接单测。

use std::fmt::Write as _;

use gt_backend::CommandRecord;
use gt_core::{ChangeSet, CommitMeta, HeadKind, RefInfo, RefKind, RepoState};
use gt_diff::{DiffLine, DiffPayload, FileDiff, LineKind};
use gt_graph::RowLayout;
use slint::{Color, ModelRc, SharedString, VecModel};

use crate::generated::{ChangeRow, ConsoleRow, DiffRow, GraphCell, LogRow, RecentRow, RefEntry};

/// 泳道配色（8 色循环）。取自可访问性对比度足够的暗色系。
const LANE_PALETTE: [u32; 8] = [
    0xff4a_8cff, // 蓝
    0xff6e_e7b7, // 青绿
    0xfff0_b429, // 琥珀
    0xfff2_799a, // 粉
    0xffa7_8bfa, // 紫
    0xff5e_ead4, // 青
    0xfffd_ba74, // 橙
    0xff93_c5fd, // 浅蓝
];

/// 把 `Vec<T>` 包成 Slint 模型。
///
/// Slint 没有为 `ModelRc<T>` 实现 `From<Vec<T>>`，必须显式经过 `VecModel`。
pub(crate) fn rc_model<T: Clone + 'static>(items: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(items))
}

/// 取泳道颜色（按泳道号循环）。
///
/// 字面量是 `0xAARRGGBB`：**alpha 必须显式给出**，否则会渲染成全透明
/// （`Color::from_argb_encoded` 不会默认补不透明）。
#[must_use]
pub fn lane_color(lane: u16) -> Color {
    let index = usize::from(lane) % LANE_PALETTE.len();
    Color::from_argb_encoded(LANE_PALETTE[index])
}

/// 泳道格的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphKind {
    /// 空列。
    Empty = 0,
    /// 竖向连线（该泳道穿过本行）。
    Line = 1,
    /// 本行的提交节点。
    Node = 2,
}

/// 由一行泳道布局构造格子序列。
///
/// 列的判定顺序是"先节点、再连线"：本行有提交的泳道必须画成节点，
/// 否则合并点上会出现"两条线交叉但没有点"的歧义画面。
#[must_use]
pub fn build_graph_cells(row: &RowLayout) -> Vec<GraphCell> {
    let lanes = row.lanes_after.len().max(row.lanes_before.len());
    let mut cells = Vec::with_capacity(lanes);
    for lane in 0..lanes {
        let lane_u16 = u16::try_from(lane).unwrap_or(u16::MAX);
        let kind = if lane_u16 == row.node_lane {
            GraphKind::Node
        } else if row.lanes_before.contains(&lane_u16) || row.lanes_after.contains(&lane_u16) {
            GraphKind::Line
        } else {
            GraphKind::Empty
        };
        cells.push(GraphCell {
            kind: kind as i32,
            color: lane_color(lane_u16),
        });
    }
    cells
}

/// 把 `opened_at`（Unix 秒）渲染成相对时间。
///
/// 相对时间比绝对时间更适合"最近提交"列表：用户关心的是"多久以前"。
#[must_use]
pub fn relative_time(timestamp: i64, now: i64) -> String {
    let delta = now.saturating_sub(timestamp);
    if delta < 0 {
        // 时钟回拨或未来时间戳：显示绝对时间比显示"-3 分钟前"诚实。
        return format_absolute(timestamp);
    }
    match delta {
        0..=59 => "刚刚".to_owned(),
        60..=3599 => format!("{} 分钟前", delta / 60),
        3600..=86_399 => format!("{} 小时前", delta / 3600),
        86_400..=2_591_999 => format!("{} 天前", delta / 86_400),
        2_592_000..=31_535_999 => format!("{} 个月前", delta / 2_592_000),
        _ => format!("{} 年前", delta / 31_536_000),
    }
}

/// 把 `timestamp` 渲染成绝对时间（用于时钟异常时的兜底）。
fn format_absolute(timestamp: i64) -> String {
    // 只做"Unix 秒 → 年-月-日"的粗略换算，避免为一个兜底分支引入时区/日历依赖。
    let days = timestamp.div_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

/// 由"1970-01-01 起的天数"换算公历年月日（Howard Hinnant 的 `civil_from_days`）。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (
        y,
        u32::try_from(m).unwrap_or(1),
        u32::try_from(d).unwrap_or(1),
    )
}

/// 构造日志行。
///
/// `nodes`、`layouts`、`refs_of` 三者下标必须一一对应；长度不一致时按最短的截断，
/// 宁可少画几行，也不要错位（错位的泳道图比缺几行难排查得多）。
#[must_use]
pub fn build_log_rows(
    nodes: &[CommitMeta],
    layouts: &[RowLayout],
    refs_of: &[String],
    selected: Option<usize>,
    now: i64,
) -> Vec<LogRow> {
    let count = nodes.len().min(layouts.len());
    let mut rows = Vec::with_capacity(count);
    for index in 0..count {
        let node = &nodes[index];
        rows.push(LogRow {
            short_id: SharedString::from(node.short_id()),
            subject: SharedString::from(node.summary.as_str()),
            author: SharedString::from(node.author_name.as_str()),
            when: SharedString::from(relative_time(node.author_time, now)),
            refs_text: SharedString::from(refs_of.get(index).map_or("", String::as_str)),
            is_selected: selected == Some(index),
            graph: rc_model(build_graph_cells(&layouts[index])),
        });
    }
    rows
}

/// 把差异载荷拆成界面行。
///
/// 占位载荷（二进制 / 过大）会合成一行说明，而不是留空 —— 空白的 diff 面板
/// 会让用户误以为"没有差异"。
#[must_use]
pub fn build_diff_rows(diff: &FileDiff) -> Vec<DiffRow> {
    match &diff.payload {
        DiffPayload::Text(hunks) => {
            let mut rows = Vec::new();
            for hunk in hunks {
                rows.push(DiffRow {
                    kind: 3,
                    old_no: SharedString::new(),
                    new_no: SharedString::new(),
                    segments: rc_model(vec![SharedString::from(hunk.header())]),
                    changed: rc_model(vec![false]),
                    is_hunk_header: true,
                });
                for line in &hunk.lines {
                    rows.push(diff_row(line));
                }
            }
            rows
        }
        DiffPayload::Binary => vec![placeholder_row("（二进制文件，不显示内容差异）")],
        DiffPayload::TooLarge {
            old_bytes,
            new_bytes,
        } => vec![placeholder_row(&format!(
            "（文件过大，已跳过差异计算：旧 {old_bytes} 字节 / 新 {new_bytes} 字节）"
        ))],
        DiffPayload::Empty => vec![placeholder_row("（两侧内容相同或为空）")],
    }
}

/// 占位说明行。
fn placeholder_row(text: &str) -> DiffRow {
    DiffRow {
        kind: 3,
        old_no: SharedString::new(),
        new_no: SharedString::new(),
        segments: rc_model(vec![SharedString::from(text)]),
        changed: rc_model(vec![false]),
        is_hunk_header: false,
    }
}

/// 单行差异 → 界面行。
fn diff_row(line: &DiffLine) -> DiffRow {
    let kind = match line.kind {
        LineKind::Context => 0,
        LineKind::Added => 1,
        LineKind::Removed => 2,
    };

    // 词级高亮：有片段就按片段拆，没有就把整行当作一个片段。
    let (segments, changed): (Vec<SharedString>, Vec<bool>) = if line.spans.is_empty() {
        (vec![SharedString::from(line.text.as_str())], vec![false])
    } else {
        (
            line.spans
                .iter()
                .map(|span| SharedString::from(span.text.as_str()))
                .collect(),
            line.spans.iter().map(|span| span.changed).collect(),
        )
    };

    DiffRow {
        kind,
        old_no: line
            .old_lineno
            .map_or_else(SharedString::new, |n| n.to_string().into()),
        new_no: line
            .new_lineno
            .map_or_else(SharedString::new, |n| n.to_string().into()),
        segments: rc_model(segments),
        changed: rc_model(changed),
        is_hunk_header: false,
    }
}

/// 构造本地更改行。
#[must_use]
pub fn build_change_rows(changes: &ChangeSet, selected: Option<usize>) -> Vec<ChangeRow> {
    changes
        .entries
        .iter()
        .enumerate()
        .map(|(index, entry)| ChangeRow {
            path: SharedString::from(entry.porcelain_path()),
            code: SharedString::from(match entry.conflict {
                Some(kind) => {
                    let (x, y) = kind.code();
                    format!("{x}{y}")
                }
                None => format!("{}{}", entry.index.letter(), entry.worktree.letter()),
            }),
            bucket: SharedString::from(entry.bucket().label()),
            note: SharedString::from(match entry.conflict {
                Some(kind) => kind.label().to_owned(),
                None => entry
                    .old_path
                    .as_ref()
                    .map_or_else(String::new, |old| format!("原路径 {old}")),
            }),
            is_selected: selected == Some(index),
        })
        .collect()
}

/// 构造引用列表行。
///
/// 头部（当前分支）排最前，其余按 `gt-core` 的分组顺序与名称排列，
/// 与侧栏"分支与标签"的阅读顺序一致。
#[must_use]
pub fn build_ref_rows(refs: &[RefInfo]) -> Vec<RefEntry> {
    let mut rows: Vec<RefEntry> = refs
        .iter()
        .map(|info| RefEntry {
            name: SharedString::from(info.name.as_str()),
            kind: SharedString::from(info.kind.group_label()),
            is_head: info.is_head,
            sync: SharedString::from(info.sync_summary().unwrap_or_default()),
        })
        .collect();
    rows.sort_by(|a, b| {
        b.is_head
            .cmp(&a.is_head)
            .then_with(|| a.name.as_str().cmp(b.name.as_str()))
    });
    rows
}

/// 构造 Console 行。
#[must_use]
pub fn build_console_rows(records: &[CommandRecord]) -> Vec<ConsoleRow> {
    records
        .iter()
        .map(|record| ConsoleRow {
            route: SharedString::from(record.route.label()),
            summary: SharedString::from(record.summary_line()),
            detail: SharedString::from(
                record
                    .error
                    .clone()
                    .or_else(|| record.stderr_tail.clone())
                    .unwrap_or_default(),
            ),
            ok: record.is_success(),
        })
        .collect()
}

/// 构造最近仓库行。
#[must_use]
pub fn build_recent_rows(recent: &[gt_config::RecentRepo]) -> Vec<RecentRow> {
    recent
        .iter()
        .map(|entry| RecentRow {
            name: SharedString::from(entry.display_name()),
            path: SharedString::from(entry.path.display().to_string()),
        })
        .collect()
}

/// 仓库状态的横幅文案（FR-STA-02）。
///
/// 返回 [`None`] 表示不需要横幅。
#[must_use]
pub fn state_banner(state: RepoState) -> Option<String> {
    if !state.needs_banner() {
        return None;
    }
    let actions = state.resumable_actions();
    let mut text = format!("当前处于「{}」进行中", state.label());
    if !actions.is_empty() {
        let _ = write!(
            text,
            "；可执行：{} / {} / {}",
            actions.continue_command.unwrap_or("—"),
            actions.skip_command.unwrap_or("—"),
            actions.abort_command.unwrap_or("—")
        );
    }
    Some(text)
}

/// 分支栏文案：`分支名 (a1b2c3d)`；分离头指针时给出警告式提示。
#[must_use]
pub fn branch_text(head: &gt_core::HeadInfo) -> String {
    let branch = head.display_branch();
    match head.target {
        Some(oid) => format!("{branch} ({})", oid.short()),
        None => branch.to_owned(),
    }
}

/// 头指针栏文案。
#[must_use]
pub fn head_text(head: &gt_core::HeadInfo) -> String {
    match head.kind {
        HeadKind::Branch => "HEAD".to_owned(),
        HeadKind::Detached => "分离头指针".to_owned(),
        HeadKind::Unborn => "尚无提交".to_owned(),
    }
}

/// 引用类别标签（供状态栏与侧栏分组使用）。
#[must_use]
pub const fn ref_kind_label(kind: RefKind) -> &'static str {
    kind.group_label()
}

#[cfg(test)]
mod tests {
    use super::{
        branch_text, build_change_rows, build_console_rows, build_diff_rows, build_graph_cells,
        build_log_rows, build_ref_rows, relative_time, state_banner,
    };
    use slint::{Model as _, ModelRc};

    /// 把 Slint 模型取回成 `Vec`（`ModelRc` 不提供 `len`/下标）。
    fn values<T: Clone>(model: &ModelRc<T>) -> Vec<T> {
        model.iter().collect()
    }
    use gt_core::{
        ChangeKind, ChangeSet, CommitMeta, ConflictKind, FileChange, HeadInfo, HeadKind, Oid,
        RefInfo, RefKind, RepoState, StatusEntry, StatusFlag,
    };
    use gt_diff::{DiffHunk, DiffLine, DiffPayload, DiffStats, FileDiff, LineKind};
    use gt_graph::LaneLayout;

    fn commit(id: u8, summary: &str, author_time: i64) -> CommitMeta {
        CommitMeta {
            id: Oid::from_bytes(&[id; 20]).expect("构造 Oid"),
            parents: Vec::new(),
            summary: summary.to_owned(),
            author_name: "张三".to_owned(),
            author_email: "z@example.com".to_owned(),
            author_time,
            committer_time: author_time,
        }
    }

    #[test]
    fn graph_cells_mark_nodes_and_lines() {
        let mut layout = LaneLayout::new();
        let first = gt_core::LogNode {
            id: Oid::from_bytes(&[1; 20]).expect("Oid"),
            parents: vec![Oid::from_bytes(&[2; 20]).expect("Oid")],
        };
        let second = gt_core::LogNode {
            id: Oid::from_bytes(&[2; 20]).expect("Oid"),
            parents: Vec::new(),
        };
        layout.push(&first);
        let row = layout.push(&second);

        let cells = build_graph_cells(&row);
        assert!(!cells.is_empty());
        // 本行节点所在的格子必须是 Node，不能只是 Line。
        let node_column = cells
            .iter()
            .position(|cell| cell.kind == super::GraphKind::Node as i32);
        assert!(node_column.is_some(), "应有一列是节点：{cells:?}");
        assert!(cells.iter().all(|cell| (0..=2).contains(&cell.kind)));
    }

    #[test]
    fn relative_time_reads_naturally() {
        let now = 1_700_000_000i64;
        assert_eq!(relative_time(now, now), "刚刚");
        assert_eq!(relative_time(now - 120, now), "2 分钟前");
        assert_eq!(relative_time(now - 7200, now), "2 小时前");
        assert_eq!(relative_time(now - 3 * 86_400, now), "3 天前");
        assert_eq!(relative_time(now - 60 * 86_400, now), "2 个月前");
        assert_eq!(relative_time(now - 800 * 86_400, now), "2 年前");
        // 未来时间戳退回绝对日期，而不是负数。
        // 1700000000 = 2023-11-14 22:13:20 UTC，再加一天即 11-15。
        assert_eq!(relative_time(now + 86_400, now), "2023-11-15");
    }

    #[test]
    fn log_rows_line_up_with_layout_rows() {
        let nodes = vec![
            commit(1, "第一个提交", 1_700_000_000),
            commit(2, "第二个提交", 1_700_000_100),
        ];
        let mut layout = LaneLayout::new();
        let layouts: Vec<gt_graph::RowLayout> = nodes
            .iter()
            .map(|meta| {
                layout.push(&gt_core::LogNode {
                    id: meta.id,
                    parents: meta.parents.clone(),
                })
            })
            .collect();

        let refs = vec!["HEAD -> main".to_owned(), String::new()];
        let rows = build_log_rows(&nodes, &layouts, &refs, Some(1), 1_700_000_200);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].subject.as_str(), "第一个提交");
        assert!(rows[1].is_selected);
        assert!(!rows[0].is_selected);
        assert_eq!(rows[0].when.as_str(), "3 分钟前");
        assert_eq!(rows[0].refs_text.as_str(), "HEAD -> main");

        // 长度不齐时按最短截断，绝不越界。
        let short = build_log_rows(&nodes, &layouts[..1], &refs, None, 0);
        assert_eq!(short.len(), 1);
        let empty_refs: Vec<String> = Vec::new();
        assert_eq!(
            build_log_rows(&nodes, &layouts, &empty_refs, None, 0).len(),
            2
        );
    }

    #[test]
    fn diff_rows_expand_hunks_and_words() {
        let mut line = DiffLine::new(LineKind::Added, Some(2), Some(2), "new text");
        line.spans = vec![
            gt_diff::LineSpan::new(false, "new "),
            gt_diff::LineSpan::new(true, "text"),
        ];
        let hunk = DiffHunk {
            old_start: 1,
            old_lines: 1,
            new_start: 2,
            new_lines: 1,
            lines: vec![
                line,
                DiffLine::new(LineKind::Context, Some(1), Some(1), "context line"),
            ],
        };

        let diff = FileDiff {
            old_path: "a.txt".to_owned(),
            new_path: "a.txt".to_owned(),
            payload: DiffPayload::Text(vec![hunk]),
            stats: DiffStats {
                added: 1,
                removed: 0,
                binary: false,
            },
        };

        let rows = build_diff_rows(&diff);
        // 一个 hunk 头 + 两行内容。
        assert_eq!(rows.len(), 3);
        assert!(rows[0].is_hunk_header);
        assert_eq!(rows[1].kind, 1, "新增行");
        assert_eq!(values(&rows[1].segments).len(), 2, "词级高亮应拆成两段");
        assert!(values(&rows[1].changed)[1]);
        assert_eq!(rows[2].kind, 0, "上下文行");
        assert_eq!(values(&rows[2].segments).len(), 1);
        assert_eq!(rows[2].old_no.as_str(), "1");
    }

    #[test]
    fn placeholder_payloads_still_produce_a_row() {
        for payload in [
            DiffPayload::Binary,
            DiffPayload::TooLarge {
                old_bytes: 3_000_000,
                new_bytes: 4_000_000,
            },
            DiffPayload::Empty,
        ] {
            let diff = FileDiff {
                old_path: "a".to_owned(),
                new_path: "a".to_owned(),
                payload,
                stats: DiffStats::default(),
            };
            let rows = build_diff_rows(&diff);
            assert_eq!(rows.len(), 1, "占位载荷必须给出一行说明而不是空白");
            assert_eq!(rows[0].kind, 3);
            assert!(!values(&rows[0].segments)[0].is_empty());
        }
    }

    #[test]
    fn change_rows_carry_code_and_bucket() {
        let set = ChangeSet::new(vec![
            StatusEntry::conflicted("c.txt", ConflictKind::BothModified),
            StatusEntry::untracked("u.txt"),
            StatusEntry::new("s.txt", StatusFlag::Added, StatusFlag::Unmodified),
            StatusEntry::new("m.txt", StatusFlag::Unmodified, StatusFlag::Modified),
        ]);
        let rows = build_change_rows(&set, Some(0));
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].code.as_str(), "UU");
        assert_eq!(rows[0].bucket.as_str(), "冲突");
        assert!(rows[0].is_selected);
        // 有争议的路径要能显示旧路径，否则重命名条目看起来像"新增"。
        assert!(!rows[3].is_selected);
    }

    #[test]
    fn change_rows_show_old_path_for_renames() {
        let mut entry = StatusEntry::new("b.rs", StatusFlag::Renamed, StatusFlag::Unmodified);
        entry.old_path = Some("a.rs".to_owned());
        let rows = build_change_rows(&ChangeSet::new(vec![entry]), None);
        assert_eq!(rows[0].path.as_str(), "a.rs -> b.rs");
        assert!(rows[0].note.as_str().contains("a.rs"));
    }

    #[test]
    fn ref_rows_put_the_current_branch_first() {
        let refs = vec![
            RefInfo {
                name: "v1.0".to_owned(),
                kind: RefKind::Tag,
                target: Oid::ZERO,
                is_head: false,
                upstream: None,
                ahead: 0,
                behind: 0,
            },
            RefInfo {
                name: "main".to_owned(),
                kind: RefKind::LocalBranch,
                target: Oid::ZERO,
                is_head: true,
                upstream: Some("origin/main".to_owned()),
                ahead: 2,
                behind: 1,
            },
        ];
        let rows = build_ref_rows(&refs);
        assert_eq!(rows[0].name.as_str(), "main");
        assert!(rows[0].is_head);
        assert!(!rows[0].sync.is_empty(), "有上游时应给出同步摘要");
    }

    #[test]
    fn console_rows_mark_failures() {
        use gt_backend::{CommandRecord, CommandRoute};
        use std::time::Duration;

        let mut failed = CommandRecord::internal(
            CommandRoute::Gix,
            None,
            "git status",
            Duration::from_millis(2),
        );
        failed.error = Some("对象损坏".to_owned());

        let rows = build_console_rows(&[failed]);
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].ok);
        assert!(rows[0].detail.contains("对象损坏"));
        assert!(rows[0].summary.contains("git status"));
    }

    #[test]
    fn banners_and_head_texts_follow_the_state() {
        assert!(state_banner(RepoState::Clean).is_none());
        let banner = state_banner(RepoState::RebaseMerge).expect("变基中应有横幅");
        assert!(banner.contains("变基"));
        assert!(banner.contains("--continue"));

        let branch = HeadInfo {
            kind: HeadKind::Branch,
            target: Some(Oid::from_bytes(&[7; 20]).expect("Oid")),
            branch: Some("main".to_owned()),
        };
        assert!(branch_text(&branch).starts_with("main ("));

        let detached = HeadInfo {
            kind: HeadKind::Detached,
            target: Some(Oid::ZERO),
            branch: None,
        };
        assert_eq!(super::head_text(&detached), "分离头指针");
        assert_eq!(
            super::head_text(&HeadInfo {
                kind: HeadKind::Unborn,
                target: None,
                branch: None,
            }),
            "尚无提交"
        );
    }

    #[test]
    fn rename_in_change_rows_uses_porcelain_arrow_form() {
        // 与 `git status --porcelain` 的 `旧 -> 新` 表示保持一致。
        let change = FileChange::with_old_path("b.rs", "a.rs", ChangeKind::Renamed);
        assert_eq!(change.name_status_line(), "R100\ta.rs\tb.rs");
    }

    #[test]
    fn lane_colors_cycle_without_panicking() {
        for lane in 0..64u16 {
            let color = super::lane_color(lane);
            assert_ne!(color, slint::Color::default(), "泳道配色不应为透明");
        }
    }
}
