//! 请求与更新的消息模型（架构规划 4.3 第 1、5 点）。
//!
//! UI 线程只做两件事：投递 [`RequestKind`]，消费 [`Delivery`]。
//! 消息里**不含任何句柄**（不携带 `GitRepo`、不携带 `Arc<Vec<..>>` 之外的共享状态），
//! 因此可以在任意线程之间自由移动。

use std::fmt::Write as _;
use std::path::PathBuf;

use gt_backend::{
    BlameLine, BranchOp, CherryPickRequest, CommitRequest, CredentialKind, FetchRequest,
    FileHistoryEntry, IgnoreTarget, MergeRequest, NetworkResult, PatchApplyRequest, PatchSource,
    PullRequest, PushPreview, PushRequest, RebaseControl, RebasePlan, RebaseProgress,
    RebaseRequest, RemoteEdit, RemoteInfo, RemoteListing, RepoLocation, ResetRequest,
    RevertRequest, StashEntry, StashOp, TagOp, WriteOutcome,
};
use gt_core::{ChangeSet, FileChange, HeadInfo, LogNodePage, LogPageRequest, RefInfo, RepoState};
use gt_diff::HunkSelection;

use crate::generation::Generation;

/// 差异的一侧。
///
/// M1 用 `old`/`new` 两个修订串 + 一个 `worktree` 布尔表示两侧来源，但这无法表达
/// Local Changes 需要的三种组合（工作区 vs 索引、索引 vs HEAD）。M2 起改为
/// 显式枚举，避免"某个布尔组合非法"这类需要在运行期才发现的错误。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DiffSide {
    /// 空树（根提交的旧侧）。
    Empty,
    /// 某个修订（分支名 / 标签 / 提交号 / `HEAD~1`）。
    Rev(String),
    /// 索引（暂存区）。
    Index,
    /// 工作区文件。
    Worktree,
}

impl DiffSide {
    /// 修订写法（用于差异标题与审计）。
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::Empty => "空树",
            Self::Rev(spec) => spec,
            Self::Index => "索引",
            Self::Worktree => "工作区",
        }
    }
}

/// hunk / 行级选择的作用方式（FR-LC-03、FR-DIF-05）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMode {
    /// 暂存所选（`git apply --cached`）。
    Stage,
    /// 反暂存所选（`git apply --cached --reverse`）。
    Unstage,
    /// 丢弃所选（`git apply --reverse`，直接改工作区，不可逆）。
    Discard,
}

impl SelectionMode {
    /// 中文标签。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Stage => "暂存所选",
            Self::Unstage => "反暂存所选",
            Self::Discard => "丢弃所选",
        }
    }

    /// 是否需要二次确认（不可逆操作）。
    #[must_use]
    pub const fn needs_confirmation(self) -> bool {
        matches!(self, Self::Discard)
    }
}

/// 一次写请求（M2 起）。
///
/// 与 [`RequestKind`] 分开：读请求会经过代次闸门（过期即丢弃），
/// **写请求绝对不能**——丢弃一次写操作的结果等于让用户以为没生效。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteRequest {
    /// 暂存路径（`git add -A`）。
    Stage(Vec<String>),
    /// 反暂存路径（`git restore --staged`）。
    Unstage(Vec<String>),
    /// 丢弃已跟踪文件的修改（不可逆）。
    Discard(Vec<String>),
    /// 删除未跟踪文件 / 目录（不可逆）。
    DeleteUntracked(Vec<String>),
    /// 暂存 / 反暂存 / 丢弃所选 hunk 与行。
    Selection {
        /// 仓库相对路径。
        path: String,
        /// 左侧来源（暂存时是索引，反暂存时是 HEAD）。
        left: DiffSide,
        /// 右侧来源（暂存时是工作区，反暂存时是索引）。
        right: DiffSide,
        /// 选择集。
        selections: Vec<HunkSelection>,
        /// 界面展示的那份差异的指纹（防止用过期选择打补丁）。
        expected_fingerprint: u64,
        /// 作用方式。
        mode: SelectionMode,
    },
    /// 提交 / 修订。
    Commit(CommitRequest),
    /// 分支操作。
    Branch(BranchOp),
    /// 写入忽略规则。
    Ignore {
        /// 目标文件。
        target: IgnoreTarget,
        /// 被忽略的路径。
        path: String,
    },

    // ---- M4：高级操作 ----
    /// 合并到当前分支（FR-MRG-01）。
    Merge(MergeRequest),
    /// 变基到指定修订（FR-MRG-02）。
    Rebase(RebaseRequest),
    /// 变基的三键：继续 / 跳过 / 中止。
    RebaseControl(RebaseControl),
    /// 交互式变基（FR-MRG-03）。
    InteractiveRebase(Box<RebasePlan>),
    /// 拣选提交（FR-MRG-04）。
    CherryPick(CherryPickRequest),
    /// 回滚提交（FR-MRG-05）。
    Revert(RevertRequest),
    /// 重置到指定修订（FR-MRG-06）。
    Reset(ResetRequest),
    /// 撤销当前分支最新提交（软重置）。
    UndoCommit {
        /// 目标提交 id（必须是 HEAD）。
        id: String,
    },
    /// 贮藏操作（FR-STH）。
    Stash(StashOp),
    /// 生成补丁（FR-PTC）。
    CreatePatch {
        /// 来源。
        source: PatchSource,
        /// 输出文件。
        output: PathBuf,
    },
    /// 应用补丁（FR-PTC）。
    ApplyPatch(PatchApplyRequest),
    /// 标签增删（FR-BR-03）。
    Tag(TagOp),
    /// 把解决后的内容写回并 `git add`（FR-CFL-01）。
    ResolveConflict {
        /// 仓库相对路径。
        path: String,
        /// 解决后的完整内容。
        content: Vec<u8>,
    },
    /// 整体取某一侧（stage 2 = 当前分支，3 = 合入分支，1 = 共同祖先）。
    TakeSide {
        /// 仓库相对路径。
        path: String,
        /// 冲突 stage。
        stage: u8,
    },
    /// 中止进行中的合并。
    AbortMerge,
    /// 继续 / 中止进行中的拣选或回滚。
    ContinueState {
        /// 进行中的操作。
        state: gt_core::RepoState,
        /// 为真表示中止。
        abort: bool,
    },
}

