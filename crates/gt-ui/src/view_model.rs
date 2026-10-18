//! 视图模型：把领域模型转换成 Slint 可直接渲染的行数据结构。
//!
//! # 为什么不把领域模型直接塞给 Slint
//!
//! 三条原因：
//!
//! 1. **渲染需要的信息比领域模型多**（泳道颜色、行号槽、分段文本、可选择标记），
//!    而这些都要在 UI 线程上算；算在 Rust 侧才能被单测覆盖；
//! 2. **Slint 的 `ModelRc` 要求 `'static` + `Clone`**，借用式领域模型无法直接传递；
//! 3. **词级高亮不做文本测量**：把行拆成"分段文本 + 是否变更"的平行数组，
//!    界面只做逐段着色，省掉每帧的排版计算（架构规划 5.7）。
//!
//! 本模块**只做纯转换**（输入领域模型 → 输出行结构），不接触 I/O 与线程，
//! 因此可以脱离 Slint 运行时直接单测。

use std::collections::BTreeSet;

use gt_backend::{
    BlameLine, CommandRecord, FileHistoryEntry, PushPreview, RemoteInfo, RemoteListing,
};
use gt_core::{ChangeSet, CommitMeta, HeadKind, RefInfo, RefKind, RepoState};
use gt_diff::{DiffLine, DiffPayload, FileDiff, LineKind};
use gt_graph::RowLayout;
use gt_service::NetworkFailure;
use slint::{Color, ModelRc, SharedString, VecModel};

use crate::text;
use crate::generated::{
    BlameRow, ChangeRow, CompareRow, ConflictFileRow, ConflictRow, ConsoleRow, DiffRow, GraphCell,
    HistRow, LogRow, PushRow, RebaseStepRow, RecentRow, RefEntry, RemoteRow, StashRow,
};

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
        0..=59 => text::tr("刚刚").to_owned(),
        60..=3599 => text::trf(text::tr("{} 分钟前"), &[&(delta / 60).to_string()]),
        3600..=86_399 => text::trf(text::tr("{} 小时前"), &[&(delta / 3600).to_string()]),
        86_400..=2_591_999 => text::trf(text::tr("{} 天前"), &[&(delta / 86_400).to_string()]),
        2_592_000..=31_535_999 => text::trf(text::tr("{} 个月前"), &[&(delta / 2_592_000).to_string()]),
        _ => text::trf(text::tr("{} 年前"), &[&(delta / 31_536_000).to_string()]),
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
///
/// `my_email` 用于 FR-LOG-06 的"我的提交加粗"：与该邮箱一致的作者视为本人。
#[must_use]
pub fn build_log_rows(
    nodes: &[CommitMeta],
    layouts: &[RowLayout],
    refs_of: &[String],
    selected: Option<usize>,
    now: i64,
    my_email: Option<&str>,
) -> Vec<LogRow> {
    let count = nodes.len().min(layouts.len());
    let mut rows = Vec::with_capacity(count);
    for index in 0..count {
        let node = &nodes[index];
        let is_mine = my_email.is_some_and(|email| {
            !email.is_empty() && node.author_email.eq_ignore_ascii_case(email)
        });
        rows.push(LogRow {
            short_id: SharedString::from(node.short_id()),
            subject: SharedString::from(node.summary.as_str()),
            author: SharedString::from(node.author_name.as_str()),
            when: SharedString::from(relative_time(node.author_time, now)),
            refs_text: SharedString::from(refs_of.get(index).map_or("", String::as_str)),
            is_selected: selected == Some(index),
            is_mine,
            is_merge: node.is_merge(),
            graph: rc_model(build_graph_cells(&layouts[index])),
        });
    }
    rows
}

/// 把差异载荷拆成界面行。
///
/// 每一行都带上"它来自哪个 hunk、hunk 内第几行"，界面据此把点击换算成补丁选择：
/// - hunk 头行：`line-index == -1`，点击表示"整块"；
/// - 增删行：`selectable == true`；
/// - 上下文行与占位行：不可选。
///
/// `selected` 里是**界面行下标**，不是 diff 行下标 —— 两者在加入 hunk 头行后不再相同。
#[must_use]
pub fn build_diff_rows(diff: &FileDiff, selected: &BTreeSet<usize>) -> Vec<DiffRow> {
    let mut rows = Vec::new();
    match &diff.payload {
        DiffPayload::Text(hunks) => {
            for (hunk_index, hunk) in hunks.iter().enumerate() {
                let hunk_index_i32 = i32::try_from(hunk_index).unwrap_or(-1);
                rows.push(DiffRow {
                    kind: 3,
                    old_no: SharedString::new(),
                    new_no: SharedString::new(),
                    segments: rc_model(vec![SharedString::from(hunk.header())]),
                    changed: rc_model(vec![false]),
                    is_hunk_header: true,
                    hunk_index: hunk_index_i32,
                    line_index: -1,
                    selectable: true,
                    selected: false,
                });
                for (line_index, line) in hunk.lines.iter().enumerate() {
                    let mut row = diff_row(line);
                    row.hunk_index = hunk_index_i32;
                    row.line_index = i32::try_from(line_index).unwrap_or(-1);
                    row.selectable = line.kind.is_change();
                    rows.push(row);
                }
            }
        }
        DiffPayload::Binary => rows.push(placeholder_row(text::tr("（二进制文件，不显示内容差异）"))),
        DiffPayload::TooLarge {
            old_bytes,
            new_bytes,
        } => rows.push(placeholder_row(&text::trf(text::tr("（文件过大，已跳过差异计算：旧 {} 字节 / 新 {} 字节）"), &[&old_bytes.to_string(), &new_bytes.to_string()]))),
        DiffPayload::Empty => rows.push(placeholder_row(text::tr("（两侧内容相同或为空）"))),
    }

    for (index, row) in rows.iter_mut().enumerate() {
        row.selected = selected.contains(&index);
    }
    rows
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
        hunk_index: -1,
        line_index: -1,
        selectable: false,
        selected: false,
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
        hunk_index: -1,
        line_index: -1,
        selectable: false,
        selected: false,
    }
}

