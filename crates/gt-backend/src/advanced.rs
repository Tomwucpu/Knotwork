//! 高级写操作：合并 / 变基（含交互式）/ 拣选 / 回滚 / 重置 / 贮藏 / 补丁 / 标签。
//!
//! # 为什么整层都走 git CLI（对实施计划 4-1 的偏离与理由）
//!
//! 实施计划把 4-1 定为"git2 集成层（merge/revert 底座）"，路由表也把 merge /
//! cherry-pick / revert 的**主实现**标为 `git2`。本模块统一选了 CLI，理由不是省事：
//!
//! 1. **验收口径就是"与 CLI 等价"**（需求文档 9 节 L3）。走 CLI 时等价性是构造出来的，
//!    不需要逐项对齐 libgit2 与 git 本体的差异（合并策略、`merge.renormalize`、
//!    `rerere`、`merge.conflictStyle=zdiff3` 的标记形态、`core.autocrlf` 的落盘时机）。
//! 2. **hooks 必须在场**。`merge` 会触发 `pre-merge-commit`/`commit-msg`，`revert` 会
//!    触发 `commit-msg`。用户装了 hook 就是要求它生效，libgit2 路径绕过它等于行为不一致。
//! 3. **冲突标记的形态由 git 决定**。三路编辑器解析的是工作区里的标记；若由 libgit2
//!    自己写标记，就会出现"我们写一套、用户在终端里 `git merge` 得到另一套"，
//!    FR-CFL-03 的"按标记解析"立刻失真。
//! 4. **不引入 C 工具链依赖**。`git2` 需要编译 libgit2，与"轻量、可复现构建"的目标相悖。
//!
//! 代价：每次操作一个子进程（10–60ms 起，merge/rebase 本身是秒级）。这类操作是
//! 用户点击级低频操作，不触及 NFR-PRF-03 的 200ms 感知延迟。
//!
//! 计划 4-1 真正要解决的约束**没有丢**：`git2` 的 `Repository` 不能并发写，CLI 同样
//! 不能并发跑（`index.lock` 会互相踩）。因此服务层给每个仓库加了一把**高级操作槽位**，
//! 同一仓库同时只允许一个高级操作（见 `gt-service` 的 `RepoHandle::begin_advanced`）。
//!
//! # 交互式变基怎么落地
//!
//! 交互式变基的本质是"序列编辑器"：git 把待办列表写到一个临时文件，等编辑器改完后
//! 按列表执行。本模块的做法是**自己生成待办列表**，再用 `GIT_SEQUENCE_EDITOR` 把它
//! 覆盖进去：
//!
//! ```text
//! GIT_SEQUENCE_EDITOR="cp <我们生成的 todo>"  git rebase -i <base>
//! ```
//!
//! `reword` 不用 git 的 `reword` 指令，而是 `pick` + 紧跟一条
//! `exec git commit --amend -F <消息文件>`：`exec` 由 git 在**该提交**之后执行，
//! 语义确定，且不需要第二个编辑器回调（少一个可失败的环节）。
//!
//! `squash` 必然让 git 打开提交消息编辑器（合并多条消息），此时用
//! `GIT_EDITOR=true` 让 git 直接采用它自己拼好的默认消息 —— 不设这个变量，git 会
//! 尝试启动真实编辑器；在没有终端的 GUI 进程里，那就是一次挂到超时的死等。
//!
//! # 中断与恢复
//!
//! 冲突不是错误，是**一种状态**。所有操作统一返回 [`AdvancedOutcome`]：
//! 冲突时 `conflicts` 非空、`state` 是 `Merge`/`RebaseMerge`/…，界面据此进入
//! 三路编辑器或给出 continue / skip / abort（FR-STA-02）。

use std::path::{Path, PathBuf};
use std::time::Duration;

use gt_core::{MarkerPresence, RepoState};

use crate::cli::{CancelProbe, CliOutput, GitCli, LOCAL_TIMEOUT};
use crate::error::{BackendError, Result};
use crate::record::SharedObserver;
use crate::repo::RepoLocation;
use crate::work::WriteOutcome;

/// 合并 / 拣选 / 回滚 / 重置的默认超时。
///
/// 比 [`LOCAL_TIMEOUT`]（30s）宽：这些操作要重写工作区、可能触发 hook，
/// 在机械硬盘上的大仓库里 30s 不够用，而它们本身是用户主动发起的等待。
pub const ADVANCED_TIMEOUT: Duration = Duration::from_secs(120);

/// 变基的超时（重放 N 个提交，每个都可能触发 hook）。
pub const REBASE_TIMEOUT: Duration = Duration::from_secs(300);

/// 交互式变基的临时目录（位于 `.git` 下，随仓库移动，不进工作区）。
pub const PLAN_DIR_NAME: &str = "knotwork-rebase";

/// 一次高级操作的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvancedOutcome {
    /// 一句话结论（直接进 toast）。
    pub summary: String,
    /// git 的原始输出（进 Console / 详情）。
    pub detail: String,
    /// 处于冲突中的文件（仓库相对路径）；为空表示没有冲突。
    pub conflicts: Vec<String>,
    /// 操作结束后的仓库状态。
    pub state: RepoState,
}

impl Default for AdvancedOutcome {
    fn default() -> Self {
        Self {
            summary: String::new(),
            detail: String::new(),
            conflicts: Vec::new(),
            state: RepoState::Clean,
        }
    }
}

impl AdvancedOutcome {
    /// 是否停在冲突上（需要用户逐块解决）。
    #[must_use]
    pub fn is_conflicted(&self) -> bool {
        !self.conflicts.is_empty()
    }

    /// 是否停在某种"进行中"状态（含冲突、含无冲突的中断）。
    #[must_use]
    pub fn is_interrupted(&self) -> bool {
        self.state.is_in_progress()
    }
}

// ---------------------------------------------------------------- 合并

/// 合并策略（FR-MRG-01）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MergeStrategy {
    /// 跟随 git 配置（默认允许快进）。
    #[default]
    Default,
    /// 始终生成合并提交（`--no-ff`）。
    NoFastForward,
    /// 只允许快进，否则拒绝（`--ff-only`）。
    FastForwardOnly,
    /// 把更改压进工作区但不提交（`--squash`）。
    Squash,
}

impl MergeStrategy {
    /// 对应的命令行参数。
    #[must_use]
    pub const fn args(self) -> &'static [&'static str] {
        match self {
            Self::Default => &[],
            Self::NoFastForward => &["--no-ff"],
            Self::FastForwardOnly => &["--ff-only"],
            Self::Squash => &["--squash"],
        }
    }

    /// 中文标签（下拉框直接展示）。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Default => "跟随仓库设置",
            Self::NoFastForward => "始终生成合并提交",
            Self::FastForwardOnly => "仅允许快进",
            Self::Squash => "压缩为未提交更改",
        }
    }
}

/// 一次合并请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeRequest {
    /// 被合入的修订（分支名 / 提交号 / `origin/main`）。
    pub rev: String,
    /// 策略。
    pub strategy: MergeStrategy,
    /// 合并提交消息；[`None`] 时用 git 的默认消息。
    pub message: Option<String>,
}

/// 一次变基请求（FR-MRG-02）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebaseRequest {
    /// 变基目标（分支 / 提交）。
    pub onto: String,
}

/// 变基的三键（FR-MRG-02）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebaseControl {
    /// 继续（冲突已解决）。
    Continue,
    /// 跳过当前提交。
    Skip,
    /// 中止并回到变基前。
    Abort,
}

impl RebaseControl {
    /// 命令行参数。
    #[must_use]
    pub const fn args(self) -> &'static [&'static str] {
        match self {
            Self::Continue => &["rebase", "--continue"],
            Self::Skip => &["rebase", "--skip"],
            Self::Abort => &["rebase", "--abort"],
        }
    }

    /// 中文标签。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Continue => "继续变基",
            Self::Skip => "跳过此提交",
            Self::Abort => "中止变基",
        }
    }

    /// 是否为破坏性动作（`skip` 会丢弃该提交的改动）。
    #[must_use]
    pub const fn is_destructive(self) -> bool {
        matches!(self, Self::Skip)
    }
}

// ------------------------------------------------------------ 交互式变基

/// 交互式变基里单个提交的动作（FR-MRG-03 只承诺这五种）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RebaseAction {
    /// 保留。
    #[default]
    Pick,
    /// 保留并改写消息。
    Reword,
    /// 与前一个提交合并，消息拼接。
    Squash,
    /// 与前一个提交合并，丢弃本条消息。
    Fixup,
    /// 丢弃该提交。
    Drop,
}

impl RebaseAction {
    /// git 待办列表里的指令名。
    ///
    /// `reword` 刻意也返回 `pick`：改写消息由紧随其后的 `exec git commit --amend`
    /// 完成（见 [`RebasePlan::build_todo`]），不使用 git 自己的 `reword` 指令 ——
    /// 那会额外要求一个消息编辑器回调，多一个可失败的环节。
    #[allow(clippy::match_same_arms)]
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Pick => "pick",
            Self::Reword => "pick",
            Self::Squash => "squash",
            Self::Fixup => "fixup",
            Self::Drop => "drop",
        }
    }

    /// 中文标签。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pick => "保留",
            Self::Reword => "改写消息",
            Self::Squash => "压缩（合并消息）",
            Self::Fixup => "压缩（丢弃消息）",
            Self::Drop => "丢弃",
        }
    }

    /// 是否要求用户提供新消息。
    #[must_use]
    pub const fn needs_message(self) -> bool {
        matches!(self, Self::Reword)
    }

    /// 是否是"把本条并入上一条"。
    #[must_use]
    pub const fn is_squash_like(self) -> bool {
        matches!(self, Self::Squash | Self::Fixup)
    }

    /// 全部动作（界面下拉框顺序）。
    pub const ALL: [Self; 5] = [
        Self::Pick,
        Self::Reword,
        Self::Squash,
        Self::Fixup,
        Self::Drop,
    ];
}

