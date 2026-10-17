//! 冲突文件模型：标记解析与逐块求解（FR-CFL-01/02/03）。
//!
//! # 为什么解析磁盘上的标记，而不是直接读三个 stage
//!
//! 冲突时 git 会把三方内容合并进工作区文件，并写入
//! `<<<<<<< ours` / `=======` / `>>>>>>> theirs` 标记（开启 `diff3` 或
//! `zdiff3` 时还多一段 `||||||| base`）。直接在标记上作文章有三个好处：
//!
//! 1. **上下文天然对齐**：非冲突行原样保留，三栏视图的"结果"列就是文件本身；
//! 2. **块边界就是冲突边界**：不需要再对 ours/theirs 做一次 diff 去猜边界；
//! 3. **FR-CFL-03 的"按标记解析"与主流程共用一套代码** —— 快捷入口不是另一条实现，
//!    因此不会出现"快捷合并的结果和逐块合并不一致"。
//!
//! 代价是**依赖工作区文件里确实有标记**。这一点由 git 保证：只有在
//! `merge.conflictStyle` 被改成 `union` 或用户手工删掉标记时才会缺失，
//! 本模块对那种情况给出 `has_markers == false`，由上层提示"请手动编辑"。
//!
//! # 行尾保真
//!
//! 冲突文件常见于 Windows（CRLF）。解析时按 `\n` 切分并保留每行的
//! `\r`，重建时**沿用原文的行尾风格**与"是否有末尾换行"——任何一处走样都会
//! 让"解决冲突"变成"顺手改了整个文件的行尾"，那是审查时最难解释的 diff。

use std::fmt::Write as _;

/// 冲突块的一侧来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConflictSide {
    /// 左侧：当前分支（ours / `:2:`）。
    Ours,
    /// 右侧：被合入的分支（theirs / `:3:`）。
    Theirs,
    /// 基线：共同祖先（base / `:1:`，仅 diff3 风格存在）。
    Base,
}

impl ConflictSide {
    /// 中文标签。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Ours => "当前分支",
            Self::Theirs => "合入分支",
            Self::Base => "共同祖先",
        }
    }
}

/// 单个冲突块的解法。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// 取左侧（ours）。
    Ours,
    /// 取右侧（theirs）。
    Theirs,
    /// 两侧都保留（先左后右）。
    Both,
    /// 取基线（base）—— "撤销这一处双方的改动"。
    Base,
    /// 手写结果（中心栏编辑后的完整内容）。
    Manual(Vec<String>),
}

impl Resolution {
    /// 中文标签。
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Ours => "取当前分支",
            Self::Theirs => "取合入分支",
            Self::Both => "两边都保留",
            Self::Base => "取共同祖先",
            Self::Manual(_) => "手动编辑",
        }
    }
}

/// 一个冲突块。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConflictHunk {
    /// 左侧内容（不含行尾）。
    pub ours: Vec<String>,
    /// 基线内容；仅 diff3 / zdiff3 风格存在。
    pub base: Option<Vec<String>>,
    /// 右侧内容（不含行尾）。
    pub theirs: Vec<String>,
    /// 标记行上的说明（`<<<<<<< HEAD` 的 `HEAD`）。
    pub ours_label: String,
    /// 标记行上的说明（`>>>>>>> feature` 的 `feature`）。
    pub theirs_label: String,
    /// 起始行号（1 基，指向 `<<<<<<<` 那一行），用于"跳转到第 N 处"。
    pub start_line: usize,
}

impl ConflictHunk {
    /// 该块的默认解法：两侧相同则直接取一侧，否则留空（必须由用户决定）。
    ///
    /// "两侧相同自动解决"是 IDEA 的行为，也是唯一不会误伤的自动动作：
    /// 双方写出了完全一样的行，此时选哪边都是同一个结果。
    #[must_use]
    pub fn identical_sides(&self) -> bool {
        self.ours == self.theirs
    }

    /// 该块的行数统计（用于界面上"该块 3 行 / 5 行"的提示）。
    #[must_use]
    pub fn line_counts(&self) -> (usize, usize) {
        (self.ours.len(), self.theirs.len())
    }
}

/// 冲突文件里的一段内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictPeer {
    /// 无冲突的上下文行。
    Context(Vec<String>),
    /// 一个冲突块。
    Hunk(ConflictHunk),
}