/// 构造本地更改行。
#[must_use]
pub fn build_change_rows(changes: &ChangeSet, selected: Option<usize>) -> Vec<ChangeRow> {
    changes
        .entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let is_untracked = entry.index == gt_core::StatusFlag::Untracked
                || entry.worktree == gt_core::StatusFlag::Untracked;
            ChangeRow {
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
                        .map_or_else(String::new, |old| text::trf("原路径 {}", &[old])),
                }),
                is_selected: selected == Some(index),
                is_conflict: entry.is_conflicted(),
                is_untracked,
            }
        })
        .collect()
}

/// 构造引用列表行。
///
/// 头部（当前分支）排最前，其余按 `gt-core` 的分组顺序与名称排列，
/// 与侧栏"分支与标签"的阅读顺序一致。
#[must_use]
pub fn build_ref_rows(refs: &[RefInfo], selected: Option<usize>) -> Vec<RefEntry> {
    // 先按"是否头指针 / 类别 / 名称"排序，再统一发 selected 标记：
    // 选择下标是**排序后**的下标，否则点一行会选中另一行。
    let mut order: Vec<&RefInfo> = refs.iter().collect();
    order.sort_by(|a, b| {
        b.is_head
            .cmp(&a.is_head)
            .then_with(|| a.kind.group_order().cmp(&b.kind.group_order()))
            .then_with(|| a.name.cmp(&b.name))
    });

    order
        .into_iter()
        .enumerate()
        .map(|(index, info)| RefEntry {
            name: SharedString::from(info.name.as_str()),
            kind: SharedString::from(info.kind.group_label()),
            is_head: info.is_head,
            sync: SharedString::from(info.sync_summary().unwrap_or_default()),
            selected: selected == Some(index),
            kind_id: match info.kind {
                RefKind::LocalBranch => 0,
                RefKind::RemoteBranch => 1,
                RefKind::Tag => 2,
                RefKind::Other => 3,
            },
        })
        .collect()
}

/// 按排序后的顺序取引用（与 [`build_ref_rows`] 的下标语义一致）。
#[must_use]
pub fn ordered_refs(refs: &[RefInfo]) -> Vec<RefInfo> {
    let mut order: Vec<RefInfo> = refs.to_vec();
    order.sort_by(|a, b| {
        b.is_head
            .cmp(&a.is_head)
            .then_with(|| a.kind.group_order().cmp(&b.kind.group_order()))
            .then_with(|| a.name.cmp(&b.name))
    });
    order
}