/// 计划中的一步。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebaseStep {
    /// 完整提交 id。
    pub id: String,
    /// 消息首行（仅用于展示与待办列表的注释位）。
    pub subject: String,
    /// 动作。
    pub action: RebaseAction,
    /// `reword` 的新消息。
    pub message: Option<String>,
}

impl RebaseStep {
    /// 新建一步（默认保留）。
    #[must_use]
    pub fn new(id: impl Into<String>, subject: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            subject: subject.into(),
            action: RebaseAction::Pick,
            message: None,
        }
    }

    /// 缩写 id（7 位，与 `git log --oneline` 一致）。
    #[must_use]
    pub fn short(&self) -> String {
        self.id.chars().take(7).collect()
    }
}

/// 一次交互式变基的完整计划。
///
/// `steps` 的顺序即**执行顺序**（从最老的提交到最新），与 git 待办列表一致。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RebasePlan {
    /// 变基目标（用户选的"变基到"）。
    pub onto: String,
    /// 待办列表的起点提交的父（`git rebase -i <base>` 的实参）。
    pub base: String,
    /// 生成计划时的 HEAD，用于执行前校验历史是否已被改动。
    pub head: Option<String>,
    /// 区间内被线性化丢弃的合并提交数。
    ///
    /// `git rebase -i` 默认不保留合并提交（等价于 `--no-rebase-merges`），
    /// 因此待办列表里不会出现它们。**这是必须让用户知道的事实**：
    /// 一个含合并的分支做交互式变基后，历史形状会变。
    pub merges_dropped: usize,
    /// 步骤。
    pub steps: Vec<RebaseStep>,
}

/// 计划非法。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// 没有步骤。
    Empty,
    /// 第一条就是 squash / fixup。
    FirstCannotSquash,
    /// reword 缺少消息。
    MissingRewordMessage {
        /// 步骤下标（0 基）。
        index: usize,
    },
    /// 全部步骤都被丢弃。
    AllDropped,
}

impl std::fmt::Display for PlanError {
    /// 面向用户的说明。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "区间内没有可重排的提交"),
            Self::FirstCannotSquash => {
                write!(f, "第一个提交不能是「压缩」：它没有可压缩的上一条提交")
            }
            Self::MissingRewordMessage { index } => {
                write!(f, "第 {} 个提交选择了「改写消息」，但新消息为空", index + 1)
            }
            Self::AllDropped => write!(f, "全部提交都被丢弃：这样会清空该分支的这些提交"),
        }
    }
}

impl RebasePlan {
    /// 校验计划。
    ///
    /// # Errors
    /// 见 [`PlanError`]。
    pub fn validate(&self) -> std::result::Result<(), PlanError> {
        if self.steps.is_empty() {
            return Err(PlanError::Empty);
        }
        if self
            .steps
            .iter()
            .all(|step| step.action == RebaseAction::Drop)
        {
            return Err(PlanError::AllDropped);
        }
        if let Some(first) = self
            .steps
            .iter()
            .find(|step| step.action != RebaseAction::Drop)
        {
            if first.action.is_squash_like() {
                return Err(PlanError::FirstCannotSquash);
            }
        }
        for (index, step) in self.steps.iter().enumerate() {
            if step.action.needs_message()
                && step
                    .message
                    .as_deref()
                    .is_none_or(|message| message.trim().is_empty())
            {
                return Err(PlanError::MissingRewordMessage { index });
            }
        }
        Ok(())
    }

    /// 实际会被保留的提交数（排除 drop）。
    #[must_use]
    pub fn kept(&self) -> usize {
        self.steps
            .iter()
            .filter(|step| step.action != RebaseAction::Drop)
            .count()
    }

    /// 生成 git 待办列表文本。
    ///
    /// `dir` 是消息文件的落地目录（`reword` 的 `exec` 行要引用它）。
    ///
    /// # Errors
    /// 计划非法时返回 [`PlanError`]。
    pub fn build_todo(&self, dir: &Path) -> std::result::Result<String, PlanError> {
        self.validate()?;
        let mut todo = String::new();
        for (index, step) in self.steps.iter().enumerate() {
            let subject = sanitize_subject(&step.subject);
            // 丢弃也显式写 `drop` 而不是"从列表里删掉"：待办列表就是用户看到的那份
            // 计划，界面与执行内容一致，排查时不需要脑内还原。
            todo.push_str(step.action.token());
            todo.push(' ');
            todo.push_str(&step.short());
            if !subject.is_empty() {
                todo.push(' ');
                todo.push_str(&subject);
            }
            todo.push('\n');

            if step.action == RebaseAction::Reword {
                // `reword` 换成 pick + exec：不依赖 git 再回调一次消息编辑器。
                // `--no-verify`：内容已由上面的 pick 提交并通过 hook，这里只改消息，
                // 若因为无关的 pre-commit 状态让整个变基停在半路，代价远大于收益。
                todo.push_str(
                    "exec git commit --amend --allow-empty --no-verify --cleanup=strip -F ",
                );
                todo.push_str(&shell_quote(&message_path(dir, index)));
                todo.push('\n');
            }
        }
        Ok(todo)
    }
}

/// 变基进度（FR-MRG-02 的"第 x/y 个"）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebaseProgress {
    /// 当前第几个（1 基）。
    pub current: u32,
    /// 总共几个。
    pub total: u32,
    /// 变基前的分支引用（`refs/heads/feature`）。
    pub head_name: Option<String>,
}

impl RebaseProgress {
    /// 面向用户的进度说明。
    #[must_use]
    pub fn label(&self) -> String {
        let branch = self
            .head_name
            .as_deref()
            .map(|name| name.trim_start_matches("refs/heads/").to_owned());
        match branch {
            Some(name) if !name.is_empty() => {
                format!(
                    "变基进行中：{name} 第 {}/{} 个提交",
                    self.current, self.total
                )
            }
            _ => format!("变基进行中：第 {}/{} 个提交", self.current, self.total),
        }
    }
}

// -------------------------------------------------- 拣选 / 回滚 / 重置

/// 一次拣选请求（FR-MRG-04）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CherryPickRequest {
    /// 提交（可含区间写法 `A^..B`，git 自己解析）。
    pub revs: Vec<String>,
    /// 追加来源行 `(cherry picked from commit ...)`（`-x`）。
    pub record_source: bool,
    /// 只改工作区不提交（`--no-commit`）。
    pub no_commit: bool,
}

/// 一次回滚请求（FR-MRG-05）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevertRequest {
    /// 提交（可含区间）。
    pub revs: Vec<String>,
    /// 只改工作区不提交。
    pub no_commit: bool,
}

/// 重置模式（FR-MRG-06）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResetMode {
    /// 只移动分支指针，索引与工作区不动。
    #[default]
    Soft,
    /// 移动指针并重置索引，工作区不动。
    Mixed,
    /// 全部重置（**丢弃**索引与工作区改动，需强确认）。
    Hard,
}

impl ResetMode {
    /// 命令行参数。
    #[must_use]
    pub const fn arg(self) -> &'static str {
        match self {
            Self::Soft => "--soft",
            Self::Mixed => "--mixed",
            Self::Hard => "--hard",
        }
    }

    /// 中文标签。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Soft => "软重置（保留暂存）",
            Self::Mixed => "混合重置（取消暂存）",
            Self::Hard => "硬重置（丢弃全部改动）",
        }
    }

    /// 是否会丢弃未提交的改动。
    #[must_use]
    pub const fn is_destructive(self) -> bool {
        matches!(self, Self::Hard)
    }
}

/// 一次重置请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetRequest {
    /// 目标修订。
    pub target: String,
    /// 模式。
    pub mode: ResetMode,
}

// ---------------------------------------------------------------- Stash

/// 贮藏操作（FR-STH）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StashOp {
    /// 创建。
    Create {
        /// 消息。
        message: String,
        /// 是否包含未跟踪文件（`-u`）。
        include_untracked: bool,
    },
    /// 应用（保留该条贮藏）。
    Apply {
        /// 下标（`stash@{n}` 的 n）。
        index: u32,
    },
    /// 弹出（应用并删除）。
    Pop {
        /// 下标。
        index: u32,
    },
    /// 删除。
    Drop {
        /// 下标。
        index: u32,
    },
    /// 从贮藏创建分支（成功后该条贮藏被删除，与 git 行为一致）。
    Branch {
        /// 下标。
        index: u32,
        /// 新分支名。
        name: String,
    },
}

impl StashOp {
    /// 中文标签。
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Create { .. } => "创建贮藏",
            Self::Apply { .. } => "应用贮藏",
            Self::Pop { .. } => "弹出贮藏",
            Self::Drop { .. } => "删除贮藏",
            Self::Branch { .. } => "从贮藏创建分支",
        }
    }

    /// 是否不可逆（`drop` 删除后只能靠 `git fsck` 捞）。
    #[must_use]
    pub const fn is_destructive(&self) -> bool {
        matches!(self, Self::Drop { .. })
    }
}

/// 一条贮藏记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StashEntry {
    /// 下标。
    pub index: u32,
    /// 选择子（`stash@{0}`）。
    pub selector: String,
    /// 完整描述（`WIP on main: 1a2b3c4 提交消息`）。
    pub message: String,
}

impl StashEntry {
    /// 摘要：去掉 `WIP on <branch>: <缩写哈希> ` 前缀后的提交消息。
    #[must_use]
    pub fn summary(&self) -> String {
        let text = &self.message;
        for prefix in ["WIP on ", "On "] {
            if let Some(rest) = text.strip_prefix(prefix) {
                let tail = rest.split_once(": ").map_or(rest, |(_, tail)| tail);
                return strip_leading_hash(tail).to_owned();
            }
        }
        text.clone()
    }

