//! 远端操作：远端管理、fetch / pull / push、推送预览与拒绝引导（FR-RMT-01/02/03/05）。
//!
//! # 为什么远端操作全部走 git CLI
//!
//! 路由表（架构规划 3.2）对 fetch/pull 写的是"gix 或 CLI 皆可"，并建议统一走 CLI：
//!
//! - **凭据与 hook 行为必须与命令行一致**。`gix` 不走 credential helper、
//!   不执行 `pre-push` hook，用户会得到"命令行能推、工具推不了"的分裂体验，
//!   这会直接摧毁信任（FR-RMT-04 的验收口径就是完全复用 CLI 的凭据协议）。
//! - **推送结果的语义只在 CLI 侧完整**：`--porcelain` 输出精确到每个 ref 的
//!   标志位与拒绝原因，足以支撑"被拒引导"（FR-RMT-03），自己实现要重造一套。
//!
//! 代价与写路径同源：每个操作一个子进程（网络操作本来就要等秒级），
//! 且受 [`crate::cli::MAX_CONCURRENT_PROCESSES`] 的并发上限约束。
//!
//! # 三类结果的区别
//!
//! - [`NetworkResult`]：fetch / pull 这类"整体成败"的操作，给一句结论 + 原文；
//! - [`PushPreview`]：**推送前**的预演（有哪些提交会被推上去、是否新建分支、
//!   是否触发受保护分支拦截）。它让"推送"从一个不可见动作变成一个可复核的动作；
//! - [`PushOutcome`]：推送后的每个 ref 的结果（含拒绝原因与下一步建议）。
//!
//! # 安全默认
//!
//! 强制推送必须显式勾选（`force_with_lease`），且受保护分支一律拒绝
//! （[`crate::protect::BranchProtection`]）。用 `--force-with-lease` 而不是
//! `--force`：前者在远端已被别人推进时失败，后者会把别人的提交直接抹掉。

use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

use crate::cli::{CancelProbe, CliOutput, GitCli, NetworkOptions, ProgressSink, LOCAL_TIMEOUT};
use crate::credential::{diagnose, host_of, is_ssh_url, redact_url, CredentialDiagnosis};
use crate::error::{BackendError, Result};
use crate::protect::BranchProtection;
use crate::record::SharedObserver;
use crate::repo::RepoLocation;

/// 推送预览里列出的提交上限。
///
/// 200 条足够覆盖"日常一次推送"，也避免一次把几万条提交搬进内存；
/// 超出时 `truncated` 为真，界面据此显示"仅显示前 200 条"。
pub const PUSH_PREVIEW_LIMIT: usize = 200;

/// 一个远端的展示信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteInfo {
    /// 远端名（`origin`）。
    pub name: String,
    /// fetch URL（**已脱敏**：URL 里可能内嵌 token）。
    pub fetch_url: Option<String>,
    /// push URL（与 fetch 不同时才单独给出）。
    pub push_url: Option<String>,
    /// URL 里是否内嵌了凭据（供界面提示"建议改用 credential helper"）。
    pub has_embedded_credentials: bool,
}

impl RemoteInfo {
    /// 用于列表展示的地址：优先 push URL（推送是用户最常关心的方向）。
    #[must_use]
    pub fn display_url(&self) -> &str {
        self.push_url
            .as_deref()
            .or(self.fetch_url.as_deref())
            .unwrap_or("—")
    }
}

/// 远端配置的修改（增删改）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteEdit {
    /// 新增远端。
    Add {
        /// 远端名。
        name: String,
        /// 地址。
        url: String,
    },
    /// 删除远端（同时删除其远程跟踪分支）。
    Remove {
        /// 远端名。
        name: String,
    },
    /// 重命名远端（git 会同步改写远程跟踪分支）。
    Rename {
        /// 原名。
        from: String,
        /// 新名。
        to: String,
    },
    /// 修改 URL；`push` 为真时改 push URL。
    SetUrl {
        /// 远端名。
        name: String,
        /// 新地址。
        url: String,
        /// 是否改 push URL。
        push: bool,
    },
}

impl RemoteEdit {
    /// 审计与提示用的简短描述。
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Add { name, url } => format!("添加远端 {name}（{}）", redact_url(url)),
            Self::Remove { name } => format!("删除远端 {name}"),
            Self::Rename { from, to } => format!("重命名远端 {from} → {to}"),
            Self::SetUrl { name, url, push } => format!(
                "修改远端 {name} 的{}地址为 {}",
                if *push { "推送" } else { "获取" },
                redact_url(url)
            ),
        }
    }
}

/// fetch 请求。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FetchRequest {
    /// 目标远端；[`None`] 表示全部远端（`--all`）。
    pub remote: Option<String>,
    /// 是否删除远端已不存在的跟踪分支（`--prune`）。
    pub prune: bool,
    /// 是否同时获取标签（`--tags`）。
    pub tags: bool,
}

/// pull 的整合策略（FR-RMT-02）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PullStrategy {
    /// 合并（`--no-rebase`）。
    #[default]
    Merge,
    /// 变基（`--rebase`）。
    Rebase,
    /// 只允许快进（`--ff-only`）：有分叉就失败，绝不产生合并提交。
    FastForwardOnly,
}

impl PullStrategy {
    /// 传给 git 的参数。
    #[must_use]
    pub const fn flag(self) -> &'static str {
        match self {
            Self::Merge => "--no-rebase",
            Self::Rebase => "--rebase",
            Self::FastForwardOnly => "--ff-only",
        }
    }

    /// 中文标签。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Merge => "合并",
            Self::Rebase => "变基",
            Self::FastForwardOnly => "仅快进",
        }
    }
}

/// pull 请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequest {
    /// 远端名。
    pub remote: String,
    /// 远端分支；[`None`] 表示使用当前分支的上游。
    pub branch: Option<String>,
    /// 整合策略。
    pub strategy: PullStrategy,
}

/// 一条推送目标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushTarget {
    /// 本地分支名。
    pub local_branch: String,
    /// 远端分支名（通常与本地同名）。
    pub remote_branch: String,
}

impl PushTarget {
    /// 同名的推送目标。
    #[must_use]
    pub fn same(branch: impl Into<String>) -> Self {
        let branch = branch.into();
        Self {
            local_branch: branch.clone(),
            remote_branch: branch,
        }
    }