/// 构造 Console 行。
#[must_use]
pub fn build_console_rows(records: &[CommandRecord]) -> Vec<ConsoleRow> {
    records
        .iter()
        .map(|record| ConsoleRow {
            // 用 `route_label()` 而不是 `route.label()`：网络操作要带上"网络"
            // 后缀，否则 Console 里一次 fetch 与一次本地命令看起来毫无区别。
            route: SharedString::from(record.route_label()),
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
    let mut text = text::trf(text::tr("当前处于「{}」进行中"), &[state.label()]);
    if !actions.is_empty() {
        text += &text::trf(
            text::tr("；可执行：{} / {} / {}"),
            &[
                actions.continue_command.unwrap_or("—"),
                actions.skip_command.unwrap_or("—"),
                actions.abort_command.unwrap_or("—"),
            ],
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
        HeadKind::Detached => text::tr("分离头指针").to_owned(),
        HeadKind::Unborn => text::tr("尚无提交").to_owned(),
    }
}

/// 引用类别标签（供状态栏与侧栏分组使用）。
#[must_use]
pub const fn ref_kind_label(kind: RefKind) -> &'static str {
    kind.group_label()
}

/// 差异选择的操作提示（显示在 Diff 面板标题栏右侧）。
#[must_use]
pub fn diff_selection_hint(selected: usize, selectable: bool, staged_view: bool) -> String {
    if !selectable {
        return text::tr("该差异不支持逐块选择").to_owned();
    }
    if selected == 0 {
        return if staged_view {
            text::tr("点击 hunk 头或增删行以选择，然后反暂存").to_owned()
        } else {
            text::tr("点击 hunk 头或增删行以选择，然后暂存 / 丢弃").to_owned()
        };
    }
    text::trf(text::tr("已选 {} 行"), &[&selected.to_string()])
}

/// 危险操作确认的正文（FR-LC-02 / D4：默认强确认）。
///
/// 文案必须包含三件事：**将要失去什么**、**不可逆**、**替代做法**。
/// 只写"确定吗？"的确认框等于没有确认。
#[must_use]
pub fn discard_confirmation(paths: &[String], untracked: bool) -> (String, String) {
    let listed = paths.iter().take(8).cloned().collect::<Vec<_>>().join("\n");
    let more = if paths.len() > 8 {
        text::trf(text::tr("\n…以及另外 {} 个路径"), &[&(paths.len() - 8).to_string()])
    } else {
        String::new()
    };

    if untracked {
        (
            text::trf(text::tr("永久删除 {} 个未跟踪路径？"), &[&paths.len().to_string()]),
            format!(
                "这些路径从未被 Git 跟踪，删除后无法通过 Git 恢复：\n\n{listed}{more}\n\n\
                 替代做法：若只是不想看到它们，用「加入 .gitignore」。"
            ),
        )
    } else {
        (
            text::trf(text::tr("丢弃 {} 个文件的未提交修改？"), &[&paths.len().to_string()]),
            format!(
                "工作区中的这些修改将被还原为索引中的内容，此操作不可撤销：\n\n{listed}{more}\n\n\
                 替代做法：先「暂存」再提交，或先复制文件备份。"
            ),
        )
    }
}

/// 丢弃所选 hunk / 行时的确认正文。
#[must_use]
pub fn discard_selection_confirmation(path: &str, lines: usize) -> (String, String) {
    (
        text::tr("丢弃所选变更？").to_owned(),
        format!(
            "将从工作区文件 {path} 中撤销所选 {lines} 行改动，此操作不可撤销。\n\n\
             替代做法：先暂存所选内容再提交，这样改动会留在历史里。"
        ),
    )
}

// ---------------------------------------------------------------- 远端视图模型

/// 构造推送预览的行模型。
///
/// 时间用相对口径（"3 分钟前"）：推送前的复核关心"这是刚提交的还是上周的"。
#[must_use]
pub fn build_push_rows(preview: &PushPreview, now: i64) -> Vec<PushRow> {
    preview
        .commits
        .iter()
        .map(|commit| PushRow {
            short_id: SharedString::from(commit.short_id.as_str()),
            subject: SharedString::from(commit.subject.as_str()),
            author: SharedString::from(commit.author.as_str()),
            when: SharedString::from(relative_time(commit.timestamp, now)),
        })
        .collect()
}

/// 构造远端列表的行模型。
#[must_use]
pub fn build_remote_rows(remotes: &[RemoteInfo], selected: Option<usize>) -> Vec<RemoteRow> {
    remotes
        .iter()
        .enumerate()
        .map(|(index, remote)| RemoteRow {
            name: SharedString::from(remote.name.as_str()),
            url: SharedString::from(remote.display_url()),
            detail: SharedString::from(if remote.has_embedded_credentials {
                text::tr("URL 内嵌凭据（建议改用 helper）")
            } else if remote.push_url.is_some() {
                text::tr("push URL 与 fetch 不同")
            } else {
                ""
            }),
            selected: selected == Some(index),
        })
        .collect()
}

/// 推送预览的摘要文案（标题下方那行）。
#[must_use]
pub fn push_preview_text(preview: &PushPreview) -> String {
    let mut text = preview.summary();
    if preview.truncated {
        text += &text::trf(text::tr("（仅列出前 {} 条）"), &[&preview.commits.len().to_string()]);
    }
    text
}

/// 推送遮罩里的提示行：把"会发生什么 / 风险 / 替代做法"写在同一处。
#[must_use]
pub fn push_hints(preview: &PushPreview, force: bool) -> String {
    let mut lines: Vec<String> = Vec::new();
    if let Some(pattern) = preview.protection.as_deref() {
        lines.push(text::trf("该分支命中受保护规则「{}」：禁止强制推送（普通推送不受影响）。", &[pattern]));
    }
    if preview.needs_update_first() {
        lines.push(text::trf(
            text::tr("远端另有 {} 个提交：普通推送会被拒绝，请先 Update。"),
            &[&preview.behind.to_string()],
        ));
    }
    if force {
        lines.push(
            "已勾选强制推送（force-with-lease）：远端若在你上次 fetch 之后被他人推进，\
             本次推送会失败而不是覆盖。"
                .to_owned(),
        );
    }
    if preview.creates_branch {
        lines.push(text::tr("远端尚无该分支，推送会在远端新建它。").to_owned());
    }
    lines.join(" ")
}

/// 网络失败的展示文本（结论 + 建议）。
#[must_use]
pub fn network_error_text(failure: &NetworkFailure) -> String {
    failure.message()
}

/// 远端管理遮罩里的凭据说明。
///
/// 说清"凭据是从哪来的"，用户才知道失败时该改哪一处：
/// 有 credential helper 就走 helper，没有才需要在本工具里存 token。
#[must_use]
pub fn credential_help(listing: &RemoteListing, stored_hosts: &[String]) -> String {
    let mut parts: Vec<String> = Vec::new();
    if listing.helpers.is_empty() {
        parts.push(text::tr("未检测到 credential helper").to_owned());
    } else {
        parts.push(format!("credential helper：{}", listing.helpers.join("、")));
    }
    parts.push(if listing.ssh_agent {
        text::tr("SSH agent 可用").to_owned()
    } else {
        text::tr("SSH agent 未运行（SSH 远端可能需要 ssh-add）").to_owned()
    });
    if !listing.keyring_available {
        parts.push(text::tr("系统密钥链不可用，无法在本工具内保存 token").to_owned());
    } else if stored_hosts.is_empty() {
        parts.push(text::tr("尚未保存任何 token").to_owned());
    } else {
        parts.push(text::trf(text::tr("已保存 token 的主机：{}"), &[&stored_hosts.join("、")]));
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::{
        branch_text, build_change_rows, build_console_rows, build_diff_rows, build_graph_cells,
        build_log_rows, build_push_rows, build_ref_rows, build_remote_rows, credential_help,
        diff_selection_hint, discard_confirmation, discard_selection_confirmation,
        network_error_text, ordered_refs, push_hints, push_preview_text, relative_time,
        state_banner,
    };
    use std::collections::BTreeSet;

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
        let rows = build_log_rows(&nodes, &layouts, &refs, Some(1), 1_700_000_200, None);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].subject.as_str(), "第一个提交");
        assert!(rows[1].is_selected);
        assert!(!rows[0].is_selected);
        assert_eq!(rows[0].when.as_str(), "3 分钟前");
        assert_eq!(rows[0].refs_text.as_str(), "HEAD -> main");
        assert!(!rows[0].is_mine, "未提供身份时不得标记为本人提交");

        // 长度不齐时按最短截断，绝不越界。
        let short = build_log_rows(&nodes, &layouts[..1], &refs, None, 0, None);
        assert_eq!(short.len(), 1);
        let empty_refs: Vec<String> = Vec::new();
        assert_eq!(
            build_log_rows(&nodes, &layouts, &empty_refs, None, 0, None).len(),
            2
        );
    }

    #[test]
    fn log_rows_mark_mine_and_merge_commits() {
        let mut merge = commit(3, "合并提交", 1_700_000_000);
        merge.parents = vec![
            Oid::from_bytes(&[1; 20]).expect("Oid"),
            Oid::from_bytes(&[2; 20]).expect("Oid"),
        ];
        merge.author_email = "other@example.com".to_owned();
        let nodes = vec![commit(1, "别人提交", 1_700_000_000), merge];
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

        let rows = build_log_rows(
            &nodes,
            &layouts,
            &[],
            None,
            1_700_000_100,
            Some("Z@example.com"),
        );
        assert!(rows[0].is_mine, "邮箱应大小写不敏感地匹配");
        assert!(!rows[0].is_merge);
        assert!(rows[1].is_merge, "双父提交必须标记为合并");
        assert!(!rows[1].is_mine);
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
                DiffLine::new(LineKind::Context, Some(1), Some(1), "context line"),
                line,
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

        let rows = build_diff_rows(&diff, &BTreeSet::new());
        // 一个 hunk 头 + 两行内容。
        assert_eq!(rows.len(), 3);
        assert!(rows[0].is_hunk_header);
        assert!(rows[0].selectable, "hunk 头可点击（整块选择）");
        assert_eq!(rows[0].line_index, -1);
        assert_eq!(rows[0].hunk_index, 0);

        assert_eq!(rows[1].kind, 0, "上下文行");
        assert!(!rows[1].selectable, "上下文行不参与选择");
        assert_eq!(rows[1].old_no.as_str(), "1");

        assert_eq!(rows[2].kind, 1, "新增行");
        assert!(rows[2].selectable);
        assert_eq!(rows[2].hunk_index, 0);
        assert_eq!(rows[2].line_index, 1, "行下标指向 hunk.lines");
        assert_eq!(values(&rows[2].segments).len(), 2, "词级高亮应拆成两段");
        assert!(values(&rows[2].changed)[1]);
    }

    #[test]
    fn diff_rows_apply_the_selection_marks() {
        let hunk = DiffHunk {
            old_start: 1,
            old_lines: 2,
            new_start: 1,
            new_lines: 2,
            lines: vec![
                DiffLine::new(LineKind::Removed, Some(1), None, "old"),
                DiffLine::new(LineKind::Added, None, Some(1), "new"),
            ],
        };
        let diff = FileDiff {
            old_path: "a".to_owned(),
            new_path: "a".to_owned(),
            payload: DiffPayload::Text(vec![hunk]),
            stats: DiffStats::default(),
        };
        let selected = BTreeSet::from([1_usize]);
        let rows = build_diff_rows(&diff, &selected);
        assert!(rows[1].selected, "第 1 行（删除行）应被标记为选中");
        assert!(!rows[0].selected);
        assert!(!rows[2].selected);
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
            let rows = build_diff_rows(&diff, &BTreeSet::new());
            assert_eq!(rows.len(), 1, "占位载荷必须给出一行说明而不是空白");
            assert_eq!(rows[0].kind, 3);
            assert!(!rows[0].selectable);
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
        assert!(rows[0].is_conflict);
        assert!(rows[0].is_selected);
        assert!(
            rows[1].is_untracked,
            "未跟踪条目要能触发「删除 / 忽略」按钮"
        );
        assert!(!rows[2].is_untracked);
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
        let rows = build_ref_rows(&refs, Some(1));
        assert_eq!(rows[0].name.as_str(), "main");
        assert!(rows[0].is_head);
        assert!(!rows[0].sync.is_empty(), "有上游时应给出同步摘要");
        assert!(!rows[0].selected);
        assert!(rows[1].selected, "选择下标按排序后的顺序解释");

        // `ordered_refs` 必须与 `build_ref_rows` 的下标语义一致。
        let ordered = ordered_refs(&refs);
        assert_eq!(ordered[0].name, "main");
        assert_eq!(ordered[1].name, "v1.0");
        assert_eq!(rows[0].kind_id, 0);
        assert_eq!(rows[1].kind_id, 2);
    }

    #[test]
    fn console_rows_tag_network_operations() {
        let mut record = gt_backend::CommandRecord::internal(
            gt_backend::CommandRoute::Cli,
            None,
            "git fetch origin",
            std::time::Duration::from_millis(5),
        );
        record.network = true;
        let rows = build_console_rows(&[record]);
        assert!(
            rows[0].route.contains("网络"),
            "网络操作必须在 Console 里可区分：{}",
            rows[0].route
        );
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

    #[test]
    fn diff_hints_explain_the_next_action() {
        assert!(diff_selection_hint(0, false, false).contains("不支持"));
        let unstaged = diff_selection_hint(0, true, false);
        assert!(unstaged.contains("暂存"), "{unstaged}");
        let staged = diff_selection_hint(0, true, true);
        assert!(staged.contains("反暂存"), "{staged}");
        assert_eq!(diff_selection_hint(3, true, false), "已选 3 行");
    }

    #[test]
    fn destructive_confirmations_state_what_is_lost() {
        let (title, detail) =
            discard_confirmation(&["a.txt".to_owned(), "b/c.txt".to_owned()], false);
        assert!(title.contains('2'));
        assert!(detail.contains("不可撤销"));
        assert!(detail.contains("a.txt"));
        assert!(
            detail.contains("替代做法"),
            "确认框必须给出代替方案：{detail}"
        );

        let (title, detail) = discard_confirmation(&["x.log".to_owned()], true);
        assert!(title.contains("永久删除"));
        assert!(detail.contains("未被 Git 跟踪"));

        let (title, detail) = discard_selection_confirmation("a.txt", 4);
        assert!(title.contains("丢弃"));
        assert!(detail.contains("4 行"));
    }

    fn preview_with(
        commits: usize,
        behind: usize,
        protected: Option<&str>,
    ) -> gt_backend::PushPreview {
        gt_backend::PushPreview {
            remote: "origin".to_owned(),
            local_branch: "main".to_owned(),
            remote_branch: "main".to_owned(),
            upstream: Some("origin/main".to_owned()),
            ahead: commits,
            behind,
            commits: (0..commits)
                .map(|index| gt_backend::PushCommit {
                    id: format!("{index:040}"),
                    short_id: format!("c{index}"),
                    author: "张三".to_owned(),
                    timestamp: 1_700_000_000 - i64::try_from(index).unwrap_or(0) * 60,
                    subject: format!("提交 {index}"),
                })
                .collect(),
            truncated: false,
            creates_branch: false,
            protection: protected.map(str::to_owned),
        }
    }

    #[test]
    fn push_rows_keep_the_preview_order_and_relative_time() {
        let preview = preview_with(3, 0, None);
        let rows = build_push_rows(&preview, 1_700_000_000);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].short_id.as_str(), "c0");
        assert_eq!(rows[0].when.as_str(), "刚刚");
        assert_eq!(rows[2].when.as_str(), "2 分钟前");
    }

    #[test]
    fn push_hints_explain_protection_and_divergence() {
        let protected = preview_with(2, 1, Some("^main$"));
        let hints = push_hints(&protected, true);
        assert!(hints.contains("受保护"), "{hints}");
        assert!(
            hints.contains("先 Update"),
            "落后远端时必须提示先更新：{hints}"
        );
        assert!(hints.contains("force-with-lease"), "{hints}");

        let summary = push_preview_text(&protected);
        assert!(summary.contains('2'), "{summary}");
    }

    #[test]
    fn remote_rows_surface_credential_leaks() {
        let remotes = vec![
            gt_backend::RemoteInfo {
                name: "origin".to_owned(),
                fetch_url: Some("https://github.com/o/r.git".to_owned()),
                push_url: None,
                has_embedded_credentials: false,
            },
            gt_backend::RemoteInfo {
                name: "legacy".to_owned(),
                fetch_url: Some("https://bob:***@git.example.com/r.git".to_owned()),
                push_url: Some("ssh://git@git.example.com/r.git".to_owned()),
                has_embedded_credentials: true,
            },
        ];
        let rows = build_remote_rows(&remotes, Some(1));
        assert_eq!(rows.len(), 2);
        assert!(!rows[0].selected);
        assert!(rows[1].selected);
        assert!(rows[1].detail.contains("内嵌凭据"), "必须提示改掉内嵌凭据");
    }

    #[test]
    fn credential_help_names_the_actual_mechanism() {
        let listing = gt_backend::RemoteListing {
            remotes: Vec::new(),
            helpers: vec!["manager".to_owned()],
            ssh_agent: true,
            keyring_available: true,
        };
        let text = credential_help(&listing, &["github.com".to_owned()]);
        assert!(text.contains("manager"), "{text}");
        assert!(text.contains("SSH agent 可用"), "{text}");
        assert!(text.contains("github.com"), "{text}");

        let bare = gt_backend::RemoteListing::default();
        let text = credential_help(&bare, &[]);
        assert!(text.contains("未检测到 credential helper"), "{text}");
        assert!(text.contains("未运行"), "{text}");
    }

    #[test]
    fn network_failure_text_is_actionable() {
        let failure = gt_service::NetworkFailure {
            summary: "origin：远端拒绝了本次认证".to_owned(),
            kind: gt_backend::CredentialKind::AuthRejected,
            hints: vec!["更新本机保存的凭据".to_owned(), "使用访问 token".to_owned()],
            detail: String::new(),
        };
        let text = network_error_text(&failure);
        assert!(text.contains("认证"), "{text}");
        assert!(text.contains("1. 更新本机保存的凭据"), "{text}");
        assert!(failure.suggests_token(), "401/403 应引导去存 token");
    }
}

