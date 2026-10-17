//! 写路径：暂存 / 反暂存 / 丢弃 / 提交 / 分支 / 忽略规则 / 搜索。
//!
//! # 为什么写操作以 git CLI 为主实现
//!
//! 路由表（架构规划 3.2）把"暂存 / 反暂存"的首要实现标为 `gix-index`。本模块
//! 采用 CLI 实现，理由与代价如实记录如下：
//!
//! - **一致性优先于自研**：直接写索引必须自己复刻 `.gitattributes` 的
//!   `text`/`eol` 过滤器、`core.autocrlf`、`core.filemode`、`core.symlinks`
//!   以及 intent-to-add 语义。任何一处复刻不完整，都会让"我们暂存的内容"与
//!   `git add` 暂存的内容不同 —— 这直接违反 NFR-COR-01（P0）。
//! - **hunk / 行级暂存仍按计划口径**：补丁由 `gt-diff` 生成，经
//!   `git apply --cached [--reverse]` 落库（[`RepoWriter::apply_patch_cached`]），
//!   与计划里"gix-index + git apply --cached 语义"的落库口径一致。
//! - **代价**：每次写操作起一个子进程（10–40ms 量级）。写操作是低频操作
//!   （用户点击级），不构成 NFR-PRF-03 的 200ms 感知延迟风险；而读热路径
//!   （status / log / diff）仍在进程内走 gix。
//!
//! 结论：本次实现以"与 CLI 逐字节一致"为准，把自研索引写入留到确有性能
//! 诉求时再做 —— 这是一个显式的、记录在案的取舍，而不是遗漏。

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cli::{CancelProbe, GitCli, LOCAL_TIMEOUT};
use crate::error::{BackendError, Result};
use crate::record::SharedObserver;
use crate::repo::{GitRepo, RepoLocation};

/// 搜索的默认超时（FR-LOG-03：超过 1.5s 应提示"构建历史索引"）。
pub const SEARCH_TIMEOUT: Duration = Duration::from_millis(1500);

/// 写锁文件名（放在 `.git` 下，随仓库移动，不影响工作区）。
pub const LOCK_FILE_NAME: &str = crate::lock::LOCK_FILE_NAME;

/// 一条分支的上游跟踪信息：`(本地分支名, 上游短名, ahead, behind)`。
pub type UpstreamRow = (String, Option<String>, u32, u32);

/// 一次写操作的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WriteOutcome {
    /// 一句话结论，直接用于 toast / 状态栏。
    pub summary: String,
    /// 细节（git 的 stdout/stderr 摘要）。
    pub detail: String,
}

impl WriteOutcome {
    /// 构造结论。
    #[must_use]
    pub fn new(summary: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            detail: String::new(),
        }
    }

    /// 附带细节。
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }
}

/// 提交请求（FR-CMT-01/02）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommitRequest {
    /// 提交消息（多行，UTF-8）。
    pub message: String,
    /// 是否修订上一条提交（`--amend`）。
    pub amend: bool,
    /// 修订时重置作者（`--reset-author`）。
    pub reset_author: bool,
    /// 允许空提交（`--allow-empty`）；默认不允许。
    pub allow_empty: bool,
}

/// 分支操作（FR-BR-01/02 的 M2 子集）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchOp {
    /// 新建分支；`checkout` 为真时同时检出（`checkout -b` 语义）。
    Create {
        /// 新分支名。
        name: String,
        /// 起点（分支名 / 提交号）；[`None`] 表示当前 HEAD。
        start: Option<String>,
        /// 是否同时检出。
        checkout: bool,
    },
    /// 检出分支。
    Checkout {
        /// 目标分支名。
        name: String,
    },
    /// 检出某个提交（分离头指针，属高危操作，需上层确认）。
    CheckoutDetached {
        /// 目标修订。
        revision: String,
    },
    /// 重命名分支。
    Rename {
        /// 旧名。
        from: String,
        /// 新名。
        to: String,
    },
    /// 删除分支；`force` 为真时使用 `-D`（未合并也删，需高危确认）。
    Delete {
        /// 目标分支名。
        name: String,
        /// 是否强制删除。
        force: bool,
    },
    /// 设置上游跟踪分支。
    SetUpstream {
        /// 本地分支。
        branch: String,
        /// 上游（如 `origin/main`）。
        upstream: String,
    },
}

impl BranchOp {
    /// 用于审计与提示的简短描述。
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Create { name, .. } => format!("新建分支 {name}"),
            Self::Checkout { name } => format!("检出分支 {name}"),
            Self::CheckoutDetached { revision } => format!("检出修订 {revision}（分离头指针）"),
            Self::Rename { from, to } => format!("重命名分支 {from} → {to}"),
            Self::Delete { name, .. } => format!("删除分支 {name}"),
            Self::SetUpstream { branch, upstream } => {
                format!("设置 {branch} 的上游为 {upstream}")
            }
        }
    }
}

/// 忽略规则写入目标（FR-LC-04）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IgnoreTarget {
    /// 仓库根的 `.gitignore`（纳入版本控制）。
    Gitignore,
    /// `.git/info/exclude`（仅本机生效，不入库）。
    InfoExclude,
}

impl IgnoreTarget {
    /// 目标文件相对路径（相对于对应根）。
    #[must_use]
    pub const fn file_name(self) -> &'static str {
        match self {
            Self::Gitignore => ".gitignore",
            Self::InfoExclude => "exclude",
        }
    }

    /// 中文说明。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Gitignore => ".gitignore",
            Self::InfoExclude => ".git/info/exclude",
        }
    }
}

/// 仓库写入器：所有会改变仓库状态的操作都收在这里。
///
/// 设计约束：
/// - **不做只读判定**：是否允许写入由服务层依据写锁结果裁决（`RepoHandle`），
///   后端只负责"执行"与"把失败原因讲清楚"；
/// - **消息与补丁走 stdin**：命令行参数有长度上限，且多行文本经转义极易出错；
/// - **失败一律带 git 原文**：用户要能把那段输出贴进 issue（FR-CMT-03）。
pub struct RepoWriter {
    location: RepoLocation,
    cli: GitCli,
    observer: SharedObserver,
}