impl WriteRequest {
    /// 审计标签。
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Stage(_) => "暂存",
            Self::Unstage(_) => "反暂存",
            Self::Discard(_) => "丢弃修改",
            Self::DeleteUntracked(_) => "删除未跟踪文件",
            Self::Selection { mode, .. } => mode.label(),
            Self::Commit(_) => "提交",
            Self::Branch(_) => "分支操作",
            Self::Ignore { .. } => "写入忽略规则",
            Self::Merge(_) => "合并",
            Self::Rebase(_) => "变基",
            Self::RebaseControl(control) => control.label(),
            Self::InteractiveRebase(_) => "交互式变基",
            Self::CherryPick(_) => "拣选",
            Self::Revert(_) => "回滚",
            Self::Reset(_) => "重置",
            Self::UndoCommit { .. } => "撤销提交",
            Self::Stash(op) => op.label(),
            Self::CreatePatch { .. } => "生成补丁",
            Self::ApplyPatch(_) => "应用补丁",
            Self::Tag(_) => "标签操作",
            Self::ResolveConflict { .. } => "标记冲突已解决",
            Self::TakeSide { .. } => "取冲突一侧",
            Self::AbortMerge => "中止合并",
            Self::ContinueState { abort, .. } => {
                if *abort {
                    "中止进行中的操作"
                } else {
                    "继续进行中的操作"
                }
            }
        }
    }

    /// 是否属于 M4 引入的高级操作（决定它走哪条执行路径）。
    #[must_use]
    pub const fn is_advanced(&self) -> bool {
        matches!(
            self,
            Self::Merge(_)
                | Self::Rebase(_)
                | Self::RebaseControl(_)
                | Self::InteractiveRebase(_)
                | Self::CherryPick(_)
                | Self::Revert(_)
                | Self::Reset(_)
                | Self::UndoCommit { .. }
                | Self::Stash(_)
                | Self::CreatePatch { .. }
                | Self::ApplyPatch(_)
                | Self::Tag(_)
                | Self::ResolveConflict { .. }
                | Self::TakeSide { .. }
                | Self::AbortMerge
                | Self::ContinueState { .. }
        )
    }

    /// 是否属于"高级操作"（需要独占该仓库的操作槽位）。
    ///
    /// 判据是"会不会长时间持有 index/HEAD 并可能停在冲突状态"：
    /// 合并、变基、拣选、回滚、重置、贮藏、补丁、冲突落盘都算；
    /// 暂存与提交是秒级操作，串行化它们只会让界面变钝。
    #[must_use]
    pub const fn needs_advanced_slot(&self) -> bool {
        matches!(
            self,
            Self::Merge(_)
                | Self::Rebase(_)
                | Self::RebaseControl(_)
                | Self::InteractiveRebase(_)
                | Self::CherryPick(_)
                | Self::Revert(_)
                | Self::Reset(_)
                | Self::UndoCommit { .. }
                | Self::Stash(_)
                | Self::ApplyPatch(_)
                | Self::ResolveConflict { .. }
                | Self::TakeSide { .. }
                | Self::AbortMerge
                | Self::ContinueState { .. }
        )
    }

    /// 是否不可逆（供高危确认清单使用）。
    #[must_use]
    pub const fn is_destructive(&self) -> bool {
        match self {
            // 丢弃类：直接销毁工作区内容，无 Git 层面的找回手段。
            Self::Discard(_)
            | Self::DeleteUntracked(_)
            | Self::Branch(
                BranchOp::CheckoutDetached { .. } | BranchOp::Delete { force: true, .. },
            )
            | Self::TakeSide { .. } => true,
            Self::Selection { mode, .. } => mode.needs_confirmation(),
            Self::Reset(request) => request.mode.is_destructive(),
            Self::Stash(op) => op.is_destructive(),
            Self::RebaseControl(control) => control.is_destructive(),
            _ => false,
        }
    }
}