// ------------------------------------------------------------ 高级操作视图模型

/// 冲突块在界面上的解法编码（与 Slint 回调参数一一对应）。
///
/// 用整数而不是字符串：Slint 侧的按钮组按下标取值，整数映射不需要解析，
/// 也不会因为文案改动（本地化）而失效。
pub mod resolve_code {
    /// 取当前分支（ours）。
    pub const OURS: i32 = 0;
    /// 取合入分支（theirs）。
    pub const THEIRS: i32 = 1;
    /// 两边都保留。
    pub const BOTH: i32 = 2;
    /// 取共同祖先（diff3 风格才有内容）。
    pub const BASE: i32 = 3;
}

/// 把"界面解法编号"翻译成领域解法。
#[must_use]
pub fn resolution_from_code(code: i32) -> Option<gt_diff::Resolution> {
    match code {
        resolve_code::OURS => Some(gt_diff::Resolution::Ours),
        resolve_code::THEIRS => Some(gt_diff::Resolution::Theirs),
        resolve_code::BOTH => Some(gt_diff::Resolution::Both),
        resolve_code::BASE => Some(gt_diff::Resolution::Base),
        _ => None,
    }
}

/// 构造贮藏列表行。
#[must_use]
pub fn build_stash_rows(entries: &[gt_backend::StashEntry]) -> Vec<StashRow> {
    entries
        .iter()
        .map(|entry| StashRow {
            index: i32::try_from(entry.index).unwrap_or(i32::MAX),
            selector: SharedString::from(entry.selector.as_str()),
            summary: SharedString::from(entry.summary()),
            branch: SharedString::from(entry.branch().unwrap_or_default()),
        })
        .collect()
}