impl std::fmt::Debug for RepoWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepoWriter")
            .field("location", &self.location)
            .finish_non_exhaustive()
    }
}

impl RepoWriter {
    /// 由仓库位置与 git 可执行文件构造。
    #[must_use]
    pub fn new(location: &RepoLocation, cli: GitCli, observer: SharedObserver) -> Self {
        Self {
            location: location.clone(),
            cli,
            observer,
        }
    }

    /// 由已打开的仓库直接构造（复用其观察者与位置）。
    ///
    /// # Errors
    /// 未探测到 git 时返回 [`BackendError::CliRequired`]。
    pub fn for_repo(repo: &GitRepo, cli: &GitCli) -> Self {
        Self::new(repo.location(), cli.clone(), repo.observer())
    }

    /// 工作目录；裸仓库退化到 `.git` 目录。
    #[must_use]
    pub fn work_dir(&self) -> &Path {
        self.location.work_dir_or_git_dir()
    }

    /// 写锁文件路径。
    #[must_use]
    pub fn lock_path(&self) -> PathBuf {
        self.location.git_dir.join(LOCK_FILE_NAME)
    }

    // ------------------------------------------------------------ 暂存与反暂存

    /// 暂存若干路径（`git add -A --`，含删除与目录）。
    ///
    /// 用 `-A` 而不是 `--` 的默认行为：`git add <path>` 在旧版本上不记录删除，
    /// 而 Local Changes 里"删了一个文件 → 暂存"必须记录删除。
    ///
    /// # Errors
    /// git 拒绝时返回 [`BackendError::Rejected`]。
    pub fn stage_paths(&self, paths: &[String]) -> Result<WriteOutcome> {
        if paths.is_empty() {
            return Ok(WriteOutcome::new("没有需要暂存的文件"));
        }
        let args = paths_with_prefix(&["add", "-A", "--"], paths);
        self.execute(&args, "暂存")?;
        Ok(WriteOutcome::new(format!("已暂存 {} 个路径", paths.len())))
    }

    /// 反暂存若干路径（`git restore --staged`）。
    ///
    /// 尚未有提交（unborn HEAD）时 `restore --staged` 会因为拿不到 HEAD 而失败，
    /// 此时退化为 `git rm --cached` —— 两者在"把内容移出索引"这一点上等价。
    ///
    /// # Errors
    /// git 拒绝时返回 [`BackendError::Rejected`]。
    pub fn unstage_paths(&self, paths: &[String]) -> Result<WriteOutcome> {
        if paths.is_empty() {
            return Ok(WriteOutcome::new("没有需要反暂存的文件"));
        }
        let has_head = self.has_head();
        let args = if has_head {
            paths_with_prefix(&["restore", "--staged", "--"], paths)
        } else {
            paths_with_prefix(&["rm", "--cached", "-r", "--quiet", "--"], paths)
        };
        self.execute(&args, "反暂存")?;
        Ok(WriteOutcome::new(format!(
            "已反暂存 {} 个路径",
            paths.len()
        )))
    }

    /// 把 `parts` 里的每一段各自拼成 `git <prefix...> <part>` 之外的统一形式。
    fn has_head(&self) -> bool {
        self.cli
            .run(
                Some(self.work_dir()),
                &["rev-parse", "--verify", "--quiet", "HEAD"],
                self.observer.as_ref(),
                LOCAL_TIMEOUT,
            )
            .map(|output| output.is_success())
            .unwrap_or(false)
    }

    // ---------------------------------------------------------------- 丢弃更改

    /// 丢弃已跟踪文件的工作区修改（`git restore --worktree --`）。
    ///
    /// **不可逆**：调用方必须先取得用户强确认（FR-LC-02 / D4）。
    ///
    /// # Errors
    /// git 拒绝时返回 [`BackendError::Rejected`]。
    pub fn discard_tracked(&self, paths: &[String]) -> Result<WriteOutcome> {
        if paths.is_empty() {
            return Ok(WriteOutcome::new("没有需要丢弃的修改"));
        }
        let args = paths_with_prefix(&["restore", "--worktree", "--"], paths);
        self.execute(&args, "丢弃修改")?;
        Ok(WriteOutcome::new(format!(
            "已丢弃 {} 个文件的修改",
            paths.len()
        )))
    }

    /// 丢弃已暂存的内容（`git restore --staged --worktree`，即回到 HEAD）。
    ///
    /// # Errors
    /// git 拒绝时返回 [`BackendError::Rejected`]。
    pub fn discard_staged(&self, paths: &[String]) -> Result<WriteOutcome> {
        if paths.is_empty() {
            return Ok(WriteOutcome::new("没有需要丢弃的暂存内容"));
        }
        let args = paths_with_prefix(&["restore", "--staged", "--worktree", "--"], paths);
        self.execute(&args, "丢弃暂存")?;
        Ok(WriteOutcome::new(format!(
            "已丢弃 {} 个路径的暂存与修改",
            paths.len()
        )))
    }

    /// 删除未跟踪文件 / 目录（`git clean -f -d --`）。
    ///
    /// 用 `git clean` 而不是直接 `fs::remove_*`：它遵循 `.gitignore` 边界，
    /// 不会误删被忽略的本地文件（那些文件往往比未跟踪文件更珍贵）。
    ///
    /// **不可逆**：调用方必须先取得用户强确认。
    ///
    /// # Errors
    /// git 拒绝时返回 [`BackendError::Rejected`]。
    pub fn delete_untracked(&self, paths: &[String]) -> Result<WriteOutcome> {
        if paths.is_empty() {
            return Ok(WriteOutcome::new("没有需要删除的未跟踪文件"));
        }
        let args = paths_with_prefix(&["clean", "-f", "-d", "--"], paths);
        self.execute(&args, "删除未跟踪文件")?;
        Ok(WriteOutcome::new(format!(
            "已删除 {} 个未跟踪路径",
            paths.len()
        )))
    }

