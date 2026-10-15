//! `gt-diff` —— 差异计算与 hunk 模型。
//!
//! 采用 Histogram 算法（`imara-diff`）：在复杂 diff 上较 Myers 快 10%–100%，
//! 较 `similar` crate 快至多 30×（架构规划 3.2、附录 B）。
//!
//! # 职责边界
//!
//! - 行级差异与 hunk 模型（FR-DIF-01）；
//! - 行内**词级**差异高亮（FR-DIF-01）；
//! - 大文件与二进制阈值判定（单侧 > 2MB 或二进制 → 占位，架构规划 5.5）；
//! - `--numstat` 语义的增删行统计（M0 待办 #5 的一致性对比对象）。
//!
//! 目录级差异（FR-DIF-03）由 `gt-backend` 的树比较产出 [`gt_core::FileChange`]
//! 列表，再逐文件调用本 crate 计算内容差异。
//!
//! # 与 `git diff` 的一致性口径
//!
//! 本 crate 保证：增删行数、增删行的文本内容、行号与 `git diff` 一致；
//! hunk 头 `@@ -a,b +c,d @@` 采用同一约定，但对"纯插入且上下文为 0"等边角
//! 情形的起始行号处理以本模块文档所述规则为准（一致性测试断言 numstat 与
//! 增删行集合，不断言 hunk 头文本）。

mod engine;
mod model;

pub use engine::{diff_bytes, diff_lines, diff_text, is_binary, word_spans};
pub use model::{
    DiffHunk, DiffLine, DiffOptions, DiffPayload, DiffStats, FileDiff, LineKind, LineSpan,
    BINARY_SNIFF_LEN, DEFAULT_CONTEXT_LINES, DEFAULT_MAX_BYTES,
};