/// 构造冲突文件行（三路编辑器的左侧列表）。
#[must_use]
pub fn build_conflict_file_rows(
    entries: &[gt_service::ConflictedEntry],
    selected: Option<usize>,
) -> Vec<ConflictFileRow> {
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| ConflictFileRow {
            row_index: i32::try_from(index).unwrap_or(i32::MAX),
            path: SharedString::from(entry.path.as_str()),
            code: SharedString::from(entry.code.as_str()),
            hunks: i32::try_from(entry.hunks).unwrap_or(i32::MAX),
            selected: selected == Some(index),
        })
        .collect()
}

/// 构造三路编辑器的内容行。
///
/// 上下文行与冲突块共用一种行结构：`kind == 0` 时三列都为空、只有 `result`
/// 有内容（渲染成一行灰字）；`kind == 1` 时渲染成三栏卡片。
/// 用一种结构而不是两种，是因为 Slint 的 `ListView` 同一次只能迭代一种模型。
#[must_use]
pub fn build_conflict_rows(
    file: &gt_diff::ConflictFile,
    resolutions: &[Option<gt_diff::Resolution>],
) -> Vec<ConflictRow> {
    let mut rows = Vec::new();
    let mut block = 0_usize;
    for peer in &file.peers {
        match peer {
            gt_diff::ConflictPeer::Context(lines) => {
                if lines.is_empty() {
                    continue;
                }
                rows.push(ConflictRow {
                    kind: 0,
                    block: -1,
                    label: SharedString::new(),
                    ours: SharedString::new(),
                    theirs: SharedString::new(),
                    result: SharedString::from(lines.join("\n")),
                    decided: true,
                });
            }
            gt_diff::ConflictPeer::Hunk(hunk) => {
                let resolution = resolutions.get(block).and_then(Option::as_ref);
                let result = match resolution {
                    Some(gt_diff::Resolution::Ours) => hunk.ours.join("\n"),
                    Some(gt_diff::Resolution::Theirs) => hunk.theirs.join("\n"),
                    Some(gt_diff::Resolution::Both) => {
                        format!("{}\n{}", hunk.ours.join("\n"), hunk.theirs.join("\n"))
                    }
                    Some(gt_diff::Resolution::Base) => {
                        hunk.base.as_deref().unwrap_or(&[]).join("\n")
                    }
                    Some(gt_diff::Resolution::Manual(lines)) => lines.join("\n"),
                    None => "（未解决）".to_owned(),
                };
                rows.push(ConflictRow {
                    kind: 1,
                    block: i32::try_from(block).unwrap_or(i32::MAX),
                    label: SharedString::from(format!(
                        "第 {} 处冲突 · 起始第 {} 行 · {} 行 / {} 行",
                        block + 1,
                        hunk.start_line,
                        hunk.ours.len(),
                        hunk.theirs.len()
                    )),
                    ours: SharedString::from(hunk.ours.join("\n")),
                    theirs: SharedString::from(hunk.theirs.join("\n")),
                    result: SharedString::from(result),
                    decided: resolution.is_some(),
                });
                block += 1;
            }
        }
    }
    rows
}