    // ------------------------------------------------------------ 补丁（hunk/行级）

    /// 把补丁应用到索引（`git apply --cached`），`reverse` 为真时反向应用。
    ///
    /// 反暂存 hunk 的语义就是"把索引里这一段退回 HEAD"，即对
    /// "HEAD → 索引"的补丁做反向应用。补丁本身由 `gt-diff` 生成。
    ///
    /// # Errors
    /// 补丁与当前索引不匹配时返回 [`BackendError::Rejected`]（通常是工作区在
    /// 用户选择期间又被改动过，提示用户刷新）。
    pub fn apply_patch_cached(&self, patch: &str, reverse: bool) -> Result<WriteOutcome> {
        if patch.trim().is_empty() {
            return Ok(WriteOutcome::new("没有选中任何变更"));
        }
        let mut args = vec!["apply", "--cached", "--recount", "--whitespace=nowarn"];
        if reverse {
            args.push("--reverse");
        }
        args.push("-");

        let output = self
            .cli
            .run_with_input(
                Some(self.work_dir()),
                &args,
                patch.as_bytes(),
                self.observer.as_ref(),
                LOCAL_TIMEOUT,
            )
            .map_err(|err| match err {
                BackendError::Process(message) => BackendError::Rejected {
                    summary: "索引已被其他操作改动，请刷新后重试".to_owned(),
                    detail: message,
                },
                other => other,
            })?;

        if !output.is_success() {
            return Err(BackendError::Rejected {
                summary: "补丁未能应用到索引（可能文件已被改动，请刷新后重试）".to_owned(),
                detail: join_streams(&output.stdout, &output.stderr),
            });
        }

        Ok(WriteOutcome::new(if reverse {
            "已反暂存所选变更"
        } else {
            "已暂存所选变更"
        })
        .with_detail(output.stderr.trim().to_owned()))
    }

    /// 丢弃工作区中选中的 hunk / 行（`git apply --reverse`，作用于工作区）。
    ///
    /// 与 [`Self::apply_patch_cached`] 的区别是**不加 `--cached`**：直接改工作区
    /// 文件。**不可逆**，调用方必须先取得确认。
    ///
    /// # Errors
    /// 补丁与工作区当前内容不匹配时返回 [`BackendError::Rejected`]。
    pub fn apply_patch_worktree(&self, patch: &str) -> Result<WriteOutcome> {
        if patch.trim().is_empty() {
            return Ok(WriteOutcome::new("没有选中任何变更"));
        }
        let args = [
            "apply",
            "--reverse",
            "--recount",
            "--whitespace=nowarn",
            "-",
        ];
        let output = self
            .cli
            .run_with_input(
                Some(self.work_dir()),
                &args,
                patch.as_bytes(),
                self.observer.as_ref(),
                LOCAL_TIMEOUT,
            )
            .map_err(|err| match err {
                BackendError::Process(message) => BackendError::Rejected {
                    summary: "工作区内容已变化，请刷新后重试".to_owned(),
                    detail: message,
                },
                other => other,
            })?;
        if !output.is_success() {
            return Err(BackendError::Rejected {
                summary: "未能丢弃所选变更（文件可能已被改动，请刷新后重试）".to_owned(),
                detail: join_streams(&output.stdout, &output.stderr),
            });
        }
        Ok(WriteOutcome::new("已丢弃所选变更"))
    }

    // -------------------------------------------------------------------- 提交

    /// 提交暂存内容（FR-CMT-01/02）。
    ///
    /// - 消息经 stdin 传入（`--file=-`），避免参数长度与转义问题；
    /// - hooks 由 git 自身执行（架构规划 3.2 "Hooks 走 CLI"），失败时原文返回；
    /// - `core.commentChar` / `--cleanup=strip` 与命令行行为一致。
    ///
    /// # Errors
    /// - 消息为空 → [`BackendError::Rejected`]；
    /// - hook 失败 / 无内容可提交 → [`BackendError::Rejected`]（附 git 原文）。
    pub fn commit(&self, request: &CommitRequest) -> Result<WriteOutcome> {
        let message = request.message.trim_end();
        if message.trim().is_empty() {
            return Err(BackendError::Rejected {
                summary: "提交消息不能为空".to_owned(),
                detail: String::new(),
            });
        }

        let mut args = vec!["commit", "--file=-", "--cleanup=strip"];
        if request.amend {
            args.push("--amend");
            if request.reset_author {
                args.push("--reset-author");
            }
        }
        if request.allow_empty {
            args.push("--allow-empty");
        }

        let output = self
            .cli
            .run_with_input(
                Some(self.work_dir()),
                &args,
                message.as_bytes(),
                self.observer.as_ref(),
                LOCAL_TIMEOUT,
            )
            .map_err(|err| match err {
                BackendError::Process(message) => BackendError::Rejected {
                    summary: "提交进程启动失败".to_owned(),
                    detail: message,
                },
                other => other,
            })?;

        if !output.is_success() {
            let combined = join_streams(&output.stdout, &output.stderr);
            let hooks = self.active_hooks();
            let summary = classify_commit_failure(&combined, request.amend, &hooks);
            return Err(BackendError::Rejected {
                summary,
                detail: combined,
            });
        }

        let head = self
            .cli
            .run(
                Some(self.work_dir()),
                &["rev-parse", "--short", "HEAD"],
                self.observer.as_ref(),
                LOCAL_TIMEOUT,
            )
            .ok()
            .filter(crate::cli::CliOutput::is_success)
            .map(|output| output.stdout.trim().to_owned())
            .unwrap_or_default();

        let verb = if request.amend {
            "已修订"
        } else {
            "已提交"
        };
        let mut outcome = WriteOutcome::new(if head.is_empty() {
            verb.to_owned()
        } else {
            format!("{verb} {head}")
        });
        let detail = join_streams(&output.stdout, &output.stderr);
        if !detail.trim().is_empty() {
            outcome = outcome.with_detail(detail);
        }
        Ok(outcome)
    }