    /// 所属分支（`WIP on main:` 的 `main`）。
    #[must_use]
    pub fn branch(&self) -> Option<String> {
        let rest = self.message.strip_prefix("WIP on ")?;
        let (branch, _) = rest.split_once(':')?;
        Some(branch.to_owned())
    }
}

// ---------------------------------------------------------------- 补丁

/// 补丁的来源（FR-PTC-01）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchSource {
    /// 单个提交，`format-patch` 语义（带作者与提交消息，可用 `git am` 应用）。
    Commit {
        /// 提交。
        rev: String,
    },
    /// 两个修订之间的差异，`diff` 语义（纯文本差异，可用 `git apply` 应用）。
    Range {
        /// 起点。
        from: String,
        /// 终点。
        to: String,
    },
    /// 工作区相对 HEAD 的差异。
    Worktree,
}

impl PatchSource {
    /// 中文标签（说明用途差异，避免用户选错）。
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Commit { .. } => "提交补丁（format-patch）",
            Self::Range { .. } => "区间差异（diff）",
            Self::Worktree => "工作区差异（diff）",
        }
    }
}

/// 应用补丁的选项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchApplyRequest {
    /// 补丁文件路径。
    pub file: PathBuf,
    /// 三方合并（`--3way`）：上下文不匹配时用 blob 信息尝试合并，可能产生冲突。
    pub three_way: bool,
    /// 反向应用（`--reverse`）。
    pub reverse: bool,
}

// ---------------------------------------------------------------- 标签

/// 标签操作（FR-BR-03）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagOp {
    /// 新建；`message` 非空时为附注标签（`-a`）。
    Create {
        /// 标签名。
        name: String,
        /// 附注消息。
        message: Option<String>,
        /// 目标修订；[`None`] 表示 HEAD。
        target: Option<String>,
    },
    /// 删除。
    Delete {
        /// 标签名。
        name: String,
    },
}

impl TagOp {
    /// 中文标签。
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Create { name, message, .. } => {
                if message.is_some() {
                    format!("新建附注标签 {name}")
                } else {
                    format!("新建轻量标签 {name}")
                }
            }
            Self::Delete { name } => format!("删除标签 {name}"),
        }
    }
}

// ------------------------------------------------------------ 执行器

/// 高级写操作执行器。
pub struct AdvancedOps {
    location: RepoLocation,
    cli: GitCli,
    observer: SharedObserver,
}

impl std::fmt::Debug for AdvancedOps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdvancedOps")
            .field("location", &self.location)
            .finish_non_exhaustive()
    }
}

impl AdvancedOps {
    /// 构造。
    #[must_use]
    pub fn new(location: &RepoLocation, cli: GitCli, observer: SharedObserver) -> Self {
        Self {
            location: location.clone(),
            cli,
            observer,
        }
    }

    /// 工作目录；裸仓库退化到 `.git`。
    #[must_use]
    pub fn work_dir(&self) -> &Path {
        self.location.work_dir_or_git_dir()
    }

    // ------------------------------------------------------------ 合并

    /// 合并一个修订到当前分支（FR-MRG-01）。
    ///
    /// # Errors
    /// 无冲突的真实失败返回 [`BackendError::Rejected`]；冲突**不是**错误。
    pub fn merge(&self, request: &MergeRequest) -> Result<AdvancedOutcome> {
        self.ensure_revision(&request.rev)?;
        let mut args: Vec<String> = vec!["merge".to_owned()];
        args.extend(request.strategy.args().iter().map(|arg| (*arg).to_owned()));
        match &request.message {
            Some(message) => {
                args.push("-m".to_owned());
                args.push(message.clone());
            }
            None => args.push("--no-edit".to_owned()),
        }
        args.push(request.rev.clone());

        let output = self.run(&args, ADVANCED_TIMEOUT, None)?;
        let success = match request.strategy {
            MergeStrategy::Squash => {
                format!("已把 {} 的更改压入工作区与索引（未提交）", request.rev)
            }
            _ => format!("已合并 {}", request.rev),
        };
        self.finish(&output, success, "合并失败", |outcome| {
            let count = outcome.conflicts.len();
            format!("合并 {} 时发生冲突：{count} 个文件待解决", request.rev)
        })
    }

    /// 中止进行中的合并（FR-STA-02）。
    ///
    /// # Errors
    /// git 拒绝时返回错误。
    pub fn abort_merge(&self) -> Result<AdvancedOutcome> {
        let output = self.run(&str_args(&["merge", "--abort"]), ADVANCED_TIMEOUT, None)?;
        self.finish(
            &output,
            "已中止合并".to_owned(),
            "中止合并失败",
            |_| "合并中止后仍有冲突残留".to_owned(),
        )
    }

    // ------------------------------------------------------------ 变基

    /// 当前分支变基到 `onto`（FR-MRG-02）。
    ///
    /// # Errors
    /// 无冲突的真实失败返回 [`BackendError::Rejected`]。
    pub fn rebase(&self, request: &RebaseRequest) -> Result<AdvancedOutcome> {
        self.ensure_revision(&request.onto)?;
        let output = self.run(
            &str_args(&["rebase", request.onto.as_str()]),
            REBASE_TIMEOUT,
            None,
        )?;
        let onto = request.onto.clone();
        self.finish(
            &output,
            format!("已变基到 {onto}"),
            "变基失败",
            |outcome| {
                let progress = self.rebase_progress();
                let at = progress.map_or_else(
                    || "某个提交".to_owned(),
                    |progress| format!("第 {}/{} 个提交", progress.current, progress.total),
                );
                format!("变基在{at}中断：{} 个文件冲突", outcome.conflicts.len())
            },
        )
    }

    /// 变基三键（FR-MRG-02）。
    ///
    /// # Errors
    /// 无冲突的真实失败返回 [`BackendError::Rejected`]。
    pub fn rebase_control(&self, control: RebaseControl) -> Result<AdvancedOutcome> {
        let output = self.run(&str_args(control.args()), REBASE_TIMEOUT, None)?;
        let summary = format!("{} 完成", control.label());
        self.finish(
            &output,
            summary,
            &format!("{}失败", control.label()),
            |outcome| format!("仍有 {} 个文件冲突", outcome.conflicts.len()),
        )
    }

    /// 交互式变基（FR-MRG-03）。
    ///
    /// # Errors
    /// 计划非法 → [`BackendError::Rejected`]（文案来自 [`PlanError`]）；
    /// 历史已变化 → [`BackendError::Rejected`]。
    pub fn interactive_rebase(
        &self,
        plan: &RebasePlan,
        cancel: Option<&dyn CancelProbe>,
    ) -> Result<AdvancedOutcome> {
        plan.validate().map_err(|error| BackendError::Rejected {
            summary: error.to_string(),
            detail: String::new(),
        })?;

        // 计划是"用户看到的那份历史"的快照。若期间 HEAD 被移动（外部提交、切分支），
        // 照旧执行会重放一组不存在或已变化的提交 —— 宁可直接拒绝。
        if let Some(expected) = &plan.head {
            let head = self.rev_parse("HEAD")?;
            if &head != expected {
                return Err(BackendError::Rejected {
                    summary: "分支已被改动，请重新打开交互式变基编辑器".to_owned(),
                    detail: format!("计划基于 {expected}，当前 HEAD 是 {head}"),
                });
            }
        }

        let dir = self.plan_dir();
        Self::prepare_plan_files(plan, &dir)?;

        let todo = dir.join("plan.todo");
        let env = Self::base_env()
            .into_iter()
            .chain([(
                "GIT_SEQUENCE_EDITOR".to_owned(),
                format!("cp {}", shell_quote(&normalize(&todo))),
            )])
            .collect();

        let output = self.run_with_env(
            &["rebase", "-i", "--no-autosquash", "-q", plan.base.as_str()],
            env,
            REBASE_TIMEOUT,
            cancel,
        )?;

        // 成功即清理临时计划；中断时**保留**：待办列表还引用着消息文件，
        // 用户 `--continue` 时 exec 行还要再读一次（这是最容易踩的坑）。
        if output.is_success() {
            self.cleanup_plan_dir();
        }

        let kept = plan.kept();
        self.finish(
            &output,
            format!("交互式变基完成（{kept} 个提交）"),
            "交互式变基失败",
            |outcome| {
                let progress = self.rebase_progress();
                let at = progress.map_or_else(
                    || "某个提交".to_owned(),
                    |progress| format!("第 {}/{} 个", progress.current, progress.total),
                );
                format!("变基在{at}中断：{} 个文件冲突", outcome.conflicts.len())
            },
        )
    }

    /// 读取变基进度（无进行中的变基时为 [`None`]）。
    #[must_use]
    pub fn rebase_progress(&self) -> Option<RebaseProgress> {
        read_rebase_progress(&self.location.git_dir)
    }

