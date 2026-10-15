//! 差异计算实现（`imara-diff` Histogram 算法）。

use imara_diff::{Algorithm, Diff, InternedInput, TokenSource};

use crate::model::{
    DiffHunk, DiffLine, DiffOptions, DiffPayload, DiffStats, FileDiff, LineKind, LineSpan,
    BINARY_SNIFF_LEN,
};

/// 行级 diff 的 token 源：把文本切为"含换行符的行"。
///
/// 与 `imara-diff` 内置的 `&str` 行切分保持一致（`split_inclusive('\n')`），
/// 从而"是否忽略空白"只影响参与比较的 token 内容，不影响行与行号的对应关系。
#[derive(Debug, Clone, Copy)]
struct LineSource<'a> {
    text: &'a str,
    trim: bool,
}

/// [`LineSource`] 的迭代器。
struct LineTokens<'a> {
    inner: std::str::SplitInclusive<'a, char>,
    trim: bool,
}

impl<'a> Iterator for LineTokens<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        let line = self.inner.next()?;
        Some(if self.trim { line.trim() } else { line })
    }
}

impl<'a> TokenSource for LineSource<'a> {
    type Token = &'a str;
    type Tokenizer = LineTokens<'a>;

    fn tokenize(&self) -> Self::Tokenizer {
        LineTokens {
            inner: self.text.split_inclusive('\n'),
            trim: self.trim,
        }
    }

    fn estimate_tokens(&self) -> u32 {
        // 仅用于预分配容量，允许高估。
        u32::try_from(self.text.len() / 24 + 1).unwrap_or(u32::MAX)
    }
}

/// 词级 diff 的 token 源：把一行切为"连续的空白"与"连续的非空白"。
///
/// 该切分是无损的（拼接所有 token 等于原文），因此可以按 token 的变更状态
/// 直接回填出高亮片段。
#[derive(Debug, Clone, Copy)]
struct WordSource<'a> {
    text: &'a str,
}

/// [`WordSource`] 的迭代器。
struct WordTokens<'a> {
    rest: &'a str,
}

impl<'a> Iterator for WordTokens<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        if self.rest.is_empty() {
            return None;
        }

        let mut chars = self.rest.char_indices();
        let first_is_whitespace = chars.next()?.1.is_whitespace();

        let mut boundary = self.rest.len();
        for (index, ch) in chars {
            if ch.is_whitespace() != first_is_whitespace {
                boundary = index;
                break;
            }
        }

        let (head, tail) = self.rest.split_at(boundary);
        self.rest = tail;
        Some(head)
    }
}

impl<'a> TokenSource for WordSource<'a> {
    type Token = &'a str;
    type Tokenizer = WordTokens<'a>;

    fn tokenize(&self) -> Self::Tokenizer {
        WordTokens { rest: self.text }
    }

    fn estimate_tokens(&self) -> u32 {
        u32::try_from(self.text.len() / 4 + 1).unwrap_or(u32::MAX)
    }
}

/// 判断字节序列是否为二进制：前 [`BINARY_SNIFF_LEN`] 字节内出现 NUL
/// （与 `git` 的 `buffer_is_binary` 判定一致）。
#[must_use]
pub fn is_binary(bytes: &[u8]) -> bool {
    let sniff_len = bytes.len().min(BINARY_SNIFF_LEN);
    bytes[..sniff_len].contains(&0)
}

/// 计算两侧文本的行级差异，返回分块结果（含上下文行）。
///
/// 两侧完全相同（含同为空串）时返回空向量。
#[must_use]
pub fn diff_lines(old: &str, new: &str, options: &DiffOptions) -> Vec<DiffHunk> {
    let input = InternedInput::new(
        LineSource {
            text: old,
            trim: options.ignore_whitespace,
        },
        LineSource {
            text: new,
            trim: options.ignore_whitespace,
        },
    );

    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);

    if diff.count_additions() == 0 && diff.count_removals() == 0 {
        return Vec::new();
    }

    let old_lines: Vec<&str> = old.split_inclusive('\n').collect();
    let new_lines: Vec<&str> = new.split_inclusive('\n').collect();

    let flat = flatten(&diff, &old_lines, &new_lines);
    let mut hunks = group_hunks(&flat, options.context_lines);

    if options.word_level {
        for hunk in &mut hunks {
            apply_word_spans(hunk);
        }
    }

    hunks
}