    // ---------------------------------------------------------------- 分支操作

    /// 执行一次分支操作。
    ///
    /// 检出统一走 `git switch`（`checkout` 在"分支名与文件名同名"时有歧义），
    /// 检出提交走 `git checkout --detach`（`switch` 不支持）。
    ///
    /// # Errors
    /// git 拒绝时返回 [`BackendError::Rejected`]；删除未合并分支时 detail 里
    /// 会带上 git 列出的未合并提交（FR-BR-02）。
    pub fn branch(&self, op: &BranchOp) -> Result<WriteOutcome> {
        let args: Vec<String> = match op {
            BranchOp::Create {
                name,
                start,
                checkout,
            } => {
                let mut args: Vec<String> = if *checkout {
                    vec!["switch".into(), "-c".into(), name.clone()]
                } else {
                    vec!["branch".into(), name.clone()]
                };
                if let Some(start) = start {
                    args.push(start.clone());
                }
                args
            }
            BranchOp::Checkout { name } => vec!["switch".into(), name.clone()],
            BranchOp::CheckoutDetached { revision } => {
                vec!["checkout".into(), "--detach".into(), revision.clone()]
            }
            BranchOp::Rename { from, to } => {
                vec!["branch".into(), "-m".into(), from.clone(), to.clone()]
            }
            BranchOp::Delete { name, force } => vec![
                "branch".into(),
                if *force { "-D" } else { "-d" }.into(),
                name.clone(),
            ],
            BranchOp::SetUpstream { branch, upstream } => vec![
                "branch".into(),
                format!("--set-upstream-to={upstream}"),
                branch.clone(),
            ],
        };

        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = self
            .cli
            .run(
                Some(self.work_dir()),
                &borrowed,
                self.observer.as_ref(),
                LOCAL_TIMEOUT,
            )
            .map_err(|err| match err {
                BackendError::Process(message) => BackendError::Rejected {
                    summary: format!("{}失败：进程启动异常", op.label()),
                    detail: message,
                },
                other => other,
            })?;

        if !output.is_success() {
            return Err(BackendError::Rejected {
                summary: format!("{}失败", op.label()),
                detail: join_streams(&output.stdout, &output.stderr),
            });
        }

        Ok(WriteOutcome::new(format!("{} 完成", op.label()))
            .with_detail(join_streams(&output.stdout, &output.stderr)))
    }

    // ------------------------------------------------------------ 忽略规则