    /// refspec（`local:remote`）。
    #[must_use]
    pub fn refspec(&self) -> String {
        format!("{}:{}", self.local_branch, self.remote_branch)
    }
}

/// push 请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushRequest {
    /// 远端名。
    pub remote: String,
    /// 推送目标（支持一次推多个分支）。
    pub targets: Vec<PushTarget>,
    /// 是否允许 `--force-with-lease`（必须由用户显式勾选）。
    pub force_with_lease: bool,
    /// 是否设置上游（`-u`）。
    pub set_upstream: bool,
    /// 是否同时推送标签（`--tags`）。
    pub tags: bool,
}

/// 预览列表里的一条提交。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushCommit {
    /// 完整对象 id。
    pub id: String,
    /// 短 id。
    pub short_id: String,
    /// 作者名。
    pub author: String,
    /// 提交时间（Unix 秒）。
    pub timestamp: i64,
    /// 标题行。
    pub subject: String,
}

/// 推送预览（FR-RMT-03 的"显示将被推送的提交列表"）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushPreview {
    /// 远端名。
    pub remote: String,
    /// 本地分支。
    pub local_branch: String,
    /// 目标远端分支。
    pub remote_branch: String,
    /// 上游引用（`origin/main`）；新分支时为 [`None`]。
    pub upstream: Option<String>,
    /// 领先上游的提交数。
    pub ahead: usize,
    /// 落后上游的提交数。
    pub behind: usize,
    /// 将被推送的提交（最新在前）。
    pub commits: Vec<PushCommit>,
    /// 提交列表是否被 [`PUSH_PREVIEW_LIMIT`] 截断。
    pub truncated: bool,
    /// 远端尚无同名分支（推送会创建它）。
    pub creates_branch: bool,
    /// 命中的受保护分支规则；[`None`] 表示不受保护。
    pub protection: Option<String>,
}

impl PushPreview {
    /// 是否没有任何需要推送的提交。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.commits.is_empty() && self.behind == 0 && !self.creates_branch
    }

    /// 一句话摘要，用于按钮与确认框。
    #[must_use]
    pub fn summary(&self) -> String {
        if self.creates_branch {
            return format!(
                "将在 {} 上新建分支 {}（{} 个提交）",
                self.remote,
                self.remote_branch,
                self.commits.len()
            );
        }
        if self.ahead == 0 && self.behind == 0 {
            return format!("{} 已是最新", self.remote_branch);
        }
        let mut text = format!("将推送 {} 个提交到 {}", self.ahead, self.remote);
        if self.behind > 0 {
            let _ = write!(text, "（远端另有 {} 个提交，推送可能被拒绝）", self.behind);
        }
        text
    }

    /// 是否建议先更新再推送：落后远端时普通推送必然被拒。
    #[must_use]
    pub fn needs_update_first(&self) -> bool {
        self.behind > 0
    }
}

/// 推送后单个 ref 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushUpdate {
    /// 目标 ref 描述（`main -> main`）。
    pub refspec: String,
    /// 是否成功。
    pub ok: bool,
    /// git 给出的原因（`[new branch]` / `[rejected] (non-fast-forward)`）。
    pub reason: String,
}

/// 一次网络操作的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetworkResult {
    /// 一句话结论（状态栏 / 提示条）。
    pub summary: String,
    /// git 的原始输出（已脱敏，供 Console 详情）。
    pub detail: String,
    /// 下一步建议。
    pub hints: Vec<String>,
    /// 逐 ref 的推送结果（仅 push 填充）。
    pub updates: Vec<PushUpdate>,
    /// 失败诊断（仅失败时填充）。
    pub diagnosis: Option<CredentialDiagnosis>,
}

impl NetworkResult {
    /// 成功结论。
    #[must_use]
    pub fn new(summary: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            ..Self::default()
        }
    }

    /// 附带原文。
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    /// 是否成功（由 `updates` 与失败诊断共同决定）。
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.diagnosis.is_none() && self.updates.iter().all(|update| update.ok)
    }
}

/// 远端能力清单（用于远端管理界面说明"凭据是怎么来的"）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteListing {
    /// 全部远端。
    pub remotes: Vec<RemoteInfo>,
    /// 配置的 credential helper（`git config --get-all credential.helper`）。
    pub helpers: Vec<String>,
    /// SSH agent 是否可用。
    pub ssh_agent: bool,
    /// 系统密钥链是否可用。
    pub keyring_available: bool,
}

/// 一次网络操作使用的 HTTPS 凭据。
///
/// 只由 `gt-service` 在**内存里**构造：值来自系统密钥链，用完即随本次调用丢弃，
/// 不写任何文件、不进审计记录（审计侧统一脱敏）。
#[derive(Clone, PartialEq, Eq)]
pub struct NetworkCredential {
    /// 用户名；为空时由 git 自己决定（多数托管服务接受任意非空用户名 + token）。
    pub username: String,
    /// 口令 / 访问 token。
    pub secret: String,
}

impl std::fmt::Debug for NetworkCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 永远不打印口令本身。
        f.debug_struct("NetworkCredential")
            .field("username", &self.username)
            .field("secret", &"<已隐藏>")
            .finish()
    }
}

/// 远端操作的执行器。
///
/// 与 [`crate::RepoWriter`] 的分工：writer 管"本地写"，这里管"跨进程/跨机器的写"。
/// 拆开而不是塞进一个类型，是因为两者的失败处理完全不同 ——
/// 本地失败是 git 拒绝，远端失败还多一层凭据与网络诊断。
pub struct RemoteOps {
    location: RepoLocation,
    cli: GitCli,
    observer: SharedObserver,
    /// 可选的 HTTPS 凭据（来自系统密钥链）；仅注入给本次子进程。
    credential: Option<NetworkCredential>,
}

impl std::fmt::Debug for RemoteOps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteOps")
            .field("location", &self.location)
            .finish_non_exhaustive()
    }
}

impl RemoteOps {
    /// 由仓库位置与 git 可执行文件构造。
    #[must_use]
    pub fn new(location: &RepoLocation, cli: GitCli, observer: SharedObserver) -> Self {
        Self {
            location: location.clone(),
            cli,
            observer,
            credential: None,
        }
    }

