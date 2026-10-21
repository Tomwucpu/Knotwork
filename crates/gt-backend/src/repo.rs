//! 仓库句柄与读路径实现（M1 走 `gix` 进程内内核）。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use gt_core::{
    BranchScope, ChangeKind, ChangeSet, CommitMeta, ConflictKind, FileChange, HeadInfo, HeadKind,
    LogPageRequest, LogSort, MarkerPresence, Oid, PorcelainFormat, RefInfo, RefKind, RepoState,
    StatusEntry, StatusFlag,
};

use crate::error::{BackendError, Result};
use crate::record::{CommandRecord, CommandRoute, SharedObserver};

/// 引用类别前缀。
const HEADS_PREFIX: &str = "refs/heads/";
const REMOTES_PREFIX: &str = "refs/remotes/";
const TAGS_PREFIX: &str = "refs/tags/";

/// 仓库的物理位置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoLocation {
    /// `.git` 目录（worktree / submodule 场景下可能是别处的实际目录）。
    pub git_dir: PathBuf,
    /// 工作区根；裸仓库为 [`None`]。
    pub work_dir: Option<PathBuf>,
    /// 是否为裸仓库。
    pub is_bare: bool,
}

impl RepoLocation {
    /// 用于窗口标题的仓库名（工作区目录名，裸仓库取 `.git` 的父目录名）。
    #[must_use]
    pub fn display_name(&self) -> String {
        let candidate = self
            .work_dir
            .clone()
            .or_else(|| self.git_dir.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| self.git_dir.clone());
        candidate.file_name().map_or_else(
            || candidate.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        )
    }

    /// 是否可用于文件系统监听（裸仓库无可监听的工作区）。
    #[must_use]
    pub fn work_dir_or_git_dir(&self) -> &Path {
        self.work_dir.as_deref().unwrap_or(&self.git_dir)
    }
}

/// 提交面板「按模块分组」的一个模块。
///
/// Knotwork 没有 IDE 的工程模型，最接近 IDEA Module 的口径是 Cargo 工作区
/// 成员：`path` 是成员的仓库相对目录，`name` 取包名。非 Cargo 仓库返回空表，
/// 由界面退化为按顶层目录分组。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoModule {
    /// 展示名（Cargo 包名；缺失时用目录名）。
    pub name: String,
    /// 仓库相对目录前缀（`/` 分隔；空串表示仓库根）。
    pub path: String,
}

/// 一对文件内容，用于文件级 diff 的两侧输入。
///
/// 用具名结构而非元组：`None` 既可能是"该侧文件不存在"，也可能是
/// "内容超过读取阈值"，调用点必须能分辨自己在看哪一侧。
#[derive(Debug, Clone, Default)]
pub struct BlobPair {
    /// 旧侧内容；`None` 表示该侧不存在或超过读取阈值。
    pub old: Option<Vec<u8>>,
    /// 新侧内容；含义同 [`BlobPair::old`]。
    pub new: Option<Vec<u8>>,
}

/// 可跨线程共享的仓库句柄。
///
/// `gix` 的 `Repository` 不是 `Sync`，而项目要"单进程多窗口共享同一份
/// packfile 映射与 odb 缓存"（架构规划 4.2）。`gix` 为此提供了
/// `ThreadSafeRepository`：它共享 odb 存储，各线程用它生成自己的
/// `Repository`（本地句柄不共享可变状态）。
///
/// 这是本项目的仓库句柄分发方式：服务层持有一个 `SharedGitRepo`，
/// 每个工作线程在任务开始时调 [`SharedGitRepo::to_thread_local`] 取本地句柄 ——
/// 既避免每请求重新 `discover`，也不会让两个线程同时碰同一个 `Repository`。
#[derive(Clone)]
pub struct SharedGitRepo {
    inner: gix::ThreadSafeRepository,
    location: RepoLocation,
    observer: SharedObserver,
}

impl std::fmt::Debug for SharedGitRepo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedGitRepo")
            .field("location", &self.location)
            .finish_non_exhaustive()
    }
}

impl SharedGitRepo {
    /// 在当前线程取出一个本地仓库句柄。
    #[must_use]
    pub fn to_thread_local(&self) -> GitRepo {
        GitRepo {
            inner: self.inner.to_thread_local(),
            location: self.location.clone(),
            observer: self.observer.clone(),
        }
    }

    /// 仓库位置信息。
    #[must_use]
    pub fn location(&self) -> &RepoLocation {
        &self.location
    }
}

/// 已打开的 Git 仓库句柄。
///
/// M1 只实现读路径（`gix`）；写路径（`git2` / CLI）由 M2 起在同一结构上扩展，
/// 从而保持"路由表集中在一处"的架构意图（架构规划 3.2、4.4）。
pub struct GitRepo {
    inner: gix::Repository,
    location: RepoLocation,
    observer: SharedObserver,
}

impl std::fmt::Debug for GitRepo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitRepo")
            .field("location", &self.location)
            .finish_non_exhaustive()
    }
}

impl GitRepo {
    /// 打开（或向上发现）仓库。
    ///
    /// 接受工作区子目录、裸仓库目录、以及 `.git` 为文件的形式
    /// （worktree / submodule 的 gitdir 链接）。
    ///
    /// # Errors
    /// - 路径不是仓库的一部分 → [`BackendError::NotARepository`]；
    /// - 仓库使用 SHA-256 → [`BackendError::UnsupportedObjectHash`]（v1.0 只支持 SHA-1）。
    pub fn open(path: &Path, observer: SharedObserver) -> Result<Self> {
        let inner = gix::discover(path).map_err(|err| BackendError::NotARepository {
            path: path.to_path_buf(),
            reason: err.to_string(),
        })?;

        let kind = inner.object_hash();
        if kind != gix::hash::Kind::Sha1 {
            return Err(BackendError::UnsupportedObjectHash {
                kind: format!("{kind:?}"),
            });
        }

        let location = RepoLocation {
            git_dir: inner.git_dir().to_path_buf(),
            work_dir: inner.workdir().map(Path::to_path_buf),
            is_bare: inner.is_bare(),
        };

        Ok(Self {
            inner,
            location,
            observer,
        })
    }

    /// 仓库位置信息。
    #[must_use]
    pub fn location(&self) -> &RepoLocation {
        &self.location
    }

    /// 命令审计观察者（写路径复用同一处，保证一次用户操作只留一条记录）。
    #[must_use]
    pub fn observer(&self) -> SharedObserver {
        Arc::clone(&self.observer)
    }

    /// 提交面板「按模块分组」的模块表（Cargo 工作区成员）。
    ///
    /// 裸仓库没有工作区可读，返回空表；非 Cargo 仓库同样返回空表，
    /// 由界面退化为按顶层目录分组。
    #[must_use]
    pub fn modules(&self) -> Vec<RepoModule> {
        self.location
            .work_dir
            .as_deref()
            .map_or_else(Vec::new, detect_repo_modules)
    }

    /// 转成可跨线程共享的句柄（共享 odb 缓存；各线程自行生成本地句柄）。
    #[must_use]
    pub fn into_shared(self) -> SharedGitRepo {
        SharedGitRepo {
            inner: self.inner.into_sync(),
            location: self.location,
            observer: self.observer,
        }
    }

    /// HEAD 状态。
    ///
    /// # Errors
    /// 引用数据库损坏时返回 [`BackendError::Reference`]。
    pub fn head(&self) -> Result<HeadInfo> {
        let began = Instant::now();
        let branch = self
            .inner
            .head_name()
            .map_err(|err| BackendError::Reference(err.to_string()))?
            .map(|name| shorten_ref(&bs(name.as_bstr())));

        let target = self.inner.head_id().ok().map(|id| to_core_oid(id.detach()));

        let kind = match (branch.as_deref(), target) {
            (_, None) => HeadKind::Unborn,
            (Some(_), Some(_)) => HeadKind::Branch,
            (None, Some(_)) => HeadKind::Detached,
        };

        self.record(
            "git symbolic-ref --short HEAD / git rev-parse HEAD",
            began,
            None,
        );

        Ok(HeadInfo {
            kind,
            target,
            branch,
        })
    }