/// 解析后的冲突文件。
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConflictFile {
    /// 仓库相对路径。
    pub path: String,
    /// 依次排列的上下文与冲突块。
    pub peers: Vec<ConflictPeer>,
    /// 原文使用 CRLF。
    pub crlf: bool,
    /// 原文以换行结束。
    pub final_newline: bool,
    /// 文件里是否含冲突标记。
    ///
    /// 为假有两种可能：内容本身就是干净的（用户已经手工解决但还没 `git add`），
    /// 或者 `merge.conflictStyle=union` 让 git 写出了无标记的合并结果。
    /// 两种情况上层都只需提示"请确认内容后标记为已解决"。
    pub has_markers: bool,
    /// 文件是否因超过阈值而未完整解析。
    pub truncated: bool,
}

impl ConflictFile {
    /// 解析一份带冲突标记的文本。
    ///
    /// 不认识的标记行按普通上下文处理 —— 宁可让用户在中心栏看到原文，
    /// 也不要因为一个畸形的标记把整个文件判成"不可解析"。
    #[must_use]
    pub fn parse(path: impl Into<String>, text: &str) -> Self {
        let crlf = text.contains("\r\n");
        let final_newline = text.ends_with('\n');

        let mut peers: Vec<ConflictPeer> = Vec::new();
        let mut context: Vec<String> = Vec::new();
        let mut has_markers = false;

        let mut ours: Vec<String> = Vec::new();
        let mut base: Option<Vec<String>> = None;
        let mut theirs: Vec<String> = Vec::new();
        let mut ours_label = String::new();
        let mut theirs_label = String::new();
        let mut start_line = 0;
        // 0 = 正常, 1 = ours 段, 2 = base 段, 3 = theirs 段
        let mut section = 0_u8;

        // `split_terminator` 而不是 `split`：后者会在"文件以换行结束"时多产出一个
        // 空行，重建后会平白多一行，正是最容易被忽略的那类差异。
        for (index, raw) in text.split_terminator('\n').enumerate() {
            let line_no = index + 1;
            let line = raw.strip_suffix('\r').unwrap_or(raw);

            if section == 0 {
                if let Some(label) = line.strip_prefix("<<<<<<<") {
                    has_markers = true;
                    flush_context(&mut peers, &mut context);
                    ours.clear();
                    base = None;
                    theirs.clear();
                    label.trim().clone_into(&mut ours_label);
                    theirs_label.clear();
                    start_line = line_no;
                    section = 1;
                    continue;
                }
                context.push(line.to_owned());
                continue;
            }

            if section == 1 {
                if let Some(label) = line.strip_prefix("|||||||") {
                    base = Some(Vec::new());
                    // 基线段的标签通常与 ours 相同（git 只打印 ours 的标签），
                    // 单独保存没有意义，这里保留原始行以备诊断。
                    let _ = label;
                    section = 2;
                    continue;
                }
                if line.starts_with("=======") {
                    section = 3;
                    continue;
                }
                ours.push(line.to_owned());
                continue;
            }

            if section == 2 {
                if line.starts_with("=======") {
                    section = 3;
                    continue;
                }
                base.get_or_insert_with(Vec::new).push(line.to_owned());
                continue;
            }

            // section == 3
            if let Some(label) = line.strip_prefix(">>>>>>>") {
                label.trim().clone_into(&mut theirs_label);
                peers.push(ConflictPeer::Hunk(ConflictHunk {
                    ours: std::mem::take(&mut ours),
                    base: base.take(),
                    theirs: std::mem::take(&mut theirs),
                    ours_label: std::mem::take(&mut ours_label),
                    theirs_label: std::mem::take(&mut theirs_label),
                    start_line,
                }));
                section = 0;
                continue;
            }
            theirs.push(line.to_owned());
        }

        // 标记没有闭合：把残留内容当上下文，避免丢字。
        if section != 0 {
            context.extend(ours);
            if let Some(base) = base {
                context.extend(base);
            }
            context.extend(theirs);
        }
        flush_context(&mut peers, &mut context);

        Self {
            path: path.into(),
            peers,
            crlf,
            final_newline,
            has_markers,
            truncated: false,
        }
    }

    /// 冲突块数量。
    #[must_use]
    pub fn hunk_count(&self) -> usize {
        self.peers
            .iter()
            .filter(|peer| matches!(peer, ConflictPeer::Hunk(_)))
            .count()
    }

    /// 依次取冲突块（只读）。
    pub fn hunks(&self) -> impl Iterator<Item = &ConflictHunk> {
        self.peers.iter().filter_map(|peer| match peer {
            ConflictPeer::Hunk(hunk) => Some(hunk),
            ConflictPeer::Context(_) => None,
        })
    }

