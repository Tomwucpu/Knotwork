//! Blame 与文件历史读路径（FR-BLM，M5）。
//!
//! 实现路线：**git CLI porcelain**。验收口径是"输出与 `git blame -l`
//! 逐行一致（含重命名跟随）"——这条语义由 git 自己保证，解析层不做任何
//! 再解释；同时天然复用 FR-CON-01 的命令审计（每次调用都经 observer
//! 投递一条记录）。
//!
//! 三个能力：
//! - [`blame`]：逐行归因（`git blame --line-porcelain`）；
//! - [`file_history`]：文件历史列表（`git log --follow`，FR-BLM-01）；
//! - [`map_line_across`]：链式导航的行号映射（FR-BLM-03，`git diff -U0`
//!   的 hunk 头换算，把"当前版本第 N 行"折算到上溯版本）。

use std::time::Duration;

use crate::cli::{CancelProbe, GitCli, LOCAL_TIMEOUT};
use crate::error::{BackendError, Result};
use crate::record::CommandObserver;
use crate::repo::RepoLocation;

/// Blame 的行数上限：超过即截断（行数×字段的开销是线性的，截断是防御性的）。
pub const MAX_BLAME_LINES: usize = 50_000;

/// 文件历史的条数上限：`--follow` 在超大历史上很慢，截断保证界面可用。
pub const MAX_HISTORY_ENTRIES: usize = 5_000;

/// 本地读取的超时。blame 在超大文件上可能比普通读慢，放宽到 2 倍本地超时。
const BLAME_TIMEOUT: Duration = LOCAL_TIMEOUT.saturating_mul(2);

/// blame 的一行结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameLine {
    /// 归属提交（完整 40 位十六进制）。
    pub commit: String,
    /// 该行在该提交中的行号（1 起）。
    pub orig_line: usize,
    /// 该行在当前被 blame 的内容中的行号（1 起）。
    pub final_line: usize,
    /// 作者名。
    pub author: String,
    /// 作者时间（Unix 秒）。
    pub author_time: i64,
    /// 提交消息首行。
    pub summary: String,
    /// 是否为边界行（历史早于被 blame 的修订 / 初始提交）。
    pub boundary: bool,
    /// 重命名前的提交与文件名（`previous` 头）；链式导航的依据。
    pub prev_commit: Option<String>,
    /// 重命名前的文件路径（与 [`BlameLine::prev_commit`] 成对出现）。
    pub prev_path: Option<String>,
}

/// 文件历史的一条（`git log --follow -- path`，FR-BLM-01）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHistoryEntry {
    /// 提交（完整 40 位十六进制）。
    pub commit: String,
    /// 短哈希（界面显示用）。
    pub short: String,
    /// 作者名。
    pub author: String,
    /// 作者时间（Unix 秒）。
    pub author_time: i64,
    /// 提交消息首行。
    pub summary: String,
}

