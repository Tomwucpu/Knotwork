//! 由差异模型生成可落库的 unified patch（hunk 级 / 行级选择）。
//!
//! # 语义
//!
//! 生成的是**标准 git 补丁文本**，交给
//! [`gt_backend::RepoWriter::apply_patch_cached`] 用
//! `git apply --cached [--reverse]` 落库 —— 这正是 `git add -p` 的底层机制：
//! 我们只负责"用户选了哪些行"，落库交给 git，因此滤镜、属性、空白处理
//! 与命令行完全一致。
//!
//! # 行级选择规则（与 `git add -p` 的 hunk 编辑一致）
//!
//! | 原行类型 | 选中 | 结果 |
//! |---|---|---|
//! | 上下文 | — | 保留为上下文 |
//! | 新增 | 是 | 保留为 `+` |
//! | 新增 | 否 | **丢弃**（该行不进入本次暂存） |
//! | 删除 | 是 | 保留为 `-` |
//! | 删除 | 否 | **降级为上下文**（旧行在新内容里继续存在） |
//!
//! 最后一条是关键：未选中的删除**不能**直接丢弃，否则补丁会少一行上下文，
//! 导致 `git apply` 对不齐后续行。降级为上下文既让这一行"留在原处"，
//! 又保持了旧侧行数不变。

use std::fmt::Write as _;

use gt_core::{quote_path, PorcelainFormat};

use crate::model::{DiffHunk, DiffPayload, FileDiff, LineKind};

/// 被选中的 hunk；`lines` 为 [`None`] 表示整块暂存。
///
/// `lines` 中的下标指向 [`DiffHunk::lines`]，**未列出的变更行即未选中**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HunkSelection {
    /// hunk 在 `payload` 里的下标。
    pub hunk: usize,
    /// 选中的行下标；[`None`] 表示该 hunk 全选。
    pub lines: Option<Vec<usize>>,
}

impl HunkSelection {
    /// 全选某个 hunk。
    #[must_use]
    pub const fn whole(hunk: usize) -> Self {
        Self { hunk, lines: None }
    }

    /// 选中某个 hunk 里的若干行。
    #[must_use]
    pub fn lines(hunk: usize, lines: Vec<usize>) -> Self {
        Self {
            hunk,
            lines: Some(lines),
        }
    }

    /// 判断某一行是否被选中。
    #[must_use]
    pub fn includes(&self, line: usize) -> bool {
        match &self.lines {
            None => true,
            Some(lines) => lines.contains(&line),
        }
    }
}

/// 生成补丁所需的路径对。
///
/// 两侧路径通常相同；重命名场景由上层传入新路径即可（M2 暂不提供重命名
/// hunk 暂存，见模块文档）。
#[derive(Debug, Clone, Copy)]
pub struct PatchPaths<'a> {
    /// 旧侧路径（仓库相对，`/` 分隔）。
    pub old: &'a str,
    /// 新侧路径。
    pub new: &'a str,
}

impl<'a> PatchPaths<'a> {
    /// 两侧同路径。
    #[must_use]
    pub const fn same(path: &'a str) -> Self {
        Self {
            old: path,
            new: path,
        }
    }
}

/// 构造补丁文本；没有任何可落库的变更时返回 [`None`]。
///
/// 返回的文本可以直接喂给 `git apply`（不含结尾多余空行）。
///
/// # 限制
///
/// - 只支持**已跟踪文件的文本差异**。新增/删除整个文件请用文件级暂存
///   （`git add` / `git rm`），二进制不做行级选择；
/// - `selections` 里越界的 hunk 下标会被忽略（而不是 panic）—— 补丁生成
///   发生在 UI 选择之后，工作区可能已被外部改动。
#[must_use]
pub fn build_patch(
    diff: &FileDiff,
    paths: PatchPaths<'_>,
    selections: &[HunkSelection],
) -> Option<String> {
    let hunks = diff.payload.hunks()?;
    let mut body = String::new();
    let mut any = false;

    for selection in selections {
        let Some(hunk) = hunks.get(selection.hunk) else {
            continue;
        };
        let Some(rendered) = render_hunk(hunk, selection) else {
            continue;
        };
        body.push_str(&rendered);
        any = true;
    }

    if !any {
        return None;
    }

    let mut patch = String::new();
    let old_path = patch_path("a/", paths.old);
    let new_path = patch_path("b/", paths.new);
    let _ = writeln!(patch, "diff --git {old_path} {new_path}");
    let _ = writeln!(patch, "--- {old_path}");
    let _ = writeln!(patch, "+++ {new_path}");
    patch.push_str(&body);
    Some(patch)
}