    /// 全部引用（本地分支 / 远程分支 / 标签 / 其他），按分组与名称排序。
    ///
    /// # Errors
    /// 引用迭代失败时返回 [`BackendError::Reference`]。
    pub fn refs(&self) -> Result<Vec<RefInfo>> {
        let began = Instant::now();
        let head_branch = self
            .inner
            .head_name()
            .ok()
            .flatten()
            .map(|name| bs(name.as_bstr()));

        let platform = self
            .inner
            .references()
            .map_err(|err| BackendError::Reference(err.to_string()))?;
        let iter = platform
            .all()
            .map_err(|err| BackendError::Reference(err.to_string()))?;

        let mut out: Vec<RefInfo> = Vec::new();
        for item in iter {
            let mut reference = item.map_err(|err| BackendError::Reference(err.to_string()))?;
            let full = bs(reference.name().as_bstr());
            let kind = classify_ref(&full);
            if kind == RefKind::Other {
                continue;
            }

            // 附注标签指向 tag 对象，需剥离到提交；剥离失败则回退为直接目标。
            let target = reference
                .peel_to_id()
                .map_or_else(|_| reference.id().detach(), gix::Id::detach);

            out.push(RefInfo {
                name: shorten_ref(&full),
                kind,
                target: to_core_oid(target),
                is_head: head_branch.as_deref() == Some(full.as_str()),
                upstream: self.upstream_of(&full),
                // ahead/behind 需要 left-right 计数，属 FR-BR-01（M2）范围。
                ahead: 0,
                behind: 0,
            });
        }

        out.sort_by(|a, b| {
            a.kind
                .group_order()
                .cmp(&b.kind.group_order())
                .then_with(|| a.name.cmp(&b.name))
        });
        out.dedup_by(|a, b| a.kind == b.kind && a.name == b.name);

        self.record("git for-each-ref", began, None);
        Ok(out)
    }

    /// 按分支范围取出 revwalk 的起始引用集合（D2 决策口径）。
    ///
    /// - [`BranchScope::LocalAndUpstream`]：本地分支 + 分离头指针下的 HEAD；
    /// - [`BranchScope::All`]：全部引用（含远程分支与标签）。
    ///
    /// # Errors
    /// 读取引用失败时返回 [`BackendError`]。
    pub fn log_tips(&self, scope: BranchScope) -> Result<Vec<Oid>> {
        let mut tips: BTreeSet<Oid> = BTreeSet::new();
        for reference in self.refs()? {
            let include = match scope {
                BranchScope::All => true,
                BranchScope::LocalAndUpstream => reference.kind == RefKind::LocalBranch,
            };
            if include {
                tips.insert(reference.target);
            }
        }

        // 分离头指针下 HEAD 不在任何本地分支上，需单独纳入。
        if let Ok(id) = self.inner.head_id() {
            if self.inner.head_name().ok().flatten().is_none() {
                tips.insert(to_core_oid(id.detach()));
            }
        }

        Ok(tips.into_iter().collect())
    }

    /// 枚举提交 id 列表（等价 `git rev-list`）。
    ///
    /// `limit` 仅用于快速冒烟；正常分页由 `gt-service` 缓存完整 id 列表后切片，
    /// 避免每翻一页重走一遍历史（M0 POC-2 实测：百万提交全量枚举 release 下 4.23s）。
    ///
    /// 返回顺序即 gix revwalk 顺序（按提交日期、子先于父），满足 `gt-graph`
    /// 的拓扑序契约。
    ///
    /// # Errors
    /// 任意一步读取失败时返回对应错误。
    pub fn walk_ids(&self, scope: BranchScope, limit: Option<usize>) -> Result<Vec<Oid>> {
        let began = Instant::now();
        let tips = self.log_tips(scope)?;
        let mut out: Vec<Oid> = Vec::new();

        if !tips.is_empty() {
            let gix_tips: Vec<gix::ObjectId> = tips.iter().map(|oid| to_gix_oid(*oid)).collect();
            let walk = self
                .inner
                .rev_walk(gix_tips)
                .all()
                .map_err(|err| BackendError::Object(err.to_string()))?;

            for info in walk {
                let info = info.map_err(|err| BackendError::Object(err.to_string()))?;
                out.push(to_core_oid(info.id));
                if limit.is_some_and(|limit| out.len() >= limit) {
                    break;
                }
            }
        }

        let command = match scope {
            BranchScope::All => "git rev-list --all",
            BranchScope::LocalAndUpstream => "git rev-list --branches HEAD",
        };
        self.record(command, began, None);
        Ok(out)
    }

    /// 读取一页提交的元数据。
    ///
    /// 单个对象损坏不会中断整页：该行会退化为占位条目并记录告警，
    /// 保证列表长度与布局行一一对应（FR-STA-01 的崩溃隔离在服务层另有一层保障）。
    ///
    /// # Errors
    /// 仅在参数非法时返回错误。
    pub fn commit_metas(&self, ids: &[Oid], request: &LogPageRequest) -> Result<Vec<CommitMeta>> {
        let began = Instant::now();
        let mut out = Vec::with_capacity(ids.len());

        for id in ids {
            match self.commit_meta(*id) {
                Ok(meta) => out.push(meta),
                Err(err) => {
                    tracing::warn!(oid = %id, %err, "提交元数据读取失败，使用占位条目");
                    out.push(placeholder_meta(*id));
                }
            }
        }

        if request.graph.sort == LogSort::Topological {
            tracing::debug!("拓扑排序在 walk_ids_with 中完成，本函数只负责取元数据");
        }

        self.record(
            format!(
                "git log --format=... -n{} --skip{}（内置引擎）",
                ids.len(),
                request.offset
            ),
            began,
            None,
        );
        Ok(out)
    }

    /// 读取单个提交的元数据。
    ///
    /// # Errors
    /// 对象缺失或损坏时返回 [`BackendError::Object`]。
    pub fn commit_meta(&self, id: Oid) -> Result<CommitMeta> {
        let commit = self
            .inner
            .find_commit(to_gix_oid(id))
            .map_err(|err| BackendError::Object(err.to_string()))?;

        let parents = commit
            .parent_ids()
            .map(|parent| to_core_oid(parent.detach()))
            .collect();

        let (author_name, author_email) = match commit.author() {
            Ok(signature) => (bs(signature.name), bs(signature.email)),
            Err(_) => (String::new(), String::new()),
        };

        // `SignatureRef::time` 是未解析的原始字符串；提交对象自带的
        // `time()` / `SignatureRef::time()` 才是已解析的 Unix 秒。
        let author_time = commit.time().map_or(0, |time| time.seconds);
        let committer_time = commit
            .committer()
            .ok()
            .and_then(|signature| signature.time().ok())
            .map_or(author_time, |time| time.seconds);

        let summary = commit
            .message()
            .map_or_else(|_| String::new(), |message| message.summary().to_string());
        let summary = summary.trim_end().to_owned();

        Ok(CommitMeta {
            id,
            parents,
            summary,
            author_name,
            author_email,
            author_time,
            committer_time,
        })
    }

    /// 解析任意版本标识（分支名、标签、提交号、`HEAD~3` 等）为提交 id。
    ///
    /// # Errors
    /// 无法解析或不是提交对象时返回 [`BackendError::UnknownRevision`]。
    pub fn resolve(&self, spec: &str) -> Result<Oid> {
        let id = self
            .inner
            .rev_parse_single(spec)
            .map_err(|_| BackendError::UnknownRevision {
                spec: spec.to_owned(),
            })?;
        Ok(to_core_oid(id.detach()))
    }

    /// 计算工作区状态（等价 `git status --porcelain=v1`）。
    ///
    /// 未跟踪内容按 **`-unormal` 口径折叠**成目录条目（`?? dir/`），
    /// 与 `git status --porcelain` 的默认行为一致；展开到单个文件的
    /// `-uall` 口径留给后续按需开启。
    ///
    /// # Errors
    /// 状态迭代失败时返回 [`BackendError::Status`]。
    pub fn status(&self) -> Result<ChangeSet> {
        self.status_with_ignored(false)
    }

    /// [`Self::status`] 的「显示忽略文件」版本。
    ///
    /// `include_ignored` 为真时额外遍历 `.gitignore` 命中的条目，并以
    /// `!!` 条目加入结果（忽略目录按目录折叠，对齐 `git status --ignored`）。
    /// 忽略文件只在提交面板勾选「显示已忽略的文件」时请求，避免默认刷新
    /// 为一棵巨大的 `target/` 树付出遍历代价。
    ///
    /// # Errors
    /// 状态迭代失败时返回 [`BackendError::Status`]。
    pub fn status_with_ignored(&self, include_ignored: bool) -> Result<ChangeSet> {
        let began = Instant::now();

        let mut platform = self
            .inner
            .status(gix::progress::Discard)
            .map_err(|err| BackendError::Status(err.to_string()))?
            .untracked_files(gix::status::UntrackedFiles::Collapsed);

        if include_ignored {
            platform = platform.dirwalk_options(|options| {
                options.emit_ignored(Some(gix::dir::walk::EmissionMode::CollapseDirectory))
            });
        }

        let iter = platform
            .into_iter(std::iter::empty::<gix::bstr::BString>())
            .map_err(|err| BackendError::Status(err.to_string()))?;

        // 冲突类型必须先从索引三阶段推导，因为 gix 的 `Summary::Conflict`
        // 不携带具体冲突码（M0 待办 #6）。
        let conflicts = self.unmerged_kinds()?;

        let mut accumulator = StatusAccumulator::default();
        for item in iter {
            let item = item.map_err(|err| BackendError::Status(err.to_string()))?;
            match item {
                gix::status::Item::TreeIndex(change) => accumulator.apply_index_change(&change),
                gix::status::Item::IndexWorktree(item) => {
                    accumulator.apply_worktree_item(&item, &conflicts);
                }
            }
        }

        let command = if include_ignored {
            "git status --porcelain=v1 --ignored（内置引擎）"
        } else {
            "git status --porcelain=v1（内置引擎）"
        };
        self.record(command, began, None);
        Ok(accumulator.into_change_set())
    }