    /// 注入 HTTPS 凭据（来自密钥链），只在需要时调用。
    ///
    /// 注入方式是 `GIT_ASKPASS`：git 需要口令时反向调用本程序，本程序从
    /// 环境变量里取值打到 stdout。口令因此只活在**该子进程**的环境里，
    /// 不落盘、不进命令行参数（命令行参数会被 `ps`/任务管理器看到）。
    #[must_use]
    pub fn with_credential(mut self, credential: Option<NetworkCredential>) -> Self {
        self.credential = credential;
        self
    }

    /// 组装 askpass 环境变量；没有凭据时为空。
    fn askpass_env(&self) -> Vec<(String, String)> {
        let Some(credential) = self.credential.as_ref() else {
            return Vec::new();
        };
        let Ok(exe) = std::env::current_exe() else {
            tracing::warn!("无法定位自身可执行文件，跳过密钥链凭据注入");
            return Vec::new();
        };
        vec![
            ("GIT_ASKPASS".to_owned(), exe.display().to_string()),
            (
                crate::credential::ASKPASS_SECRET_ENV.to_owned(),
                credential.secret.clone(),
            ),
            (
                crate::credential::ASKPASS_USERNAME_ENV.to_owned(),
                credential.username.clone(),
            ),
        ]
    }

    /// 工作目录（裸仓库退化到 `.git`）。
    fn work_dir(&self) -> &Path {
        self.location.work_dir_or_git_dir()
    }

    // ------------------------------------------------------------ 远端管理

    /// 列出全部远端（FR-RMT-01）。
    ///
    /// 用 `git remote -v` 而不是逐个 `git config --get remote.<name>.url`：
    /// 前者一次拿全 fetch/push 两类 URL，且顺序稳定。
    ///
    /// # Errors
    /// 执行失败时返回 [`BackendError::Rejected`]。
    pub fn list_remotes(&self) -> Result<Vec<RemoteInfo>> {
        let output = self.run_local(&["remote", "-v"])?;
        if !output.is_success() {
            return Err(rejected("读取远端列表失败", &output));
        }

        let mut remotes: Vec<RemoteInfo> = Vec::new();
        for line in output.stdout.lines() {
            // 形如：`origin\thttps://host/r.git (fetch)`
            let Some((name, rest)) = line.split_once('\t') else {
                continue;
            };
            let Some((url, kind)) = rest.rsplit_once(' ') else {
                continue;
            };
            let raw = url.trim();
            let embedded = raw.contains('@') && raw.contains("://");
            let shown = redact_url(raw);
            let entry = if let Some(entry) = remotes.iter_mut().find(|item| item.name == name) {
                entry
            } else {
                remotes.push(RemoteInfo {
                    name: name.to_owned(),
                    fetch_url: None,
                    push_url: None,
                    has_embedded_credentials: false,
                });
                remotes.last_mut().expect("刚插入的条目")
            };
            entry.has_embedded_credentials |= embedded;
            match kind {
                "(fetch)" => entry.fetch_url = Some(shown),
                "(push)" => entry.push_url = Some(shown),
                _ => {}
            }
        }

        // push URL 与 fetch 相同时不重复展示（git 默认如此），保持列表简洁。
        for remote in &mut remotes {
            if remote.fetch_url == remote.push_url {
                remote.push_url = None;
            }
        }
        remotes.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(remotes)
    }

    /// 执行一次远端配置修改（FR-RMT-01）。
    ///
    /// # Errors
    /// git 拒绝时返回 [`BackendError::Rejected`]。
    pub fn edit_remote(&self, edit: &RemoteEdit) -> Result<NetworkResult> {
        let output = match edit {
            RemoteEdit::Add { name, url } => self.run_local(&["remote", "add", name, url])?,
            RemoteEdit::Remove { name } => self.run_local(&["remote", "remove", name])?,
            RemoteEdit::Rename { from, to } => self.run_local(&["remote", "rename", from, to])?,
            RemoteEdit::SetUrl { name, url, push } => {
                if *push {
                    self.run_local(&["remote", "set-url", "--push", name, url])?
                } else {
                    self.run_local(&["remote", "set-url", name, url])?
                }
            }
        };

        if !output.is_success() {
            return Err(rejected(&format!("{}失败", edit.label()), &output));
        }
        Ok(NetworkResult::new(format!("{} 完成", edit.label())))
    }

    // ------------------------------------------------------------ fetch / pull

    /// 获取远端更新（FR-RMT-01）。
    ///
    /// # Errors
    /// - 取消 → [`BackendError::Cancelled`]；超时 → [`BackendError::ProcessTimeout`]；
    /// - 认证 / 网络失败 → [`BackendError::Rejected`]（文案含建议动作）。
    pub fn fetch(
        &self,
        request: &FetchRequest,
        cancel: &dyn CancelProbe,
        progress: Option<Arc<dyn ProgressSink>>,
    ) -> Result<NetworkResult> {
        let mut args = vec!["fetch".to_owned(), "--progress".to_owned()];
        if request.prune {
            args.push("--prune".to_owned());
        }
        if request.tags {
            args.push("--tags".to_owned());
        }
        match request.remote.as_deref() {
            Some(remote) => args.push(remote.to_owned()),
            None => args.push("--all".to_owned()),
        }

        let target = request
            .remote
            .clone()
            .unwrap_or_else(|| "全部远端".to_owned());
        let (output, url) = self.run_remote(&args, cancel, progress, request.remote.as_deref())?;
        if !output.is_success() {
            return Err(network_failure(&target, &output, url.as_deref()));
        }

        // fetch 成功时 git 只在有新引用时输出；没有输出即"已是最新"。
        let detail = output.merged();
        let summary = if detail.trim().is_empty() {
            format!("{target}：没有新内容")
        } else {
            let updated = count_ref_updates(&detail);
            if updated == 0 {
                format!("已从 {target} 获取更新")
            } else {
                format!("已从 {target} 获取 {updated} 个引用更新")
            }
        };
        Ok(NetworkResult::new(summary).with_detail(detail))
    }