/// 推送预览请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushPreviewRequest {
    /// 远端名。
    pub remote: String,
    /// 分支；[`None`] 表示当前分支。
    pub branch: Option<String>,
    /// 受保护分支正则（来自设置，由界面填入）。
    pub protected_patterns: Vec<String>,
}

/// 一次网络操作请求（FR-RMT）。
///
/// 与 [`WriteRequest`] 并列而不是塞进去：网络操作的失败原因（凭据/网络/远端策略）
/// 与本地写完全不同，混在一起会让错误处理变成一堆 `if`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkRequest {
    /// 列出远端与凭据能力（只读）。
    Remotes,
    /// 增删改远端配置。
    EditRemote(RemoteEdit),
    /// 获取更新。
    Fetch(FetchRequest),
    /// 拉取并整合。
    Pull(PullRequest),
    /// 推送（含受保护分支拦截）。
    Push {
        /// 推送参数。
        request: PushRequest,
        /// 受保护分支正则。
        protected_patterns: Vec<String>,
    },
    /// 生成推送预览（只读，不产生远端动作）。
    Preview(PushPreviewRequest),
    /// 把 HTTPS token 存入系统密钥链。
    SaveToken {
        /// 主机名。
        host: String,
        /// 用户名。
        username: String,
        /// token（不落盘、不入审计）。
        secret: String,
    },
    /// 删除某主机的 token。
    ForgetToken {
        /// 主机名。
        host: String,
    },
}

impl NetworkRequest {
    /// 审计与提示用的标签。
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Remotes => "读取远端",
            Self::EditRemote(_) => "修改远端配置",
            Self::Fetch(_) => "获取更新",
            Self::Pull(_) => "拉取更新",
            Self::Push { .. } => "推送",
            Self::Preview(_) => "推送预览",
            Self::SaveToken { .. } => "保存凭据",
            Self::ForgetToken { .. } => "删除凭据",
        }
    }

    /// 是否会改变仓库状态（决定只读模式是否一律拒绝）。
    ///
    /// 只读模式下"读到远端有什么"仍然有意义（能让用户看懂状态），
    /// 所以只有真正写引用的操作才拦。
    #[must_use]
    pub const fn mutates_repo(&self) -> bool {
        matches!(
            self,
            Self::EditRemote(_) | Self::Fetch(_) | Self::Pull(_) | Self::Push { .. }
        )
    }

    /// 是否需要"同一仓库同时只允许一个网络操作"的独占。
    ///
    /// 凭据读写不占用网络槽位：它们不碰远端，且用户在失败后立刻补 token 是
    /// 最常见的动作 —— 此时若还被"上一个 fetch 未收尾"挡住，体验会很糟。
    #[must_use]
    pub const fn needs_exclusive_slot(&self) -> bool {
        match self {
            Self::Remotes
            | Self::Preview(_)
            | Self::SaveToken { .. }
            | Self::ForgetToken { .. } => false,
            Self::EditRemote(_) | Self::Fetch(_) | Self::Pull(_) | Self::Push { .. } => true,
        }
    }
}

/// 一次网络失败的完整描述。
///
/// 比 [`Outcome::Failed`] 多带三个字段，全部是为了让界面能给出**可执行的下一步**：
/// `kind` 决定是否提示"为此主机保存 token"，`hints` 直接展示，`detail` 进 Console。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkFailure {
    /// 一句话结论。
    pub summary: String,
    /// 失败类别。
    pub kind: CredentialKind,
    /// 建议动作。
    pub hints: Vec<String>,
    /// git 原始输出（已脱敏）。
    pub detail: String,
}

impl NetworkFailure {
    /// 是否值得提示"保存 token"。
    #[must_use]
    pub fn suggests_token(&self) -> bool {
        matches!(
            self.kind,
            CredentialKind::AuthRequired | CredentialKind::AuthRejected
        )
    }

    /// 拼成可直接展示的文本（结论 + 建议）。
    #[must_use]
    pub fn message(&self) -> String {
        let mut out = self.summary.clone();
        if !self.hints.is_empty() {
            out.push_str("\n建议：");
            for (index, hint) in self.hints.iter().enumerate() {
                let _ = write!(out, "\n  {}. {hint}", index + 1);
            }
        }
        out
    }
}