/// 对一个文件做 blame。
///
/// `spec` 为 [`None`] 时 blame 工作区内容（最常用）；`Some(rev)` 时 blame
/// 该修订的内容（链式导航与历史浏览用）。重命名跟随由 git 自动完成；
/// 解析层额外记录每行的 `previous` 头，供链式导航换算上溯路径。
///
/// # Errors
/// git 失败（路径在该修订不存在等）时返回 [`BackendError::Rejected`]，
/// detail 为原始 stderr，可直接贴进 issue。
pub fn blame(
    cli: &GitCli,
    location: &RepoLocation,
    observer: &dyn CommandObserver,
    spec: Option<&str>,
    path: &str,
    cancel: Option<&dyn CancelProbe>,
) -> Result<Vec<BlameLine>> {
    let mut args: Vec<&str> = vec!["blame", "--line-porcelain"];
    let mut owned: Vec<String> = Vec::new();
    if let Some(rev) = spec {
        owned.push(rev.to_owned());
        args.push(owned.last().expect("刚压入").as_str());
    }
    args.push("--");
    args.push(path);

    let output = run(cli, location, observer, &args, cancel)?;

    let mut rows: Vec<BlameLine> = Vec::new();
    // 一个行块的累计字段；porcelain 按块给出全部头，逐块 flush。
    let mut commit = String::new();
    let mut orig_line = 0usize;
    let mut final_line = 0usize;
    let mut author = String::new();
    let mut author_time = 0i64;
    let mut summary = String::new();
    let mut boundary = false;
    let mut prev_commit: Option<String> = None;
    let mut prev_path: Option<String> = None;

    let text = output.merged();
    for line in text.lines() {
        if let Some(content) = line.strip_prefix('\t') {
            let _ = content; // 行内容暂不需要：归因信息都在头部里
            if !commit.is_empty() {
                rows.push(BlameLine {
                    commit: std::mem::take(&mut commit),
                    orig_line,
                    final_line,
                    author: std::mem::take(&mut author),
                    author_time,
                    summary: std::mem::take(&mut summary),
                    boundary,
                    prev_commit: prev_commit.take(),
                    prev_path: prev_path.take(),
                });
            }
            boundary = false;
            continue;
        }
        let mut fields = line.splitn(2, ' ');
        let key = fields.next().unwrap_or("");
        let value = fields.next().unwrap_or("");
        match key {
            // 块首：`<sha> <orig> <final> [<count>]`
            k if k.len() == 40 && !k.is_empty() && k.bytes().all(|b| b.is_ascii_hexdigit()) => {
                k.clone_into(&mut commit);
                let mut nums = value.split_whitespace();
                orig_line = nums.next().and_then(|n| n.parse().ok()).unwrap_or(0);
                final_line = nums.next().and_then(|n| n.parse().ok()).unwrap_or(0);
            }
            "author" => value.clone_into(&mut author),
            "author-time" => {
                author_time = value
                    .split_whitespace()
                    .next()
                    .and_then(|t| t.parse().ok())
                    .unwrap_or(0);
            }
            "summary" => value.clone_into(&mut summary),
            "boundary" => boundary = true,
            "previous" => {
                let mut parts = value.splitn(2, ' ');
                prev_commit = parts.next().map(str::to_owned);
                prev_path = parts.next().map(str::to_owned);
            }
            _ => {}
        }
    }
    // 尾部没有内容行的块（理论不出现，防御性收尾）。
    if !commit.is_empty() {
        rows.push(BlameLine {
            commit: std::mem::take(&mut commit),
            orig_line,
            final_line,
            author,
            author_time,
            summary,
            boundary,
            prev_commit,
            prev_path,
        });
    }

    rows.truncate(MAX_BLAME_LINES);
    Ok(rows)
}

/// 列出触及一个文件的全部提交（`--follow` 自动跟随重命名，FR-BLM-01）。
///
/// `all_refs` 为 `true` 时扫全部引用（`--all`），否则只看当前分支（HEAD）。
///
/// # Errors
/// git 失败时返回 [`BackendError::Rejected`]。
pub fn file_history(
    cli: &GitCli,
    location: &RepoLocation,
    observer: &dyn CommandObserver,
    all_refs: bool,
    path: &str,
    cancel: Option<&dyn CancelProbe>,
) -> Result<Vec<FileHistoryEntry>> {
    let mut args: Vec<&str> = vec![
        "log",
        "--follow",
        "--format=%H%x1f%h%x1f%an%x1f%at%x1f%s",
    ];
    if all_refs {
        args.push("--all");
    }
    args.push("--");
    args.push(path);

    let output = run(cli, location, observer, &args, cancel)?;
    let mut rows: Vec<FileHistoryEntry> = Vec::new();
    for line in output.lines() {
        // %H ␟ %h ␟ %an ␟ %at ␟ %s
        let mut fields = line.split('\u{1f}');
        let commit = fields.next().unwrap_or("").to_owned();
        if commit.is_empty() {
            continue;
        }
        let short = fields.next().unwrap_or("").to_owned();
        let author = fields.next().unwrap_or("").to_owned();
        let author_time = fields.next().and_then(|t| t.parse().ok()).unwrap_or(0);
        let summary = fields.next().unwrap_or("").to_owned();
        rows.push(FileHistoryEntry {
            commit,
            short,
            author,
            author_time,
            summary,
        });
        if rows.len() >= MAX_HISTORY_ENTRIES {
            break;
        }
    }
    Ok(rows)
}