    /// 拉取并整合远端更新（FR-RMT-02）。
    ///
    /// 用 `git pull` 而不是 `fetch + merge/rebase` 两步：两步之间用户可能
    /// 在终端里插入操作，而我们无法像 git 那样在同一个进程内保证原子性。
    /// 代价是进度输出由 pull 自己聚合（够用）。
    ///
    /// # Errors
    /// 同 [`Self::fetch`]；快进策略下出现分叉时额外给出"改用 rebase/merge"的建议。
    pub fn pull(
        &self,
        request: &PullRequest,
        cancel: &dyn CancelProbe,
        progress: Option<Arc<dyn ProgressSink>>,
    ) -> Result<NetworkResult> {
        let mut args = vec![
            "pull".to_owned(),
            "--progress".to_owned(),
            "--no-edit".to_owned(),
            request.strategy.flag().to_owned(),
            request.remote.clone(),
        ];
        if let Some(branch) = request.branch.as_deref() {
            args.push(branch.to_owned());
        }

        let (output, url) = self.run_remote(&args, cancel, progress, Some(&request.remote))?;
        if !output.is_success() {
            let mut failure = network_failure(&request.remote, &output, url.as_deref());
            if let BackendError::Network { summary, .. } = &mut failure {
                if request.strategy == PullStrategy::FastForwardOnly && summary.contains("快进") {
                    summary.push_str("（策略为「仅快进」：改用「合并」或「变基」可处理分叉）");
                }
            }
            return Err(failure);
        }

        let detail = output.merged();
        let summary = classify_pull(&detail, request.strategy, &request.remote);
        Ok(NetworkResult::new(summary).with_detail(detail))
    }

    // ---------------------------------------------------------------- 推送

    /// 生成推送预览（FR-RMT-03）。
    ///
    /// 预览只读、不产生任何远端动作 —— 它是"推送前复核"的依据。
    ///
    /// # Errors
    /// - 分离头指针 / 无提交 → [`BackendError::Rejected`]（文案说明原因）；
    /// - git 执行失败 → [`BackendError::Rejected`]。
    pub fn push_preview(
        &self,
        remote: &str,
        branch: Option<&str>,
        protection: &BranchProtection,
    ) -> Result<PushPreview> {
        let local_branch = match branch {
            Some(branch) => branch.to_owned(),
            None => self
                .current_branch()?
                .ok_or_else(|| BackendError::Rejected {
                    summary: "当前处于分离头指针状态，没有可推送的分支".to_owned(),
                    detail: "先检出一个分支（或新建分支）再推送。".to_owned(),
                })?,
        };

        // 上游优先，其次 `<remote>/<branch>`，都没有就是新建分支。
        let upstream = self.upstream_of(&local_branch);
        let base = upstream
            .clone()
            .or_else(|| self.resolve(&format!("refs/remotes/{remote}/{local_branch}")));
        let creates_branch = base.is_none();

        let (ahead, behind) = match base.as_deref() {
            Some(base) => self.ahead_behind(base, "HEAD")?,
            None => (self.count_commits("HEAD --not --remotes")?, 0),
        };

        let commits = match base.as_deref() {
            Some(base) => self.preview_commits(&format!("{base}..HEAD"))?,
            // 新分支：只列"远端所有分支都没有的提交"，避免把整段历史都算进去。
            // `--not` 对**其后**的修订生效，因此 HEAD 必须写在 `--not` 之前。
            None => self.preview_commits("HEAD --not --remotes")?,
        };
        let truncated = commits.len() >= PUSH_PREVIEW_LIMIT;

        let remote_branch = local_branch_from_base(upstream.as_deref(), remote, &local_branch);
        let protection_hit = protection.matched(&local_branch).map(str::to_owned);
        Ok(PushPreview {
            remote: remote.to_owned(),
            local_branch,
            remote_branch,
            upstream,
            ahead,
            behind,
            commits,
            truncated,
            creates_branch,
            protection: protection_hit,
        })
    }

    /// 执行推送（FR-RMT-03）。
    ///
    /// 强制推送的拦截在这里做**最后一道**判定：界面会先拦一次（给出可读文案），
    /// 但服务层可能被其他调用方复用，不能只依赖界面。
    ///
    /// # Errors
    /// - 受保护分支 + 强制推送 → [`BackendError::Rejected`]（含替代做法）；
    /// - 被远端拒绝 → [`BackendError::Rejected`]（含"先更新再推"引导）。
    pub fn push(
        &self,
        request: &PushRequest,
        protection: &BranchProtection,
        cancel: &dyn CancelProbe,
        progress: Option<Arc<dyn ProgressSink>>,
    ) -> Result<NetworkResult> {
        if request.targets.is_empty() {
            return Err(BackendError::Rejected {
                summary: "没有选择要推送的分支".to_owned(),
                detail: String::new(),
            });
        }
        if request.force_with_lease {
            for target in &request.targets {
                let verdict = protection.check_push(&target.remote_branch, true);
                if !verdict.is_allowed() {
                    return Err(BackendError::Rejected {
                        summary: verdict.message(),
                        detail: "受保护分支禁止强制推送（FR-RMT-03）".to_owned(),
                    });
                }
            }
        }

        let mut args = vec![
            "push".to_owned(),
            "--progress".to_owned(),
            "--porcelain".to_owned(),
        ];
        if request.force_with_lease {
            // 用 lease 而不是 --force：远端已被别人推进时失败，绝不静默覆盖。
            args.push("--force-with-lease".to_owned());
        }
        if request.set_upstream {
            args.push("--set-upstream".to_owned());
        }
        if request.tags {
            args.push("--tags".to_owned());
        }
        args.push(request.remote.clone());
        for target in &request.targets {
            args.push(target.refspec());
        }

        let (output, url) = self.run_remote(&args, cancel, progress, Some(&request.remote))?;
        let updates = parse_push_porcelain(&output.stdout);

        if !output.is_success() {
            // 被拒（non-fast-forward 等）属于"可以引导"的失败：在诊断结论后
            // 追加逐 ref 的下一步建议，让用户不必自己去读 git 原文。
            let failure = network_failure(&request.remote, &output, url.as_deref());
            let BackendError::Network {
                summary,
                detail,
                kind,
            } = failure
            else {
                return Err(failure);
            };
            let mut full = summary;
            for hint in push_hints(&output.merged(), request.force_with_lease, &output) {
                full.push_str("\n  · ");
                full.push_str(&hint);
            }
            return Err(BackendError::Network {
                summary: full,
                detail,
                kind,
            });
        }

        let pushed = updates.iter().filter(|update| update.ok).count();
        let summary = if pushed == 0 {
            format!("{} 已是最新", request.remote)
        } else if updates.len() > 1 {
            format!("已向 {} 推送 {pushed} 个分支", request.remote)
        } else {
            let target = &request.targets[0];
            match updates.iter().find(|update| update.ok) {
                Some(update) if update.reason.contains("new branch") => format!(
                    "已在 {} 上创建分支 {}",
                    request.remote, target.remote_branch
                ),
                _ => format!("已推送 {} 到 {}", target.local_branch, request.remote),
            }
        };
        Ok(NetworkResult {
            summary,
            detail: output.merged(),
            hints: Vec::new(),
            updates,
            diagnosis: None,
        })
    }