/// 凭据存储操作的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialOutcome {
    /// 主机名。
    pub host: String,
    /// 是保存（`true`）还是删除（`false`）。
    pub saved: bool,
    /// 面向用户的说明。
    pub message: String,
}

/// 网络操作的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkOutcome {
    /// 远端与凭据能力清单。
    Remotes(Box<RemoteListing>),
    /// 推送预览。
    Preview(Box<PushPreview>),
    /// 执行完成（fetch/pull/push/edit）。
    Done(Box<NetworkResult>),
    /// 凭据存取结果。
    Token(Box<CredentialOutcome>),
    /// 失败（结构化，便于界面引导）。
    Failed(Box<NetworkFailure>),
}

impl NetworkOutcome {
    /// 是否成功。
    #[must_use]
    pub const fn is_ok(&self) -> bool {
        !matches!(self, Self::Failed(_))
    }
}

/// 远端列表项的便捷别名（界面层不需要直接依赖 `gt-backend`）。
pub type RemoteEntry = RemoteInfo;

/// 一次搜索请求（FR-LOG-03）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRequest {
    /// 关键字 / 正则 / 哈希前缀。
    pub query: String,
    /// 是否按正则解释。
    pub regex: bool,
    /// 是否大小写敏感。
    pub case_sensitive: bool,
    /// 是否搜索全部引用（含远程分支）。
    pub all_refs: bool,
    /// 结果上限。
    pub limit: usize,
}

impl Default for SearchRequest {
    fn default() -> Self {
        Self {
            query: String::new(),
            regex: false,
            case_sensitive: false,
            all_refs: false,
            limit: 500,
        }
    }
}

/// Blame 结果（FR-BLM-02）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameResult {
    /// 被归因的文件。
    pub path: String,
    /// 被 blame 的修订；[`None`] = 工作区。
    pub spec: Option<String>,
    /// 逐行归因。
    pub rows: Vec<BlameLine>,
    /// 是否因行数上限被截断。
    pub truncated: bool,
    /// 链式导航的滚动锚点（行下标，0 起）；普通 blame 为 [`None`]。
    pub anchor: Option<usize>,
}

/// 文件历史结果（FR-BLM-01）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHistoryResult {
    /// 文件。
    pub path: String,
    /// 是否扫了全部引用。
    pub all_refs: bool,
    /// 提交列表（新 → 旧）。
    pub rows: Vec<FileHistoryEntry>,
    /// 是否因条数上限被截断。
    pub truncated: bool,
}

/// 图片差异的**单侧**载荷（FR-DIF-04）。
///
/// `rgba` 为空表示"解码受限，只展示元信息"（源图超大）；
/// `None` 表示该侧不存在（新增 / 删除）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageSide {
    /// 是否成功解码为图片；`false` 时只有文件大小可展示（真二进制）。
    pub decodable: bool,
    /// 像素宽。
    pub width: u32,
    /// 像素高。
    pub height: u32,
    /// 文件字节数。
    pub file_bytes: usize,
    /// RGBA8 像素（`width * height * 4`；解码受限时为空）。
    pub rgba: Vec<u8>,
    /// 解码是否因超像素上限被限制（仅提供尺寸元信息）。
    pub limited: bool,
}

/// 图片差异结果（FR-DIF-04）：并排与叠加透明度对比的数据源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageDiffResult {
    /// 仓库相对路径。
    pub path: String,
    /// 旧侧（[`None`] = 新增文件）。
    pub old: Option<ImageSide>,
    /// 新侧（[`None`] = 删除文件）。
    pub new: Option<ImageSide>,
    /// 两侧字节是否完全相同。
    pub equal: bool,
}