/// 冲突遮罩的摘要：还剩多少处未决定。
#[must_use]
pub fn conflict_summary(
    file: &gt_diff::ConflictFile,
    resolutions: &[Option<gt_diff::Resolution>],
) -> String {
    let total = file.hunk_count();
    let decided = resolutions
        .iter()
        .take(total)
        .filter(|item| item.is_some())
        .count();
    if total == 0 {
        return "该文件已无冲突标记，可直接标记为已解决".to_owned();
    }
    format!(
        "{decided} / {total} 处冲突已决定{}",
        if decided == total {
            "　（全部已决定，可标记为已解决）"
        } else {
            ""
        }
    )
}

/// 构造交互式变基的步骤行。
#[must_use]
pub fn build_rebase_step_rows(
    plan: &gt_backend::RebasePlan,
    selected: Option<usize>,
) -> Vec<RebaseStepRow> {
    plan.steps
        .iter()
        .enumerate()
        .map(|(index, step)| RebaseStepRow {
            row_index: i32::try_from(index).unwrap_or(i32::MAX),
            short_id: SharedString::from(step.short()),
            subject: SharedString::from(step.subject.as_str()),
            action: rebase_action_code(step.action),
            selected: selected == Some(index),
            has_message: step
                .message
                .as_deref()
                .is_some_and(|message| !message.trim().is_empty()),
        })
        .collect()
}