    /// 当前仓库生效的 `user.email`（FR-LOG-06 的"我的提交"判定依据）。
    ///
    /// 读配置快照，不产生子进程；未配置时返回 [`None`]（此时不做高亮，
    /// 而不是把所有提交都当成"我的"）。
    #[must_use]
    pub fn user_email(&self) -> Option<String> {
        self.inner
            .config_snapshot()
            .string("user.email")
            .map(|value| bs(&value))
            .filter(|value| !value.is_empty())
    }

    /// 当前仓库适用的 porcelain 路径渲染口径（读取 `core.quotepath`）。
    ///
    /// git 出厂配置为 `true`（非 ASCII 转义成 `\344\270\255`）；用户显式设为
    /// `false` 时原样输出。用于让 [`gt_core::ChangeSet::to_porcelain_with`]
    /// 与用户自己的 `git status --porcelain` 逐字节一致。
    #[must_use]
    pub fn porcelain_format(&self) -> PorcelainFormat {
        let quote_non_ascii = self
            .inner
            .config_snapshot()
            .boolean("core.quotepath")
            .unwrap_or(true);
        PorcelainFormat::from_quotepath(quote_non_ascii)
    }

    /// 按本仓库配置渲染工作区状态（与 `git status --porcelain=v1` 逐字节对齐）。
    ///
    /// # Errors
    /// 同 [`Self::status`]。
    pub fn status_porcelain(&self) -> Result<String> {
        let status = self.status()?;
        Ok(status.to_porcelain_with(self.porcelain_format()))
    }

    /// 从索引的三阶段条目推导每个冲突路径的 porcelain 冲突码。
    ///
    /// 索引把未合并路径拆成最多三条条目：stage 1 = base、stage 2 = ours、
    /// stage 3 = theirs（见 `gix_index::entry::Stage`）。是否存在某个阶段即编码了
    /// 冲突类型，映射规则见 [`conflict_kind_from_stages`]。
    ///
    /// 仓库无冲突时返回空表，因此这条路径对绝大多数状态查询是"只读一次索引文件"的开销。
    ///
    /// # Errors
    /// 索引文件缺失或损坏时返回 [`BackendError::Status`]。
    fn unmerged_kinds(&self) -> Result<BTreeMap<String, ConflictKind>> {
        // `git init` 之后、首次 `git add` 之前并没有索引文件；git 把这种
        // 状态等同于空索引（它自己也不会创建该文件），因此这里返回空表
        // 而不是把"打开索引失败"当成错误冒泡出去。
        if !self.inner.index_path().exists() {
            tracing::debug!("索引文件不存在，视为无冲突（git init 之后的正常状态）");
            return Ok(BTreeMap::new());
        }

        let index = self
            .inner
            .open_index()
            .map_err(|err| BackendError::Status(err.to_string()))?;

        // path → (has_base, has_ours, has_theirs)
        let mut stages: BTreeMap<String, (bool, bool, bool)> = BTreeMap::new();

        for entry in index.entries() {
            let stage = entry.stage();
            if stage == gix::index::entry::Stage::Unconflicted {
                continue;
            }
            let path = bs(entry.path(&index));
            let slot = stages.entry(path).or_insert((false, false, false));
            match stage {
                gix::index::entry::Stage::Base => slot.0 = true,
                gix::index::entry::Stage::Ours => slot.1 = true,
                gix::index::entry::Stage::Theirs => slot.2 = true,
                gix::index::entry::Stage::Unconflicted => {}
            }
        }

        Ok(stages
            .into_iter()
            .map(|(path, (base, ours, theirs))| {
                (path, conflict_kind_from_stages(base, ours, theirs))
            })
            .collect())
    }

    /// 当前索引中某路径是否处于"未合并"状态（冲突）。
    ///
    /// 供 `gt-service` 在写操作前给出更准确的提示：冲突文件的 hunk 暂存会被
    /// `git apply --cached` 拒绝，提前说明比让用户看到一句 fatal 更友好。
    ///
    /// # Errors
    /// 索引损坏时返回 [`BackendError::Status`]。
    pub fn path_is_conflicted(&self, path: &str) -> Result<bool> {
        Ok(self.unmerged_kinds()?.contains_key(path))
    }

    /// 计算两个修订之间的树差异（等价 `git diff --name-status <old> <new>`）。
    ///
    /// `old` 为 [`None`] 表示**空树**：即根提交与"无"的差异，对应
    /// `git diff-tree --root <new>` 的口径。
    ///
    /// `gix` 把 [`None`] 理解为空树，而**不是**索引或工作区。因此
    /// "暂存区 vs 工作区"这类差异不走本函数，而是由 `gt-service` 组合
    /// [`Self::blob_at`]（修订侧）与 [`Self::worktree_blob`]（工作区侧）逐文件完成。
    ///
    /// 重命名检测沿用 git 默认（相似度 50%、不检测复制），由
    /// `gix::diff::Options::from_configuration` 读取 `diff.renames` 决定，
    /// 因此与 `git diff --name-status` 的默认行为一致。
    ///
    /// # Errors
    /// 树对象读取或比较失败时返回 [`BackendError::Diff`]。
    pub fn tree_changes(&self, old: Option<&str>, new: &str) -> Result<Vec<FileChange>> {
        let began = Instant::now();
        let tree_old = old.map(|spec| self.tree_of(spec)).transpose()?;
        let tree_new = self.tree_of(new)?;

        let changes = self
            .inner
            .diff_tree_to_tree(tree_old.as_ref(), Some(&tree_new), None)
            .map_err(|err| BackendError::Diff(err.to_string()))?;

        let mut out = Vec::with_capacity(changes.len());
        for change in &changes {
            // `git diff --name-status` 递归展开到文件层；`gix` 会额外产出中间树
            // 条目（`A dir` 与 `A dir/b.txt` 并存），此处过滤掉树，
            // 否则文件列表里会出现 git 不会输出的"只有目录"的行。
            if is_tree_entry(change) {
                continue;
            }
            out.push(convert_tree_change(change));
        }

        self.record(
            format!(
                "git diff --name-status {} {new}（内置引擎）",
                old.unwrap_or("--root")
            ),
            began,
            None,
        );
        Ok(out)
    }

    /// 按图选项枚举提交 id（FR-LOG-05）。
    ///
    /// 与 [`Self::walk_ids`] 的差别：
    /// - `first_parent` 走 revwalk 的第一父项模式（`git log --first-parent`）；
    /// - `hide_merges` 在遍历过程中就地过滤（父项数在 `Info` 里已有，不额外读对象）；
    /// - `sort == Topological` 在本函数内做一次拓扑重排（见 [`topo_order`]）；
    /// - `collapse_linear` 只保留"结构点"（见 [`collapse_linear`]）。
    ///
    /// 返回值附带一个可选的降级说明：拓扑排序需要把整段历史的父项关系留在内存里，
    /// 超过 [`TOPO_MAX_NODES`] 时退回日期序并如实告知界面 —— 静默降级会让用户
    /// 以为"排序没生效"是 bug。
    ///
    /// # Errors
    /// 遍历失败时返回 [`BackendError::Object`]。
    pub fn walk_ids_with(
        &self,
        request: &LogPageRequest,
        limit: Option<usize>,
    ) -> Result<(Vec<Oid>, Option<String>)> {
        let began = Instant::now();
        let graph = request.graph;
        let tips = self.log_tips(request.filter.scope)?;
        let mut note = None;

        let mut nodes: Vec<GraphNode> = Vec::new();
        if !tips.is_empty() {
            let gix_tips: Vec<gix::ObjectId> = tips.iter().map(|oid| to_gix_oid(*oid)).collect();
            // `first_parent_only` 是 Platform 上的构建期选项，必须在 `.all()` 之前设。
            let mut platform = self.inner.rev_walk(gix_tips);
            if graph.first_parent {
                platform = platform.first_parent_only();
            }
            let walk = platform
                .all()
                .map_err(|err| BackendError::Object(err.to_string()))?;

            for info in walk {
                let info = info.map_err(|err| BackendError::Object(err.to_string()))?;
                let parents: Vec<Oid> = info.parent_ids.iter().map(|id| to_core_oid(*id)).collect();
                if graph.hide_merges && parents.len() > 1 {
                    continue;
                }
                nodes.push(GraphNode {
                    id: to_core_oid(info.id),
                    parents,
                });
                // 必须先收完整段再排序/折叠，因此这里的 limit 只用于防失控。
                if limit.is_some_and(|limit| nodes.len() >= limit) {
                    break;
                }
            }
        }

        let mut ids: Vec<Oid> = if graph.sort == LogSort::Topological {
            if nodes.len() > TOPO_MAX_NODES {
                note = Some(format!(
                    "提交数超过 {TOPO_MAX_NODES}，拓扑排序已退回提交日期序（结果仍为拓扑有效序）"
                ));
                nodes.iter().map(|node| node.id).collect()
            } else {
                topo_order(&nodes)
            }
        } else {
            nodes.iter().map(|node| node.id).collect()
        };

        if graph.collapse_linear {
            let mut anchors: BTreeSet<Oid> = tips.iter().copied().collect();
            for reference in self.refs()? {
                anchors.insert(reference.target);
            }
            ids = collapse_linear(&nodes, &ids, &anchors);
        }

        if let Some(limit) = limit {
            ids.truncate(limit);
        }

        self.record(
            format!(
                "git rev-list{} {}（内置引擎）",
                match request.filter.scope {
                    BranchScope::All => " --all",
                    BranchScope::LocalAndUpstream => " --branches HEAD",
                },
                graph_flags(graph)
            ),
            began,
            None,
        );
        Ok((ids, note))
    }