/// 把 diff 展开为"行 + 行号"的平坦序列。
///
/// 顺序即 unified diff 的输出顺序：相等区间 → 删除行 → 新增行 → …… → 尾部相等区间。
fn flatten(diff: &Diff, old_lines: &[&str], new_lines: &[&str]) -> Vec<DiffLine> {
    let mut flat: Vec<DiffLine> = Vec::with_capacity(old_lines.len() + new_lines.len());
    let mut old_index = 0usize;
    let mut new_index = 0usize;
    let mut old_lineno = 1u32;
    let mut new_lineno = 1u32;

    for hunk in diff.hunks() {
        let before_start = hunk.before.start as usize;
        let after_start = hunk.after.start as usize;

        // 相等区间：两侧同时前进，行号同步递增。
        while old_index < before_start {
            flat.push(DiffLine::new(
                LineKind::Context,
                Some(old_lineno),
                Some(new_lineno),
                strip_newline(old_lines[old_index]),
            ));
            old_index += 1;
            old_lineno += 1;
            new_index += 1;
            new_lineno += 1;
        }
        debug_assert_eq!(new_index, after_start, "相等区间的两侧长度必须一致");

        let before_end = hunk.before.end as usize;
        let after_end = hunk.after.end as usize;

        while old_index < before_end {
            flat.push(DiffLine::new(
                LineKind::Removed,
                Some(old_lineno),
                None,
                strip_newline(old_lines[old_index]),
            ));
            old_index += 1;
            old_lineno += 1;
        }

        while new_index < after_end {
            flat.push(DiffLine::new(
                LineKind::Added,
                None,
                Some(new_lineno),
                strip_newline(new_lines[new_index]),
            ));
            new_index += 1;
            new_lineno += 1;
        }
    }

    // 尾部相等区间：两侧剩余行数必须一致；只按旧侧索引推进即可。
    debug_assert_eq!(
        new_lines.len() - new_index,
        old_lines.len() - old_index,
        "尾部相等区间的两侧长度必须一致"
    );
    while old_index < old_lines.len() {
        flat.push(DiffLine::new(
            LineKind::Context,
            Some(old_lineno),
            Some(new_lineno),
            strip_newline(old_lines[old_index]),
        ));
        old_index += 1;
        old_lineno += 1;
        new_lineno += 1;
    }

    flat
}

/// 按上下文行数把平坦序列聚合为 hunk。
fn group_hunks(flat: &[DiffLine], context: usize) -> Vec<DiffHunk> {
    let mut changed_indices = Vec::new();
    for (index, line) in flat.iter().enumerate() {
        if line.kind.is_change() {
            changed_indices.push(index);
        }
    }
    if changed_indices.is_empty() {
        return Vec::new();
    }

    // 前缀计数：`old_prefix[i]` = `flat[..i]` 中旧侧行数，供"纯插入"块推导起始行号。
    let mut old_prefix = vec![0u32; flat.len() + 1];
    let mut new_prefix = vec![0u32; flat.len() + 1];
    for index in 0..flat.len() {
        old_prefix[index + 1] = old_prefix[index] + u32::from(flat[index].old_lineno.is_some());
        new_prefix[index + 1] = new_prefix[index] + u32::from(flat[index].new_lineno.is_some());
    }

    // 相邻变更的距离不超过 2*context 时并入同一个 hunk（与 git 的合并规则一致）。
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for &index in &changed_indices {
        let start = index.saturating_sub(context);
        let end = (index + context + 1).min(flat.len());
        match ranges.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => ranges.push((start, end)),
        }
    }

    ranges
        .into_iter()
        .map(|(start, end)| {
            let lines = flat[start..end].to_vec();
            let old_lines = lines
                .iter()
                .filter(|line| line.old_lineno.is_some())
                .count();
            let new_lines = lines
                .iter()
                .filter(|line| line.new_lineno.is_some())
                .count();
            let old_start = lines
                .iter()
                .find_map(|line| line.old_lineno)
                .unwrap_or(old_prefix[start]);
            let new_start = lines
                .iter()
                .find_map(|line| line.new_lineno)
                .unwrap_or(new_prefix[start]);
            DiffHunk {
                old_start,
                old_lines: old_lines as u32,
                new_start,
                new_lines: new_lines as u32,
                lines,
            }
        })
        .collect()
}