/// 把整份差异转成补丁（整个文件级 hunk 全选）。
///
/// 用于"暂存该文件的所有 hunk"这类入口；语义等价于把 [`HunkSelection::whole`]
/// 逐个列出。
#[must_use]
pub fn build_patch_all(diff: &FileDiff, paths: PatchPaths<'_>) -> Option<String> {
    let count = diff.payload.hunks()?.len();
    let selections: Vec<HunkSelection> = (0..count).map(HunkSelection::whole).collect();
    build_patch(diff, paths, &selections)
}

/// 生成单个 hunk 的文本；该 hunk 没有实际变更时返回 [`None`]。
fn render_hunk(hunk: &DiffHunk, selection: &HunkSelection) -> Option<String> {
    let mut lines: Vec<(char, &str)> = Vec::with_capacity(hunk.lines.len());

    for (index, line) in hunk.lines.iter().enumerate() {
        let selected = selection.includes(index);
        match line.kind {
            LineKind::Context => lines.push((' ', line.text.as_str())),
            LineKind::Added => {
                if selected {
                    lines.push(('+', line.text.as_str()));
                }
            }
            LineKind::Removed => {
                if selected {
                    lines.push(('-', line.text.as_str()));
                } else {
                    // 未选中的删除降级为上下文（见模块文档）。
                    lines.push((' ', line.text.as_str()));
                }
            }
        }
    }

    let old_lines = lines
        .iter()
        .filter(|(prefix, _)| *prefix == ' ' || *prefix == '-')
        .count();
    let new_lines = lines
        .iter()
        .filter(|(prefix, _)| *prefix == ' ' || *prefix == '+')
        .count();

    // 没有 +/- 就说明选中的内容不构成变更，跳过整块。
    if lines.iter().all(|(prefix, _)| *prefix == ' ') {
        return None;
    }

    let mut out = String::new();
    let _ = writeln!(
        out,
        "@@ -{} +{} @@",
        format_range(hunk.old_start, old_lines),
        format_range(hunk.new_start, new_lines)
    );
    for (prefix, text) in lines {
        out.push(prefix);
        out.push_str(text);
        out.push('\n');
    }
    Some(out)
}

/// `git` 的区间写法：行数为 1 时省略 `,1`。
fn format_range(start: u32, count: usize) -> String {
    if count == 1 {
        start.to_string()
    } else {
        format!("{start},{count}")
    }
}

/// 补丁头里的路径：`a/` 前缀 + 路径，整体按 git 的 C 风格引号规则处理。
///
/// 非 ASCII 路径必须走引号形式（`"a/\344\270\255..."`）：补丁头是行式格式，
/// 空格与特殊字符会破坏解析；`git apply` 的 `unquote_c_style` 负责还原。
fn patch_path(prefix: &str, path: &str) -> String {
    let full = format!("{prefix}{path}");
    quote_path(&full, PorcelainFormat::GIT_DEFAULT)
}