    /// 读取索引中某路径的内容（"已暂存"一侧的差异输入）。
    ///
    /// `HEAD` 侧没有对应文件时（新增文件）返回 [`None`]；超过 `max_bytes`
    /// 同样返回 [`None`] 交由上层显示占位。
    ///
    /// # Errors
    /// 索引损坏时返回 [`BackendError::Status`]。
    pub fn index_blob(&self, path: &str, max_bytes: usize) -> Result<Option<Vec<u8>>> {
        let began = Instant::now();
        if !self.inner.index_path().exists() {
            return Ok(None);
        }
        let index = self
            .inner
            .open_index()
            .map_err(|err| BackendError::Status(err.to_string()))?;

        let bpath = gix::bstr::BStr::new(path.as_bytes());
        let Some(entry) = index.entry_by_path(bpath) else {
            self.record(
                format!("git show :{path}（内置引擎，索引中不存在）"),
                began,
                None,
            );
            return Ok(None);
        };

        let id = entry.id;
        let header = self
            .inner
            .find_header(id)
            .map_err(|err| BackendError::Object(err.to_string()))?;
        if header.size() > max_bytes as u64 {
            self.record(
                format!("git show :{path}（内置引擎，超阈值跳过）"),
                began,
                None,
            );
            return Ok(None);
        }

        let mut blob = self
            .inner
            .find_object(id)
            .map_err(|err| BackendError::Object(err.to_string()))?
            .into_blob();
        let data = blob.take_data();
        self.record(format!("git show :{path}（内置引擎）"), began, None);
        Ok(Some(data))
    }

    /// 读取某个修订下某路径的内容。
    ///
    /// 超过 `max_bytes` 的 blob 不读取（返回 [`None`]），交由上层显示占位
    /// —— 这是架构规划 5.5 的内存尖峰防线。
    ///
    /// # Errors
    /// 修订无法解析时返回错误；路径不存在返回 `Ok(None)`。
    pub fn blob_at(&self, spec: &str, path: &str, max_bytes: usize) -> Result<Option<Vec<u8>>> {
        let began = Instant::now();
        let tree = self.tree_of(spec)?;
        let entry = tree
            .lookup_entry_by_path(path)
            .map_err(|err| BackendError::Object(err.to_string()))?;

        let Some(entry) = entry else {
            self.record(format!("git show {spec}:{path}（内置引擎）"), began, None);
            return Ok(None);
        };

        let id = entry.id().detach();
        let header = self
            .inner
            .find_header(id)
            .map_err(|err| BackendError::Object(err.to_string()))?;

        if header.size() > max_bytes as u64 {
            tracing::debug!(path, size = header.size(), "blob 超过阈值，跳过读取");
            self.record(
                format!("git show {spec}:{path}（内置引擎，超阈值跳过）"),
                began,
                None,
            );
            return Ok(None);
        }

        let mut blob = self
            .inner
            .find_object(id)
            .map_err(|err| BackendError::Object(err.to_string()))?
            .into_blob();
        let data = blob.take_data();

        self.record(format!("git show {spec}:{path}（内置引擎）"), began, None);
        Ok(Some(data))
    }

    /// 读取工作区中某文件的内容（本地更改 diff 的"工作区侧"）。
    ///
    /// 直接读原始字节，**不**应用 smudge / CRLF / attribute 过滤器。
    /// 对 `core.autocrlf` 生效的仓库，行尾表现可能与 git 不一致；这是 M1
    /// 的已知差异，随 FR-CON-01 的过滤器支持在 M5 处理。
    ///
    /// 超过 `max_bytes` 的文件不读取（返回 [`None`]），与 [`Self::blob_at`]
    /// 保持同一套内存防线。
    ///
    /// # Errors
    /// - 路径越出工作区（含 `..` 穿越）→ [`BackendError::PathOutsideWorktree`]；
    /// - 其他读取失败 → [`BackendError::Io`]；文件不存在返回 `Ok(None)`。
    pub fn worktree_blob(&self, path: &str, max_bytes: usize) -> Result<Option<Vec<u8>>> {
        let Some(work_dir) = self.location.work_dir.as_deref() else {
            return Ok(None);
        };

        // 先做纯词法的越界判定：绝对路径与 `..` 一律拒绝，且这一步不依赖
        // 文件是否存在 —— 否则"目标不存在"会走成 `Ok(None)`，把越界尝试
        // 静默吞掉。
        if !is_safe_relative_path(path) {
            return Err(BackendError::PathOutsideWorktree {
                path: PathBuf::from(path),
            });
        }

        let joined = work_dir.join(path);
        let metadata = match std::fs::metadata(&joined) {
            Ok(metadata) => metadata,
            // 文件不存在是"已删除"这一正常状态，不是错误。
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(BackendError::Io {
                    path: joined,
                    reason: err.to_string(),
                })
            }
        };

        // 目录、符号链接指向的目录等一律视为"无内容可比较"。
        if !metadata.is_file() {
            return Ok(None);
        }
        if metadata.len() > max_bytes as u64 {
            tracing::debug!(path, size = metadata.len(), "工作区文件超过阈值，跳过读取");
            return Ok(None);
        }

        // 词法检查拦不住符号链接：规范化后必须仍落在工作区内。
        let root = work_dir.canonicalize().map_err(|err| BackendError::Io {
            path: work_dir.to_path_buf(),
            reason: err.to_string(),
        })?;
        let target = joined.canonicalize().map_err(|err| BackendError::Io {
            path: joined.clone(),
            reason: err.to_string(),
        })?;
        if !target.starts_with(&root) {
            return Err(BackendError::PathOutsideWorktree { path: target });
        }