/// 为同一个 hunk 内成对的删除/新增行回填词级高亮片段。
fn apply_word_spans(hunk: &mut DiffHunk) {
    let mut index = 0usize;
    while index < hunk.lines.len() {
        if hunk.lines[index].kind != LineKind::Removed {
            index += 1;
            continue;
        }

        let removed_start = index;
        while index < hunk.lines.len() && hunk.lines[index].kind == LineKind::Removed {
            index += 1;
        }
        let removed_end = index;

        let added_start = index;
        while index < hunk.lines.len() && hunk.lines[index].kind == LineKind::Added {
            index += 1;
        }
        let added_end = index;

        let pairs = (removed_end - removed_start).min(added_end - added_start);
        for offset in 0..pairs {
            let (old_spans, new_spans) = word_spans(
                &hunk.lines[removed_start + offset].text,
                &hunk.lines[added_start + offset].text,
            );
            hunk.lines[removed_start + offset].spans = old_spans;
            hunk.lines[added_start + offset].spans = new_spans;
        }
    }
}

/// 计算两行文本的词级差异，返回（旧侧片段, 新侧片段）。
///
/// 两侧片段拼接后分别等于原文，因此 UI 可以直接按顺序渲染。
#[must_use]
pub fn word_spans(old_line: &str, new_line: &str) -> (Vec<LineSpan>, Vec<LineSpan>) {
    if old_line == new_line {
        let spans = if old_line.is_empty() {
            Vec::new()
        } else {
            vec![LineSpan::new(false, old_line)]
        };
        return (spans.clone(), spans);
    }

    let old_words: Vec<&str> = WordSource { text: old_line }.tokenize().collect();
    let new_words: Vec<&str> = WordSource { text: new_line }.tokenize().collect();

    let input = InternedInput::new(WordSource { text: old_line }, WordSource { text: new_line });
    let diff = Diff::compute(Algorithm::Histogram, &input);

    (
        collect_spans(&old_words, |index| diff.is_removed(index)),
        collect_spans(&new_words, |index| diff.is_added(index)),
    )
}

/// 把连续的同类 token 合并为片段。
fn collect_spans(words: &[&str], is_changed: impl Fn(u32) -> bool) -> Vec<LineSpan> {
    let mut spans: Vec<LineSpan> = Vec::new();
    for (index, word) in words.iter().enumerate() {
        let changed = is_changed(index as u32);
        match spans.last_mut() {
            Some(last) if last.changed == changed => last.text.push_str(word),
            _ => spans.push(LineSpan::new(changed, *word)),
        }
    }
    spans
}

/// 去除行尾换行符（保留 `\r`，与 `git diff` 对 CRLF 文件的处理一致）。
fn strip_newline(line: &str) -> &str {
    line.strip_suffix('\n').unwrap_or(line)
}