    // ------------------------------------------------------------ 能力探测

    /// credential helper 与 SSH agent 的可用情况（FR-RMT-04 的诊断信息）。
    #[must_use]
    pub fn credential_helpers(&self) -> Vec<String> {
        self.run_local(&["config", "--get-all", "credential.helper"])
            .ok()
            .filter(CliOutput::is_success)
            .map(|output| {
                output
                    .stdout
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// SSH agent 是否可用。
    ///
    /// 只检查环境变量：Windows 上 OpenSSH agent 与 Pageant 都会导出
    /// `SSH_AUTH_SOCK`。更深层的探测（`ssh-add -l`）需要起进程，不值得为一个
    /// 提示文案付出代价。
    #[must_use]
    pub fn ssh_agent_available(&self) -> bool {
        std::env::var_os("SSH_AUTH_SOCK").is_some()
    }

    /// 远端主机的凭据服务键（用于密钥链存取）。
    #[must_use]
    pub fn credential_host(&self, remote: &str) -> Option<String> {
        host_of(&self.remote_url(remote)?)
    }

    /// 从远端 URL 里取内嵌的用户名（`https://bob@host/r` → `bob`）。
    ///
    /// 密钥链只存口令，用户名靠 URL 或上次保存的值补齐；两者都没有时返回空串，
    /// 由 git 用任意非空占位符（多数托管服务的 token 认证接受这种形式）。
    #[must_use]
    pub fn credential_username(&self, remote: &str) -> String {
        let Some(url) = self.remote_url(remote) else {
            return String::new();
        };
        let Some((scheme, rest)) = url.split_once("://") else {
            return String::new();
        };
        if !scheme.eq_ignore_ascii_case("https") && !scheme.eq_ignore_ascii_case("http") {
            return String::new();
        }
        let authority = rest.split('/').next().unwrap_or(rest);
        let Some((userinfo, _)) = authority.rsplit_once('@') else {
            return String::new();
        };
        userinfo.split(':').next().unwrap_or_default().to_owned()
    }

    // ---------------------------------------------------------------- 内部

    /// 执行一条本地命令（远端配置、只读查询）。
    fn run_local(&self, args: &[&str]) -> Result<CliOutput> {
        self.cli.run(
            Some(self.work_dir()),
            args,
            self.observer.as_ref(),
            LOCAL_TIMEOUT,
        )
    }

    /// 执行一条网络命令，并按远端名查出 URL（供诊断使用）。
    ///
    /// 返回值里的 URL 是**原始地址**（含可能的凭据），只用于组装诊断与
    /// `askpass`，绝不进入审计记录 —— 审计侧统一走 [`crate::credential::redact`]。
    fn run_remote(
        &self,
        args: &[String],
        cancel: &dyn CancelProbe,
        progress: Option<Arc<dyn ProgressSink>>,
        remote: Option<&str>,
    ) -> Result<(CliOutput, Option<String>)> {
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        let url = remote.and_then(|name| self.remote_url(name));
        let output = self.cli.run_network(
            Some(self.work_dir()),
            &borrowed,
            self.observer.as_ref(),
            NetworkOptions {
                cancel: Some(cancel),
                progress,
                env: self.askpass_env(),
            },
        )?;
        Ok((output, url))
    }

    /// 某远端的 fetch URL（原始）。
    fn remote_url(&self, remote: &str) -> Option<String> {
        let key = format!("remote.{remote}.url");
        let output = self.run_local(&["config", "--get", &key]).ok()?;
        if !output.is_success() {
            return None;
        }
        let url = output.stdout.trim();
        if url.is_empty() {
            None
        } else {
            Some(url.to_owned())
        }
    }

    /// 当前分支名；分离头指针时为 [`None`]。
    fn current_branch(&self) -> Result<Option<String>> {
        let output = self.run_local(&["symbolic-ref", "--quiet", "--short", "HEAD"])?;
        if !output.is_success() {
            return Ok(None);
        }
        let name = output.stdout.trim();
        if name.is_empty() {
            Ok(None)
        } else {
            Ok(Some(name.to_owned()))
        }
    }

    /// 分支的上游（`origin/main`）。
    fn upstream_of(&self, branch: &str) -> Option<String> {
        let spec = format!("{branch}@{{u}}");
        let output = self
            .run_local(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", &spec])
            .ok()?;
        if !output.is_success() {
            return None;
        }
        let name = output.stdout.trim();
        if name.is_empty() {
            None
        } else {
            Some(name.to_owned())
        }
    }

    /// 解析一个引用为完整对象 id。
    fn resolve(&self, spec: &str) -> Option<String> {
        let output = self
            .run_local(&["rev-parse", "--verify", "--quiet", spec])
            .ok()?;
        if !output.is_success() {
            return None;
        }
        let id = output.stdout.trim();
        (id.len() == 40 || id.len() == 64).then(|| id.to_owned())
    }

    /// `(ahead, behind)`：`base...HEAD` 的两侧计数。
    fn ahead_behind(&self, base: &str, head: &str) -> Result<(usize, usize)> {
        let range = format!("{base}...{head}");
        let output = self.run_local(&["rev-list", "--left-right", "--count", &range])?;
        if !output.is_success() {
            return Ok((0, 0));
        }
        let mut parts = output.stdout.split_whitespace();
        let behind = parts.next().and_then(|text| text.parse().ok()).unwrap_or(0);
        let ahead = parts.next().and_then(|text| text.parse().ok()).unwrap_or(0);
        Ok((ahead, behind))
    }

    /// 某个 range 内的提交数（range 可含多个 token，如 `--not --remotes HEAD`）。
    fn count_commits(&self, range: &str) -> Result<usize> {
        let mut args: Vec<&str> = vec!["rev-list", "--count"];
        args.extend(range.split_whitespace());
        args.push("--");
        let output = self.run_local(&args)?;
        if !output.is_success() {
            // 空仓库（无 HEAD）时 `rev-list` 会失败：当作 0 个提交。
            return Ok(0);
        }
        Ok(output.stdout.trim().parse().unwrap_or(0_usize))
    }

    /// 预览范围内的提交（最新在前，最多 [`PUSH_PREVIEW_LIMIT`] 条）。
    fn preview_commits(&self, range: &str) -> Result<Vec<PushCommit>> {
        let format = "--format=%H%x00%h%x00%an%x00%ct%x00%s";
        let limit = format!("--max-count={PUSH_PREVIEW_LIMIT}");
        // range 可能是 `--not --remotes HEAD` 这样的多 token 写法，按空白拆开。
        let mut args: Vec<&str> = vec!["log", format, &limit, "--no-color"];
        args.extend(range.split_whitespace());
        let output = self.run_local(&args)?;
        if !output.is_success() {
            return Ok(Vec::new());
        }
        Ok(parse_commit_lines(&output.stdout))
    }
}

/// 把失败翻译成可行动的说明。
///
/// 做成自由函数而不是方法：它不依赖仓库位置与 git 句柄，只吃"输出 + URL"，
/// 因此可以脱离 `RemoteOps` 单独测试（`diagnose` 的分支覆盖走的就是这条路）。
fn network_failure(target: &str, output: &CliOutput, url: Option<&str>) -> BackendError {
    let merged = output.merged();
    let diagnosis = diagnose(&merged, url);
    let summary = format!(
        "{target}：{}\n{}",
        diagnosis.summary,
        format_actions(&diagnosis)
    );
    let mut detail = merged;
    if let Some(url) = url {
        let _ = write!(detail, "\n远端：{}", redact_url(url));
    }
    if is_ssh_url(url.unwrap_or_default()) {
        detail.push_str("\n传输：SSH");
    }
    BackendError::Network {
        summary,
        detail,
        kind: diagnosis.kind,
    }
}

/// 由上游引用推导远端分支名。
fn local_branch_from_base(upstream: Option<&str>, remote: &str, local: &str) -> String {
    upstream
        .and_then(|name| name.strip_prefix(&format!("{remote}/")))
        .map_or_else(|| local.to_owned(), str::to_owned)
}

/// 把诊断的建议拼成多行文本。
fn format_actions(diagnosis: &CredentialDiagnosis) -> String {
    if diagnosis.actions.is_empty() {
        return String::new();
    }
    let mut out = String::from("建议：");
    for (index, action) in diagnosis.actions.iter().enumerate() {
        let _ = write!(out, "\n  {}. {action}", index + 1);
    }
    out
}

/// 组装"被拒绝"错误。
fn rejected(summary: &str, output: &CliOutput) -> BackendError {
    BackendError::Rejected {
        summary: summary.to_owned(),
        detail: output.merged(),
    }
}

/// 解析 `git log --format` 的输出。
fn parse_commit_lines(text: &str) -> Vec<PushCommit> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| {
            let mut parts = line.split('\0');
            let id = parts.next()?.to_owned();
            let short_id = parts.next()?.to_owned();
            let author = parts.next()?.to_owned();
            let timestamp = parts
                .next()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
            let subject = parts.next().unwrap_or_default().to_owned();
            Some(PushCommit {
                id,
                short_id,
                author,
                timestamp,
                subject,
            })
        })
        .collect()
}

/// 解析 `git push --porcelain` 的标准输出。
///
/// 每行形如 `<flag>\t<from>:<to>\t<summary>`；`Done` 是结束标记。
fn parse_push_porcelain(stdout: &str) -> Vec<PushUpdate> {
    let mut updates = Vec::new();
    for line in stdout.lines() {
        if line.trim().is_empty() || line.trim() == "Done" {
            continue;
        }
        let mut parts = line.splitn(3, '\t');
        let flag = parts.next().unwrap_or_default();
        let refspec = parts.next().unwrap_or_default();
        let reason = parts.next().unwrap_or_default();
        // 第一行是 `To <url>`，没有 flag 字段。
        if flag.starts_with("To ") {
            continue;
        }
        updates.push(PushUpdate {
            refspec: refspec.to_owned(),
            ok: flag != "!",
            reason: reason.to_owned(),
        });
    }
    updates
}

/// 统计 fetch 的输出里有多少个引用更新。
fn count_ref_updates(detail: &str) -> usize {
    detail
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("* [new") || trimmed.contains("->") || trimmed.starts_with('+')
        })
        .count()
}