/// 一次读取请求的类别。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestKind {
    /// 仓库概览：HEAD + 引用列表 + 进行中的操作状态。
    ///
    /// 合成一个请求是因为这三项总是一起用（窗口标题、分支面板、状态横幅），
    /// 分成三次请求只会多两次线程往返。
    Overview,
    /// 工作区状态（Local Changes 视图）。
    Status {
        /// 是否把 `.gitignore` 命中的条目一并返回（提交面板「显示已忽略的文件」）。
        include_ignored: bool,
    },
    /// 日志页。
    LogPage(LogPageRequest),
    /// 两个修订之间的文件级变更。
    TreeChanges {
        /// 旧侧修订；[`None`] 表示空树（根提交）。
        old: Option<String>,
        /// 新侧修订。
        new: String,
    },
    /// 某个提交相对第一父提交的文件清单（日志详情栏）。
    CommitFiles {
        /// 旧侧修订；[`None`] 表示根提交（空树）。
        old: Option<String>,
        /// 提交完整哈希；同时作为结果归属校验键。
        commit: String,
    },
    /// 单个文件的差异。
    FileDiff {
        /// 左侧来源。
        left: DiffSide,
        /// 右侧来源。
        right: DiffSide,
        /// 仓库相对路径。
        path: String,
    },
    /// 提交消息搜索。
    Search(SearchRequest),
    /// 待解决的冲突文件清单（FR-CFL-02）。
    Conflicts,
    /// 单个冲突文件的解析结果（三路编辑器数据源，FR-CFL-01/03）。
    ConflictFile {
        /// 仓库相对路径。
        path: String,
    },
    /// 生成交互式变基的提交计划（FR-MRG-03）。
    RebasePlan {
        /// 变基目标。
        onto: String,
        /// 区间起点（不含）。
        base: String,
    },
    /// 贮藏列表（FR-STH）。
    StashList,
    /// 逐行归因（FR-BLM-02）。`spec` 为 [`None`] 表示 blame 工作区内容。
    Blame {
        /// 被 blame 的修订；[`None`] = 工作区。
        spec: Option<String>,
        /// 仓库相对路径。
        path: String,
    },
    /// 文件历史列表（FR-BLM-01，`--follow` 跟随重命名）。
    FileHistory {
        /// 是否扫全部引用（`--all`）；否则只看当前分支。
        all_refs: bool,
        /// 仓库相对路径。
        path: String,
    },
    /// Blame 行点击后的"提交详情"（FR-BLM-02）：解析父提交并给出该文件
    /// 在该提交上的差异。复用 [`Outcome::FileDiff`] 送达，界面无需新分支。
    BlameDetail {
        /// 归属提交（完整哈希）。
        commit: String,
        /// 仓库相对路径。
        path: String,
    },
    /// 链式导航（FR-BLM-03）：在归属提交的父侧重新 blame，并把当前行号
    /// 映射到旧侧行号。映射与新 blame 都在服务线程完成，一次投递同时
    /// 带回行数据与滚动锚点（[`BlameResult::anchor`]）。
    BlameChain {
        /// 当前行归属的提交（完整哈希）。
        commit: String,
        /// 重命名前的提交（porcelain `previous` 头；无重命名为 [`None`]）。
        prev_commit: Option<String>,
        /// 重命名前的路径。
        prev_path: Option<String>,
        /// 当前被 blame 的路径。
        path: String,
        /// 当前 blame 的修订（[`None`] = 工作区，映射时新侧取工作区）。
        from_spec: Option<String>,
        /// 当前选中的行号（1 起，工作区/修订内容中的行号）。
        line: usize,
    },
    /// 写操作（不经代次闸门）。
    Write(Box<WriteRequest>),
    /// 网络操作（FR-RMT，不经代次闸门，但可取消）。
    Network(Box<NetworkRequest>),
}

/// 请求所属的**视图通道**。
///
/// 代次闸门按通道隔离：Log 的新请求只作废旧的 Log 请求，不会把同时进行的
/// "工作区状态"请求一起废掉。少了这层隔离，一次首屏装载里连发的三个请求
/// 会互相作废，只有最后一个能落地（实测踩到过）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RequestLane {
    /// 仓库概览（HEAD / 引用 / 进行中操作）。
    Overview,
    /// 工作区状态。
    Status,
    /// 日志页。
    Log,
    /// 文件差异。
    Diff,
    /// 写操作。
    ///
    /// 单独一条通道，且**不参与代次作废**：写请求的结果必须送达界面，
    /// 无论在此期间用户又点了几次别的东西。
    Write,
    /// 网络操作。
    ///
    /// 同样不作废结果（一次 fetch 的成败必须被看见），但**显式可取消**：
    /// 用户要能在"拉一个巨型远端"时按下停止，并且真的杀掉进程树。
    Network,
    /// 高级操作的读取需求（冲突清单 / 冲突文件 / 变基计划 / 贮藏列表）。
    ///
    /// 这些读取都是为了填满某个遮罩或面板；用户关掉遮罩或刷新仓库时，
    /// 在飞的结果已经没有接收方，按代次作废是正确的。
    Advanced,
}

impl RequestLane {
    /// 全部通道。
    pub const ALL: [Self; 7] = [
        Self::Overview,
        Self::Status,
        Self::Log,
        Self::Diff,
        Self::Write,
        Self::Network,
        Self::Advanced,
    ];

    /// 诊断标签。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Overview => "概览",
            Self::Status => "状态",
            Self::Log => "日志",
            Self::Diff => "差异",
            Self::Write => "写操作",
            Self::Network => "网络操作",
            Self::Advanced => "高级操作",
        }
    }

    /// 该通道的结果是否受代次闸门约束。
    #[must_use]
    pub const fn is_gated(self) -> bool {
        !matches!(self, Self::Write | Self::Network)
    }
}