/// 计算两侧字节内容的完整差异结果。
///
/// `None` 表示该侧文件不存在（新增 / 删除）。判定顺序：先二进制，再大小阈值，
/// 最后才做行级 diff —— 与架构规划 5.5 的"防止内存尖峰最有效的一刀"一致。
#[must_use]
pub fn diff_bytes(
    old_path: &str,
    new_path: &str,
    old: Option<&[u8]>,
    new: Option<&[u8]>,
    options: &DiffOptions,
) -> FileDiff {
    let old_len = old.map_or(0, <[u8]>::len);
    let new_len = new.map_or(0, <[u8]>::len);

    if old.is_some_and(is_binary) || new.is_some_and(is_binary) {
        return FileDiff::new(
            old_path,
            new_path,
            DiffPayload::Binary,
            DiffStats {
                added: 0,
                removed: 0,
                binary: true,
            },
        );
    }

    if old_len > options.max_bytes || new_len > options.max_bytes {
        return FileDiff::new(
            old_path,
            new_path,
            DiffPayload::TooLarge {
                old_bytes: old_len,
                new_bytes: new_len,
            },
            DiffStats::default(),
        );
    }

    let empty = String::new();
    let old_owned = old.map(String::from_utf8_lossy);
    let new_owned = new.map(String::from_utf8_lossy);
    let old_text: &str = old_owned.as_deref().unwrap_or(empty.as_str());
    let new_text: &str = new_owned.as_deref().unwrap_or(empty.as_str());

    let hunks = diff_lines(old_text, new_text, options);
    let added = hunks.iter().map(DiffHunk::added).sum();
    let removed = hunks.iter().map(DiffHunk::removed).sum();

    FileDiff::new(
        old_path,
        new_path,
        DiffPayload::Text(hunks),
        DiffStats {
            added,
            removed,
            binary: false,
        },
    )
}

/// 两侧均为文本时的便捷入口。
#[must_use]
pub fn diff_text(
    old_path: &str,
    new_path: &str,
    old: &str,
    new: &str,
    options: &DiffOptions,
) -> FileDiff {
    diff_bytes(
        old_path,
        new_path,
        Some(old.as_bytes()),
        Some(new.as_bytes()),
        options,
    )
}

#[cfg(test)]
mod tests {
    use super::{diff_bytes, diff_lines, diff_text, is_binary, word_spans};
    use crate::model::{DiffOptions, DiffPayload, LineKind};

    fn hunks(old: &str, new: &str) -> Vec<crate::model::DiffHunk> {
        diff_lines(old, new, &DiffOptions::default())
    }

    #[test]
    fn identical_text_has_no_hunks() {
        assert!(hunks("a\nb\n", "a\nb\n").is_empty());
        assert!(hunks("", "").is_empty());
    }

    #[test]
    fn modification_produces_paired_removal_and_addition() {
        let result = hunks("a\nb\nc\n", "a\nB\nc\n");
        assert_eq!(result.len(), 1);
        let hunk = &result[0];
        assert_eq!(hunk.added(), 1);
        assert_eq!(hunk.removed(), 1);
        assert_eq!(hunk.header(), "@@ -1,3 +1,3 @@");
        // 平坦序列为：上下文 a、删除 b、新增 B、上下文 c，共 4 行。
        assert_eq!(hunk.lines.len(), 4);
        assert_eq!(hunk.old_lines, 3);
        assert_eq!(hunk.new_lines, 3);
    }

    #[test]
    fn insertion_between_lines_keeps_context_counts() {
        // 与 `git diff` 一致：默认 3 行上下文下，插入点的上下文计入两侧行数。
        let result = hunks("a\nb\n", "a\nx\nb\n");
        assert_eq!(result.len(), 1);
        let hunk = &result[0];
        assert_eq!(hunk.header(), "@@ -1,2 +1,3 @@");
        assert_eq!(hunk.added(), 1);
        assert_eq!(hunk.removed(), 0);
        assert_eq!(hunk.old_lines, 2);
        assert_eq!(hunk.new_lines, 3);
    }

    #[test]
    fn pure_insertion_with_zero_context_has_empty_old_range() {
        let options = DiffOptions::default().with_context_lines(0);
        let result = diff_lines("a\nb\n", "a\nx\nb\n", &options);
        assert_eq!(result.len(), 1);
        let hunk = &result[0];
        assert_eq!(hunk.old_start, 1, "插入点在旧文件第 1 行之后");
        assert_eq!(hunk.old_lines, 0);
        assert_eq!(hunk.new_start, 2);
        assert_eq!(hunk.new_lines, 1);
        assert_eq!(hunk.header(), "@@ -1,0 +2 @@");
    }