    /// 把路径追加到忽略文件（FR-LC-04）。
    ///
    /// 目录形态的路径（以 `/` 结尾，`git status` 折叠后的形态）写成
    /// `dir/` 形式 —— 与用户手写 `.gitignore` 的习惯一致，且能覆盖新文件。
    ///
    /// 幂等：已有完全相同的行时不重复追加。
    ///
    /// # Errors
    /// 目标文件不可写时返回 [`BackendError::Io`]。
    pub fn append_ignore(&self, target: IgnoreTarget, path: &str) -> Result<WriteOutcome> {
        let pattern = ignore_pattern(path);
        if pattern.is_empty() {
            return Ok(WriteOutcome::new("忽略规则为空，未写入"));
        }

        let file = match target {
            IgnoreTarget::Gitignore => self
                .location
                .work_dir
                .clone()
                .unwrap_or_else(|| self.location.git_dir.clone())
                .join(IgnoreTarget::Gitignore.file_name()),
            IgnoreTarget::InfoExclude => self
                .location
                .git_dir
                .join("info")
                .join(IgnoreTarget::InfoExclude.file_name()),
        };

        let existing = std::fs::read_to_string(&file).unwrap_or_default();
        if existing.lines().any(|line| line.trim() == pattern) {
            return Ok(WriteOutcome::new(format!(
                "{} 中已有该规则",
                target.label()
            )));
        }

        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).map_err(|err| BackendError::Io {
                path: parent.to_path_buf(),
                reason: err.to_string(),
            })?;
        }

        let mut text = existing;
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&pattern);
        text.push('\n');

        std::fs::write(&file, text).map_err(|err| BackendError::Io {
            path: file.clone(),
            reason: err.to_string(),
        })?;

        Ok(
            WriteOutcome::new(format!("已写入 {}：{pattern}", target.label()))
                .with_detail(file.display().to_string()),
        )
    }

    /// 查询某路径是否被忽略（`git check-ignore`，退出码 0 表示被忽略）。
    ///
    /// # Errors
    /// 子进程启动失败时返回错误；**未被忽略不是错误**（返回 `Ok(false)`）。
    pub fn is_ignored(&self, path: &str) -> Result<bool> {
        let output = self.cli.run(
            Some(self.work_dir()),
            &["check-ignore", "-q", "--", path],
            self.observer.as_ref(),
            LOCAL_TIMEOUT,
        )?;
        // `check-ignore` 用 1 表示"未被忽略"，128 才是真的出错。
        Ok(matches!(output.exit_code, Some(0)))
    }

    // ------------------------------------------------------------------ 搜索

    /// 搜索提交（FR-LOG-03）。
    ///
    /// 语义与 `git log --grep` 一致：`regex` 为真时按 POSIX 基本正则匹配提交
    /// 消息，为假时用 `--fixed-strings`（纯文本包含）。
    ///
    /// 输入形如完整/缩写哈希时先尝试 `rev-parse`，命中则把该提交排在最前 ——
    /// 用户粘贴哈希时不该得到"没有匹配"。
    ///
    /// # Errors
    /// - 搜索超时 → [`BackendError::ProcessTimeout`]（上层据此提示构建索引）；
    /// - 其他执行失败 → [`BackendError::Rejected`]。
    pub fn search_commits(
        &self,
        query: &str,
        regex: bool,
        case_sensitive: bool,
        all_refs: bool,
        limit: usize,
        cancel: Option<&dyn CancelProbe>,
    ) -> Result<Vec<String>> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }

        let mut results: Vec<String> = Vec::new();
        if let Some(oid) = self.resolve_hash(query) {
            results.push(oid);
        }

        let limit_arg = format!("--max-count={limit}");
        let mut args: Vec<&str> = vec!["log", "--format=%H", limit_arg.as_str()];
        if all_refs {
            args.push("--all");
        } else {
            args.push("--branches");
            args.push("HEAD");
        }
        if !regex {
            args.push("--fixed-strings");
        }
        if !case_sensitive {
            args.push("--regexp-ignore-case");
        }
        let grep = format!("--grep={query}");
        args.push(&grep);

        let output = match cancel {
            Some(probe) => self.cli.run_cancellable(
                Some(self.work_dir()),
                &args,
                None,
                self.observer.as_ref(),
                SEARCH_TIMEOUT,
                probe,
            ),
            None => self.cli.run(
                Some(self.work_dir()),
                &args,
                self.observer.as_ref(),
                SEARCH_TIMEOUT,
            ),
        };

        let output = match output {
            Ok(output) => output,
            // 超时是"需要索引"的信号，不当成错误吞掉。
            Err(BackendError::ProcessTimeout { .. }) => {
                if results.is_empty() {
                    return Err(BackendError::ProcessTimeout {
                        seconds: SEARCH_TIMEOUT.as_secs(),
                        program: self.cli.program().display().to_string(),
                        args: vec![format!("搜索 {query}")],
                    });
                }
                return Ok(results);
            }
            Err(err) => return Err(err),
        };

        if !output.is_success() {
            return Err(BackendError::Rejected {
                summary: format!("搜索 {query} 失败"),
                detail: join_streams(&output.stdout, &output.stderr),
            });
        }

        for line in output.lines() {
            let id = line.trim();
            if id.len() >= 7 && !results.iter().any(|existing| existing == id) {
                results.push(id.to_owned());
            }
            if results.len() >= limit {
                break;
            }
        }

        Ok(results)
    }

    /// 尝试把查询串解析成提交（完整或缩写哈希）。
    fn resolve_hash(&self, query: &str) -> Option<String> {
        let is_hexish =
            query.len() >= 4 && query.len() <= 64 && query.chars().all(|ch| ch.is_ascii_hexdigit());
        if !is_hexish {
            return None;
        }
        let spec = format!("{query}^{{commit}}");
        let output = self
            .cli
            .run(
                Some(self.work_dir()),
                &["rev-parse", "--verify", "--quiet", spec.as_str()],
                self.observer.as_ref(),
                LOCAL_TIMEOUT,
            )
            .ok()?;
        if !output.is_success() {
            return None;
        }
        let id = output.stdout.trim().to_owned();
        if id.len() == 40 {
            Some(id)
        } else {
            None
        }
    }

    // -------------------------------------------------------------- 上游计数

    /// 一次取回全部分支的上游与领先/落后计数（FR-BR-01 验收口径）。
    ///
    /// 用 `for-each-ref` 一次拿全，而不是每个分支跑一次 `rev-list --left-right`：
    /// 十个分支就是十个进程，而这条请求在每次概览刷新时都会跑。
    ///
    /// 返回 `(本地分支短名, 上游短名, ahead, behind)`，`ahead/behind` 在无上游时为 0。
    ///
    /// # Errors
    /// 执行失败时返回 [`BackendError::Rejected`]。
    pub fn upstream_tracking(&self) -> Result<Vec<UpstreamRow>> {
        let format = "--format=%(refname:short)%00%(upstream:short)%00%(upstream:track,nobracket)";
        let output = self.cli.run(
            Some(self.work_dir()),
            &["for-each-ref", format, "refs/heads/"],
            self.observer.as_ref(),
            LOCAL_TIMEOUT,
        )?;
        if !output.is_success() {
            return Err(BackendError::Rejected {
                summary: "读取分支跟踪信息失败".to_owned(),
                detail: join_streams(&output.stdout, &output.stderr),
            });
        }

        let mut rows = Vec::new();
        for line in output.stdout.lines() {
            let mut parts = line.split('\0');
            let name = parts.next().unwrap_or_default().trim();
            if name.is_empty() {
                continue;
            }
            let upstream = parts.next().unwrap_or_default().trim().to_owned();
            let track = parts.next().unwrap_or_default().trim();
            let (ahead, behind) = parse_track(track);
            rows.push((
                name.to_owned(),
                if upstream.is_empty() {
                    None
                } else {
                    Some(upstream)
                },
                ahead,
                behind,
            ));
        }
        Ok(rows)
    }

    // ---------------------------------------------------------------- 内部实现

    /// 仓库里实际生效的提交 hook 名称。
    ///
    /// 之所以要主动探测：**git 在 hook 自己失败时通常什么也不打印**
    /// （hook 的 stderr 才是我们唯一能拿到的输出）。不探测就只能对用户说
    /// "提交失败：自定义拦截"，用户不知道是谁拦的。实测确认过这一点。
    ///
    /// 已知边界：只认默认的 `.git/hooks`；仓库若用 `core.hooksPath` 指向别处，
    /// 分类会退化为通用的"提交失败"。这不影响正确性（提交确实被拒绝了），
    /// 只影响提示文案的精确度。
    fn active_hooks(&self) -> Vec<String> {
        const CANDIDATES: [&str; 3] = ["pre-commit", "commit-msg", "prepare-commit-msg"];
        let dir = self.location.git_dir.join("hooks");
        CANDIDATES
            .iter()
            .filter(|name| {
                let path = dir.join(name);
                std::fs::metadata(path).is_ok_and(|meta| meta.is_file())
            })
            .map(|name| (*name).to_owned())
            .collect()
    }

    /// 执行一条不需要 stdin 的命令，非零退出码转成 [`BackendError::Rejected`]。
    fn execute(&self, args: &[String], operation: &'static str) -> Result<crate::cli::CliOutput> {
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = self.cli.run(
            Some(self.work_dir()),
            &borrowed,
            self.observer.as_ref(),
            LOCAL_TIMEOUT,
        )?;
        if !output.is_success() {
            let combined = join_streams(&output.stdout, &output.stderr);
            return Err(BackendError::Rejected {
                summary: format!("{operation}失败：{}", first_line(&combined)),
                detail: combined,
            });
        }
        Ok(output)
    }
}