impl RequestKind {
    /// 该请求走哪条通道。
    #[must_use]
    pub const fn lane(&self) -> RequestLane {
        match self {
            Self::Overview => RequestLane::Overview,
            Self::Status { .. } => RequestLane::Status,
            Self::LogPage(_) | Self::Search(_) | Self::CommitFiles { .. } => RequestLane::Log,
            // 差异族（含 Blame / 文件历史）共用一条视图通道：它们都在"看
            // 某个文件的内容"，新请求作废旧请求是正确的语义。
            Self::TreeChanges { .. }
            | Self::FileDiff { .. }
            | Self::Blame { .. }
            | Self::FileHistory { .. }
            | Self::BlameDetail { .. }
            | Self::BlameChain { .. } => RequestLane::Diff,
            Self::Conflicts
            | Self::ConflictFile { .. }
            | Self::RebasePlan { .. }
            | Self::StashList => RequestLane::Advanced,
            Self::Write(_) => RequestLane::Write,
            Self::Network(_) => RequestLane::Network,
        }
    }

    /// 简短标签，用于诊断日志与 Console。
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Overview => "仓库概览",
            Self::Status { .. } => "工作区状态",
            Self::LogPage(_) => "日志页",
            Self::TreeChanges { .. } => "文件级变更",
            Self::CommitFiles { .. } => "提交文件列表",
            Self::FileDiff { .. } => "文件差异",
            Self::Search(_) => "提交搜索",
            Self::Conflicts => "冲突清单",
            Self::ConflictFile { .. } => "冲突文件",
            Self::RebasePlan { .. } => "变基计划",
            Self::StashList => "贮藏列表",
            Self::Blame { .. } => "Blame",
            Self::FileHistory { .. } => "文件历史",
            Self::BlameDetail { .. } => "提交详情（Blame）",
            Self::BlameChain { .. } => "Blame 上溯",
            Self::Write(write) => write.label(),
            Self::Network(network) => network.label(),
        }
    }
}

/// 一个待解决的冲突文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictedEntry {
    /// 仓库相对路径。
    pub path: String,
    /// porcelain 的两个字母（`UU` / `AA` …）。
    pub code: String,
    /// 冲突类型。
    pub kind: gt_core::ConflictKind,
    /// 文件里当前的冲突块数量（0 表示文件已无标记、只差 `git add`）。
    pub hunks: usize,
}

/// 一次高级操作的结果（FR-MRG / FR-STH / FR-PTC / FR-CFL）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvancedResult {
    /// 一句话结论。
    pub summary: String,
    /// git 原始输出（已脱敏）。
    pub detail: String,
    /// 仍处于冲突的文件。
    pub conflicts: Vec<String>,
    /// 操作后的仓库状态。
    pub state: gt_core::RepoState,
}

impl Default for AdvancedResult {
    fn default() -> Self {
        Self {
            summary: String::new(),
            detail: String::new(),
            conflicts: Vec::new(),
            state: gt_core::RepoState::Clean,
        }
    }
}

impl From<gt_backend::AdvancedOutcome> for AdvancedResult {
    fn from(outcome: gt_backend::AdvancedOutcome) -> Self {
        Self {
            summary: outcome.summary,
            detail: outcome.detail,
            conflicts: outcome.conflicts,
            state: outcome.state,
        }
    }
}

impl AdvancedResult {
    /// 是否停在冲突上。
    #[must_use]
    pub fn is_conflicted(&self) -> bool {
        !self.conflicts.is_empty()
    }
}

/// 仓库概览结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoOverview {
    /// HEAD 状态。
    pub head: HeadInfo,
    /// 全部引用（本地分支的 ahead/behind 已填充）。
    pub refs: Vec<RefInfo>,
    /// 进行中的操作。
    pub state: RepoState,
    /// 仓库位置（用于窗口标题与状态栏）。
    pub location: RepoLocation,
    /// 只读原因（FR-MR-04）；[`None`] 表示可写。
    pub read_only: Option<String>,
    /// 仓库配置的 `user.email`（FR-LOG-06 的"我的提交"判定）。
    pub user_email: Option<String>,
    /// 变基进度（FR-MRG-02 的"第 x/y 个"）；无进行中的变基时为 [`None`]。
    pub rebase: Option<RebaseProgress>,
    /// 提交面板「按模块分组」的模块表（Cargo 工作区成员；可为空）。
    pub modules: Vec<gt_backend::RepoModule>,
}

/// 日志页结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogPageResult {
    /// 本页节点。
    pub page: LogNodePage,
    /// 当前筛选口径下的总提交数（供滚动条与"共 N 条"使用）。
    pub total: usize,
    /// 该 id 列表是否来自缓存（诊断用：首次枚举大仓库需要数秒）。
    pub from_cache: bool,
    /// 降级说明（例如拓扑排序因规模退回日期序）。
    pub note: Option<String>,
}