    /// 为交互式变基准备计划（生成提交列表，不执行）。
    ///
    /// `base` 是区间起点（不含），通常是用户选中的提交的父提交。
    ///
    /// # Errors
    /// git 读取失败时返回错误。
    pub fn plan_from_base(&self, onto: &str, base: &str) -> Result<RebasePlan> {
        self.ensure_revision(base)?;
        let range = format!("{base}..HEAD");
        let output = self.run(
            &str_args(&[
                "log",
                "--reverse",
                "--no-merges",
                "--format=%H%x1f%s",
                range.as_str(),
            ]),
            LOCAL_TIMEOUT,
            None,
        )?;
        if !output.is_success() {
            return Err(BackendError::Rejected {
                summary: format!("读取 {range} 的提交失败"),
                detail: output.merged(),
            });
        }
        let steps: Vec<RebaseStep> = output
            .stdout
            .lines()
            .filter_map(|line| {
                let (id, subject) = line.split_once('\u{1f}')?;
                let id = id.trim();
                if id.is_empty() {
                    return None;
                }
                Some(RebaseStep::new(id, subject.trim()))
            })
            .collect();

        // 合并提交不会进待办列表；单独数一次，交给界面如实提示。
        let merges = self
            .run_local(&["rev-list", "--merges", "--count", range.as_str()])
            .ok()
            .and_then(|(ok, out)| if ok { out.trim().parse().ok() } else { None })
            .unwrap_or(0_usize);

        let head = self.rev_parse("HEAD").ok();
        Ok(RebasePlan {
            onto: onto.to_owned(),
            base: base.to_owned(),
            head,
            merges_dropped: merges,
            steps,
        })
    }

    // -------------------------------------------------- 拣选 / 回滚 / 重置

    /// 拣选提交（FR-MRG-04）。
    ///
    /// # Errors
    /// 无冲突的真实失败返回 [`BackendError::Rejected`]。
    pub fn cherry_pick(&self, request: &CherryPickRequest) -> Result<AdvancedOutcome> {
        if request.revs.is_empty() {
            return Err(BackendError::Rejected {
                summary: "没有选中任何提交".to_owned(),
                detail: String::new(),
            });
        }
        let mut args: Vec<String> = vec!["cherry-pick".to_owned()];
        if request.record_source {
            args.push("-x".to_owned());
        }
        if request.no_commit {
            args.push("--no-commit".to_owned());
        }
        args.extend(request.revs.iter().cloned());

        let output = self.run(&args, ADVANCED_TIMEOUT, None)?;
        let count = request.revs.len();
        self.finish(
            &output,
            format!("已拣选 {count} 个修订"),
            "拣选失败",
            |outcome| format!("拣选时发生冲突：{} 个文件待解决", outcome.conflicts.len()),
        )
    }

    /// 回滚提交（FR-MRG-05）。
    ///
    /// # Errors
    /// 无冲突的真实失败返回 [`BackendError::Rejected`]。
    pub fn revert(&self, request: &RevertRequest) -> Result<AdvancedOutcome> {
        if request.revs.is_empty() {
            return Err(BackendError::Rejected {
                summary: "没有选中任何提交".to_owned(),
                detail: String::new(),
            });
        }
        let mut args: Vec<String> = vec!["revert".to_owned(), "--no-edit".to_owned()];
        if request.no_commit {
            args.push("--no-commit".to_owned());
        }
        args.extend(request.revs.iter().cloned());

        let output = self.run(&args, ADVANCED_TIMEOUT, None)?;
        let count = request.revs.len();
        self.finish(
            &output,
            format!("已回滚 {count} 个提交"),
            "回滚失败",
            |outcome| format!("回滚时发生冲突：{} 个文件待解决", outcome.conflicts.len()),
        )
    }

    /// 继续 / 跳过 / 中止进行中的拣选或回滚（横幅上的"继续"按钮）。
    ///
    /// # Errors
    /// git 拒绝时返回错误。
    pub fn continue_or_abort(&self, state: RepoState, abort: bool) -> Result<AdvancedOutcome> {
        let verb = match state {
            RepoState::CherryPick => "cherry-pick",
            RepoState::Revert => "revert",
            other => {
                return Err(BackendError::Rejected {
                    summary: format!("{} 不支持继续/中止操作", other.label()),
                    detail: String::new(),
                });
            }
        };
        let flag = if abort { "--abort" } else { "--continue" };
        let output = self.run(&str_args(&[verb, flag]), ADVANCED_TIMEOUT, None)?;
        let summary = format!("{verb} {flag} 完成");
        self.finish(&output, summary, "操作失败", |outcome| {
            format!("仍有 {} 个文件冲突", outcome.conflicts.len())
        })
    }

    /// 重置当前分支（FR-MRG-06）。
    ///
    /// **`hard` 不可逆**：调用方必须先取得强确认。
    ///
    /// # Errors
    /// 目标无法解析等失败返回 [`BackendError::Rejected`]。
    pub fn reset(&self, request: &ResetRequest) -> Result<AdvancedOutcome> {
        self.ensure_revision(&request.target)?;
        let args = str_args(&["reset", request.mode.arg(), request.target.as_str()]);
        let output = self.run(&args, ADVANCED_TIMEOUT, None)?;
        let summary = format!("已{}到 {}", request.mode.label(), request.target);
        self.finish(&output, summary, "重置失败", |_| {
            "重置后仍有冲突".to_owned()
        })
    }

    /// 撤销（UNDO）当前分支最新的一次提交：软重置到它的父提交。
    ///
    /// 只支持 HEAD 上的提交：非最新提交的"撤销"实质是历史重写，IDEA 也不提供，
    /// 这里给出可执行的替代路径而不是悄悄改写历史。
    ///
    /// # Errors
    /// 目标不是 HEAD、或仓库只有一个提交时返回 [`BackendError::Rejected`]。
    pub fn undo_commit(&self, id: &str) -> Result<AdvancedOutcome> {
        let head = self.rev_parse("HEAD")?;
        if !id.is_empty() && !head.starts_with(&id.to_ascii_lowercase()) {
            return Err(BackendError::Rejected {
                summary: "只能撤销当前分支最新的提交".to_owned(),
                detail: format!(
                    "所选提交 {id} 不是 HEAD（{head}）。要移除历史中间某个提交，\
                     请用「从此处交互式变基」并把它设为「丢弃」。"
                ),
            });
        }
        if self.rev_parse("HEAD^").is_err() {
            return Err(BackendError::Rejected {
                summary: "这是第一个提交，没有父提交可回退".to_owned(),
                detail: String::new(),
            });
        }
        let output = self.run(
            &str_args(&["reset", "--soft", "HEAD^"]),
            LOCAL_TIMEOUT,
            None,
        )?;
        self.finish(
            &output,
            "已撤销最后一次提交（改动仍在暂存区）".to_owned(),
            "撤销提交失败",
            |_| "撤销后状态异常".to_owned(),
        )
    }

    // ------------------------------------------------------------ Stash

    /// 列出全部贮藏（FR-STH）。
    ///
    /// # Errors
    /// 执行失败时返回错误。
    pub fn stash_list(&self) -> Result<Vec<StashEntry>> {
        let output = self.run(
            &str_args(&["stash", "list", "--format=%gd%x1f%gs"]),
            LOCAL_TIMEOUT,
            None,
        )?;
        if !output.is_success() {
            return Err(BackendError::Rejected {
                summary: "读取贮藏列表失败".to_owned(),
                detail: output.merged(),
            });
        }
        Ok(parse_stash_list(&output.stdout))
    }

    /// 执行一次贮藏操作。
    ///
    /// # Errors
    /// 无冲突的真实失败返回 [`BackendError::Rejected`]；`apply`/`pop` 冲突不是错误。
    pub fn stash(&self, op: &StashOp) -> Result<AdvancedOutcome> {
        let (args, success, failure) = match op {
            StashOp::Create {
                message,
                include_untracked,
            } => {
                let mut args: Vec<String> = vec!["stash".to_owned(), "push".to_owned()];
                if *include_untracked {
                    args.push("-u".to_owned());
                }
                if !message.trim().is_empty() {
                    args.push("-m".to_owned());
                    args.push(message.clone());
                }
                (args, "已创建贮藏".to_owned(), "创建贮藏失败")
            }
            StashOp::Apply { index } => (
                vec![
                    "stash".to_owned(),
                    "apply".to_owned(),
                    format!("stash@{{{index}}}"),
                ],
                format!("已应用 stash@{{{index}}}（该贮藏仍保留）"),
                "应用贮藏失败",
            ),
            StashOp::Pop { index } => (
                vec![
                    "stash".to_owned(),
                    "pop".to_owned(),
                    format!("stash@{{{index}}}"),
                ],
                format!("已弹出 stash@{{{index}}}"),
                "弹出贮藏失败",
            ),
            StashOp::Drop { index } => (
                vec![
                    "stash".to_owned(),
                    "drop".to_owned(),
                    format!("stash@{{{index}}}"),
                ],
                format!("已删除 stash@{{{index}}}"),
                "删除贮藏失败",
            ),
            StashOp::Branch { index, name } => (
                vec![
                    "stash".to_owned(),
                    "branch".to_owned(),
                    name.clone(),
                    format!("stash@{{{index}}}"),
                ],
                format!("已从 stash@{{{index}}} 创建并检出分支 {name}"),
                "从贮藏创建分支失败",
            ),
        };

        let output = self.run(&args, ADVANCED_TIMEOUT, None)?;
        // `git stash push` 在没有改动时也是退出码 0，只说 "No local changes to save"。
        // 照 "已创建贮藏" 回报会让用户以为存下了东西，这里如实纠正。
        let success = if output.is_success() && output.merged().contains("No local changes") {
            "没有可贮藏的更改（工作区已干净）".to_owned()
        } else {
            success
        };
        self.finish(&output, success, failure, |outcome| {
            format!(
                "{}时发生冲突：{} 个文件待解决",
                op.label(),
                outcome.conflicts.len()
            )
        })
    }

    // ------------------------------------------------------------ 补丁