/// `git rev-parse` 结果的便捷判定（避免在链式调用里展开 `CliOutput`）。
/// 把 `paths` 拼到 `prefix` 之后，得到完整参数表。
///
/// 返回拥有型 `String` 而不是 `&str`：前缀是调用点的临时数组，返回借用会在
/// 语句结束时悬空（编译器同样不允许）。
fn paths_with_prefix(prefix: &[&str], paths: &[String]) -> Vec<String> {
    let mut args: Vec<String> = prefix.iter().map(|arg| (*arg).to_owned()).collect();
    args.extend(paths.iter().cloned());
    args
}

/// 忽略规则文本：目录形态（尾随 `/`）原样保留，其余按文件处理。
fn ignore_pattern(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    // `git status` 折叠目录带尾随 `/`；`.gitignore` 里 `dir/` 表示"整个目录"，
    // 语义比 `dir` 更精确（不会误匹配同名文件）。
    trimmed.replace('\\', "/")
}

/// 解析 `git for-each-ref` 的 `%(upstream:track,nobracket)` 输出。
///
/// 形如 `ahead 2, behind 1` / `ahead 3` / `behind 4` / 空串。
fn parse_track(track: &str) -> (u32, u32) {
    let mut ahead = 0;
    let mut behind = 0;
    for part in track.split(',') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix("ahead ") {
            ahead = rest.trim().parse().unwrap_or(0);
        } else if let Some(rest) = part.strip_prefix("behind ") {
            behind = rest.trim().parse().unwrap_or(0);
        }
    }
    (ahead, behind)
}

/// 合并 stdout 与 stderr（按"先 stdout 后 stderr"的顺序，去空行）。
fn join_streams(stdout: &str, stderr: &str) -> String {
    let mut out = String::new();
    for chunk in [stdout.trim_end(), stderr.trim_end()] {
        if chunk.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(chunk);
    }
    out
}

/// 取首行（用于一句话结论）。
fn first_line(text: &str) -> String {
    let line = text.lines().find(|line| !line.trim().is_empty());
    line.map_or_else(|| "未知原因".to_owned(), |line| line.trim().to_owned())
}

/// 把提交失败归类成用户能行动的一句话（FR-CMT-03）。
///
/// `hooks` 是探测到的生效 hook：git 在 hook 失败时往往不打印 "hook" 字样，
/// 只把 hook 自己的 stderr 透出来，因此分类必须结合"存在哪些 hook"这个事实。
fn classify_commit_failure(combined: &str, amend: bool, hooks: &[String]) -> String {
    let lower = combined.to_ascii_lowercase();
    if lower.contains("nothing to commit") || lower.contains("no changes added to commit") {
        return if amend {
            "没有可修订的内容：暂存区与 HEAD 一致".to_owned()
        } else {
            "没有可提交的内容：暂存区为空".to_owned()
        };
    }
    if lower.contains("author identity unknown") || lower.contains("empty ident") {
        return "缺少作者身份：请先配置 user.name / user.email".to_owned();
    }
    if lower.contains("hook") || lower.contains("commit-msg") {
        return "提交被 Git hook 拒绝（详见下方输出）".to_owned();
    }
    if lower.contains("gpg") || lower.contains("signing") {
        return "提交签名失败（core.gpgsign 生效中）".to_owned();
    }
    if !hooks.is_empty() {
        return format!(
            "提交被 Git hook（{}）拒绝（详见下方输出）",
            hooks.join(" / ")
        );
    }
    format!("提交失败：{}", first_line(combined))
}

#[cfg(test)]
mod tests {
    use super::{
        classify_commit_failure, ignore_pattern, parse_track, BranchOp, CommitRequest,
        IgnoreTarget, RepoWriter,
    };
    use crate::cli::GitCli;
    use crate::record::NoopObserver;
    use crate::repo::GitRepo;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::Arc;