/// 搜索结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    /// 命中的提交（按搜索顺序）。
    pub nodes: Vec<gt_core::CommitMeta>,
    /// 原始查询串（供界面回显）。
    pub query: String,
    /// 是否因超时降级（FR-LOG-03：提示可构建历史索引）。
    pub timed_out: bool,
}

/// 工作线程回传给 UI 的一条更新。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    /// 发起时的代次。UI 侧可用它对账（正常情况下过期结果已被服务层丢弃）。
    pub generation: Generation,
    /// 请求类别。
    pub kind: RequestKind,
    /// 执行结果。
    pub outcome: Outcome,
}

/// 一次请求的执行结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// 仓库概览。
    Overview(Box<RepoOverview>),
    /// 工作区状态。
    Status(Box<ChangeSet>),
    /// 日志页。
    LogPage(Box<LogPageResult>),
    /// 文件级变更。
    TreeChanges(Vec<FileChange>),
    /// 日志详情栏的文件清单（附带提交哈希，界面据此丢弃过期结果）。
    CommitFiles {
        /// 提交完整哈希。
        commit: String,
        /// 相对第一父提交的文件清单。
        files: Vec<FileChange>,
    },
    /// 文件差异。
    FileDiff(Box<gt_diff::FileDiff>),
    /// 搜索结果。
    Search(Box<SearchResult>),
    /// 写操作结果。
    Write(Box<WriteOutcome>),
    /// 网络操作结果。
    Network(Box<NetworkOutcome>),
    /// 高级操作的写结果（合并 / 变基 / 贮藏 / 补丁 / 冲突落盘…）。
    Advanced(Box<AdvancedResult>),
    /// 冲突文件清单。
    Conflicts(Vec<ConflictedEntry>),
    /// 冲突文件的解析结果。
    ConflictFile(Box<gt_diff::ConflictFile>),
    /// 交互式变基的提交计划。
    RebasePlan(Box<RebasePlan>),
    /// 贮藏列表。
    StashList(Vec<StashEntry>),
    /// Blame 逐行归因（FR-BLM-02）。
    Blame(Box<BlameResult>),
    /// 文件历史（FR-BLM-01）。
    FileHistory(Box<FileHistoryResult>),
    /// 图片差异（FR-DIF-04）：并排 / 叠加对比的数据源。
    ImageDiff(Box<ImageDiffResult>),
    /// 失败：错误文本已经过"面向用户"的转换，可直接展示。
    Failed(String),
}

impl Outcome {
    /// 是否成功。
    #[must_use]
    pub const fn is_ok(&self) -> bool {
        match self {
            Self::Failed(_) => false,
            Self::Network(network) => network.is_ok(),
            _ => true,
        }
    }

    /// 失败原因；成功时为 [`None`]。
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        match self {
            Self::Failed(message) => Some(message),
            _ => None,
        }
    }

    /// 是否为写操作结果（供界面触发写后刷新）。
    #[must_use]
    pub const fn is_write(&self) -> bool {
        matches!(self, Self::Write(_) | Self::Network(_) | Self::Advanced(_))
    }
}

/// 请求无法入队的原因。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RequestError {
    /// 仓库 id 未注册（窗口可能已经关闭）。
    #[error("仓库 {0:?} 未注册")]
    UnknownRepo(gt_core::RepoId),

    /// 仓库以只读模式打开，写操作被拒（FR-MR-04）。
    #[error("{0}")]
    ReadOnly(String),

    /// 该仓库已有网络操作在进行。
    #[error("该仓库已有网络操作在进行（{0}），请等待其完成或先取消")]
    NetworkBusy(String),

    /// 工作队列已满。
    #[error("请求过多，请稍后重试（{0}）")]
    QueueFull(#[from] crate::pool::SubmitError),
}

#[cfg(test)]
mod tests {
    use super::{
        Delivery, DiffSide, NetworkRequest, Outcome, RequestError, RequestKind, RequestLane,
        SearchRequest, SelectionMode, WriteRequest,
    };
    use crate::generation::Generation;
    use gt_core::{LogPageRequest, RepoId};

    #[test]
    fn request_labels_cover_every_kind() {
        let kinds = [
            RequestKind::Overview,
            RequestKind::Status {
                include_ignored: false,
            },
            RequestKind::LogPage(LogPageRequest::default()),
            RequestKind::TreeChanges {
                old: Some("HEAD~1".to_owned()),
                new: "HEAD".to_owned(),
            },
            RequestKind::FileDiff {
                left: DiffSide::Index,
                right: DiffSide::Worktree,
                path: "src/main.rs".to_owned(),
            },
            RequestKind::Search(SearchRequest::default()),
            RequestKind::Write(Box::new(WriteRequest::Stage(vec!["a.txt".to_owned()]))),
        ];
        for kind in &kinds {
            assert!(!kind.label().is_empty(), "{kind:?} 缺少标签");
        }
    }