        std::fs::read(&target)
            .map(Some)
            .map_err(|err| BackendError::Io {
                path: target,
                reason: err.to_string(),
            })
    }

    /// 读取某路径在两个修订下的内容（用于文件级 diff）。
    ///
    /// # Errors
    /// 同 [`Self::blob_at`]。
    pub fn blob_pair(
        &self,
        old: Option<&str>,
        new: Option<&str>,
        path: &str,
        max_bytes: usize,
    ) -> Result<BlobPair> {
        let old_blob = old
            .map(|spec| self.blob_at(spec, path, max_bytes))
            .transpose()?
            .flatten();
        let new_blob = new
            .map(|spec| self.blob_at(spec, path, max_bytes))
            .transpose()?
            .flatten();
        Ok(BlobPair {
            old: old_blob,
            new: new_blob,
        })
    }

    /// 探测进行中的操作状态（FR-STA-02）。
    ///
    /// # Errors
    /// 不会失败；总是返回一个状态（无法读取 `.git` 时视为 [`RepoState::Clean`]）。
    pub fn repo_state(&self) -> RepoState {
        let began = Instant::now();
        let git_dir = self.inner.git_dir();

        let markers = MarkerPresence {
            merge_head: git_dir.join("MERGE_HEAD").exists(),
            rebase_merge: git_dir.join("rebase-merge").is_dir(),
            rebase_apply: git_dir.join("rebase-apply").is_dir(),
            cherry_pick_head: git_dir.join("CHERRY_PICK_HEAD").exists(),
            revert_head: git_dir.join("REVERT_HEAD").exists(),
            bisect_log: git_dir.join("BISECT_LOG").exists(),
        };

        let state = RepoState::from_markers(markers);
        self.record(
            "git status --porcelain=v2 --branch（内置引擎）",
            began,
            None,
        );
        state
    }

    /// 读取某个修订对应的树对象。
    fn tree_of(&self, spec: &str) -> Result<gix::Tree<'_>> {
        let id = self
            .inner
            .rev_parse_single(spec)
            .map_err(|_| BackendError::UnknownRevision {
                spec: spec.to_owned(),
            })?;
        let object = id
            .object()
            .map_err(|err| BackendError::Object(err.to_string()))?;
        object
            .peel_to_tree()
            .map_err(|err| BackendError::Object(err.to_string()))
    }

    /// 读取分支的上游引用名（`branch.<name>.remote` + `.merge`）。
    fn upstream_of(&self, full_ref: &str) -> Option<String> {
        let snapshot = self.inner.config_snapshot();
        let branch = full_ref.strip_prefix(HEADS_PREFIX)?;
        let remote_key = format!("branch.{branch}.remote");
        let merge_key = format!("branch.{branch}.merge");
        let remote = snapshot.string(remote_key.as_str())?;
        let merge = snapshot.string(merge_key.as_str())?;
        let merge_ref = bs(&merge);
        let short = merge_ref.strip_prefix(HEADS_PREFIX).unwrap_or(&merge_ref);
        let remote = bs(&remote);
        if remote == "." {
            Some(short.to_owned())
        } else {
            Some(format!("{remote}/{short}"))
        }
    }

    /// 向审计回调投递一条进程内路由记录。
    fn record(&self, command_line: impl Into<String>, began: Instant, error: Option<String>) {
        let mut record = CommandRecord::internal(
            CommandRoute::Gix,
            Some(self.location.work_dir_or_git_dir().to_path_buf()),
            command_line,
            began.elapsed(),
        );
        record.error = error;
        self.observer.observe(&record);
    }
}

/// 状态迭代过程中累积的原始信号。
///
/// 索引侧与工作区侧**分开收集**而不是按路径合并成一条：`git status --porcelain=v1`
/// 把它们当成两张独立的表，因此同一路径可以同时出现两次
/// （典型例子是 `git rm --cached f` 之后文件仍在磁盘上 —— git 输出 `D  f` 与 `?? f`）。
#[derive(Default)]
struct StatusAccumulator {
    /// 索引侧相对 HEAD 的变化（porcelain 的 `X`），值里的 [`Option`] 为
    /// 重命名 / 复制时的旧路径（porcelain 的 `旧 -> 新` 左半边）。
    staged: BTreeMap<String, (StatusFlag, Option<String>)>,
    /// 工作区侧相对索引的变化（porcelain 的 `Y`）。
    worktree: BTreeMap<String, StatusFlag>,
    /// 未跟踪路径（porcelain 的 `??`）；独立成行，不参与 `X`/`Y` 合并。
    untracked: BTreeSet<String>,
    /// 已忽略路径（porcelain 的 `!!`）；仅在请求中显式开启时才会出现。
    ignored: BTreeSet<String>,
    /// 冲突路径 → 冲突类型（porcelain 的 `UU`/`AA`/…）。
    conflicted: BTreeMap<String, ConflictKind>,
}

impl StatusAccumulator {
    /// 合并 `gix` 的索引侧变更（HEAD 树 vs 索引）。
    fn apply_index_change(&mut self, change: &gix::diff::index::ChangeRef<'_, '_>) {
        use gix::diff::index::ChangeRef;

        let path = bs(change.location());
        let (flag, old_path) = match change {
            ChangeRef::Addition { .. } => (StatusFlag::Added, None),
            ChangeRef::Deletion { .. } => (StatusFlag::Deleted, None),
            ChangeRef::Modification { .. } => (StatusFlag::Modified, None),
            ChangeRef::Rewrite {
                source_location,
                copy,
                ..
            } => {
                let flag = if *copy {
                    StatusFlag::Copied
                } else {
                    StatusFlag::Renamed
                };
                // `ChangeRef` 的源路径是 `Cow<BStr>`，需显式解引用才能取到字节。
                let old = bs(&**source_location);
                // 同路径的改写（内容重排）在 porcelain 里不显示箭头形式。
                let old = if old == path { None } else { Some(old) };
                (flag, old)
            }
        };
        self.staged.insert(path, (flag, old_path));
    }

    /// 合并 `gix` 的工作区侧条目（索引 vs 工作区）。
    ///
    /// `Summary` 是 `gix` 对"索引 vs 工作区"的语义摘要（见
    /// `gix_status::index_as_worktree_with_renames::Summary`）；冲突变体本身不携带
    /// 具体冲突类型，因此类型由调用方预先从索引三阶段推导
    /// （[`GitRepo::unmerged_kinds`]）后经 `conflicts` 传入。
    fn apply_worktree_item(
        &mut self,
        item: &gix::status::index_worktree::Item,
        conflicts: &BTreeMap<String, ConflictKind>,
    ) {
        use gix::status::index_worktree::iter::Summary;

        // 忽略条目不是 `Summary`（gix 会把它们过滤成 None），必须在摘要前识别。
        if let gix::status::index_worktree::Item::DirectoryContents { entry, .. } = item {
            if matches!(entry.status, gix::dir::entry::Status::Ignored(_)) {
                let mut path = bs(item.rela_path());
                if entry.disk_kind == Some(gix::dir::entry::Kind::Directory) {
                    path.push('/');
                }
                self.ignored.insert(path);
                return;
            }
        }

        let Some(summary) = item.summary() else {
            // 仅表示索引条目需要更新，或目录遍历产生的非未跟踪项 —— 不构成状态变化。
            return;
        };

        match summary {
            Summary::Conflict => {
                let path = bs(item.rela_path());
                let kind = conflicts
                    .get(&path)
                    .copied()
                    .unwrap_or(ConflictKind::BothModified);
                self.conflicted.insert(path, kind);
            }
            Summary::Added => {
                // 折叠的未跟踪目录带尾随 `/`，对齐 `git status --porcelain` 的 `?? dir/`。
                let mut path = bs(item.rela_path());
                if is_collapsed_directory(item) {
                    path.push('/');
                }
                self.untracked.insert(path);
            }
            Summary::Removed => self.insert_worktree(item, StatusFlag::Deleted),
            Summary::Modified => self.insert_worktree(item, StatusFlag::Modified),
            Summary::TypeChange => self.insert_worktree(item, StatusFlag::TypeChanged),
            Summary::Renamed => self.insert_worktree(item, StatusFlag::Renamed),
            Summary::Copied => self.insert_worktree(item, StatusFlag::Copied),
            // `git add --intent-to-add` 之后文件仍在磁盘上，索引条目的变化
            // 会由索引侧单独报告，这里不额外产出条目。
            Summary::IntentToAdd => {}
        }
    }

    /// 记录一条工作区侧变化。
    fn insert_worktree(&mut self, item: &gix::status::index_worktree::Item, flag: StatusFlag) {
        self.worktree.insert(bs(item.rela_path()), flag);
    }

    /// 按 porcelain v1 的口径组装条目列表。
    fn into_change_set(self) -> ChangeSet {
        let Self {
            staged,
            worktree,
            untracked,
            ignored,
            conflicted,
        } = self;

        let mut entries: Vec<StatusEntry> = Vec::with_capacity(
            conflicted.len() + staged.len().max(worktree.len()) + untracked.len() + ignored.len(),
        );

        // 冲突路径在 porcelain 里只输出冲突码，不再输出 `X`/`Y`。
        for (path, kind) in &conflicted {
            entries.push(StatusEntry::conflicted(path.clone(), *kind));
        }

        // `X`/`Y` 合并成一条。
        let paths: BTreeSet<&String> = staged.keys().chain(worktree.keys()).collect();
        for path in paths {
            if conflicted.contains_key(path) {
                continue;
            }
            let (index, old_path) = staged
                .get(path)
                .map_or((StatusFlag::Unmodified, None), |(flag, old)| {
                    (*flag, old.clone())
                });
            let worktree = worktree
                .get(path)
                .copied()
                .unwrap_or(StatusFlag::Unmodified);
            if index == StatusFlag::Unmodified && worktree == StatusFlag::Unmodified {
                continue;
            }
            let mut entry = StatusEntry::new(path.clone(), index, worktree);
            entry.old_path = old_path;
            entries.push(entry);
        }

        // 未跟踪独立成行（见 [`StatusAccumulator`] 的说明）。
        for path in &untracked {
            if conflicted.contains_key(path) {
                continue;
            }
            entries.push(StatusEntry::untracked(path.clone()));
        }

        // 忽略条目同样独立成行；默认路径下集合为空，不产生额外开销。
        for path in ignored {
            if untracked.contains(&path) || conflicted.contains_key(&path) {
                continue;
            }
            entries.push(StatusEntry::ignored(path));
        }

        ChangeSet::new(entries)
    }
}