    /// 造一个临时仓库并返回 (路径, 写入器)。
    fn fixture(label: &str) -> Option<(PathBuf, RepoWriter)> {
        let cli = GitCli::detect()?;
        let root =
            std::env::temp_dir().join(format!("knotwork-work-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).ok()?;

        let run = |args: &[&str]| {
            let output = Command::new(cli.program())
                .args(args)
                .current_dir(&root)
                .output()
                .expect("执行 git");
            assert!(
                output.status.success(),
                "git {args:?} 失败：{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "work@knotwork.invalid"]);
        run(&["config", "user.name", "Knotwork Work"]);
        run(&["config", "core.autocrlf", "false"]);
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\n").expect("写文件");
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "初始提交"]);

        let repo = GitRepo::open(&root, Arc::new(NoopObserver)).expect("打开仓库");
        let writer = RepoWriter::new(repo.location(), cli, repo.observer());
        Some((root, writer))
    }

    fn git(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("执行 git");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    #[test]
    fn stage_then_unstage_round_trips_with_the_index() {
        let Some((root, writer)) = fixture("stage") else {
            return;
        };
        std::fs::write(root.join("a.txt"), "one\nCHANGED\nthree\n").expect("改文件");
        std::fs::write(root.join("b.txt"), "new\n").expect("新文件");

        writer
            .stage_paths(&["a.txt".to_owned(), "b.txt".to_owned()])
            .expect("暂存");
        let staged = git(&root, &["diff", "--cached", "--name-only"]);
        assert!(
            staged.contains("a.txt") && staged.contains("b.txt"),
            "{staged}"
        );

        writer
            .unstage_paths(&["a.txt".to_owned(), "b.txt".to_owned()])
            .expect("反暂存");
        let staged_after = git(&root, &["diff", "--cached", "--name-only"]);
        assert!(
            staged_after.trim().is_empty(),
            "反暂存后索引应干净：{staged_after}"
        );
        let status = git(&root, &["status", "--porcelain"]);
        assert!(
            status.contains("?? b.txt"),
            "未跟踪文件应回到未跟踪：{status}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unstage_works_before_the_first_commit() {
        let Some((root, writer)) = fixture("unborn") else {
            return;
        };
        // 造一个 unborn HEAD：把 main 指回不存在。
        git(&root, &["update-ref", "-d", "refs/heads/main"]);
        std::fs::write(root.join("c.txt"), "c\n").expect("写文件");
        git(&root, &["add", "c.txt"]);

        writer
            .unstage_paths(&["c.txt".to_owned()])
            .expect("unborn 状态下反暂存应成功");
        let status = git(&root, &["status", "--porcelain"]);
        assert!(status.contains("?? c.txt"), "{status}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn discard_restores_the_worktree_copy() {
        let Some((root, writer)) = fixture("discard") else {
            return;
        };
        std::fs::write(root.join("a.txt"), "garbage\n").expect("改文件");
        writer.discard_tracked(&["a.txt".to_owned()]).expect("丢弃");
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).expect("读回"),
            "one\ntwo\nthree\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_untracked_respects_the_ignore_boundary() {
        let Some((root, writer)) = fixture("clean") else {
            return;
        };
        std::fs::create_dir_all(root.join("tmp")).expect("建目录");
        std::fs::write(root.join("tmp/x.txt"), "x\n").expect("写文件");
        std::fs::write(root.join(".gitignore"), "ignored/\n").expect("写忽略");
        std::fs::create_dir_all(root.join("ignored")).expect("建目录");
        std::fs::write(root.join("ignored/keep.txt"), "k\n").expect("写文件");

        writer
            .delete_untracked(&["tmp/".to_owned()])
            .expect("删除未跟踪目录");
        assert!(!root.join("tmp/x.txt").exists(), "未跟踪文件应被删除");
        assert!(
            root.join("ignored/keep.txt").exists(),
            "被忽略的文件不属于未跟踪，不得删除"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn commit_writes_the_message_and_reports_the_short_id() {
        let Some((root, writer)) = fixture("commit") else {
            return;
        };
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\nfour\n").expect("改文件");
        writer.stage_paths(&["a.txt".to_owned()]).expect("暂存");

        let outcome = writer
            .commit(&CommitRequest {
                message: "feat: 第四行\n\n正文第二段".to_owned(),
                ..CommitRequest::default()
            })
            .expect("提交");
        assert!(outcome.summary.contains("已提交"), "{outcome:?}");
        let log = git(&root, &["log", "-1", "--format=%s%n%b"]);
        assert!(log.contains("feat: 第四行"), "{log}");
        assert!(log.contains("正文第二段"), "多行消息必须完整保留：{log}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn commit_rejects_an_empty_message() {
        let Some((root, writer)) = fixture("empty-msg") else {
            return;
        };
        let err = writer
            .commit(&CommitRequest {
                message: "   \n".to_owned(),
                ..CommitRequest::default()
            })
            .expect_err("空消息必须被拒");
        assert!(err.to_string().contains("不能为空"), "{err}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn amend_replaces_the_last_commit() {
        let Some((root, writer)) = fixture("amend") else {
            return;
        };
        writer
            .commit(&CommitRequest {
                message: "改过的提交消息".to_owned(),
                amend: true,
                ..CommitRequest::default()
            })
            .expect("revise");
        let log = git(&root, &["log", "--format=%s"]);
        assert_eq!(log.lines().count(), 1, "amend 不应新增提交：{log}");
        assert!(log.contains("改过的提交消息"), "{log}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn hook_failure_is_reported_with_its_output() {
        let Some((root, writer)) = fixture("hook") else {
            return;
        };
        let hooks = root.join(".git/hooks");
        std::fs::create_dir_all(&hooks).expect("建 hooks 目录");
        let hook = hooks.join("pre-commit");
        // Windows 上 git 用 sh 执行 hook；内容保持 POSIX 写法即可。
        std::fs::write(&hook, "#!/bin/sh\necho '自定义拦截' >&2\nexit 1\n").expect("写 hook");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut perms = std::fs::metadata(&hook).expect("stat").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&hook, perms).expect("chmod");
        }

        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\nhook\n").expect("改文件");
        writer.stage_paths(&["a.txt".to_owned()]).expect("暂存");

        let err = writer
            .commit(&CommitRequest {
                message: "应被 hook 拒绝".to_owned(),
                ..CommitRequest::default()
            })
            .expect_err("hook 失败必须阻断提交");
        let text = err.to_string();
        assert!(text.contains("hook"), "{text}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn branch_operations_match_the_cli() {
        let Some((root, writer)) = fixture("branch") else {
            return;
        };
        writer
            .branch(&BranchOp::Create {
                name: "feature/x".to_owned(),
                start: None,
                checkout: true,
            })
            .expect("新建并检出");
        assert_eq!(
            git(&root, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
            "feature/x"
        );

        writer
            .branch(&BranchOp::Rename {
                from: "feature/x".to_owned(),
                to: "feature/y".to_owned(),
            })
            .expect("重命名");
        assert_eq!(
            git(&root, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
            "feature/y"
        );

        writer
            .branch(&BranchOp::Checkout {
                name: "main".to_owned(),
            })
            .expect("检出 main");
        writer
            .branch(&BranchOp::Delete {
                name: "feature/y".to_owned(),
                force: false,
            })
            .expect("删除已合并分支");
        let branches = git(&root, &["branch", "--format=%(refname:short)"]);
        assert!(!branches.contains("feature/y"), "{branches}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn deleting_an_unmerged_branch_reports_the_commits() {
        let Some((root, writer)) = fixture("unmerged") else {
            return;
        };
        writer
            .branch(&BranchOp::Create {
                name: "wip".to_owned(),
                start: None,
                checkout: true,
            })
            .expect("新建分支");
        std::fs::write(root.join("wip.txt"), "wip\n").expect("写文件");
        writer.stage_paths(&["wip.txt".to_owned()]).expect("暂存");
        writer
            .commit(&CommitRequest {
                message: "wip: 未合并的提交".to_owned(),
                ..CommitRequest::default()
            })
            .expect("提交");
        writer
            .branch(&BranchOp::Checkout {
                name: "main".to_owned(),
            })
            .expect("回到 main");

        let err = writer
            .branch(&BranchOp::Delete {
                name: "wip".to_owned(),
                force: false,
            })
            .expect_err("未合并分支不得被静默删除");
        let text = err.to_string();
        assert!(text.contains("wip"), "{text}");
        // 高危确认需要的"未合并提交"信息在 detail 里。
        if let crate::error::BackendError::Rejected { detail, .. } = &err {
            assert!(
                detail.contains("wip: 未合并的提交") || detail.contains("not fully merged"),
                "{detail}"
            );
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ignore_rules_are_appended_idempotently() {
        let Some((root, writer)) = fixture("ignore") else {
            return;
        };
        std::fs::write(root.join("scratch.log"), "x\n").expect("写文件");

        writer
            .append_ignore(IgnoreTarget::Gitignore, "scratch.log")
            .expect("写忽略");
        writer
            .append_ignore(IgnoreTarget::Gitignore, "scratch.log")
            .expect("重复写入应成功");
        let text = std::fs::read_to_string(root.join(".gitignore")).expect("读忽略文件");
        assert_eq!(text.matches("scratch.log").count(), 1, "{text}");
        assert!(writer.is_ignored("scratch.log").expect("check-ignore"));

        writer
            .append_ignore(IgnoreTarget::InfoExclude, "build/")
            .expect("写 exclude");
        let exclude = std::fs::read_to_string(root.join(".git/info/exclude")).expect("读 exclude");
        assert!(exclude.contains("build/"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn upstream_tracking_reports_ahead_and_behind() {
        let Some((root, writer)) = fixture("track") else {
            return;
        };
        // 造一个"上游领先 1、本地领先 1"的场景。
        git(&root, &["checkout", "-q", "-b", "topic"]);
        git(&root, &["branch", "--set-upstream-to=main", "topic"]);
        std::fs::write(root.join("t.txt"), "t\n").expect("写文件");
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "topic 提交"]);

        let rows = writer.upstream_tracking().expect("读跟踪信息");
        let topic = rows
            .iter()
            .find(|(name, _, _, _)| name == "topic")
            .expect("应包含 topic");
        assert_eq!(topic.1.as_deref(), Some("main"));
        assert_eq!(topic.2, 1, "本地领先 1");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn search_finds_messages_and_hashes() {
        let Some((root, writer)) = fixture("search") else {
            return;
        };
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\nsearch\n").expect("改文件");
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "feat: 增加搜索功能"]);

        let hits = writer
            .search_commits("搜索", false, true, false, 50, None)
            .expect("搜索中文");
        assert_eq!(hits.len(), 1, "{hits:?}");

        let regex_hits = writer
            .search_commits("feat: .*功能", true, true, false, 50, None)
            .expect("正则搜索");
        assert_eq!(regex_hits.len(), 1, "{regex_hits:?}");

        let head = git(&root, &["rev-parse", "HEAD"]).trim().to_owned();
        let short = &head[..8];
        let by_hash = writer
            .search_commits(short, false, true, false, 50, None)
            .expect("按哈希搜索");
        assert_eq!(by_hash.first().map(String::as_str), Some(head.as_str()));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn track_parser_handles_all_shapes() {
        assert_eq!(parse_track("ahead 2, behind 1"), (2, 1));
        assert_eq!(parse_track("ahead 3"), (3, 0));
        assert_eq!(parse_track("behind 4"), (0, 4));
        assert_eq!(parse_track(""), (0, 0));
    }

    #[test]
    fn ignore_pattern_normalizes_separators() {
        assert_eq!(ignore_pattern("dir\\file.txt"), "dir/file.txt");
        assert_eq!(ignore_pattern("dir/"), "dir/");
        assert_eq!(ignore_pattern("   "), "");
    }

    #[test]
    fn commit_failures_are_classified() {
        let no_hooks: Vec<String> = Vec::new();
        assert!(
            classify_commit_failure("pre-commit hook failed", false, &no_hooks).contains("hook")
        );
        assert!(
            classify_commit_failure("nothing to commit, working tree clean", false, &no_hooks)
                .contains("暂存区为空")
        );
        assert!(
            classify_commit_failure("Author identity unknown", false, &no_hooks)
                .contains("user.email")
        );
        assert!(classify_commit_failure("fatal: 未知", false, &no_hooks).starts_with("提交失败"));

        // 存在 hook 但输出里没有 "hook" 字样（实测 git 的常见行为）也必须能归因。
        let hooks = vec!["pre-commit".to_owned()];
        let classified = classify_commit_failure("自定义拦截", false, &hooks);
        assert!(classified.contains("pre-commit"), "{classified}");
    }

    #[test]
    fn lock_path_lives_in_the_git_directory() {
        let Some((root, writer)) = fixture("lockpath") else {
            return;
        };
        let lock = writer.lock_path();
        assert!(lock.starts_with(root.join(".git")), "{}", lock.display());
        assert_eq!(
            lock.file_name().and_then(|n| n.to_str()),
            Some(super::LOCK_FILE_NAME)
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