/// 把"较新内容中的第 `line` 行"映射到"较旧内容中的行号"（FR-BLM-03）。
///
/// 依据 `git diff -U0 <older> [<newer>] -- paths` 的 hunk 头换算：
///
/// - `newer` 为 [`None`] 时比较 older 与**工作区**（blame 工作区内容的上溯）；
/// - `paths` 传两个路径时（重命名上溯：旧名 + 新名），diff 会自动配对识别；
/// - 未被改动的行按累计偏移折算；落在改动块内的行取该块旧侧的最后一行
///   （"滚动到最近的来源行"，与 IDEA 的链式导航观感一致）。
///
/// 找不到对应（文件在旧侧不存在等）返回 [`None`]。
///
/// # Errors
/// git 失败时返回 [`BackendError::Rejected`]。
pub fn map_line_to_older(
    cli: &GitCli,
    location: &RepoLocation,
    observer: &dyn CommandObserver,
    older: &str,
    newer: Option<&str>,
    paths: &[&str],
    line: usize,
) -> Result<Option<usize>> {
    let mut owned: Vec<String> = Vec::new();
    let mut args: Vec<&str> = vec!["diff", "-U0", "-M", older];
    if let Some(rev) = newer {
        owned.push(rev.to_owned());
        args.push(owned.last().expect("刚压入").as_str());
    }
    args.push("--");
    for path in paths {
        args.push(path);
    }
    let output = run(cli, location, observer, &args, None)?;
    if !output.is_success() {
        // 旧侧没有这个文件（新增文件上溯）是正常业务形态，不算错误。
        return Ok(None);
    }
    let hunks = collect_hunks(&output.merged());
    Ok(Some(map_line(&hunks, line)))
}

/// 一个 hunk 头：旧侧 (起始行, 行数) 与新侧 (起始行, 行数)，行号 1 起。
type Hunk = ((usize, usize), (usize, usize));

/// 从 `git diff -U0` 输出收集全部 hunk 头。
fn collect_hunks(diff: &str) -> Vec<Hunk> {
    let mut hunks: Vec<Hunk> = Vec::new();
    for line in diff.lines() {
        let Some(head) = line.strip_prefix("@@ ") else {
            continue;
        };
        if let Some(h) = parse_hunk_head(head) {
            hunks.push(h);
        }
    }
    hunks
}

/// 解析 `@@ -a[,b] +c[,d] @@ …` 的两个区间（缺省行数按 1 处理）。
fn parse_hunk_head(head: &str) -> Option<Hunk> {
    let mut parts = head.split_whitespace();
    let old = parts.next()?.trim_start_matches('-');
    let new = parts.next()?.trim_start_matches('+');
    let parse_range = |s: &str| -> Option<(usize, usize)> {
        let mut it = s.split(',');
        let start: usize = it.next()?.parse().ok()?;
        let count: usize = match it.next() {
            Some(n) => n.parse().ok()?,
            None => 1,
        };
        Some((start, count))
    };
    Some((parse_range(old)?, parse_range(new)?))
}

/// 纯换算：较新侧第 `line` 行对应较旧侧哪一行。
///
/// 未改动区按累计偏移折算；落在改动块内的行取旧侧块尾（最近的来源行）。
fn map_line(hunks: &[Hunk], line: usize) -> usize {
    // 偏移量 = 累计新侧行数 - 累计旧侧行数（可能为负），拆成两个计数避免
    // 有符号转换：older = line + o_total - n_total。
    let (mut n_total, mut o_total) = (0usize, 0usize);
    for &((o_start, o_count), (n_start, n_count)) in hunks {
        let n_end = n_start + n_count; // 开区间
        if line < n_start {
            return (line + o_total).saturating_sub(n_total).max(1);
        }
        if line < n_end {
            // 块内：旧侧块尾（b=0 的纯插入取块前位置）。
            let tail = if o_count == 0 {
                o_start
            } else {
                o_start + o_count - 1
            };
            return tail.max(1);
        }
        n_total += n_count;
        o_total += o_count;
    }
    (line + o_total).saturating_sub(n_total).max(1)
}