/// 变基动作 → 界面下标（与下拉/按钮顺序一致）。
#[must_use]
pub fn rebase_action_code(action: gt_backend::RebaseAction) -> i32 {
    match action {
        gt_backend::RebaseAction::Pick => 0,
        gt_backend::RebaseAction::Reword => 1,
        gt_backend::RebaseAction::Squash => 2,
        gt_backend::RebaseAction::Fixup => 3,
        gt_backend::RebaseAction::Drop => 4,
    }
}

/// 界面下标 → 变基动作。
#[must_use]
pub fn rebase_action_from_code(code: i32) -> Option<gt_backend::RebaseAction> {
    match code {
        0 => Some(gt_backend::RebaseAction::Pick),
        1 => Some(gt_backend::RebaseAction::Reword),
        2 => Some(gt_backend::RebaseAction::Squash),
        3 => Some(gt_backend::RebaseAction::Fixup),
        4 => Some(gt_backend::RebaseAction::Drop),
        _ => None,
    }
}

/// 构造"两提交比较 / 快照浏览"的文件行。
#[must_use]
pub fn build_compare_rows(
    changes: &[gt_core::FileChange],
    selected: Option<usize>,
) -> Vec<CompareRow> {
    changes
        .iter()
        .enumerate()
        .map(|(index, change)| CompareRow {
            path: SharedString::from(change.path.as_str()),
            code: SharedString::from(change.kind.letter().to_string()),
            status: SharedString::from(change.kind.label()),
            selected: selected == Some(index),
        })
        .collect()
}

/// 重置的确认文案（FR-MRG-06 / D4：丢失什么 / 不可逆 / 替代做法）。
#[must_use]
pub fn reset_confirmation(target: &str, mode: gt_backend::ResetMode) -> (String, String, bool) {
    match mode {
        gt_backend::ResetMode::Soft => (
            format!("把当前分支重置到 {target}？"),
            "软重置只移动分支指针：被回退的提交内容会完整保留在暂存区，\
             你可以立刻重新提交或修改。\n\n这是三种模式里唯一不丢东西的一种。"
                .to_owned(),
            false,
        ),
        gt_backend::ResetMode::Mixed => (
            format!("把当前分支重置到 {target}？"),
            "混合重置会移动分支指针并清空暂存区：被回退的提交内容仍在工作区，\
             但「已暂存」状态会丢失，需要重新选择要提交的改动。\n\n\
             替代做法：用软重置，保留暂存状态。"
                .to_owned(),
            false,
        ),
        gt_backend::ResetMode::Hard => (
            format!("硬重置到 {target}？"),
            "硬重置会把分支、索引与工作区全部改成目标提交的状态：\
             当前所有未提交的修改与未推送的提交都将永久丢失，Git 无法找回。\n\n\
             替代做法：先「贮藏」当前改动，或改用软重置后再手动处理。"
                .to_owned(),
            true,
        ),
    }
}

/// 贮藏删除的确认文案。
#[must_use]
pub fn stash_drop_confirmation(selector: &str) -> (String, String, bool) {
    (
        format!("删除 {selector}？"),
        "删除后该条贮藏不再出现在列表里。它的提交通常还留在对象库中，\
         但只能通过 `git fsck --lost-found` 手工找回。\n\n\
         替代做法：需要时先「应用」再用，或「从贮藏创建分支」把它固定下来。"
            .to_owned(),
        true,
    )
}

/// 变基跳过的确认文案。
#[must_use]
pub fn rebase_skip_confirmation() -> (String, String, bool) {
    (
        "跳过当前提交？".to_owned(),
        "被跳过的这个提交不会出现在变基结果里。如果它的改动还没别的地方保存，\
         这些改动将只存在于 reflog（`git reflog`）可追溯的旧提交中。\n\n\
         替代做法：先「中止变基」，把该提交拣选到别处，或解决冲突后「继续」。"
            .to_owned(),
        true,
    )
}

/// 取某一侧解决冲突的确认文案（会覆盖工作区文件）。
#[must_use]
pub fn take_side_confirmation(path: &str, side: &str) -> (String, String, bool) {
    (
        format!("用{side}的版本覆盖 {path}？"),
        "这会丢弃该文件里另一侧的改动与所有冲突标记，且无法撤销。\n\n\
         替代做法：在冲突编辑器里逐块选择，只保留你需要的部分。"
            .to_owned(),
        true,
    )
}

#[cfg(test)]
mod advanced_tests {
    use super::{
        build_compare_rows, build_conflict_rows, build_rebase_step_rows, build_stash_rows,
        conflict_summary, rebase_action_code, rebase_action_from_code, reset_confirmation,
        resolution_from_code, resolve_code,
    };
    use gt_backend::{RebaseAction, RebasePlan, RebaseStep, ResetMode, StashEntry};
    use gt_core::{ChangeKind, FileChange};
    use gt_diff::{ConflictFile, Resolution};