    #[test]
    fn every_kind_maps_to_exactly_one_lane() {
        assert_eq!(RequestKind::Overview.lane(), RequestLane::Overview);
        assert_eq!(
            RequestKind::Status {
                include_ignored: false
            }
            .lane(),
            RequestLane::Status
        );
        assert_eq!(
            RequestKind::LogPage(LogPageRequest::default()).lane(),
            RequestLane::Log
        );
        assert_eq!(
            RequestKind::Search(SearchRequest::default()).lane(),
            RequestLane::Log,
            "搜索复用日志通道：它替换的是同一块列表"
        );
        // 提交级与文件级差异共用一条通道：它们都在填同一个面板。
        assert_eq!(
            RequestKind::TreeChanges {
                old: None,
                new: "HEAD".to_owned()
            }
            .lane(),
            RequestLane::Diff
        );
        assert_eq!(
            RequestKind::Write(Box::new(WriteRequest::Commit(
                gt_backend::CommitRequest::default()
            )))
            .lane(),
            RequestLane::Write
        );
        assert_eq!(
            RequestKind::Network(Box::new(NetworkRequest::Remotes)).lane(),
            RequestLane::Network,
            "网络操作必须单独成道：它可取消，但不能被后续请求作废"
        );

        assert_eq!(RequestLane::ALL.len(), 7);
        for lane in RequestLane::ALL {
            assert!(!lane.label().is_empty());
            // 写通道与网络通道不受代次闸门约束（结果必须送达），其余必须受约束。
            let expect_gated = !matches!(lane, RequestLane::Write | RequestLane::Network);
            assert_eq!(lane.is_gated(), expect_gated, "{lane:?} 的闸门语义不符");
        }
    }

    #[test]
    fn writes_classify_destructive_operations() {
        assert!(WriteRequest::Discard(vec!["a".to_owned()]).is_destructive());
        assert!(WriteRequest::DeleteUntracked(vec!["a".to_owned()]).is_destructive());
        assert!(!WriteRequest::Stage(vec!["a".to_owned()]).is_destructive());
        assert!(!WriteRequest::Unstage(vec!["a".to_owned()]).is_destructive());
        assert!(!WriteRequest::Branch(gt_backend::BranchOp::Checkout {
            name: "main".to_owned()
        })
        .is_destructive());
        assert!(WriteRequest::Branch(gt_backend::BranchOp::Delete {
            name: "wip".to_owned(),
            force: true
        })
        .is_destructive());
        assert!(SelectionMode::Discard.needs_confirmation());
        assert!(!SelectionMode::Stage.needs_confirmation());
    }

    #[test]
    fn failed_outcome_carries_a_user_facing_message() {
        let failed = Outcome::Failed("不是有效的 Git 仓库".to_owned());
        assert!(!failed.is_ok());
        assert_eq!(failed.error(), Some("不是有效的 Git 仓库"));
        assert!(!failed.is_write());

        let ok = Outcome::TreeChanges(Vec::new());
        assert!(ok.is_ok());
        assert_eq!(ok.error(), None);
    }

    #[test]
    fn request_error_renders_for_users() {
        let unknown = RequestError::UnknownRepo(RepoId::from_raw(3));
        assert!(unknown.to_string().contains("未注册"));

        let full = RequestError::QueueFull(crate::pool::SubmitError::QueueFull { capacity: 64 });
        assert!(full.to_string().contains("请求过多"));

        let read_only = RequestError::ReadOnly("仓库正被其他实例写入".to_owned());
        assert!(read_only.to_string().contains("其他实例"));
    }

    #[test]
    fn delivery_is_send_and_cheap_to_clone() {
        // 消息必须能跨线程移动；`Clone` 用于"同一结果给多个订阅者"的诊断场景。
        fn assert_send<T: Send>() {}
        assert_send::<Delivery>();
        assert_send::<Outcome>();

        let delivery = Delivery {
            generation: Generation::INITIAL,
            kind: RequestKind::Status {
                include_ignored: false,
            },
            outcome: Outcome::Failed("x".to_owned()),
        };
        assert_eq!(delivery.clone(), delivery);
    }

    #[test]
    fn diff_side_labels_are_readable() {
        assert_eq!(DiffSide::Index.label(), "索引");
        assert_eq!(DiffSide::Worktree.label(), "工作区");
        assert_eq!(DiffSide::Empty.label(), "空树");
        assert_eq!(DiffSide::Rev("main".to_owned()).label(), "main");
    }
}