/// 查询一个提交的父提交列表（`rev-list --parents -n 1`；根提交为空）。
///
/// Blame 行点击跳转（FR-BLM-02）需要父提交来构造差异旧侧；根提交没有父，
/// 返回空列表，调用方以空树作旧侧。
///
/// # Errors
/// git 失败时返回 [`BackendError::Rejected`]。
pub fn commit_parents(
    cli: &GitCli,
    location: &RepoLocation,
    observer: &dyn CommandObserver,
    commit: &str,
) -> Result<Vec<String>> {
    let args: Vec<&str> = vec!["rev-list", "--parents", "-n", "1", commit];
    let output = run(cli, location, observer, &args, None)?;
    let merged = output.merged();
    let mut parts = merged.split_whitespace();
    let _self = parts.next();
    Ok(parts.map(str::to_owned).collect())
}

/// 统一的 CLI 执行入口：本地超时、审计、错误打包。
fn run(
    cli: &GitCli,
    location: &RepoLocation,
    observer: &dyn CommandObserver,
    args: &[&str],
    cancel: Option<&dyn CancelProbe>,
) -> Result<crate::cli::CliOutput> {
    let output = match cancel {
        Some(probe) => cli.run_cancellable(
            Some(location.work_dir_or_git_dir()),
            args,
            None,
            observer,
            BLAME_TIMEOUT,
            probe,
        ),
        None => cli.run(
            Some(location.work_dir_or_git_dir()),
            args,
            observer,
            BLAME_TIMEOUT,
        ),
    };
    match output {
        Ok(output) => {
            if !output.is_success() {
                return Err(BackendError::Rejected {
                    summary: format!("git {} 失败", args.first().unwrap_or(&"")),
                    detail: output.merged(),
                });
            }
            Ok(output)
        }
        Err(BackendError::ProcessTimeout { .. }) => Err(BackendError::ProcessTimeout {
            seconds: BLAME_TIMEOUT.as_secs(),
            program: cli.program().display().to_string(),
            args: vec![args.join(" ")],
        }),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::{collect_hunks, map_line, parse_hunk_head};

    #[test]
    fn hunk_head_parses_both_forms() {
        assert_eq!(parse_hunk_head("-1,3 +1,5 @@ x"), Some(((1, 3), (1, 5))));
        assert_eq!(parse_hunk_head("-1 +1 @@ x"), Some(((1, 1), (1, 1))));
        assert_eq!(parse_hunk_head("-0,0 +1,2 @@ x"), Some(((0, 0), (1, 2))));
    }

    #[test]
    fn hunks_collect_in_order() {
        let diff = "diff --git a/f b/f\n\
                    @@ -1,2 +1,3 @@\n a\n b\n+c\n\
                    @@ -10,1 +12,1 @@\n-x\n+y\n";
        assert_eq!(
            collect_hunks(diff),
            vec![((1, 2), (1, 3)), ((10, 1), (12, 1))]
        );
    }

    #[test]
    fn map_line_walks_offsets() {
        // 新侧在头部插了 1 行（1,2 → 1,3），第 5 行应来自旧侧第 4 行。
        let hunks = vec![((1, 2), (1, 3))];
        assert_eq!(map_line(&hunks, 5), 4);
        // 落在改动块内：映射到旧侧块尾。
        assert_eq!(map_line(&hunks, 3), 2);
        // 纯插入块（旧侧行数 0）：映射到插入点。
        let ins = vec![((4, 0), (4, 2))];
        assert_eq!(map_line(&ins, 4), 4);
        assert_eq!(map_line(&ins, 5), 4);
    }

    #[test]
    fn map_line_without_hunks_is_identity() {
        assert_eq!(map_line(&[], 42), 42);
    }
}