/// 仓库相对路径是否安全（不含绝对路径、盘符、`..`）。
///
/// 只允许 [`std::path::Component::Normal`]：`git status` 输出的路径本就是
/// 仓库相对的、`/` 分隔的普通路径，任何其它形状都说明调用方传入了不该传的东西。
fn is_safe_relative_path(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    Path::new(path)
        .components()
        .all(|component| matches!(component, std::path::Component::Normal(_)))
}

/// 该条目是否为"折叠的未跟踪目录"。
///
/// 折叠目录在 porcelain 里带尾随斜杠（`?? dir/`），需要据此与普通未跟踪文件区分。
fn is_collapsed_directory(item: &gix::status::index_worktree::Item) -> bool {
    match item {
        gix::status::index_worktree::Item::DirectoryContents { entry, .. } => {
            entry.disk_kind == Some(gix::dir::entry::Kind::Directory)
        }
        _ => false,
    }
}

/// 从仓库根 `Cargo.toml` 读取工作区成员，作为「模块」分组的模块表。
///
/// 只处理工作区成员与根包两种形态：成员支持 `crates/*` 这类末段通配，
/// 复杂 glob 直接忽略（宁可少一个模块，也不猜错归属）。非 Cargo 仓库
/// 返回空表，由界面退化为按顶层目录分组。
fn detect_repo_modules(root: &Path) -> Vec<RepoModule> {
    let Ok(manifest) = fs::read_to_string(root.join("Cargo.toml")) else {
        return Vec::new();
    };
    let Ok(value) = manifest.parse::<toml::Value>() else {
        return Vec::new();
    };

    let mut member_dirs = BTreeSet::new();
    let mut excluded = BTreeSet::new();
    if let Some(workspace) = value.get("workspace") {
        if let Some(members) = workspace.get("members").and_then(toml::Value::as_array) {
            for member in members.iter().filter_map(toml::Value::as_str) {
                collect_member_dirs(root, member, &mut member_dirs);
            }
        }
        if let Some(excludes) = workspace.get("exclude").and_then(toml::Value::as_array) {
            for path in excludes.iter().filter_map(toml::Value::as_str) {
                excluded.insert(normalize_relative_path(path));
            }
        }
    }

    let mut modules = Vec::new();
    for rel in member_dirs {
        if is_excluded_member(&rel, &excluded) {
            continue;
        }
        let manifest = fs::read_to_string(root.join(&rel).join("Cargo.toml")).ok();
        let parsed = manifest.and_then(|text| text.parse::<toml::Value>().ok());
        let name = parsed
            .as_ref()
            .and_then(|value| value.get("package"))
            .and_then(|package| package.get("name"))
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                Path::new(&rel)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            });
        let Some(name) = name else {
            continue;
        };
        modules.push(RepoModule { name, path: rel });
    }

    // 根包（非工作区仓库，或同时是 package 的虚拟根）。
    if let Some(name) = value
        .get("package")
        .and_then(|package| package.get("name"))
        .and_then(toml::Value::as_str)
    {
        if modules.iter().all(|module| !module.path.is_empty()) {
            modules.push(RepoModule {
                name: name.to_owned(),
                path: String::new(),
            });
        }
    }

    modules.sort_by(|left, right| left.path.cmp(&right.path));
    modules
}

/// 展开一个 workspace member：支持末段 `*` 通配，其它 glob 语法忽略。
fn collect_member_dirs(root: &Path, member: &str, out: &mut BTreeSet<String>) {
    let member = normalize_relative_path(member);
    if let Some(prefix) = member.strip_suffix("/*") {
        let Ok(entries) = fs::read_dir(root.join(prefix)) else {
            return;
        };
        for entry in entries.flatten() {
            if entry.path().join("Cargo.toml").is_file() {
                let name = entry.file_name().to_string_lossy().into_owned();
                out.insert(format!("{prefix}/{name}"));
            }
        }
    } else if !member.contains(['*', '?', '[']) {
        out.insert(member);
    }
}

/// 统一成仓库相对路径：`/` 分隔、无首尾斜杠。
fn normalize_relative_path(path: &str) -> String {
    path.replace('\\', "/").trim_matches('/').to_owned()
}

