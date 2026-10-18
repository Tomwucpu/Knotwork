//! 界面动态文案（NFR-L10N-01，M5；由 `scripts/i18n_sweep.py` 的映射表驱动）。
//!
//! Slint 侧的静态文案在 `ui/texts.slint`；本模块负责 Rust 侧拼出来的
//! 动态文案（提示条、确认框、标题等）。键即中文原文（源语言），
//! 英文未收录时回落中文 —— 缺译不算缺陷，游离文案才算（扫描测试把关）。

use std::sync::atomic::{AtomicBool, Ordering};

static EN: AtomicBool = AtomicBool::new(false);

/// 切换界面语言（应用启动与设置保存时调用）。
pub fn set_english(enabled: bool) {
    EN.store(enabled, Ordering::Relaxed);
}

/// 当前是否英文界面。
#[must_use]
pub fn is_english() -> bool {
    EN.load(Ordering::Relaxed)
}

/// 取动态文案：以中文原文为键，英文缺失时回落原文。
#[must_use]
pub fn tr(zh: &'static str) -> &'static str {
    if !is_english() {
        return zh;
    }
    translate(zh).unwrap_or(zh)
}

/// 带占位符的动态文案：模板中的 `{}` 依序用 `args` 替换。
///
/// `format!` 的模板必须是字面量，因此带参数的文案走这里：
/// `text::trf("{} 失败", &[&kind])`。
#[must_use]
pub fn trf(zh: &'static str, args: &[&str]) -> String {
    let mut out = tr(zh).to_owned();
    for arg in args {
        out = out.replacen("{}", arg, 1);
    }
    out
}

/// 静态映射表：中文原文 → 英文。缺译回落原文（见 [`tr`]）。
#[allow(clippy::too_many_lines)]
fn translate(zh: &str) -> Option<&'static str> {
    Some(match zh {
        "强制删除分支 {}？" => "Force-delete branch {name}?",
        "删除分支 {}？" => "Delete branch {name}?",
        "检出修订 {}？" => "Checkout revision {revision}?",
        "合入分支" => "Theirs (incoming)",
        "当前分支" => "Current branch",
        "中止{}？" => "Abort {}?",
        "强制推送到 {}/{}？" => "Force push to {}/{}?",
        "合并到当前分支" => "Merge into current branch",
        "填写要合入的修订（分支名 / 标签 / 提交 id）。冲突时会进入三路冲突编辑器。" => "Revision to merge (branch / tag / commit id). Conflicts open the three-way editor.",
        "例如 feature/login 或 origin/main" => "e.g. feature/login or origin/main",
        "变基到指定修订" => "Rebase onto revision",
        "当前分支的提交会被重放到该修订之上。冲突时可继续 / 跳过 / 中止。" => "Commits on the current branch are replayed onto the revision. Continue / skip / abort on conflicts.",
        "例如 main 或 origin/main" => "e.g. main or origin/main",
        "重置当前分支" => "Reset current branch",
        "软重置保留暂存；混合重置取消暂存；硬重置会丢弃全部未提交改动（需二次确认）。" => "Soft keeps staged; mixed unstages; hard discards all uncommitted changes (needs confirmation).",
        "例如 HEAD~1 或提交 id" => "e.g. HEAD~1 or commit id",
        "生成补丁" => "Create patch",
        "补丁输出路径" => "Patch output path",
        "区间起点（如 origin/main）" => "Range start (e.g. origin/main)",
        "提交补丁（{}）" => "Patch commits ({short})",
        "区间差异" => "Range diff",
        "工作区差异" => "Working tree diff",
        "拣选提交" => "Cherry-pick commit",
        "要拣选的提交或区间" => "Commit or range to cherry-pick",
        "不追加来源行" => "No origin line",
        "追加来源行（-x）" => "Append origin line (-x)",
        "应用补丁" => "Apply patch",
        "三方合并（--3way）在上下文不匹配时会用 blob 信息尝试合并，冲突进入三路编辑器。" => "3-way (--3way) falls back to blob info when context mismatches; conflicts open the three-way editor.",
        "补丁文件路径" => "Patch file path",
        "普通应用" => "Normal apply",
        "三方合并（--3way）" => "3-way merge (--3way)",
        "新建标签" => "Create tag",
        "附注消息留空即创建轻量标签；填写则会创建附注标签（含创建者与日期）。" => "Leave the message empty for a lightweight tag; filling it creates an annotated tag (with tagger and date).",
        "标签名，例如 v1.0.0" => "Tag name, e.g. v1.0.0",
        "附注消息（可留空）" => "Annotation message (optional)",
        "差异" => "Diff",
        "git {} · 内置引擎读取" => "git {version} · built-in engine reads",
        "未找到 git（只读浏览）" => "git not found (read-only browsing)",
        "已暂存" => "Staged",
        "工作区" => "Working tree",
        "无法提交：{}" => "Cannot commit: {reason}",
        "暂存区为空：先暂存要提交的内容" => "Nothing staged: stage what you want to commit first",
        "将提交 {} 个已暂存路径" => "Will commit {} staged path(s)",
        "请先选择一个文件" => "Select a file first",
        "请先选择一个未跟踪路径" => "Select an untracked path first",
        "请先选择一个路径" => "Select a path first",
        "请先在差异视图中选择 hunk 或行" => "Select hunks or lines in the diff view first",
        "当前差异不支持按块操作；新增文件请用「暂存」整文件操作" => "This diff does not support block operations; stage new files as a whole with \"Stage\"",
        "当前差异不支持按块操作" => "This diff does not support block operations",
        "所选内容不包含可落库的变更" => "The selection contains no applicable changes",
        "请先在输入框填写新分支名" => "Type a new branch name in the input first",
        "请先在侧栏选择一个分支" => "Select a branch in the sidebar first",
        "只能直接检出本地分支；远程分支请先「新建」本地分支" => "Only local branches can be checked out directly; create a local branch from the remote first",
        "请在输入框填写新的分支名" => "Type the new branch name in the input",
        "不能删除当前所在的分支" => "Cannot delete the branch you are on",
        "请先在提交列表中选择一条提交" => "Select a commit in the history first",
        "请先在侧栏选中一个标签" => "Select a tag in the sidebar first",
        "请输入搜索内容" => "Type something to search",
        "请填写远端名称与地址" => "Fill in the remote name and URL",
        "请先选择一个远端" => "Select a remote first",
        "地址不能为空" => "URL cannot be empty",
        "当前远端不是 HTTPS 地址，无法保存 token" => "The current remote is not HTTPS; cannot save a token",
        "请先填写 token" => "Type the token first",
        "当前远端不是 HTTPS 地址，没有可删除的 token" => "The current remote is not HTTPS; no token to delete",
        "该仓库没有配置远端，请先在「远端」里添加" => "No remotes configured; add one under \"Remotes\" first",
        "没有需要推送的提交" => "No commits to push",
        "已请求取消，正在终止 git 子进程…" => "Cancel requested, terminating git subprocess…",
        "当前没有进行中的网络操作" => "No network operation in progress",
        "正在{}…" => "{label} in progress…",
        "选择已失效，请重新选择" => "Selection is stale, please reselect",
        "未找到 git（只读模式）" => "git not found (read-only mode)",
        "上一个操作还未完成，请稍候" => "Previous operation still running, please wait",
        "请求失败" => "Request failed",
        "{}失败：{}" => "{} failed: {message}",
        "读取失败：{}" => "Read failed: {message}",
        "搜索「{}」超过 1.5 秒已中止；大仓库可等待后续版本的历史索引" => "Search \"{}\" aborted after 1.5s; a history index may come in a future release",
        "搜索：{}" => "Search: {query}",
        "已加载 {} / {} 条" => "Loaded {} / {}",
        "(冲突 {} · 未跟踪 {} · 已暂存 {} · 已修改 {})" => "(conflicts {} · untracked {} · staged {} · modified {})",
        "二进制" => "binary",
        "日志页不是顺序追加，已丢弃" => "Log page is not a sequential append; dropped",
        "无法按图片解码（{} KB）" => "Cannot decode as image ({} KB)",
        "{}×{} · {} KB（过大，仅显示尺寸）" => "{}×{} · {} KB (too large, size info only)",
        "（旧侧不存在：新增文件）" => "(old side missing: new file)",
        "（新侧不存在：已删除）" => "(new side missing: deleted)",
        "未知的提交菜单动作" => "Unknown commit menu action",
        "已复制哈希 {}" => "Copied hash {}",
        "复制失败：{}（哈希：{}）" => "Copy failed: {err} (hash: {})",
        "浏览版本 {}（{}）" => "Browsing revision {} ({})",
        "只读" => "read-only",
        "{} 与 HEAD 比较" => "{} vs HEAD",
        "先在本地更改中选择一个文件" => "Select a file in Local Changes first",
        "该行尚未提交，没有可跳转的提交" => "This line is uncommitted; nothing to jump to",
        "提交详情 · {}" => "Commit details · {}",
        "先选中一行再上溯" => "Select a line before tracing",
        "该行已追溯到文件最初的来源" => "This line is already traced to the file's origin",
        "请先填写必填项" => "Fill in the required fields first",
        "区间差异需要填写起点" => "Range diff needs a start point",
        "未知的操作对话框类型" => "Unknown operation dialog type",
        "根提交没有父提交，无法从此处开始交互式变基" => "The root commit has no parent; cannot start an interactive rebase here",
        "正在生成变基计划…" => "Generating rebase plan…",
        "第一个提交没有上一条可并入：请改用「保留」或「改写」" => "The first commit has no previous one to fold into: use Keep or Reword",
        "请先在某一处冲突上点「手动编辑」" => "Click \"Manual edit\" on a conflict first",
        "请先选择一个冲突文件" => "Select a conflicted file first",
        "该文件没有冲突标记：请用「整文件取当前分支 / 合入分支」" => "This file has no conflict markers: use \"Take ours/theirs for whole file\"",
        "还有 {} 处冲突未决定" => "{undecided} conflict(s) undecided",
        "请先在侧栏输入新分支名" => "Type a new branch name in the sidebar first",
        "当前没有进行中的操作" => "No operation in progress",
        "当前操作不支持跳过" => "This operation cannot be skipped",
        "交互式变基：{} → {}" => "Interactive rebase: {} → {}",
        " ⚠ 区间内还有 {} 个合并提交：交互式变基会线性化历史，这些合并提交不会保留。" => " ⚠ {} merge commit(s) in range: interactive rebase linearizes history; they will not be preserved.",
        "共 {} 个提交，其中 {} 个会保留。压缩/吞并会把该提交并入上一条；改写会先重放再改消息。{}" => "{} commits total, {kept} will be kept. Squash/Fixup fold into the previous commit; Reword replays with a new message.{merges}",
        "冲突解决（正在读取文件…）" => "Conflict resolution (reading file…)",
        "还有 {} 个文件待解决" => "{} file(s) left to resolve",
        "冲突解决：{}（还有 {} 个文件待解决）" => "Conflict resolution: {} ({} file(s) left)",
        "版本 {}" => "Revision {}",
        "（行数过多，已截断）" => "(too many lines, truncated)",
        "文件历史 · {}" => "File history · {}",
        "刚刚" => "just now",
        "{} 分钟前" => "{} min ago",
        "{} 小时前" => "{} h ago",
        "{} 天前" => "{} d ago",
        "{} 个月前" => "{} mo ago",
        "{} 年前" => "{} yr ago",
        "（二进制文件，不显示内容差异）" => "(binary file, no content diff)",
        "（文件过大，已跳过差异计算：旧 {} 字节 / 新 {} 字节）" => "(file too large, diff skipped: old {old_bytes} B / new {new_bytes} B)",
        "（两侧内容相同或为空）" => "(both sides identical or empty)",
        "原路径 {}" => "was {old}",
        "当前处于「{}」进行中" => "Currently in \"{}\"",
        "；可执行：{} / {} / {}" => "; available: {} / {} / {}",
        "分离头指针" => "detached HEAD",
        "尚无提交" => "no commits yet",
        "该差异不支持逐块选择" => "This diff does not support block selection",
        "点击 hunk 头或增删行以选择，然后反暂存" => "Click hunk headers or changed lines to select, then unstage",
        "点击 hunk 头或增删行以选择，然后暂存 / 丢弃" => "Click hunk headers or changed lines to select, then stage / discard",
        "已选 {} 行" => "{} lines selected",
        "\n…以及另外 {} 个路径" => "\n…and {} more paths",
        "永久删除 {} 个未跟踪路径？" => "Permanently delete {} untracked path(s)?",
        "丢弃 {} 个文件的未提交修改？" => "Discard uncommitted changes in {} file(s)?",
        "丢弃所选变更？" => "Discard the selected changes?",
        "URL 内嵌凭据（建议改用 helper）" => "URL embeds credentials (prefer a credential helper)",
        "push URL 与 fetch 不同" => "push URL differs from fetch",
        "（仅列出前 {} 条）" => "(showing first {})",
        "该分支命中受保护规则「{}」：禁止强制推送（普通推送不受影响）。" => "Branch matches protected rule \"{}\": force push denied (normal push unaffected).",
        "远端另有 {} 个提交：普通推送会被拒绝，请先 Update。" => "Remote has {} extra commits: a normal push would be rejected, update first.",
        "远端尚无该分支，推送会在远端新建它。" => "The branch does not exist on the remote yet; pushing will create it.",
        "未检测到 credential helper" => "No credential helper detected",
        "SSH agent 可用" => "SSH agent available",
        "SSH agent 未运行（SSH 远端可能需要 ssh-add）" => "SSH agent not running (SSH remotes may need ssh-add)",
        "系统密钥链不可用，无法在本工具内保存 token" => "System keychain unavailable; cannot store tokens in-app",
        "尚未保存任何 token" => "No tokens stored yet",
        "已保存 token 的主机：{}" => "Hosts with stored tokens: {}",
        "已保存并应用（staging 模式、主题与语言即时生效；日志级别在下次启动生效）" => "Saved & applied (staging mode, theme and language take effect instantly; log level on next start)",
        "已应用到本窗口，但写盘失败：{err}" => "Applied to this window, but writing the config failed: {err}",
        "无法按图片解码" => "Cannot decode as image",
        "过大，仅显示尺寸" => "too large, size info only",
        "版本" => "Revision",
        _ => return None,
    })
}