    /// 生成补丁文件（FR-PTC-01），返回写入的路径。
    ///
    /// # Errors
    /// git 执行失败或文件不可写时返回错误。
    pub fn create_patch(&self, source: &PatchSource, output: &Path) -> Result<PathBuf> {
        let args: Vec<String> = match source {
            PatchSource::Commit { rev } => {
                self.ensure_revision(rev)?;
                vec![
                    "format-patch".to_owned(),
                    "-1".to_owned(),
                    "--stdout".to_owned(),
                    rev.clone(),
                ]
            }
            PatchSource::Range { from, to } => {
                self.ensure_revision(from)?;
                self.ensure_revision(to)?;
                vec!["diff".to_owned(), from.clone(), to.clone()]
            }
            PatchSource::Worktree => vec!["diff".to_owned(), "HEAD".to_owned()],
        };

        let result = self.run(&args, LOCAL_TIMEOUT, None)?;
        if !result.is_success() {
            return Err(BackendError::Rejected {
                summary: format!("生成{}失败", source.label()),
                detail: result.merged(),
            });
        }
        if result.stdout.trim().is_empty() {
            return Err(BackendError::Rejected {
                summary: "所选范围没有差异，未生成补丁".to_owned(),
                detail: String::new(),
            });
        }

        if let Some(parent) = output.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|err| BackendError::Io {
                    path: parent.to_path_buf(),
                    reason: err.to_string(),
                })?;
            }
        }
        std::fs::write(output, result.stdout.as_bytes()).map_err(|err| BackendError::Io {
            path: output.to_path_buf(),
            reason: err.to_string(),
        })?;
        Ok(output.to_path_buf())
    }

    /// 应用补丁（FR-PTC-01），`three_way` 冲突时由三路编辑器接管。
    ///
    /// # Errors
    /// 补丁不匹配且无法三方合并时返回 [`BackendError::Rejected`]。
    pub fn apply_patch(&self, request: &PatchApplyRequest) -> Result<AdvancedOutcome> {
        if !request.file.is_file() {
            return Err(BackendError::Rejected {
                summary: format!("补丁文件不存在：{}", request.file.display()),
                detail: String::new(),
            });
        }
        let mut args: Vec<String> = vec!["apply".to_owned()];
        if request.three_way {
            // `--3way` 需要索引里有 blob 信息；format-patch 产物天然满足。
            args.push("--3way".to_owned());
        }
        if request.reverse {
            args.push("--reverse".to_owned());
        }
        args.push(request.file.display().to_string());

        let output = self.run(&args, ADVANCED_TIMEOUT, None)?;
        self.finish(
            &output,
            "已应用补丁".to_owned(),
            "应用补丁失败",
            |outcome| format!("补丁冲突：{} 个文件待解决", outcome.conflicts.len()),
        )
    }

    // ------------------------------------------------------------ 标签

    /// 执行一次标签操作（FR-BR-03）。
    ///
    /// # Errors
    /// git 拒绝时返回 [`BackendError::Rejected`]。
    pub fn tag(&self, op: &TagOp) -> Result<AdvancedOutcome> {
        let args: Vec<String> = match op {
            TagOp::Create {
                name,
                message,
                target,
            } => {
                let mut args: Vec<String> = vec!["tag".to_owned()];
                match message {
                    Some(message) if !message.trim().is_empty() => {
                        args.push("-a".to_owned());
                        args.push(name.clone());
                        args.push("-m".to_owned());
                        args.push(message.clone());
                    }
                    _ => args.push(name.clone()),
                }
                if let Some(target) = target {
                    self.ensure_revision(target)?;
                    args.push(target.clone());
                }
                args
            }
            TagOp::Delete { name } => vec!["tag".to_owned(), "-d".to_owned(), name.clone()],
        };

        let output = self.run(&args, LOCAL_TIMEOUT, None)?;
        self.finish(
            &output,
            format!("{} 完成", op.label()),
            "标签操作失败",
            |_| "标签操作后状态异常".to_owned(),
        )
    }

    // ------------------------------------------------------------ 冲突

    /// 当前处于冲突中的文件（仓库相对路径）。
    ///
    /// # Errors
    /// git 执行失败时返回错误。
    pub fn conflicted_paths(&self) -> Result<Vec<String>> {
        let output = self.run(
            &str_args(&[
                "diff",
                "--name-only",
                "--diff-filter=U",
                "-z",
                "--no-renames",
            ]),
            LOCAL_TIMEOUT,
            None,
        )?;
        if !output.is_success() {
            return Ok(Vec::new());
        }
        Ok(split_z(&output.stdout))
    }

    /// 把解决后的内容写回工作区并 `git add`（FR-CFL-01 的"标记为已解决"）。
    ///
    /// 拒绝两种输入：**仍含冲突标记的内容**（防止把标记提交进历史），
    /// 以及指向仓库之外的路径。
    ///
    /// # Errors
    /// 路径越界 → [`BackendError::PathOutsideWorktree`]；内容仍含标记或写入失败 → 相应错误。
    pub fn resolve_conflict(&self, path: &str, content: &[u8]) -> Result<WriteOutcome> {
        let target = self.resolve_worktree_path(path)?;
        if let Ok(text) = std::str::from_utf8(content) {
            if text
                .lines()
                .any(|line| line.starts_with("<<<<<<<") || line.starts_with(">>>>>>>"))
            {
                return Err(BackendError::Rejected {
                    summary: "文件里仍有冲突标记，未标记为已解决".to_owned(),
                    detail: "请处理完全部冲突块，或改用「全部取当前分支 / 合入分支」。".to_owned(),
                });
            }
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|err| BackendError::Io {
                path: parent.to_path_buf(),
                reason: err.to_string(),
            })?;
        }
        std::fs::write(&target, content).map_err(|err| BackendError::Io {
            path: target.clone(),
            reason: err.to_string(),
        })?;

        let output = self.run(&str_args(&["add", "--", path]), LOCAL_TIMEOUT, None)?;
        if !output.is_success() {
            return Err(BackendError::Rejected {
                summary: format!("已写入 {path}，但 git add 失败"),
                detail: output.merged(),
            });
        }

        let remaining = self.conflicted_paths().unwrap_or_default();
        Ok(WriteOutcome::new(if remaining.is_empty() {
            format!("{path} 已标记为已解决，冲突已全部处理")
        } else {
            format!("{path} 已标记为已解决，还剩 {} 个文件", remaining.len())
        })
        .with_detail(output.merged()))
    }

    /// 取冲突某一侧的完整内容（`:2:` = 当前分支，`:3:` = 合入分支，`:1:` = 共同祖先）。
    ///
    /// 用于"全部取当前分支 / 合入分支"的快捷入口与三路编辑器左侧栏 ——
    /// 从对象库读而不是从带标记的工作区文件里猜，能正确处理 CRLF 与双删场景。
    ///
    /// # Errors
    /// git 执行失败时返回错误；该侧不存在时返回 [`None`]。
    pub fn stage_content(&self, stage: u8, path: &str) -> Result<Option<Vec<u8>>> {
        if !matches!(stage, 1..=3) {
            return Err(BackendError::Parse(format!("非法的冲突 stage：{stage}")));
        }
        let spec = format!(":{stage}:{path}");
        let output = self.run_local_bytes(&["show", spec.as_str()])?;
        if !output.0 {
            return Ok(None);
        }
        Ok(Some(output.1))
    }

    /// 用某一侧的内容整体解决冲突。
    ///
    /// # Errors
    /// 该侧不存在（例如一方删除了文件）或写入失败时返回错误。
    pub fn take_side(&self, path: &str, stage: u8) -> Result<WriteOutcome> {
        let side = match stage {
            2 => "当前分支",
            3 => "合入分支",
            1 => "共同祖先",
            other => {
                return Err(BackendError::Parse(format!("非法的冲突 stage：{other}")));
            }
        };
        let Some(content) = self.stage_content(stage, path)? else {
            return Err(BackendError::Rejected {
                summary: format!("{side}没有该文件的内容，无法整体取用"),
                detail:
                    "该场景通常是「一方删除、一方修改」。请在更下方用「删除文件」或手动编辑处理。"
                        .to_owned(),
            });
        };
        self.resolve_conflict(path, &content).map(|outcome| {
            WriteOutcome::new(format!("已取{side}的版本：{}", outcome.summary))
                .with_detail(outcome.detail)
        })
    }

    // ---------------------------------------------------- 内部：执行与判定

    /// 执行一条命令并统一判定成败。
    ///
    /// 判定顺序（顺序即语义）：
    /// 1. 退出码 0 → 成功；
    /// 2. 有冲突文件或仓库进入"进行中"状态 → **不是失败**，是中断；
    /// 3. 其余 → 真实失败，原文进 detail。
    fn finish(
        &self,
        output: &CliOutput,
        success: String,
        failure: &str,
        conflict_summary: impl Fn(&AdvancedOutcome) -> String,
    ) -> Result<AdvancedOutcome> {
        let state = self.state();
        let conflicts = self.conflicted_paths().unwrap_or_default();
        let detail = output.merged();

        if output.is_success() && conflicts.is_empty() && !state.is_in_progress() {
            return Ok(AdvancedOutcome {
                summary: success,
                detail,
                conflicts,
                state,
            });
        }

        if !conflicts.is_empty() || state.is_in_progress() {
            let mut outcome = AdvancedOutcome {
                summary: String::new(),
                detail,
                conflicts,
                state,
            };
            outcome.summary = conflict_summary(&outcome);
            return Ok(outcome);
        }

        let reason = first_line(&detail);
        Err(BackendError::Rejected {
            summary: format!("{failure}：{reason}"),
            detail,
        })
    }

    /// 探测仓库状态（与 `GitRepo::repo_state` 同一套标记，保持口径一致）。
    fn state(&self) -> RepoState {
        let git_dir = &self.location.git_dir;
        RepoState::from_markers(MarkerPresence {
            merge_head: git_dir.join("MERGE_HEAD").exists(),
            rebase_merge: git_dir.join("rebase-merge").is_dir(),
            rebase_apply: git_dir.join("rebase-apply").is_dir(),
            cherry_pick_head: git_dir.join("CHERRY_PICK_HEAD").exists(),
            revert_head: git_dir.join("REVERT_HEAD").exists(),
            bisect_log: git_dir.join("BISECT_LOG").exists(),
        })
    }

    /// 校验一个修订能被解析（避免把 git 的 128 号错误当业务失败去解释）。
    fn ensure_revision(&self, rev: &str) -> Result<()> {
        if rev.trim().is_empty() {
            return Err(BackendError::UnknownRevision {
                spec: rev.to_owned(),
            });
        }
        let spec = format!("{rev}^{{commit}}");
        let output = self.run_local(&["rev-parse", "--verify", "--quiet", spec.as_str()])?;
        if output.0 {
            Ok(())
        } else {
            Err(BackendError::UnknownRevision {
                spec: rev.to_owned(),
            })
        }
    }

    /// 解析成完整提交 id。
    fn rev_parse(&self, rev: &str) -> Result<String> {
        let output = self.run_local(&["rev-parse", "--verify", rev])?;
        if !output.0 {
            return Err(BackendError::UnknownRevision {
                spec: rev.to_owned(),
            });
        }
        Ok(output.1.trim().to_owned())
    }

    /// 执行并返回 `(是否成功, stdout)`。
    fn run_local(&self, args: &[&str]) -> Result<(bool, String)> {
        let output = self.run(&str_args(args), LOCAL_TIMEOUT, None)?;
        Ok((output.is_success(), output.stdout))
    }

    /// 执行并返回 `(是否成功, stdout 字节)`（用于读 blob，避免 UTF-8 有损解码）。
    fn run_local_bytes(&self, args: &[&str]) -> Result<(bool, Vec<u8>)> {
        let output = self.run(&str_args(args), LOCAL_TIMEOUT, None)?;
        let ok = output.is_success();
        Ok((ok, output.stdout_bytes))
    }

    /// 执行一条命令（本地语义，带统一的编辑器环境）。
    fn run(
        &self,
        args: &[String],
        timeout: Duration,
        cancel: Option<&dyn CancelProbe>,
    ) -> Result<CliOutput> {
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        self.cli.run_with_env(
            Some(self.work_dir()),
            &borrowed,
            Self::base_env(),
            self.observer.as_ref(),
            timeout,
            cancel,
        )
    }

    /// 执行一条命令并注入额外环境变量。
    fn run_with_env(
        &self,
        args: &[&str],
        env: Vec<(String, String)>,
        timeout: Duration,
        cancel: Option<&dyn CancelProbe>,
    ) -> Result<CliOutput> {
        self.cli.run_with_env(
            Some(self.work_dir()),
            args,
            env,
            self.observer.as_ref(),
            timeout,
            cancel,
        )
    }

    /// 所有高级操作共享的环境变量。
    ///
    /// - `GIT_EDITOR=true`：绝不会有人类终端在等着编辑消息。不设它，`squash` 与
    ///   `rebase --continue` 都会尝试启动编辑器，在 GUI 进程里等于挂到超时。
    /// - `GIT_MERGE_AUTOEDIT=no`：合并提交不进入编辑器（消息由 `-m`/`--no-edit` 决定）。
    fn base_env() -> Vec<(String, String)> {
        vec![
            ("GIT_EDITOR".to_owned(), "true".to_owned()),
            ("GIT_MERGE_AUTOEDIT".to_owned(), "no".to_owned()),
        ]
    }

    /// 交互式变基的临时目录。
    fn plan_dir(&self) -> PathBuf {
        self.location.git_dir.join(PLAN_DIR_NAME)
    }

    /// 写入待办列表与 reword 的消息文件。
    fn prepare_plan_files(plan: &RebasePlan, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir).map_err(|err| BackendError::Io {
            path: dir.to_path_buf(),
            reason: err.to_string(),
        })?;
        for (index, step) in plan.steps.iter().enumerate() {
            if !step.action.needs_message() {
                continue;
            }
            let Some(message) = &step.message else {
                continue;
            };
            let path = message_path(dir, index);
            std::fs::write(&path, message).map_err(|err| BackendError::Io {
                path: path.clone(),
                reason: err.to_string(),
            })?;
        }
        let todo = plan
            .build_todo(dir)
            .map_err(|error| BackendError::Rejected {
                summary: error.to_string(),
                detail: String::new(),
            })?;
        let path = dir.join("plan.todo");
        std::fs::write(&path, todo).map_err(|err| BackendError::Io {
            path: path.clone(),
            reason: err.to_string(),
        })
    }

    /// 清理计划目录（成功后调用；中断时**保留**）。
    fn cleanup_plan_dir(&self) {
        let dir = self.plan_dir();
        if dir.exists() {
            if let Err(err) = std::fs::remove_dir_all(&dir) {
                tracing::debug!(path = %dir.display(), %err, "清理交互式变基临时目录失败");
            }
        }
    }

    /// 把仓库相对路径解析成工作区里的绝对路径，并拒绝越界。
    fn resolve_worktree_path(&self, path: &str) -> Result<PathBuf> {
        let root =
            self.location
                .work_dir
                .clone()
                .ok_or_else(|| BackendError::PathOutsideWorktree {
                    path: PathBuf::from(path),
                })?;
        let relative = Path::new(path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(BackendError::PathOutsideWorktree {
                path: PathBuf::from(path),
            });
        }
        Ok(root.join(relative))
    }
}