/// 该差异是否适合做 hunk / 行级选择（文本、且至少有一个 hunk）。
#[must_use]
pub fn is_hunk_selectable(diff: &FileDiff) -> bool {
    match &diff.payload {
        DiffPayload::Text(hunks) => !hunks.is_empty(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{build_patch, build_patch_all, is_hunk_selectable, HunkSelection, PatchPaths};
    use crate::model::{DiffHunk, DiffLine, DiffPayload, DiffStats, FileDiff, LineKind};

    fn line(kind: LineKind, old: Option<u32>, new: Option<u32>, text: &str) -> DiffLine {
        DiffLine::new(kind, old, new, text)
    }

    /// 构造一个两 hunk 的差异：
    ///
    /// ```text
    ///  hunk0:  -b / +B  (第 2 行)
    ///  hunk1:  -d / +D  (第 4 行)，外加 -e / +E
    /// ```
    fn fixture() -> FileDiff {
        let hunk0 = DiffHunk {
            old_start: 1,
            old_lines: 3,
            new_start: 1,
            new_lines: 3,
            lines: vec![
                line(LineKind::Context, Some(1), Some(1), "a"),
                line(LineKind::Removed, Some(2), None, "b"),
                line(LineKind::Added, None, Some(2), "B"),
                line(LineKind::Context, Some(3), Some(3), "c"),
            ],
        };
        let hunk1 = DiffHunk {
            old_start: 4,
            old_lines: 4,
            new_start: 4,
            new_lines: 4,
            lines: vec![
                line(LineKind::Removed, Some(4), None, "d"),
                line(LineKind::Added, None, Some(4), "D"),
                line(LineKind::Removed, Some(5), None, "e"),
                line(LineKind::Added, None, Some(5), "E"),
                line(LineKind::Context, Some(6), Some(6), "f"),
                line(LineKind::Context, Some(7), Some(7), "g"),
            ],
        };
        FileDiff {
            old_path: "a.txt".to_owned(),
            new_path: "a.txt".to_owned(),
            payload: DiffPayload::Text(vec![hunk0, hunk1]),
            stats: DiffStats {
                added: 3,
                removed: 3,
                binary: false,
            },
        }
    }

    #[test]
    fn whole_hunk_patch_matches_the_canonical_form() {
        let diff = fixture();
        let patch = build_patch(&diff, PatchPaths::same("a.txt"), &[HunkSelection::whole(0)])
            .expect("应生成补丁");
        assert_eq!(
            patch,
            "diff --git a/a.txt b/a.txt\n\
             --- a/a.txt\n\
             +++ b/a.txt\n\
             @@ -1,3 +1,3 @@\n \
             a\n\
             -b\n\
             +B\n \
             c\n"
        );
    }

    #[test]
    fn selecting_only_the_added_line_drops_nothing_else() {
        let diff = fixture();
        // hunk0 的行下标：0=上下文 a、1=-b、2=+B、3=上下文 c
        let patch = build_patch(
            &diff,
            PatchPaths::same("a.txt"),
            &[HunkSelection::lines(0, vec![2])],
        )
        .expect("应生成补丁");
        // -b 未选中 → 降级为上下文；+B 选中 → 保留为新增。
        assert!(patch.contains(" b\n+B\n"), "{patch}");
        assert!(!patch.contains("-b"), "{patch}");
        // 旧侧 3 行（a、b、c），新侧 4 行（a、b、B、c）。
        assert!(patch.contains("@@ -1,3 +1,4 @@"), "{patch}");
    }

    #[test]
    fn selecting_only_the_removed_line_yields_a_pure_deletion() {
        let diff = fixture();
        let patch = build_patch(
            &diff,
            PatchPaths::same("a.txt"),
            &[HunkSelection::lines(0, vec![1])],
        )
        .expect("应生成补丁");
        assert!(patch.contains("-b\n"), "{patch}");
        assert!(!patch.contains("+B"), "{patch}");
        assert!(patch.contains("@@ -1,3 +1,2 @@"), "{patch}");
    }

    #[test]
    fn selecting_nothing_returns_none() {
        let diff = fixture();
        assert!(
            build_patch(
                &diff,
                PatchPaths::same("a.txt"),
                &[HunkSelection::lines(0, vec![])]
            )
            .is_none(),
            "没有选中变更不应生成补丁"
        );
    }

    #[test]
    fn partial_selection_in_a_two_change_hunk_keeps_the_other_pair() {
        let diff = fixture();
        // hunk1：0=-d 1=+D 2=-e 3=+E 4=上下文 f 5=上下文 g
        // 只选第二对（-e/+E）：第一对应该消失（+D 丢弃、-d 降级为上下文）。
        let patch = build_patch(
            &diff,
            PatchPaths::same("a.txt"),
            &[HunkSelection::lines(1, vec![2, 3])],
        )
        .expect("应生成补丁");
        assert!(!patch.contains("+D"), "{patch}");
        assert!(patch.contains("-e\n+E\n"), "{patch}");
        assert!(
            patch.contains(" d\n"),
            "未选中的删除应降级为上下文：{patch}"
        );
        assert!(patch.contains("@@ -4,4 +4,4 @@"), "{patch}");
    }

    #[test]
    fn multiple_hunks_are_concatenated_in_selection_order() {
        let diff = fixture();
        let patch = build_patch(
            &diff,
            PatchPaths::same("a.txt"),
            &[HunkSelection::whole(0), HunkSelection::whole(1)],
        )
        .expect("应生成补丁");
        assert_eq!(patch.matches("@@ ").count(), 2, "{patch}");
        assert_eq!(patch.matches("diff --git").count(), 1, "只应有一个文件头");
    }

    #[test]
    fn build_patch_all_covers_every_hunk() {
        let diff = fixture();
        let patch = build_patch_all(&diff, PatchPaths::same("a.txt")).expect("应生成补丁");
        assert!(patch.contains("-b\n+B\n"), "{patch}");
        assert!(patch.contains("-d\n+D\n"), "{patch}");
        assert!(patch.contains("-e\n+E\n"), "{patch}");
    }

    #[test]
    fn out_of_range_hunks_are_ignored() {
        let diff = fixture();
        assert!(build_patch(
            &diff,
            PatchPaths::same("a.txt"),
            &[HunkSelection::whole(99)]
        )
        .is_none());
    }

    #[test]
    fn non_ascii_paths_are_quoted_for_the_patch_header() {
        let diff = fixture();
        let patch = build_patch(
            &diff,
            PatchPaths::same("中文/文件.txt"),
            &[HunkSelection::whole(0)],
        )
        .expect("应生成补丁");
        let header = patch.lines().next().expect("有头部");
        assert!(
            header.starts_with("diff --git \""),
            "非 ASCII 路径必须加引号：{header}"
        );
        assert!(header.contains("\\344"), "应按八进制转义：{header}");
    }

    #[test]
    fn placeholder_payloads_are_not_selectable() {
        for payload in [
            DiffPayload::Binary,
            DiffPayload::TooLarge {
                old_bytes: 3_000_000,
                new_bytes: 4_000_000,
            },
            DiffPayload::Empty,
        ] {
            let diff = FileDiff {
                old_path: "x".to_owned(),
                new_path: "x".to_owned(),
                payload,
                stats: DiffStats::default(),
            };
            assert!(!is_hunk_selectable(&diff));
            assert!(
                build_patch(&diff, PatchPaths::same("x"), &[HunkSelection::whole(0)]).is_none()
            );
        }
    }

    #[test]
    fn zero_context_hunks_are_supported() {
        // 空文件里只有一行新增：没有上下文行也必须能生成补丁。
        let diff = FileDiff {
            old_path: "new.txt".to_owned(),
            new_path: "new.txt".to_owned(),
            payload: DiffPayload::Text(vec![DiffHunk {
                old_start: 0,
                old_lines: 0,
                new_start: 1,
                new_lines: 1,
                lines: vec![line(LineKind::Added, None, Some(1), "hello")],
            }]),
            stats: DiffStats {
                added: 1,
                removed: 0,
                binary: false,
            },
        };
        let patch = build_patch(
            &diff,
            PatchPaths::same("new.txt"),
            &[HunkSelection::whole(0)],
        )
        .expect("应生成补丁");
        assert!(patch.contains("@@ -0,0 +1 @@"), "{patch}");
        assert!(patch.contains("+hello\n"), "{patch}");
    }
}