    /// 按 `resolutions` 逐个冲突块求解，返回完整文件内容。
    ///
    /// `resolutions` 的长度必须与冲突块数量一致；缺失的块**不静默取默认值**
    /// （那会在用户没注意时丢掉一侧的改动），而是返回 [`SolveError::Undecided`]。
    ///
    /// # Errors
    /// 未决定的冲突块、或解法数量不匹配。
    pub fn solve(&self, resolutions: &[Resolution]) -> Result<String, SolveError> {
        let hunks = self.hunk_count();
        if resolutions.len() != hunks {
            return Err(SolveError::CountMismatch {
                hunks,
                resolutions: resolutions.len(),
            });
        }

        let newline = if self.crlf { "\r\n" } else { "\n" };
        let mut out = String::new();
        let mut index = 0;
        for peer in &self.peers {
            match peer {
                ConflictPeer::Context(lines) => append(&mut out, lines, newline),
                ConflictPeer::Hunk(hunk) => {
                    let resolution = &resolutions[index];
                    index += 1;
                    let picked: &[String] = match resolution {
                        Resolution::Ours => &hunk.ours,
                        Resolution::Theirs => &hunk.theirs,
                        Resolution::Both => {
                            append(&mut out, &hunk.ours, newline);
                            &hunk.theirs
                        }
                        Resolution::Base => hunk.base.as_deref().unwrap_or(&[]),
                        Resolution::Manual(lines) => lines,
                    };
                    append(&mut out, picked, newline);
                }
            }
        }

        // 解析时把末尾换行变成了一条空行，这里按原样收口。
        if !self.final_newline && out.ends_with(newline) {
            let cut = out.len() - newline.len();
            out.truncate(cut);
        }
        Ok(out)
    }

    /// 全部块一律取某一侧（FR-CFL-03 的"全部取左 / 取右 / 取双方"快捷入口）。
    #[must_use]
    pub fn solve_all(&self, resolution: &Resolution) -> String {
        let resolutions = vec![resolution.clone(); self.hunk_count()];
        self.solve(&resolutions).unwrap_or_default()
    }

    /// 中心栏的预览文本：每个未决定的块显示占位。
    ///
    /// 界面用它做"结果"列的初始内容，用户在其中逐块替换。
    #[must_use]
    pub fn preview(&self) -> String {
        let newline = if self.crlf { "\r\n" } else { "\n" };
        let mut out = String::new();
        for peer in &self.peers {
            match peer {
                ConflictPeer::Context(lines) => append(&mut out, lines, newline),
                ConflictPeer::Hunk(hunk) => {
                    if hunk.identical_sides() {
                        append(&mut out, &hunk.ours, newline);
                    } else {
                        let _ = writeln!(out, "<< 第 {} 处冲突待决 >>", hunk.start_line);
                    }
                }
            }
        }
        out
    }
}

/// 求解失败的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SolveError {
    /// 冲突块数量与解法数量不匹配。
    CountMismatch {
        /// 文件里的冲突块数。
        hunks: usize,
        /// 调用方给出的解法数。
        resolutions: usize,
    },
}

impl std::fmt::Display for SolveError {
    /// 面向用户的说明（直接展示在冲突遮罩上）。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CountMismatch { hunks, resolutions } => write!(
                f,
                "冲突块数量（{hunks}）与解法数量（{resolutions}）不一致，请刷新后重试"
            ),
        }
    }
}

impl std::error::Error for SolveError {}

/// 把一段行追加到输出，按文件原有的行尾风格。
fn append(out: &mut String, lines: &[String], newline: &str) {
    for line in lines {
        out.push_str(line);
        out.push_str(newline);
    }
}

/// 把累积的上下文收进 peers（空段不产生节点，避免界面出现空行组）。
fn flush_context(peers: &mut Vec<ConflictPeer>, context: &mut Vec<String>) {
    if context.is_empty() {
        return;
    }
    peers.push(ConflictPeer::Context(std::mem::take(context)));
}