/// 归类 pull 的结果。
fn classify_pull(detail: &str, strategy: PullStrategy, remote: &str) -> String {
    let lower = detail.to_ascii_lowercase();
    if lower.contains("already up to date") || lower.contains("already up-to-date") {
        return format!("{remote}：已是最新");
    }
    if lower.contains("fast-forward") || lower.contains("fast forward") {
        return format!("已从 {remote} 快进更新");
    }
    if strategy == PullStrategy::Rebase || lower.contains("successfully rebased") {
        return format!("已从 {remote} 拉取并变基");
    }
    if lower.contains("merge made by") || lower.contains("files changed") {
        return format!("已从 {remote} 拉取并合并");
    }
    format!("已从 {remote} 拉取")
}

/// 推送失败后的下一步建议。
fn push_hints(detail: &str, force: bool, output: &CliOutput) -> Vec<String> {
    let lower = detail.to_ascii_lowercase();
    let mut hints = Vec::new();

    if lower.contains("non-fast-forward") || lower.contains("fetch first") {
        hints.push("先执行 Update（拉取并合并/变基）把远端变更整合进来，再重新推送。".to_owned());
        hints.push(
            "若确认要以本地历史覆盖远端，勾选「强制（force-with-lease）」后重试。".to_owned(),
        );
    }
    if lower.contains("stale info") {
        hints.push(
            "force-with-lease 的租约已过期：远端在你上次 fetch 之后被推进过。先 Fetch 复核别人的提交，再决定是否强制推送。"
                .to_owned(),
        );
    }
    if lower.contains("remote rejected") || lower.contains("hook declined") {
        hints.push(
            "远端策略拒绝了本次推送（保护分支或服务端 hook），详见 Console 中的完整输出。"
                .to_owned(),
        );
    }
    if lower.contains("does not appear to be a git repository")
        || lower.contains("repository not found")
    {
        hints.push("远端地址不可用或无权访问：检查 URL 与账号权限。".to_owned());
    }
    if force && lower.contains("protected") {
        hints.push("目标分支在远端受保护，无法强制推送（本工具的受保护策略无关）。".to_owned());
    }
    if hints.is_empty() {
        if let Some(code) = output.exit_code {
            hints.push(format!(
                "git 退出码 {code}：详见 Console 中该条记录的完整输出。"
            ));
        }
    }
    hints
}