    #[test]
    fn stash_rows_strip_the_wip_prefix() {
        let entries = vec![StashEntry {
            index: 0,
            selector: "stash@{0}".to_owned(),
            message: "WIP on main: 1a2b3c4 修复登录".to_owned(),
        }];
        let rows = build_stash_rows(&entries);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].summary.as_str(), "修复登录");
        assert_eq!(rows[0].branch.as_str(), "main");
        assert_eq!(rows[0].index, 0);
    }

    #[test]
    fn conflict_rows_pairs_context_and_blocks() {
        let text = "line 1\n<<<<<<< HEAD\na\n=======\nb\n>>>>>>> x\nline 2\n";
        let file = ConflictFile::parse("a.txt", text);
        let rows = build_conflict_rows(&file, &[None]);
        assert_eq!(rows.len(), 3, "{rows:?}");
        assert_eq!(rows[0].kind, 0);
        assert_eq!(rows[1].kind, 1);
        assert_eq!(rows[1].block, 0);
        assert!(!rows[1].decided);
        assert!(rows[1].label.as_str().contains("第 1 处冲突"));
        assert_eq!(rows[2].kind, 0);
        assert_eq!(conflict_summary(&file, &[None]), "0 / 1 处冲突已决定");

        let decided = build_conflict_rows(&file, &[Some(Resolution::Theirs)]);
        assert!(decided[1].decided);
        assert_eq!(decided[1].result.as_str(), "b");
        assert!(conflict_summary(&file, &[Some(Resolution::Theirs)]).contains("可标记为已解决"));
    }

    #[test]
    fn resolution_codes_round_trip() {
        assert_eq!(
            resolution_from_code(resolve_code::OURS),
            Some(Resolution::Ours)
        );
        assert_eq!(
            resolution_from_code(resolve_code::BASE),
            Some(Resolution::Base)
        );
        assert_eq!(resolution_from_code(9), None);
    }

    #[test]
    fn rebase_action_codes_round_trip() {
        for action in RebaseAction::ALL {
            assert_eq!(
                rebase_action_from_code(rebase_action_code(action)),
                Some(action)
            );
        }
        assert_eq!(rebase_action_from_code(9), None);
    }

    #[test]
    fn rebase_rows_carry_actions_and_message_state() {
        let mut plan = RebasePlan {
            onto: "main".to_owned(),
            base: "abc".to_owned(),
            head: None,
            merges_dropped: 0,
            steps: vec![
                RebaseStep::new("1111111111", "一"),
                RebaseStep::new("2222222222", "二"),
            ],
        };
        plan.steps[1].action = RebaseAction::Reword;
        let rows = build_rebase_step_rows(&plan, Some(1));
        assert_eq!(rows[0].action, 0);
        assert_eq!(rows[1].action, 1);
        assert!(rows[1].selected);
        assert!(!rows[1].has_message, "未填消息时应提示用户补");

        plan.steps[1].message = Some("新消息".to_owned());
        let rows = build_rebase_step_rows(&plan, None);
        assert!(rows[1].has_message);
    }

    #[test]
    fn compare_rows_use_git_letters() {
        let changes = vec![
            FileChange::new("a.txt", ChangeKind::Modified),
            FileChange::with_old_path("b.txt", "c.txt", ChangeKind::Renamed),
        ];
        let rows = build_compare_rows(&changes, Some(0));
        assert_eq!(rows[0].code.as_str(), "M");
        assert_eq!(rows[1].code.as_str(), "R");
        assert!(rows[0].selected);
    }

    #[test]
    fn reset_confirmation_flags_hard_as_dangerous() {
        let (_, _, danger) = reset_confirmation("HEAD~1", ResetMode::Soft);
        assert!(!danger);
        let (title, detail, danger) = reset_confirmation("HEAD~1", ResetMode::Hard);
        assert!(danger);
        assert!(title.contains("硬重置"));
        assert!(detail.contains("永久丢失"));
        assert!(detail.contains("替代做法"), "高危文案必须给出替代做法");
    }
}

/// 把 blame 载荷拆成界面行（FR-BLM-02）。
///
/// 提交显示短哈希（前 8 位）；未提交行（全零提交）显示"未提交"。
#[must_use]
pub fn build_blame_rows(
    rows: &[BlameLine],
    selected: Option<usize>,
    now: i64,
) -> Vec<BlameRow> {
    rows.iter()
        .enumerate()
        .map(|(index, row)| {
            let uncommitted = row.commit.bytes().all(|b| b == b'0');
            BlameRow {
                line_no: i32::try_from(row.final_line).unwrap_or(0),
                commit: SharedString::from(if uncommitted {
                    "未提交"
                } else {
                    &row.commit[..row.commit.len().min(8)]
                }),
                author: SharedString::from(row.author.as_str()),
                when: SharedString::from(relative_time(row.author_time, now)),
                summary: SharedString::from(row.summary.as_str()),
                is_selected: selected == Some(index),
                is_boundary: row.boundary,
            }
        })
        .collect()
}

/// 把文件历史拆成界面行（FR-BLM-01）。
#[must_use]
pub fn build_history_rows(
    rows: &[FileHistoryEntry],
    selected: Option<usize>,
    now: i64,
) -> Vec<HistRow> {
    rows.iter()
        .enumerate()
        .map(|(index, row)| HistRow {
            short_id: SharedString::from(row.short.as_str()),
            subject: SharedString::from(row.summary.as_str()),
            author: SharedString::from(row.author.as_str()),
            when: SharedString::from(relative_time(row.author_time, now)),
            is_selected: selected == Some(index),
        })
        .collect()
}