/// member 是否被 workspace.exclude 覆盖（含子目录）。
fn is_excluded_member(member: &str, excluded: &BTreeSet<String>) -> bool {
    excluded.iter().any(|path| {
        member == path
            || member
                .strip_prefix(path)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// 由索引三阶段推导冲突类型（`git status` 的 `XY` 冲突码）。
///
/// 映射表（与 `git status` 文档一致）：
///
/// | 阶段 1(base) | 2(ours) | 3(theirs) | 代码 |
/// |---|---|---|---|
/// | ✓ | ✓ | ✓ | `UU` 双方修改 |
/// | ✗ | ✓ | ✓ | `AA` 双方新增 |
/// | ✓ | ✓ | ✗ | `UD` 我们修改 / 他们删除 |
/// | ✗ | ✓ | ✗ | `AU` 我们新增 |
/// | ✓ | ✗ | ✓ | `DU` 我们删除 / 他们修改 |
/// | ✗ | ✗ | ✓ | `UA` 他们新增 |
/// | ✓ | ✗ | ✗ | `DD` 双方删除 |
///
/// `gix` 的 `Summary::Conflict` 不区分具体类型，故此处显式推导，
/// 以满足 NFR-COR-01 对 porcelain 输出的逐字符一致要求（M0 待办 #6）。
#[must_use]
pub fn conflict_kind_from_stages(has_base: bool, has_ours: bool, has_theirs: bool) -> ConflictKind {
    match (has_base, has_ours, has_theirs) {
        (true, true, true) | (false, false, false) => ConflictKind::BothModified,
        (false, true, true) => ConflictKind::BothAdded,
        (true, true, false) => ConflictKind::DeletedByThem,
        (false, true, false) => ConflictKind::AddedByUs,
        (true, false, true) => ConflictKind::DeletedByUs,
        (false, false, true) => ConflictKind::AddedByThem,
        (true, false, false) => ConflictKind::BothDeleted,
    }
}

/// 该树差异条目是否指向一棵子树（而非文件 / 符号链接 / 子模块）。
fn is_tree_entry(change: &gix::diff::tree_with_rewrites::Change) -> bool {
    use gix::diff::tree_with_rewrites::Change;

    match change {
        Change::Addition { entry_mode, .. }
        | Change::Deletion { entry_mode, .. }
        | Change::Modification { entry_mode, .. }
        | Change::Rewrite { entry_mode, .. } => entry_mode.is_tree(),
    }
}

/// 把 `gix` 的树差异变更转为领域模型。
///
/// 重命名 / 复制会带上相似度分数：`gix` 在源与目标内容完全相同时把 `diff`
/// 置为 [`None`]（无需实际比较），此时分数即 100，与 `git diff --name-status`
/// 的 `R100` 一致。
fn convert_tree_change(change: &gix::diff::tree_with_rewrites::Change) -> FileChange {
    use gix::diff::tree_with_rewrites::Change;

    let path = bs(change.location());
    match change {
        Change::Addition { entry_mode, .. } => {
            debug_assert!(!entry_mode.is_tree(), "树条目应在 tree_changes 中被过滤");
            FileChange::new(path, ChangeKind::Added)
        }
        Change::Deletion { entry_mode, .. } => {
            debug_assert!(!entry_mode.is_tree(), "树条目应在 tree_changes 中被过滤");
            FileChange::new(path, ChangeKind::Deleted)
        }
        Change::Modification { entry_mode, .. } => {
            // 子模块指针变化与普通文件修改在 CLI 上都记为 `M`；类型变化不单独成码。
            debug_assert!(!entry_mode.is_tree(), "树条目应在 tree_changes 中被过滤");
            FileChange::new(path, ChangeKind::Modified)
        }
        Change::Rewrite {
            source_location,
            copy,
            diff,
            ..
        } => {
            let kind = if *copy {
                ChangeKind::Copied
            } else {
                ChangeKind::Renamed
            };
            let score = diff
                .as_ref()
                .map_or(100, |stats| similarity_percent(stats.similarity));
            let old = bs(source_location);
            let mut change = if old == path {
                FileChange::new(path, kind)
            } else {
                FileChange::with_old_path(path, old, kind)
            };
            change.similarity = Some(score);
            change
        }
    }
}

/// `gix` 的相似度（0.0–1.0）→ `git diff --name-status` 的整数百分比。
///
/// 夹到 `0..=100`：`f32` 在极端输入下可能给出 `101` 或负数。
fn similarity_percent(similarity: f32) -> u8 {
    let scaled = (similarity * 100.0).round();
    if scaled.is_finite() {
        scaled.clamp(0.0, 100.0) as u8
    } else {
        100
    }
}

/// 由完整引用名判定类别。
fn classify_ref(full: &str) -> RefKind {
    if full.starts_with(HEADS_PREFIX) {
        RefKind::LocalBranch
    } else if full.starts_with(REMOTES_PREFIX) {
        RefKind::RemoteBranch
    } else if full.starts_with(TAGS_PREFIX) {
        RefKind::Tag
    } else {
        RefKind::Other
    }
}

/// 去掉 `refs/heads/`、`refs/remotes/`、`refs/tags/` 前缀。
fn shorten_ref(full: &str) -> String {
    for prefix in [HEADS_PREFIX, REMOTES_PREFIX, TAGS_PREFIX] {
        if let Some(rest) = full.strip_prefix(prefix) {
            return rest.to_owned();
        }
    }
    full.to_owned()
}

/// 字节序列 → `String`（UTF-8 有损转换）。
///
/// 同时接受 `BStr` 与 `BString`：gix 的引用名多为前者，配置快照返回后者。
fn bs(value: impl AsRef<[u8]>) -> String {
    String::from_utf8_lossy(value.as_ref()).into_owned()
}

/// 领域 [`Oid`] → `gix::ObjectId`。调用方已保证仓库为 SHA-1。
fn to_gix_oid(oid: Oid) -> gix::ObjectId {
    gix::ObjectId::from_bytes_or_panic(oid.as_bytes())
}

/// `gix::ObjectId` → 领域 [`Oid`]。
fn to_core_oid(oid: gix::ObjectId) -> Oid {
    Oid::from_bytes(oid.as_slice()).unwrap_or(Oid::ZERO)
}

/// 对象不可读时的占位元数据，保证行数与布局一一对应。
fn placeholder_meta(id: Oid) -> CommitMeta {
    CommitMeta {
        id,
        parents: Vec::new(),
        summary: "<对象不可读>".to_owned(),
        author_name: String::new(),
        author_email: String::new(),
        author_time: 0,
        committer_time: 0,
    }
}

/// 提交图窗口中的一个节点（遍历时顺带留下父项，供排序与折叠使用）。
#[derive(Debug, Clone)]
pub struct GraphNode {
    /// 提交 id。
    pub id: Oid,
    /// 父提交（第一父项在前）。
    pub parents: Vec<Oid>,
}

/// 拓扑排序允许留在内存里的节点上限。
///
/// 200k 节点 ≈ 逐节点 20 字节 id + 父项表，实测约 15–25MB；作为"排序"这种
/// 纯展示选项，不值得为它突破内存预算（NFR-MEM-03），因此超限即退回日期序
/// 并把降级原因交给界面。
pub const TOPO_MAX_NODES: usize = 200_000;

/// 把"按提交日期"的节点序列重排成拓扑序（IntelliSort 的排序基础）。
///
/// 算法是带"就绪栈"的 Kahn：只有当一个提交的**全部子提交都已输出**时它才就绪；
/// 就绪后优先沿第一父项继续下潜（这正是 `git rev-list --topo-order` 的
/// "不把多条历史交错显示"的行为：8,6,5,3,7,4,2,1）。
///
/// 与"先做 DFS 再兜底"的写法相比，`pending_children` 计数保证了
/// **任何提交都不会出现在它的某个子提交之前** —— 这是 `gt-graph` 泳道布局的
/// 硬前提（`LaneLayout` 的拓扑校验会直接报违规）。
#[must_use]
pub fn topo_order(nodes: &[GraphNode]) -> Vec<Oid> {
    use std::collections::{HashMap, VecDeque};

    let position: HashMap<Oid, usize> = nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (node.id, index))
        .collect();

    // pending[index] = "还有多少个窗口内子提交没输出"。
    let mut pending = vec![0u32; nodes.len()];
    for node in nodes {
        for parent in &node.parents {
            if let Some(&index) = position.get(parent) {
                pending[index] += 1;
            }
        }
    }

    // 初始就绪集合按日期序入队（保证同级节点之间的顺序与 `git log` 一致）。
    let mut queue: VecDeque<usize> = pending
        .iter()
        .enumerate()
        .filter(|(_, count)| **count == 0)
        .map(|(index, _)| index)
        .collect();

    let mut stack: Vec<usize> = Vec::new();
    let mut emitted = vec![false; nodes.len()];
    let mut out: Vec<Oid> = Vec::with_capacity(nodes.len());

    loop {
        let next = stack.pop().or_else(|| queue.pop_front());
        let Some(index) = next else {
            break;
        };
        if emitted[index] {
            continue;
        }
        emitted[index] = true;
        out.push(nodes[index].id);

        // 逆序压栈 → 第一父项最后压入、最先弹出，形成"沿主干下潜"的次序。
        for parent in nodes[index].parents.iter().rev() {
            let Some(&parent_index) = position.get(parent) else {
                continue;
            };
            pending[parent_index] -= 1;
            if pending[parent_index] == 0 {
                stack.push(parent_index);
            }
        }
    }

    // 理论上不会有剩余（窗口内无环）；真出现环时也要把剩下的按日期序补齐，
    // 否则界面会少行 —— 少行比顺序不完美更难解释。
    if out.len() < nodes.len() {
        for (index, node) in nodes.iter().enumerate() {
            if !emitted[index] {
                out.push(node.id);
            }
        }
    }
    out
}

/// 只保留"结构点"：合并、根、分叉、分支尖端、以及有引用 / HEAD 的提交。
///
/// 这是"折叠线性分支"在 id 列表层的实现（见
/// [`gt_core::GraphOptions::collapse_linear`] 的交付口径说明）。
#[must_use]
pub fn collapse_linear(nodes: &[GraphNode], order: &[Oid], anchors: &BTreeSet<Oid>) -> Vec<Oid> {
    use std::collections::HashMap;

    let mut children: HashMap<Oid, u32> = HashMap::new();
    for node in nodes {
        for parent in &node.parents {
            *children.entry(*parent).or_insert(0) += 1;
        }
    }

    let by_id: HashMap<Oid, &GraphNode> = nodes.iter().map(|node| (node.id, node)).collect();
    order
        .iter()
        .filter(|id| {
            let Some(node) = by_id.get(id) else {
                // 不在窗口内的 id（理论上不会出现）保留，避免意外丢行。
                return true;
            };
            let parents = node.parents.len();
            let child_count = children.get(*id).copied().unwrap_or(0);
            parents != 1 || child_count != 1 || anchors.contains(*id)
        })
        .copied()
        .collect()
}

/// 图选项的审计标签。
fn graph_flags(graph: gt_core::GraphOptions) -> String {
    let mut flags = Vec::new();
    if graph.first_parent {
        flags.push("--first-parent");
    }
    if graph.hide_merges {
        flags.push("--no-merges");
    }
    if graph.sort == LogSort::Topological {
        flags.push("--topo-order");
    }
    if flags.is_empty() {
        String::new()
    } else {
        format!(" {}", flags.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::{classify_ref, conflict_kind_from_stages, shorten_ref, RepoLocation};
    use gt_core::{ConflictKind, RefKind};
    use std::path::PathBuf;
    #[test]
    fn conflict_stages_map_to_porcelain_codes() {
        // 与 `git status --porcelain` 文档中的冲突码逐条对应。
        let cases = [
            ((true, true, true), ConflictKind::BothModified, ('U', 'U')),
            ((false, true, true), ConflictKind::BothAdded, ('A', 'A')),
            ((true, true, false), ConflictKind::DeletedByThem, ('U', 'D')),
            ((false, true, false), ConflictKind::AddedByUs, ('A', 'U')),
            ((true, false, true), ConflictKind::DeletedByUs, ('D', 'U')),
            ((false, false, true), ConflictKind::AddedByThem, ('U', 'A')),
            ((true, false, false), ConflictKind::BothDeleted, ('D', 'D')),
        ];
        for ((base, ours, theirs), expected, code) in cases {
            let kind = conflict_kind_from_stages(base, ours, theirs);
            assert_eq!(kind, expected, "阶段组合 ({base},{ours},{theirs})");
            assert_eq!(
                kind.code(),
                code,
                "阶段组合 ({base},{ours},{theirs}) 的 porcelain 码"
            );
        }

        // 三阶段全缺是索引损坏的兜底，按最保守的"双方修改"处理，不应 panic。
        assert_eq!(
            conflict_kind_from_stages(false, false, false),
            ConflictKind::BothModified
        );
    }

    #[test]
    fn shared_repo_handle_is_send_and_sync() {
        // 编译期契约：服务层的"每线程取本地句柄"设计依赖这个性质。
        // 一旦 `SharedGitRepo` 不再满足，这里会直接编译失败而不是运行期踩坑。
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<super::SharedGitRepo>();
        assert_send_sync::<super::RepoLocation>();
    }

    #[test]
    fn ref_classification_covers_all_namespaces() {
        assert_eq!(classify_ref("refs/heads/main"), RefKind::LocalBranch);
        assert_eq!(
            classify_ref("refs/remotes/origin/main"),
            RefKind::RemoteBranch
        );
        assert_eq!(classify_ref("refs/tags/v1.0"), RefKind::Tag);
        assert_eq!(classify_ref("refs/stash"), RefKind::Other);
    }

    #[test]
    fn ref_shortening_strips_known_prefixes() {
        assert_eq!(shorten_ref("refs/heads/feature/x"), "feature/x");
        assert_eq!(shorten_ref("refs/remotes/origin/main"), "origin/main");
        assert_eq!(shorten_ref("refs/tags/v1.0"), "v1.0");
        assert_eq!(shorten_ref("refs/stash"), "refs/stash");
    }

    #[test]
    fn location_display_name_prefers_worktree() {
        let location = RepoLocation {
            git_dir: PathBuf::from("/tmp/repos/demo/.git"),
            work_dir: Some(PathBuf::from("/tmp/repos/demo")),
            is_bare: false,
        };
        assert_eq!(location.display_name(), "demo");
        assert_eq!(
            location.work_dir_or_git_dir(),
            PathBuf::from("/tmp/repos/demo")
        );

        let bare = RepoLocation {
            git_dir: PathBuf::from("/tmp/bare/other.git"),
            work_dir: None,
            is_bare: true,
        };
        assert_eq!(bare.display_name(), "bare");
        assert_eq!(
            bare.work_dir_or_git_dir(),
            PathBuf::from("/tmp/bare/other.git")
        );
    }

    /// 构造图节点用的 oid。
    fn oid(seed: u8) -> gt_core::Oid {
        gt_core::Oid::from_bytes(&[seed; 20]).expect("Oid")
    }

    fn node(seed: u8, parents: &[u8]) -> super::GraphNode {
        super::GraphNode {
            id: oid(seed),
            parents: parents.iter().copied().map(oid).collect(),
        }
    }

    #[test]
    fn topo_order_keeps_children_before_parents() {
        // 0 → (1, 2)；1 → 3；2 → 3。日期序恰好是 0,1,2,3。
        let nodes = vec![node(0, &[1, 2]), node(1, &[3]), node(2, &[3]), node(3, &[])];
        let order = super::topo_order(&nodes);
        assert_eq!(order.len(), 4);
        // 不变量：任何提交都不出现在它的子提交之前。
        let position: std::collections::HashMap<_, _> = order
            .iter()
            .enumerate()
            .map(|(index, id)| (*id, index))
            .collect();
        for item in &nodes {
            for parent in &item.parents {
                assert!(
                    position[&item.id] < position[parent],
                    "子提交必须排在父提交之前"
                );
            }
        }
    }

    #[test]
    fn topo_order_keeps_the_first_parent_chain_together() {
        // 8 的两父：6（第一父）与 7（第二父，侧枝）。
        // 期望：8 之后紧跟第一父链 6,5,3，再回到 7,4,2,1。
        let nodes = vec![
            node(8, &[6, 7]),
            node(7, &[4]),
            node(6, &[5]),
            node(5, &[3]),
            node(4, &[2]),
            node(3, &[]),
            node(2, &[1]),
            node(1, &[]),
        ];
        let order = super::topo_order(&nodes);
        assert_eq!(
            order,
            vec![
                oid(8),
                oid(6),
                oid(5),
                oid(3),
                oid(7),
                oid(4),
                oid(2),
                oid(1)
            ],
            "应按第一父项下潜，与 git rev-list --topo-order 的形状一致"
        );
    }

    #[test]
    fn topo_order_survives_a_disconnected_chain() {
        // 两条互不相关的历史（多分支且无公共祖先）：不得丢行。
        let nodes = vec![node(1, &[2]), node(2, &[]), node(3, &[4]), node(4, &[])];
        let order = super::topo_order(&nodes);
        assert_eq!(order.len(), 4, "任何节点都不能被丢掉：{order:?}");
    }

    #[test]
    fn collapse_keeps_only_structural_points() {
        // 纯线性链 1-2-3-4：只保留尖端与根。
        let nodes = vec![node(1, &[2]), node(2, &[3]), node(3, &[4]), node(4, &[])];
        let order: Vec<_> = nodes.iter().map(|item| item.id).collect();
        let collapsed = super::collapse_linear(&nodes, &order, &std::collections::BTreeSet::new());
        assert_eq!(collapsed, vec![oid(1), oid(4)], "线性中间提交应被折叠");

        // 同样的链，但把 2 标成引用点（分支指向它）→ 必须保留。
        let anchors = std::collections::BTreeSet::from([oid(2)]);
        let collapsed = super::collapse_linear(&nodes, &order, &anchors);
        assert_eq!(collapsed, vec![oid(1), oid(2), oid(4)], "引用点不得被折叠");
    }

    #[test]
    fn collapse_keeps_merge_and_branch_points() {
        // 10 是合并提交；12 有两个子（11 与 20），是分叉点；
        // 11 与 20 都是"单子单父"的线性中间节点，应被折叠。
        let nodes = vec![
            node(10, &[11, 20]),
            node(11, &[12]),
            node(12, &[13]),
            node(13, &[]),
            node(20, &[12]),
        ];
        let order: Vec<_> = nodes.iter().map(|item| item.id).collect();
        let collapsed = super::collapse_linear(&nodes, &order, &std::collections::BTreeSet::new());
        assert_eq!(
            collapsed,
            vec![oid(10), oid(12), oid(13)],
            "合并点 10、分叉点 12、根 13 保留；线性中间节点 11/20 折叠"
        );
    }

    #[test]
    fn collapse_keeps_branch_points() {
        // 5 有两个子（4 与 6），是分叉点，必须保留。
        let nodes = vec![node(4, &[5]), node(5, &[7]), node(6, &[5]), node(7, &[])];
        let order: Vec<_> = nodes.iter().map(|node| node.id).collect();
        let collapsed = super::collapse_linear(&nodes, &order, &std::collections::BTreeSet::new());
        assert!(
            collapsed.contains(&oid(5)),
            "分叉点不得被折叠：{collapsed:?}"
        );
        assert!(
            collapsed.contains(&oid(4)) && collapsed.contains(&oid(6)),
            "尖端保留"
        );
    }

    #[test]
    fn graph_flags_render_expected_cli_arguments() {
        use gt_core::GraphOptions;
        assert_eq!(super::graph_flags(GraphOptions::default()), "");
        assert_eq!(
            super::graph_flags(GraphOptions {
                first_parent: true,
                ..GraphOptions::default()
            }),
            " --first-parent"
        );
        assert_eq!(
            super::graph_flags(GraphOptions {
                hide_merges: true,
                sort: gt_core::LogSort::Topological,
                ..GraphOptions::default()
            }),
            " --no-merges --topo-order"
        );
    }

    /// 模块识别：工作区成员（含末段通配、exclude、根包）按包名收录。
    #[test]
    fn cargo_workspace_members_become_modules() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);

        let root = std::env::temp_dir().join(format!(
            "knotwork-modules-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["crates/alpha", "crates/beta", "crates/skip", "app"] {
            std::fs::create_dir_all(root.join(dir)).expect("建模块目录");
        }
        std::fs::write(
            root.join("Cargo.toml"),
            r#"
[workspace]
members = ["crates/*", "app"]
exclude = ["crates/skip"]

[package]
name = "root-demo"
"#,
        )
        .expect("写根清单");
        std::fs::write(
            root.join("crates/alpha/Cargo.toml"),
            "[package]\nname = \"alpha-core\"\n",
        )
        .expect("写 alpha");
        std::fs::write(
            root.join("crates/beta/Cargo.toml"),
            "[package]\nname = \"beta-ui\"\n",
        )
        .expect("写 beta");
        std::fs::write(
            root.join("crates/skip/Cargo.toml"),
            "[package]\nname = \"skip-me\"\n",
        )
        .expect("写 skip");
        std::fs::write(
            root.join("app/Cargo.toml"),
            "[package]\nname = \"demo-app\"\n",
        )
        .expect("写 app");

        let modules = super::detect_repo_modules(&root);
        let pairs: Vec<(&str, &str)> = modules
            .iter()
            .map(|module| (module.path.as_str(), module.name.as_str()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("", "root-demo"),
                ("app", "demo-app"),
                ("crates/alpha", "alpha-core"),
                ("crates/beta", "beta-ui"),
            ],
            "成员按路径排序，exclude 生效，根包单列"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 模块识别：没有 Cargo 清单时返回空表（界面退化为顶层目录）。
    #[test]
    fn non_cargo_repo_has_no_modules() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);

        let root = std::env::temp_dir().join(format!(
            "knotwork-no-modules-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("建临时目录");
        assert!(super::detect_repo_modules(&root).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