#[cfg(test)]
mod tests {
    use super::{
        local_branch_from_base, parse_commit_lines, parse_push_porcelain, push_hints, FetchRequest,
        PullStrategy, PushPreview, PushRequest, PushTarget, RemoteEdit, RemoteOps,
    };
    use crate::cli::GitCli;
    use crate::protect::BranchProtection;
    use crate::record::noop_observer;
    use crate::repo::RepoLocation;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// 建一个临时仓库目录（唯一名字，避免并行测试互踩）。
    fn temp_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "knotwork-remote-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        ));
        std::fs::create_dir_all(&path).expect("创建临时目录");
        path
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("执行 git");
        assert!(
            status.status.success(),
            "git {args:?} 失败：{}",
            String::from_utf8_lossy(&status.stderr)
        );
    }

    fn location(dir: &Path) -> RepoLocation {
        RepoLocation {
            git_dir: dir.join(".git"),
            work_dir: Some(dir.to_path_buf()),
            is_bare: false,
        }
    }

    /// 准备"一个工作仓库 + 一个裸远端"，并建立首次推送。
    fn fixture(label: &str) -> Option<(PathBuf, PathBuf, RemoteOps)> {
        let cli = GitCli::detect()?;
        let root = temp_dir(label);
        let work = root.join("work");
        let bare = root.join("remote.git");
        std::fs::create_dir_all(&work).ok()?;

        git(
            &root,
            &["init", "--bare", "--initial-branch=main", "remote.git"],
        );
        git(&work, &["init", "--initial-branch=main"]);
        git(&work, &["config", "user.name", "Knotwork Test"]);
        git(&work, &["config", "user.email", "test@example.com"]);
        std::fs::write(work.join("a.txt"), "one\n").ok()?;
        git(&work, &["add", "."]);
        git(&work, &["commit", "-m", "first"]);
        git(&work, &["remote", "add", "origin", bare.to_str()?]);

        let ops = RemoteOps::new(&location(&work), cli, noop_observer());
        Some((work, bare, ops))
    }

    fn cleanup(root: &Path) {
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn porcelain_parsing_covers_success_rejection_and_new_branch() {
        let stdout = "To https://host/r.git\n\
                      \trefs/heads/main:refs/heads/main\t1234..5678\n\
                      *\trefs/heads/feat:refs/heads/feat\t[new branch]\n\
                      !\trefs/heads/other:refs/heads/other\t[rejected] (non-fast-forward)\n\
                      Done\n";
        let updates = parse_push_porcelain(stdout);
        assert_eq!(updates.len(), 3, "应跳过 To 行与 Done 行");
        assert!(updates[0].ok);
        assert!(updates[1].ok);
        assert!(updates[1].reason.contains("new branch"));
        assert!(!updates[2].ok, "拒绝必须被标记为失败");
        assert!(updates[2].reason.contains("non-fast-forward"));
    }

    #[test]
    fn commit_lines_parse_five_fields() {
        let text = "abc\x00abc123\x00Alice\x001700000000\x00subject one\n";
        let commits = parse_commit_lines(text);
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].author, "Alice");
        assert_eq!(commits[0].subject, "subject one");
        assert_eq!(commits[0].timestamp, 1_700_000_000);
    }

    #[test]
    fn remote_branch_name_follows_the_upstream() {
        assert_eq!(
            local_branch_from_base(Some("origin/main"), "origin", "main"),
            "main"
        );
        assert_eq!(
            local_branch_from_base(Some("upstream/release"), "upstream", "release"),
            "release"
        );
        assert_eq!(
            local_branch_from_base(None, "origin", "feature/x"),
            "feature/x"
        );
    }

    #[test]
    fn hints_point_at_update_then_retry() {
        let output = crate::cli::CliOutput {
            stdout: String::new(),
            stdout_bytes: Vec::new(),
            stderr: String::from("! [rejected] main -> main (non-fast-forward)"),
            exit_code: Some(1),
            truncated: false,
        };
        let hints = push_hints(&output.merged(), false, &output);
        assert!(hints.iter().any(|hint| hint.contains("Update")));
        assert!(hints.iter().any(|hint| hint.contains("force-with-lease")));
    }

    #[test]
    fn preview_summary_distinguishes_new_branch_and_divergence() {
        let mut preview = PushPreview {
            remote: "origin".to_owned(),
            local_branch: "main".to_owned(),
            remote_branch: "main".to_owned(),
            upstream: Some("origin/main".to_owned()),
            ahead: 2,
            behind: 1,
            commits: Vec::new(),
            truncated: false,
            creates_branch: false,
            protection: None,
        };
        assert!(preview.summary().contains('2'));
        assert!(preview.summary().contains("可能被拒绝"));
        assert!(preview.needs_update_first());

        preview.creates_branch = true;
        assert!(preview.summary().contains("新建分支"));
    }

    #[test]
    fn listing_and_editing_remotes_round_trips_on_a_real_repo() {
        let Some((work, bare, ops)) = fixture("edit") else {
            eprintln!("未检测到 git，跳过");
            return;
        };

        let remotes = ops.list_remotes().expect("列远端");
        assert_eq!(remotes.len(), 1);
        assert_eq!(remotes[0].name, "origin");
        assert_eq!(
            remotes[0].display_url(),
            bare.to_str().expect("路径可转字符串")
        );
        assert!(!remotes[0].has_embedded_credentials);

        ops.edit_remote(&RemoteEdit::Add {
            name: "mirror".to_owned(),
            url: "https://alice:sekrit@example.com/r.git".to_owned(),
        })
        .expect("新增远端");
        let remotes = ops.list_remotes().expect("列远端");
        let mirror = remotes
            .iter()
            .find(|item| item.name == "mirror")
            .expect("新远端");
        assert!(
            mirror.has_embedded_credentials,
            "URL 内嵌凭据必须被标记（供界面提示）"
        );
        assert!(
            !mirror.display_url().contains("sekrit"),
            "展示用 URL 必须脱敏：{}",
            mirror.display_url()
        );

        ops.edit_remote(&RemoteEdit::Rename {
            from: "mirror".to_owned(),
            to: "backup".to_owned(),
        })
        .expect("重命名");
        ops.edit_remote(&RemoteEdit::Remove {
            name: "backup".to_owned(),
        })
        .expect("删除");
        assert_eq!(ops.list_remotes().expect("列远端").len(), 1);

        let host = ops.credential_host("origin");
        assert!(host.is_none(), "本地路径远端没有主机名");

        cleanup(work.parent().expect("父目录"));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn fetch_pull_and_push_work_against_a_local_remote() {
        let Some((work, bare, ops)) = fixture("flow") else {
            eprintln!("未检测到 git，跳过");
            return;
        };

        // 首次推送：远端尚无 main，预览应标记为新建分支。
        let preview = ops
            .push_preview("origin", Some("main"), &BranchProtection::default())
            .expect("预览");
        assert!(preview.creates_branch, "远端没有 main，应识别为新建分支");
        assert_eq!(preview.commits.len(), 1);

        ops.push(
            &PushRequest {
                remote: "origin".to_owned(),
                targets: vec![PushTarget::same("main")],
                force_with_lease: false,
                set_upstream: true,
                tags: false,
            },
            &BranchProtection::default(),
            &|| false,
            None,
        )
        .expect("首次推送");

        // 远端已有 main：再取预览应是"已是最新"。
        let preview = ops
            .push_preview("origin", Some("main"), &BranchProtection::default())
            .expect("预览");
        assert!(!preview.creates_branch);
        assert_eq!(preview.ahead, 0);
        assert!(preview.is_empty(), "无待推内容时应为空: {preview:?}");

        // 本地再提交一次，预览应显示 1 个待推提交。
        std::fs::write(work.join("a.txt"), "two\n").expect("写文件");
        git(&work, &["add", "."]);
        git(&work, &["commit", "-m", "second"]);
        let preview = ops
            .push_preview("origin", Some("main"), &BranchProtection::default())
            .expect("预览");
        assert_eq!(preview.ahead, 1);
        assert_eq!(preview.commits.len(), 1);
        assert_eq!(preview.commits[0].subject, "second");

        let result = ops
            .push(
                &PushRequest {
                    remote: "origin".to_owned(),
                    targets: vec![PushTarget::same("main")],
                    force_with_lease: false,
                    set_upstream: false,
                    tags: false,
                },
                &BranchProtection::default(),
                &|| false,
                None,
            )
            .expect("推送");
        assert!(result.is_success(), "{result:?}");
        assert!(result.summary.contains("已推送"), "{}", result.summary);

        // 另一个克隆推进远端后，本地再推必须被拒（非快进）。
        let other = work.parent().expect("父目录").join("other");
        git(
            work.parent().expect("父目录"),
            &[
                "clone",
                bare.to_str().expect("路径"),
                other.to_str().expect("路径"),
            ],
        );
        git(&other, &["config", "user.name", "Other"]);
        git(&other, &["config", "user.email", "other@example.com"]);
        std::fs::write(other.join("b.txt"), "x\n").expect("写文件");
        git(&other, &["add", "."]);
        git(&other, &["commit", "-m", "from other"]);
        git(&other, &["push", "origin", "main"]);

        std::fs::write(work.join("a.txt"), "three\n").expect("写文件");
        git(&work, &["add", "."]);
        git(&work, &["commit", "-m", "third"]);

        let failure = ops.push(
            &PushRequest {
                remote: "origin".to_owned(),
                targets: vec![PushTarget::same("main")],
                force_with_lease: false,
                set_upstream: false,
                tags: false,
            },
            &BranchProtection::default(),
            &|| false,
            None,
        );
        let message = failure.expect_err("被拒绝的推送必须是错误").to_string();
        assert!(
            message.contains("Update") || message.contains("快进") || message.contains("拒绝"),
            "拒绝必须给出可行动建议：{message}"
        );

        // fetch 之后 behind 应为 1。
        ops.fetch(
            &FetchRequest {
                remote: Some("origin".to_owned()),
                prune: true,
                tags: false,
            },
            &|| false,
            None,
        )
        .expect("fetch");
        let preview = ops
            .push_preview("origin", Some("main"), &BranchProtection::default())
            .expect("预览");
        assert_eq!(preview.behind, 1, "fetch 后应看到远端领先 1 个提交");

        // pull（合并）后本地包含远端提交。
        let pulled = ops
            .pull(
                &super::PullRequest {
                    remote: "origin".to_owned(),
                    branch: Some("main".to_owned()),
                    strategy: PullStrategy::Merge,
                },
                &|| false,
                None,
            )
            .expect("pull");
        assert!(pulled.is_success());

        cleanup(work.parent().expect("父目录"));
    }

    #[test]
    fn force_push_to_a_protected_branch_is_refused_before_spawning() {
        let Some((work, _bare, ops)) = fixture("protect") else {
            eprintln!("未检测到 git，跳过");
            return;
        };
        let protection = BranchProtection::new(&["^main$".to_owned()]);
        let error = ops
            .push(
                &PushRequest {
                    remote: "origin".to_owned(),
                    targets: vec![PushTarget::same("main")],
                    force_with_lease: true,
                    set_upstream: false,
                    tags: false,
                },
                &protection,
                &|| false,
                None,
            )
            .expect_err("受保护分支的强制推送必须被拒");
        let message = error.to_string();
        assert!(message.contains("受保护分支"), "{message}");
        assert!(message.contains("替代做法"), "{message}");

        cleanup(work.parent().expect("父目录"));
    }

    #[test]
    fn missing_remote_produces_an_actionable_error() {
        let Some((work, _bare, ops)) = fixture("missing") else {
            eprintln!("未检测到 git，跳过");
            return;
        };
        let error = ops
            .fetch(
                &FetchRequest {
                    remote: Some("nowhere".to_owned()),
                    prune: false,
                    tags: false,
                },
                &|| false,
                None,
            )
            .expect_err("不存在的远端必须失败");
        let message = error.to_string();
        assert!(
            message.contains("建议") || message.contains("远端"),
            "失败信息必须可行动：{message}"
        );

        cleanup(work.parent().expect("父目录"));
    }
}