/// 统计文本里的冲突标记数量（用于"还可能有多少处没解决"的粗判）。
#[must_use]
pub fn count_conflict_markers(text: &str) -> usize {
    text.lines()
        .filter(|line| {
            line.starts_with("<<<<<<<")
                || line.starts_with(">>>>>>>")
                || line.starts_with("|||||||")
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::{ConflictFile, ConflictPeer, Resolution, SolveError};

    const SIMPLE: &str = "\
line 1
<<<<<<< HEAD
ours a
ours b
=======
theirs a
>>>>>>> feature/x
line 2
";

    #[test]
    fn parses_context_and_hunks() {
        let file = ConflictFile::parse("a.txt", SIMPLE);
        assert!(file.has_markers);
        assert_eq!(file.hunk_count(), 1);
        assert!(!file.crlf);
        assert!(file.final_newline);

        let hunks: Vec<_> = file.hunks().collect();
        assert_eq!(hunks[0].ours, vec!["ours a", "ours b"]);
        assert_eq!(hunks[0].theirs, vec!["theirs a"]);
        assert_eq!(hunks[0].ours_label, "HEAD");
        assert_eq!(hunks[0].theirs_label, "feature/x");
        assert_eq!(hunks[0].start_line, 2);
        assert_eq!(hunks[0].line_counts(), (2, 1));
        assert!(hunks[0].base.is_none());
    }

    #[test]
    fn solves_each_side_and_both() {
        let file = ConflictFile::parse("a.txt", SIMPLE);
        assert_eq!(
            file.solve(&[Resolution::Ours]).expect("取左"),
            "line 1\nours a\nours b\nline 2\n"
        );
        assert_eq!(
            file.solve(&[Resolution::Theirs]).expect("取右"),
            "line 1\ntheirs a\nline 2\n"
        );
        assert_eq!(
            file.solve(&[Resolution::Both]).expect("取双方"),
            "line 1\nours a\nours b\ntheirs a\nline 2\n"
        );
    }

    #[test]
    fn manual_resolution_replaces_the_block() {
        let file = ConflictFile::parse("a.txt", SIMPLE);
        let text = file
            .solve(&[Resolution::Manual(vec![
                "merged".to_owned(),
                "result".to_owned(),
            ])])
            .expect("手动");
        assert_eq!(text, "line 1\nmerged\nresult\nline 2\n");
    }

    #[test]
    fn detects_diff3_base_section() {
        let text = "\
<<<<<<< ours
a
||||||| base
b
=======
c
>>>>>>> theirs
";
        let file = ConflictFile::parse("a.txt", text);
        let hunk = file.hunks().next().expect("一个冲突块");
        assert_eq!(hunk.base.as_deref(), Some(&["b".to_owned()][..]));
        assert_eq!(
            file.solve(&[Resolution::Base]).expect("取基线"),
            "b\n",
            "基线解法用于撤销双方的这处改动"
        );
    }

    #[test]
    fn preserves_crlf_and_missing_final_newline() {
        let text = "a\r\n<<<<<<< HEAD\r\nours\r\n=======\r\ntheirs\r\n>>>>>>> x\r\nb";
        let file = ConflictFile::parse("a.txt", text);
        assert!(file.crlf);
        assert!(!file.final_newline);
        let solved = file.solve(&[Resolution::Theirs]).expect("取右");
        assert_eq!(solved, "a\r\ntheirs\r\nb", "行尾与末尾换行必须原样保留");
    }

    #[test]
    fn identical_sides_are_detected() {
        let text = "<<<<<<< HEAD\nsame\n=======\nsame\n>>>>>>> x\n";
        let file = ConflictFile::parse("a.txt", text);
        let hunk = file.hunks().next().expect("一个冲突块");
        assert!(hunk.identical_sides());
        assert_eq!(file.preview(), "same\n", "两侧相同时预览不应出现占位");
    }

    #[test]
    fn rejects_mismatched_or_empty_resolutions() {
        let file = ConflictFile::parse("a.txt", SIMPLE);
        let err = file.solve(&[]).expect_err("数量不足必须被拒");
        assert!(matches!(
            err,
            SolveError::CountMismatch {
                hunks: 1,
                resolutions: 0
            }
        ));
    }

    #[test]
    fn unclosed_marker_does_not_swallow_text() {
        let text = "keep\n<<<<<<< HEAD\nours\n";
        let file = ConflictFile::parse("a.txt", text);
        assert_eq!(file.hunk_count(), 0, "未闭合的标记不产生冲突块");
        let joined = file
            .peers
            .iter()
            .map(|peer| match peer {
                ConflictPeer::Context(lines) => lines.join("|"),
                ConflictPeer::Hunk(_) => String::new(),
            })
            .collect::<Vec<_>>()
            .join("/");
        assert!(joined.contains("ours"), "残留内容不得丢失：{joined}");
    }

    #[test]
    fn multiple_hunks_keep_order() {
        let text = "\
<<<<<<< HEAD
a1
=======
a2
>>>>>>> x
mid
<<<<<<< HEAD
b1
=======
b2
>>>>>>> x
";
        let file = ConflictFile::parse("a.txt", text);
        assert_eq!(file.hunk_count(), 2);
        let solved = file
            .solve(&[Resolution::Theirs, Resolution::Ours])
            .expect("两个块");
        assert_eq!(solved, "a2\nmid\nb1\n");
    }

    #[test]
    fn clean_file_has_no_markers() {
        let file = ConflictFile::parse("a.txt", "just text\n");
        assert!(!file.has_markers);
        assert_eq!(file.hunk_count(), 0);
        assert_eq!(file.solve(&[]).expect("无冲突"), "just text\n");
        assert_eq!(super::count_conflict_markers("no markers"), 0);
    }
}