// ---------------------------------------------------------------- 工具

/// 把 `&[&str]` 转成拥有型参数表（调用点常见"临时数组"写法）。
fn str_args(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

/// 把路径规范成可被 git 的 sh 理解的形态（正斜杠）。
fn normalize(path: &Path) -> PathBuf {
    PathBuf::from(path.display().to_string().replace('\\', "/"))
}

/// 给 shell 参数加单引号（`'` 自身按 POSIX 规则转义）。
fn shell_quote(path: &Path) -> String {
    let text = normalize(path).display().to_string();
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// 待办列表里某一步的消息文件路径。
fn message_path(dir: &Path, index: usize) -> PathBuf {
    dir.join(format!("msg-{index}.txt"))
}

/// 待办列表里的提交说明：去掉换行与控制字符，避免把待办列表写坏。
fn sanitize_subject(subject: &str) -> String {
    subject
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>()
        .trim()
        .to_owned()
}

/// 按 NUL 拆分 `git ... -z` 的输出。
fn split_z(text: &str) -> Vec<String> {
    text.split('\0')
        .map(|part| part.trim_end_matches(['\r', '\n']))
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

/// 解析 `git stash list --format=%gd%x1f%gs`。
fn parse_stash_list(text: &str) -> Vec<StashEntry> {
    text.lines()
        .filter_map(|line| {
            let (selector, message) = line.split_once('\u{1f}')?;
            let selector = selector.trim();
            let index = selector
                .strip_prefix("stash@{")?
                .strip_suffix('}')?
                .parse()
                .ok()?;
            Some(StashEntry {
                index,
                selector: selector.to_owned(),
                message: message.trim().to_owned(),
            })
        })
        .collect()
}

/// 去掉摘要开头的缩写哈希（git 的 stash 描述形如 `WIP on main: 1a2b3c4 消息`）。
fn strip_leading_hash(text: &str) -> &str {
    let (head, tail) = text.split_once(' ').unwrap_or((text, ""));
    let looks_like_hash = (7..=40).contains(&head.len())
        && head.chars().all(|ch| ch.is_ascii_hexdigit())
        && !tail.is_empty();
    if looks_like_hash {
        tail
    } else {
        text
    }
}

/// 从 `.git` 目录读取变基进度。
///
/// 独立成自由函数：服务层构造仓库概览时只需要读两个小文件，
/// 不该为了这个再去构造一个执行器（那会要求 git CLI 必须存在，
/// 而进度读取本身是纯文件系统操作）。
#[must_use]
pub fn read_rebase_progress(git_dir: &Path) -> Option<RebaseProgress> {
    let merge_dir = git_dir.join("rebase-merge");
    let apply_dir = git_dir.join("rebase-apply");
    let (dir, current_file, total_file) = if merge_dir.is_dir() {
        (&merge_dir, "msgnum", "end")
    } else if apply_dir.is_dir() {
        (&apply_dir, "next", "last")
    } else {
        return None;
    };
    let current = read_number(&dir.join(current_file))?;
    let total = read_number(&dir.join(total_file))?;
    let head_name = std::fs::read_to_string(dir.join("head-name"))
        .ok()
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty());
    Some(RebaseProgress {
        current,
        total,
        head_name,
    })
}

/// 读一个小整数文件（`msgnum` / `end` / `next` / `last`）。
fn read_number(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// 取输出的第一行非空文本。
fn first_line(text: &str) -> String {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .map_or_else(|| "未知原因".to_owned(), |line| line.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::{
        parse_stash_list, sanitize_subject, shell_quote, split_z, AdvancedOps, CherryPickRequest,
        MergeRequest, MergeStrategy, PatchSource, PlanError, RebaseAction, RebaseControl,
        RebasePlan, RebaseStep, ResetMode, ResetRequest, StashOp, TagOp,
    };
    use crate::cli::GitCli;
    use crate::record::NoopObserver;
    use crate::repo::GitRepo;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::Arc;

    /// 造一个临时仓库。
    fn fixture(label: &str) -> Option<(PathBuf, GitCli, AdvancedOps)> {
        let cli = GitCli::detect()?;
        let root =
            std::env::temp_dir().join(format!("knotwork-adv-{}-{label}", std::process::id()));
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
            String::from_utf8_lossy(&output.stdout).into_owned()
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "adv@knotwork.invalid"]);
        run(&["config", "user.name", "Knotwork Advanced"]);
        run(&["config", "core.autocrlf", "false"]);
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\n").expect("写文件");
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "初始提交"]);

        let repo = GitRepo::open(&root, Arc::new(NoopObserver)).expect("打开仓库");
        let ops = AdvancedOps::new(repo.location(), cli.clone(), repo.observer());
        Some((root, cli, ops))
    }

    fn git(root: &std::path::Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("执行 git");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    #[test]
    fn plan_rejects_invalid_sequences() {
        let mut plan = RebasePlan::default();
        assert_eq!(plan.validate(), Err(PlanError::Empty));

        plan.steps = vec![RebaseStep::new("a", "one"), RebaseStep::new("b", "two")];
        assert!(plan.validate().is_ok(), "默认全是 pick 应合法");

        // 第一条 squash 没有可压缩对象。
        plan.steps[0].action = RebaseAction::Squash;
        assert_eq!(plan.validate(), Err(PlanError::FirstCannotSquash));

        // 只有 drop 也不行。
        let mut all_drop = RebasePlan {
            steps: vec![RebaseStep::new("a", "one"), RebaseStep::new("b", "two")],
            ..RebasePlan::default()
        };
        all_drop
            .steps
            .iter_mut()
            .for_each(|s| s.action = RebaseAction::Drop);
        assert_eq!(all_drop.validate(), Err(PlanError::AllDropped));

        // reword 必须有消息。
        let mut reword = RebasePlan {
            steps: vec![RebaseStep::new("a", "one")],
            ..RebasePlan::default()
        };
        reword.steps[0].action = RebaseAction::Reword;
        assert_eq!(
            reword.validate(),
            Err(PlanError::MissingRewordMessage { index: 0 })
        );
        reword.steps[0].message = Some("新消息".to_owned());
        assert!(reword.validate().is_ok());
    }

    #[test]
    fn todo_text_matches_the_plan() {
        let dir = PathBuf::from("C:/tmp/rebase");
        let mut plan = RebasePlan {
            onto: "main".to_owned(),
            base: "abc1234".to_owned(),
            head: None,
            merges_dropped: 0,
            steps: vec![
                RebaseStep::new("1111111111", "第一个提交"),
                RebaseStep::new("2222222222", "第二个提交"),
                RebaseStep::new("3333333333", "第三个提交"),
            ],
        };
        plan.steps[1].action = RebaseAction::Squash;
        plan.steps[2].action = RebaseAction::Reword;
        plan.steps[2].message = Some("改写后的消息".to_owned());

        let todo = plan.build_todo(&dir).expect("生成待办列表");
        let lines: Vec<&str> = todo.lines().collect();
        assert_eq!(lines[0], "pick 1111111 第一个提交");
        assert_eq!(lines[1], "squash 2222222 第二个提交");
        assert_eq!(lines[2], "pick 3333333 第三个提交");
        assert!(
            lines[3].starts_with("exec git commit --amend"),
            "reword 必须转成 exec 改写：{lines:?}"
        );
        assert!(
            lines[3].contains("msg-2.txt"),
            "exec 必须引用该步的消息文件：{lines:?}"
        );
        assert_eq!(plan.kept(), 3);
    }

    #[test]
    fn todo_marks_dropped_steps_explicitly() {
        let mut plan = RebasePlan {
            steps: vec![
                RebaseStep::new("aaaaaaa1", "保留"),
                RebaseStep::new("bbbbbbb2", "丢弃"),
            ],
            ..RebasePlan::default()
        };
        plan.steps[1].action = RebaseAction::Drop;
        let todo = plan.build_todo(&PathBuf::from("/tmp")).expect("生成");
        assert!(
            todo.starts_with("pick aaaaaaa 保留\ndrop bbbbbbb 丢弃"),
            "{todo}"
        );
        assert_eq!(plan.kept(), 1);
    }

    #[test]
    fn subjects_with_control_characters_are_sanitized() {
        assert_eq!(sanitize_subject("a\tb"), "a b");
        assert_eq!(sanitize_subject("  x  "), "x");
    }

    #[test]
    fn shell_quote_handles_spaces_and_quotes() {
        let quoted = shell_quote(&PathBuf::from("C:/a b/msg-0.txt"));
        assert_eq!(quoted, "'C:/a b/msg-0.txt'");
        assert!(shell_quote(&PathBuf::from("C:/o'brien")).contains("'\\''"));
    }

    #[test]
    fn stash_list_is_parsed() {
        let text = "stash@{0}\u{1f}WIP on main: 1a2b3c4 修复登录\n\
                    stash@{1}\u{1f}On feature: 自定义消息\n";
        let entries = parse_stash_list(text);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].index, 0);
        assert_eq!(entries[0].summary(), "修复登录");
        assert_eq!(entries[0].branch().as_deref(), Some("main"));
        assert_eq!(entries[1].message, "On feature: 自定义消息");
    }

    #[test]
    fn nul_separated_paths_are_split() {
        assert_eq!(
            split_z("a.txt\0dir/b.txt\0"),
            vec!["a.txt".to_owned(), "dir/b.txt".to_owned()]
        );
    }

    #[test]
    fn merge_fast_forwards_and_reports_state() {
        let Some((root, _cli, ops)) = fixture("merge-ff") else {
            return;
        };
        git(&root, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(root.join("f.txt"), "feature\n").expect("写文件");
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "feature 提交"]);
        git(&root, &["checkout", "-q", "main"]);

        let outcome = ops
            .merge(&MergeRequest {
                rev: "feature".to_owned(),
                strategy: MergeStrategy::Default,
                message: None,
            })
            .expect("合并");
        assert!(!outcome.is_conflicted(), "{outcome:?}");
        assert_eq!(outcome.state, gt_core::RepoState::Clean);
        assert!(root.join("f.txt").exists(), "快进后文件应出现在主分支");
        assert_eq!(
            git(&root, &["rev-parse", "main"]).trim(),
            git(&root, &["rev-parse", "feature"]).trim()
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn conflicting_merge_reports_files_without_erroring() {
        let Some((root, _cli, ops)) = fixture("merge-conflict") else {
            return;
        };
        git(&root, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(root.join("a.txt"), "feature 侧\n").expect("写文件");
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "feature 改 a.txt"]);
        git(&root, &["checkout", "-q", "main"]);
        std::fs::write(root.join("a.txt"), "main 侧\n").expect("写文件");
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "main 改 a.txt"]);

        let outcome = ops
            .merge(&MergeRequest {
                rev: "feature".to_owned(),
                strategy: MergeStrategy::Default,
                message: None,
            })
            .expect("冲突不应是 Err");
        assert!(outcome.is_conflicted(), "{outcome:?}");
        assert_eq!(outcome.conflicts, vec!["a.txt".to_owned()]);
        assert_eq!(outcome.state, gt_core::RepoState::Merge);
        assert!(outcome.summary.contains("冲突"), "{}", outcome.summary);

        // 工作区必须有标记，三路编辑器才有内容可解析。
        let text = std::fs::read_to_string(root.join("a.txt")).expect("读文件");
        assert!(text.contains("<<<<<<<"), "{text}");

        // 三侧内容都能取到。
        assert_eq!(
            ops.stage_content(2, "a.txt").expect("取 ours").as_deref(),
            Some(&b"main \xe4\xbe\xa7\n"[..])
        );
        assert!(ops.stage_content(3, "a.txt").expect("取 theirs").is_some());

        let aborted = ops.abort_merge().expect("中止合并");
        assert!(!aborted.is_interrupted(), "{aborted:?}");
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).expect("读回"),
            "main 侧\n"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_conflict_writes_and_stages_but_refuses_markers() {
        let Some((root, _cli, ops)) = fixture("resolve") else {
            return;
        };
        git(&root, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(root.join("a.txt"), "theirs\n").expect("写文件");
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "feature"]);
        git(&root, &["checkout", "-q", "main"]);
        std::fs::write(root.join("a.txt"), "ours\n").expect("写文件");
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "main"]);
        ops.merge(&MergeRequest {
            rev: "feature".to_owned(),
            strategy: MergeStrategy::Default,
            message: None,
        })
        .expect("合并");

        let refused = ops
            .resolve_conflict("a.txt", b"<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> x\n")
            .expect_err("含标记的内容必须被拒");
        assert!(refused.to_string().contains("冲突标记"), "{refused}");

        let outcome = ops
            .resolve_conflict("a.txt", b"resolved\n")
            .expect("标记为已解决");
        assert!(
            outcome.summary.contains("已全部处理"),
            "{}",
            outcome.summary
        );
        assert!(ops.conflicted_paths().expect("读冲突").is_empty());
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).expect("读回"),
            "resolved\n"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rebase_plan_lists_commits_from_the_base() {
        let Some((root, _cli, ops)) = fixture("plan") else {
            return;
        };
        git(&root, &["checkout", "-q", "-b", "feature"]);
        for index in 0..3 {
            std::fs::write(root.join(format!("f{index}.txt")), "x\n").expect("写文件");
            git(&root, &["add", "-A"]);
            git(&root, &["commit", "-q", "-m", &format!("提交 {index}")]);
        }
        let base = git(&root, &["rev-parse", "HEAD~2"]).trim().to_owned();
        let plan = ops.plan_from_base("main", &base).expect("生成计划");
        assert_eq!(plan.steps.len(), 2, "{plan:?}");
        assert_eq!(plan.steps[0].subject, "提交 1", "计划应为从旧到新的顺序");
        assert_eq!(plan.steps[1].subject, "提交 2");
        assert!(plan.head.is_some());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn interactive_rebase_squashes_and_rewords() {
        let Some((root, _cli, ops)) = fixture("rebase-i") else {
            return;
        };
        git(&root, &["checkout", "-q", "-b", "feature"]);
        for index in 0..3 {
            std::fs::write(root.join(format!("f{index}.txt")), "x\n").expect("写文件");
            git(&root, &["add", "-A"]);
            git(&root, &["commit", "-q", "-m", &format!("提交 {index}")]);
        }

        let base = git(&root, &["rev-parse", "HEAD~2"]).trim().to_owned();
        let mut plan = ops.plan_from_base("main", &base).expect("生成计划");
        assert_eq!(plan.steps.len(), 2);
        plan.steps[1].action = RebaseAction::Squash;
        plan.steps[0].action = RebaseAction::Reword;
        plan.steps[0].message = Some("改写后的提交 1\n".to_owned());

        let outcome = ops
            .interactive_rebase(&plan, None)
            .expect("交互式变基不应报错");
        assert!(
            !outcome.is_interrupted(),
            "交互式变基应完成：{outcome:?} / {}",
            outcome.detail
        );

        let log = git(&root, &["log", "--format=%s"]);
        // 初始 + 提交 0 + 压缩后的一个 = 3 个；原先是 4 个。
        assert_eq!(log.lines().count(), 3, "两个提交应压缩成一个：{log}");
        let subjects: Vec<&str> = log.lines().collect();
        assert!(
            subjects.iter().any(|line| line.contains("改写后的提交 1")),
            "reword 必须生效：{log}"
        );
        // 压缩后的提交里应同时含有两条提交引入的文件。
        assert!(root.join("f0.txt").exists() && root.join("f1.txt").exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn interactive_rebase_drops_and_keeps_the_rest() {
        let Some((root, _cli, ops)) = fixture("rebase-drop") else {
            return;
        };
        git(&root, &["checkout", "-q", "-b", "feature"]);
        for index in 0..3 {
            std::fs::write(root.join(format!("d{index}.txt")), "x\n").expect("写文件");
            git(&root, &["add", "-A"]);
            git(
                &root,
                &["commit", "-q", "-m", &format!("drop 测试 {index}")],
            );
        }
        let base = git(&root, &["rev-parse", "HEAD~2"]).trim().to_owned();
        let mut plan = ops.plan_from_base("main", &base).expect("生成计划");
        assert_eq!(plan.steps.len(), 2);
        plan.steps[0].action = RebaseAction::Drop;

        ops.interactive_rebase(&plan, None).expect("变基");
        let subjects = git(&root, &["log", "--format=%s"]);
        assert!(!subjects.contains("drop 测试 1"), "{subjects}");
        assert!(subjects.contains("drop 测试 2"), "{subjects}");
        assert!(!root.join("d1.txt").exists(), "被丢弃提交引入的文件应消失");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rebase_conflict_then_abort_restores_the_branch() {
        let Some((root, _cli, ops)) = fixture("rebase-abort") else {
            return;
        };
        git(&root, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(root.join("a.txt"), "feature\n").expect("写文件");
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "feature 改 a.txt"]);
        git(&root, &["checkout", "-q", "main"]);
        std::fs::write(root.join("a.txt"), "main\n").expect("写文件");
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "main 改 a.txt"]);
        git(&root, &["checkout", "-q", "feature"]);
        let before = git(&root, &["rev-parse", "HEAD"]).trim().to_owned();

        let outcome = ops
            .rebase(&super::RebaseRequest {
                onto: "main".to_owned(),
            })
            .expect("冲突不应当是 Err");
        assert!(outcome.is_conflicted(), "{outcome:?}");
        assert!(outcome.state.is_rebase(), "{:?}", outcome.state);
        let progress = ops.rebase_progress().expect("应有变基进度");
        assert_eq!((progress.current, progress.total), (1, 1), "{progress:?}");

        let aborted = ops.rebase_control(RebaseControl::Abort).expect("中止");
        assert!(!aborted.is_interrupted(), "{aborted:?}");
        assert_eq!(
            git(&root, &["rev-parse", "HEAD"]).trim(),
            before,
            "中止后分支必须回到原位置"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cherry_pick_and_revert_are_inverse() {
        let Some((root, _cli, ops)) = fixture("pick-revert") else {
            return;
        };
        git(&root, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(root.join("p.txt"), "picked\n").expect("写文件");
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "待拣选"]);
        let pick = git(&root, &["rev-parse", "HEAD"]).trim().to_owned();
        git(&root, &["checkout", "-q", "main"]);

        ops.cherry_pick(&CherryPickRequest {
            revs: vec![pick.clone()],
            record_source: true,
            no_commit: false,
        })
        .expect("拣选");
        assert!(root.join("p.txt").exists());
        assert!(git(&root, &["log", "-1", "--format=%b"]).contains("cherry picked from"));

        ops.revert(&super::RevertRequest {
            revs: vec!["HEAD".to_owned()],
            no_commit: false,
        })
        .expect("回滚");
        assert!(!root.join("p.txt").exists(), "回滚后文件应被删除");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reset_modes_move_the_branch_and_the_index() {
        let Some((root, _cli, ops)) = fixture("reset") else {
            return;
        };
        std::fs::write(root.join("b.txt"), "b\n").expect("写文件");
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "第二个提交"]);
        let first = git(&root, &["rev-parse", "HEAD~1"]).trim().to_owned();

        ops.reset(&ResetRequest {
            target: first.clone(),
            mode: ResetMode::Soft,
        })
        .expect("软重置");
        assert_eq!(git(&root, &["rev-parse", "HEAD"]).trim(), first);
        assert!(
            git(&root, &["diff", "--cached", "--name-only"]).contains("b.txt"),
            "软重置应保留暂存"
        );

        ops.reset(&ResetRequest {
            target: first,
            mode: ResetMode::Hard,
        })
        .expect("硬重置");
        assert!(git(&root, &["status", "--porcelain"]).trim().is_empty());
        assert!(!root.join("b.txt").exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn stash_create_apply_and_pop_round_trip() {
        let Some((root, _cli, ops)) = fixture("stash") else {
            return;
        };
        std::fs::write(root.join("a.txt"), "changed\n").expect("改文件");
        ops.stash(&StashOp::Create {
            message: "WIP 测试".to_owned(),
            include_untracked: false,
        })
        .expect("创建贮藏");
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).expect("读回"),
            "one\ntwo\nthree\n",
            "贮藏后工作区应干净"
        );

        let list = ops.stash_list().expect("列表");
        assert_eq!(list.len(), 1, "{list:?}");
        assert!(list[0].message.contains("WIP 测试"), "{list:?}");

        ops.stash(&StashOp::Apply { index: 0 }).expect("应用");
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).expect("读回"),
            "changed\n"
        );
        assert_eq!(ops.stash_list().expect("列表").len(), 1, "apply 不删除贮藏");

        git(&root, &["checkout", "--", "a.txt"]);
        ops.stash(&StashOp::Pop { index: 0 }).expect("弹出");
        assert!(ops.stash_list().expect("列表").is_empty(), "pop 应删除贮藏");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn patch_round_trips_through_format_patch() {
        let Some((root, cli, ops)) = fixture("patch") else {
            return;
        };
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\nfour\n").expect("改文件");
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "加一行"]);
        let rev = git(&root, &["rev-parse", "HEAD"]).trim().to_owned();

        let file = root.join("out.patch");
        ops.create_patch(&PatchSource::Commit { rev }, &file)
            .expect("生成补丁");
        let text = std::fs::read_to_string(&file).expect("读补丁");
        // 非 ASCII 主题会被 format-patch 按 RFC 2047 编码，因此只断言结构与内容。
        assert!(text.contains("Subject: [PATCH]"), "{text}");
        assert!(text.contains("+four"), "补丁必须包含新增行：{text}");

        // 回到父提交后应用补丁，工作区应与原提交一致。
        git(&root, &["reset", "--hard", "HEAD~1"]);
        let request = super::PatchApplyRequest {
            file: file.clone(),
            three_way: false,
            reverse: false,
        };
        let outcome = ops.apply_patch(&request).expect("应用补丁");
        assert!(!outcome.is_conflicted(), "{outcome:?}");
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).expect("读回"),
            "one\ntwo\nthree\nfour\n"
        );

        // 反向应用应撤销它。
        let reverse = super::PatchApplyRequest {
            file,
            three_way: false,
            reverse: true,
        };
        ops.apply_patch(&reverse).expect("反向应用");
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).expect("读回"),
            "one\ntwo\nthree\n"
        );
        let _ = cli;

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tags_create_and_delete() {
        let Some((root, _cli, ops)) = fixture("tag") else {
            return;
        };
        ops.tag(&TagOp::Create {
            name: "v1.0".to_owned(),
            message: Some("首个版本".to_owned()),
            target: None,
        })
        .expect("附注标签");
        ops.tag(&TagOp::Create {
            name: "light".to_owned(),
            message: None,
            target: None,
        })
        .expect("轻量标签");
        assert_eq!(git(&root, &["tag", "-n"]).lines().count(), 2);
        assert!(git(&root, &["cat-file", "-t", "v1.0"]).trim() == "tag");

        ops.tag(&TagOp::Delete {
            name: "light".to_owned(),
        })
        .expect("删除");
        assert!(!git(&root, &["tag"]).contains("light"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn undo_commit_keeps_changes_staged() {
        let Some((root, _cli, ops)) = fixture("undo") else {
            return;
        };
        std::fs::write(root.join("u.txt"), "u\n").expect("写文件");
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "待撤销"]);
        let head = git(&root, &["rev-parse", "HEAD"]).trim().to_owned();

        ops.undo_commit(&head).expect("撤销");
        assert_eq!(git(&root, &["log", "--format=%s"]).lines().count(), 1);
        assert!(
            git(&root, &["diff", "--cached", "--name-only"]).contains("u.txt"),
            "撤销应把改动留在暂存区"
        );

        // 非 HEAD 的提交必须被拒，并给出可执行的替代路径。
        let err = ops.undo_commit("0000000000000000000000000000000000000000");
        assert!(err.is_err());

        let _ = std::fs::remove_dir_all(&root);
    }
}