    #[test]
    fn insertion_at_beginning_keeps_old_start_one() {
        // git 对"在首行之前插入"给出 `@@ -1 +1,2 @@`，旧侧起始行号为 1。
        let result = hunks("b\n", "a\nb\n");
        assert_eq!(result[0].old_start, 1);
        assert_eq!(result[0].old_lines, 1);
        assert_eq!(result[0].added(), 1);
    }

    #[test]
    fn insertion_at_beginning_with_zero_context_starts_at_zero() {
        let options = DiffOptions::default().with_context_lines(0);
        let result = diff_lines("b\n", "a\nb\n", &options);
        assert_eq!(
            result[0].old_start, 0,
            "文件开头插入且无上下文时起始行号为 0"
        );
        assert_eq!(result[0].old_lines, 0);
        assert_eq!(result[0].new_start, 1);
    }

    #[test]
    fn addition_from_empty_file_counts_all_lines() {
        let result = hunks("", "a\nb\n");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].added(), 2);
        assert_eq!(result[0].removed(), 0);
        assert_eq!(result[0].old_start, 0);
        assert_eq!(result[0].old_lines, 0);
    }

    #[test]
    fn deletion_to_empty_file_counts_all_lines() {
        let result = hunks("a\nb\n", "");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].removed(), 2);
        assert_eq!(result[0].added(), 0);
        assert_eq!(result[0].new_start, 0);
    }

    #[test]
    fn line_numbers_track_both_sides() {
        let result = hunks("1\n2\n3\n4\n5\n", "1\n2\nX\n4\n5\n");
        let lines = &result[0].lines;
        let removed = lines
            .iter()
            .find(|line| line.kind == LineKind::Removed)
            .expect("应有删除行");
        let added = lines
            .iter()
            .find(|line| line.kind == LineKind::Added)
            .expect("应有新增行");
        assert_eq!(
            (
                removed.old_lineno,
                removed.new_lineno,
                removed.text.as_str()
            ),
            (Some(3), None, "3")
        );
        assert_eq!(
            (added.old_lineno, added.new_lineno, added.text.as_str()),
            (None, Some(3), "X")
        );
    }

    #[test]
    fn separated_changes_form_separate_hunks() {
        let old = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n20\n";
        let new = "X\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\nY\n";
        let result = hunks(old, new);
        assert_eq!(
            result.len(),
            2,
            "相距超过 2*context 的变更应拆分为两个 hunk"
        );
    }

    #[test]
    fn word_spans_reconstruct_original_text() {
        let (old_spans, new_spans) = word_spans("let x = 1;", "let x = 2;");
        let old_joined: String = old_spans.iter().map(|span| span.text.as_str()).collect();
        let new_joined: String = new_spans.iter().map(|span| span.text.as_str()).collect();
        assert_eq!(old_joined, "let x = 1;");
        assert_eq!(new_joined, "let x = 2;");
        assert!(
            old_spans.iter().any(|span| span.changed),
            "应有被标记为变更的片段"
        );
        assert!(new_spans.iter().any(|span| span.changed));
    }

    #[test]
    fn word_spans_marks_only_the_changed_word() {
        let (old_spans, _) = word_spans("hello world", "hello there");
        let changed: String = old_spans
            .iter()
            .filter(|span| span.changed)
            .map(|span| span.text.as_str())
            .collect();
        assert_eq!(changed, "world");
    }

    #[test]
    fn identical_lines_have_single_unchanged_span() {
        let (old_spans, new_spans) = word_spans("same", "same");
        assert_eq!(old_spans.len(), 1);
        assert!(!old_spans[0].changed);
        assert_eq!(old_spans, new_spans);
    }

    #[test]
    fn hunk_lines_carry_word_spans() {
        let result = hunks("foo bar\n", "foo baz\n");
        let removed = &result[0].lines[0];
        let added = &result[0].lines[1];
        assert!(!removed.spans.is_empty(), "删除行应带词级片段");
        assert!(!added.spans.is_empty(), "新增行应带词级片段");
    }

    #[test]
    fn word_level_can_be_disabled() {
        let options = DiffOptions::default().without_word_level();
        let result = diff_lines("foo bar\n", "foo baz\n", &options);
        assert!(result[0].lines[0].spans.is_empty());
    }

    #[test]
    fn ignore_whitespace_suppresses_pure_reindent() {
        let strict = diff_lines("a\n  b\n", "a\nb\n", &DiffOptions::default());
        assert_eq!(strict.len(), 1, "默认应报出空白变化");

        let lax = DiffOptions {
            ignore_whitespace: true,
            ..DiffOptions::default()
        };
        assert!(
            diff_lines("a\n  b\n", "a\nb\n", &lax).is_empty(),
            "忽略空白后不应有任何 hunk"
        );
    }

    #[test]
    fn zero_context_produces_tight_hunks() {
        let options = DiffOptions::default().with_context_lines(0);
        let result = diff_lines("a\nb\nc\n", "a\nX\nc\n", &options);
        assert_eq!(result[0].lines.len(), 2, "仅删除行与新增行");
        assert_eq!(result[0].old_lines, 1);
        assert_eq!(result[0].new_lines, 1);
    }

    #[test]
    fn binary_detection_matches_git_heuristic() {
        assert!(is_binary(b"abc\0def"));
        assert!(!is_binary(b"plain text\n"));
        assert!(!is_binary(b""));
        // NUL 出现在嗅探窗口之外则不判为二进制。
        let mut beyond = vec![b'a'; super::BINARY_SNIFF_LEN + 10];
        beyond.push(0);
        assert!(!is_binary(&beyond));
    }

    #[test]
    fn binary_files_yield_placeholder() {
        let result = diff_bytes(
            "a.bin",
            "a.bin",
            Some(&[0u8, 1, 2]),
            Some(&[0u8, 1, 3]),
            &DiffOptions::default(),
        );
        assert!(result.is_binary());
        assert_eq!(result.stats.numstat_columns(), "-\t-");
        assert!(result.payload.hunks().is_none());
    }

    #[test]
    fn oversized_files_yield_placeholder_without_diffing() {
        let options = DiffOptions::default().with_context_lines(3);
        let big = vec![b'a'; options.max_bytes + 1];
        let result = diff_bytes("big.txt", "big.txt", Some(&big), Some(b"small"), &options);
        assert!(result.is_too_large());
        assert!(result.stats.is_empty());
        match result.payload {
            DiffPayload::TooLarge {
                old_bytes,
                new_bytes,
            } => {
                assert_eq!(old_bytes, options.max_bytes + 1);
                assert_eq!(new_bytes, 5);
            }
            other => panic!("期望 TooLarge，实际 {other:?}"),
        }
    }

    #[test]
    fn numstat_totals_match_change_counts() {
        let result = diff_text(
            "a.txt",
            "a.txt",
            "1\n2\n3\n",
            "1\nX\n3\n4\n",
            &DiffOptions::default(),
        );
        assert_eq!(result.stats.added, 2);
        assert_eq!(result.stats.removed, 1);
        assert_eq!(result.numstat_line(), "2\t1\ta.txt");
        assert!(!result.is_identical());
    }

    #[test]
    fn missing_side_is_treated_as_empty_file() {
        let added = diff_bytes(
            "new.txt",
            "new.txt",
            None,
            Some(b"a\n"),
            &DiffOptions::default(),
        );
        assert_eq!(added.stats.added, 1);
        assert_eq!(added.stats.removed, 0);

        let deleted = diff_bytes(
            "gone.txt",
            "gone.txt",
            Some(b"a\nb\n"),
            None,
            &DiffOptions::default(),
        );
        assert_eq!(deleted.stats.removed, 2);
    }

    #[test]
    fn crlf_lines_keep_carriage_return_like_git() {
        let result = hunks("a\r\nb\r\n", "a\r\nB\r\n");
        let removed = result[0]
            .lines
            .iter()
            .find(|line| line.kind == LineKind::Removed)
            .expect("应有删除行");
        assert_eq!(removed.text, "b\r", "与 git 相同：只剥离 \\n，保留 \\r");
    }
}
