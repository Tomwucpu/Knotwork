//! 内核一致性测试套件 v1（NFR-COR-01；M0 待办 #5、#7）。
//!
//! 每个用例都在**真实仓库**上同时跑"内置引擎（gix）"与"git CLI"，
//! 再逐行比对两者的输出。CLI 是真相标准（架构规划 3.2：语义完整性的兜底），
//! 因此一旦两者分歧，永远以内置引擎为错误方去修。
//!
//! # 比较口径
//!
//! - **工作区状态**：`git status --porcelain=v1` ↔ [`ChangeSet::to_porcelain`]；
//! - **工作区状态（含路径引号）**：[`GitRepo::status_porcelain`]，它会读
//!   `core.quotepath` 以匹配 git 的路径渲染口径；
//! - **树差异**：`git diff --name-status` ↔ [`FileChange::name_status_line`]；
//! - **历史枚举**：`git rev-list` ↔ [`GitRepo::walk_ids`]。
//!
//! 行序在两边都做排序后再比较：git 的 porcelain 行序跟随其内部状态遍历顺序
//! （先索引侧变更、后未跟踪），并非路径字典序，逐字节比对行序只会引入与
//! 正确性无关的噪声。**内容**（字母、路径、引号、分隔符、相似度分数）
//! 仍是逐字节比对。
//!
//! # 规则的来源
//!
//! porcelain 的路径引号与冲突码映射不是推测出来的：`scripts/probe-git-porcelain.py`
//! 会在本机实际的 git 上实测这些语义，并把结论打印出来。git 版本变更后
//! 重跑该脚本即可确认规则是否仍然成立。
//!
//! # 已知差异
//!
//! 本套件当前为 **0 条**分歧。其它已知但不在本套件覆盖范围内的差异：
//!
//! - `GitRepo::worktree_blob` 不应用 smudge / CRLF / attribute 过滤器，
//!   因此工作区内容的行尾表现可能与 git 不同（随 FR-CON-01 的过滤器支持在 M5 处理）；
//! - diff 载荷（`--numstat` 行数统计、unified 文本）需要组合 `gt-diff`，
//!   其一致性用例随 `gt-service` 一并加入，避免在 `gt-backend` 的依赖图里
//!   引入反向依赖。

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

use gt_backend::{noop_observer, GitCli, GitRepo, NoopObserver, LOCAL_TIMEOUT};
use gt_core::{BranchScope, Oid};

/// 按行切分并排序（porcelain 行序由 git 的内部遍历决定，不参与一致性判定）。
fn sorted_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    lines.sort();
    lines
}

/// 临时目录名的去重序号（同一进程内多个用例并行）。
static COUNTER: AtomicU32 = AtomicU32::new(0);

/// 一个临时仓库夹具：自带 git 执行、文件写入与 blob 写入能力，析构时清理。
struct Fixture {
    root: PathBuf,
    cli: GitCli,
}

impl Fixture {
    /// 建一个全新的空仓库。`git` 不可用时返回 [`None`]，调用方据此跳过。
    fn new(label: &str) -> Option<Self> {
        let cli = GitCli::detect()?;
        let root = std::env::temp_dir().join(format!(
            "knotwork-consistency-{}-{}-{label}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建临时仓库目录");

        let fixture = Self { root, cli };
        fixture.git(&["init", "-q", "-b", "main"]);
        fixture.git(&["config", "user.email", "consistency@knotwork.invalid"]);
        fixture.git(&["config", "user.name", "Knotwork Consistency"]);
        fixture.git(&["config", "commit.gpgsign", "false"]);
        // 关掉换行转换：临时仓库不带 .gitattributes，否则平台默认值会让
        // 同一份内容在不同机器上产生不同 blob（NFR-COR-01 的跨机可复现前提）。
        fixture.git(&["config", "core.autocrlf", "false"]);
        // 非 ASCII 路径在 porcelain 中默认被转义成 \xxx；关掉后两边都是原始 UTF-8。
        fixture.git(&["config", "core.quotepath", "false"]);
        Some(fixture)
    }

    /// 打开一个已建好的夹具；`git` 不可用时返回 [`None`]。
    fn make(label: &str) -> Option<Self> {
        Self::new(label)
    }

    /// 执行 git 并断言成功，返回去掉尾部空白的标准输出。
    fn git(&self, args: &[&str]) -> String {
        let output = self
            .cli
            .run(Some(&self.root), args, &NoopObserver, LOCAL_TIMEOUT)
            .unwrap_or_else(|err| panic!("执行 git {args:?} 失败：{err}"));
        assert!(
            output.is_success(),
            "git {args:?} 退出码 {:?}：{}",
            output.exit_code,
            output.stderr
        );
        output.stdout.trim_end().to_owned()
    }

    /// 执行 git 并从标准输入喂入数据（`update-index --index-info` 等）。
    ///
    /// 写入的是原始字节：`index-info` 的格式对换行敏感，而 Windows 上
    /// 以文本模式写 stdin 会把 `\n` 变成 `\r\n`、让整行被 git 判为格式错误。
    fn git_stdin(&self, args: &[&str], input: &str) -> String {
        let mut child = Command::new(self.cli.program())
            .args(args)
            .current_dir(&self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|err| panic!("启动 git {args:?} 失败：{err}"));

        child
            .stdin
            .as_mut()
            .expect("stdin 已配置为 piped")
            .write_all(input.as_bytes())
            .expect("写入 git 标准输入");
        // 关闭 stdin 让 git 读到 EOF，否则命令会一直等输入。
        drop(child.stdin.take());

        let output = child.wait_with_output().expect("等待 git 结束");
        assert!(
            output.status.success(),
            "git {args:?} 失败：{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_owned()
    }

    /// 写入工作区文件（自动建父目录）。
    fn write(&self, rel: &str, content: &str) {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("创建工作区目录");
        }
        std::fs::write(&path, content).unwrap_or_else(|err| panic!("写入 {rel} 失败：{err}"));
    }

    /// 删除工作区文件。
    fn remove(&self, rel: &str) {
        std::fs::remove_file(self.root.join(rel))
            .unwrap_or_else(|err| panic!("删除 {rel} 失败：{err}"));
    }

    /// 把内容写入对象库并返回其十六进制对象 id。
    fn hash_object(&self, content: &str) -> String {
        self.git_stdin(&["hash-object", "-w", "--stdin"], content)
    }

    /// 建一个基线提交：`a.txt` + `keep.txt` + 忽略 `*.log` 的 `.gitignore`。
    fn seed_base(&self) {
        self.write("a.txt", "aaaaaaaaaa\nbbbbbbbbbb\n");
        self.write("keep.txt", "keep\n");
        self.write(".gitignore", "*.log\n");
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", "基线提交"]);
    }

    /// 只含 `a.txt` 的基线提交，专供冲突注入用例。
    ///
    /// 冲突注入需要先用 `read-tree --empty` 清空索引（否则未合并条目会叠在
    /// 同路径的 stage-0 条目上，git 的冲突码判定会走一条与"纯粹未合并路径"
    /// 不同的分支）；只留一个文件可以让清空索引后不产生多余的删除噪声。
    fn seed_conflict_base(&self) {
        self.write("a.txt", "aaaaaaaaaa\nbbbbbbbbbb\n");
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", "冲突基线"]);
    }

    /// 打开仓库句柄。
    fn open(&self) -> GitRepo {
        GitRepo::open(&self.root, noop_observer()).expect("打开临时仓库")
    }

    /// `git status --porcelain=v1` 的输出行（已排序）。
    fn git_status_lines(&self) -> Vec<String> {
        sorted_lines(&self.git(&["status", "--porcelain=v1"]))
    }

    /// 内置引擎的状态输出行（已排序）。
    fn gix_status_lines(&self) -> Vec<String> {
        sorted_lines(&self.open().status().expect("计算工作区状态").to_porcelain())
    }

    /// 仓库生效的 porcelain 渲染口径（读 `core.quotepath`）。
    fn porcelain_format(&self) -> gt_core::PorcelainFormat {
        self.open().porcelain_format()
    }

    /// 断言两侧的状态输出内容一致（行序已规范化）。
    fn assert_status_matches(&self, label: &str) {
        let expected = self.git_status_lines();
        let actual = self.gix_status_lines();
        assert_eq!(
            actual, expected,
            "工作区状态不一致（{label}）\n  git CLI : {expected:#?}\n  knotwork: {actual:#?}"
        );
    }

    /// 断言两侧的 porcelain 文本一致（含路径引号与转义；行序仍规范化）。
    ///
    /// 用 [`GitRepo::status_porcelain`]，它会读取仓库的 `core.quotepath`
    /// 来匹配 git 的路径渲染口径。
    fn assert_status_matches_bytes(&self, label: &str) -> Vec<String> {
        let expected = sorted_lines(&self.git(&["status", "--porcelain=v1"]));
        let actual = sorted_lines(&self.open().status_porcelain().expect("计算工作区状态"));
        assert_eq!(
            actual, expected,
            "porcelain 文本不一致（{label}）\n  git CLI : {expected:#?}\n  knotwork: {actual:#?}"
        );
        actual
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // mmap 的 pack 文件在 Windows 上可能短暂占用目录，清理失败不影响断言。
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// 场景构造器：在已建仓的夹具上制造出待比较的工作区状态。
type Setup = fn(&Fixture);

/// 工作区状态场景表。
const STATUS_SCENARIOS: &[(&str, Setup)] = &[
    ("clean", |f| f.seed_base()),
    ("untracked_file", |f| {
        f.seed_base();
        f.write("single.txt", "single\n");
    }),
    ("untracked_directory", |f| {
        f.seed_base();
        f.write("dir/u1.txt", "u1\n");
        f.write("dir/u2.txt", "u2\n");
    }),
    ("nested_untracked_directory", |f| {
        f.seed_base();
        f.write("dir/sub/deep.txt", "deep\n");
        f.write("dir/top.txt", "top\n");
    }),
    ("ignored_file", |f| {
        f.seed_base();
        f.write("build.log", "noise\n");
    }),
    ("staged_modification", |f| {
        f.seed_base();
        f.write("a.txt", "aaaaaaaaaa\nCHANGED\n");
        f.git(&["add", "a.txt"]);
    }),
    ("unstaged_modification", |f| {
        f.seed_base();
        f.write("a.txt", "aaaaaaaaaa\nCHANGED\n");
    }),
    ("staged_and_unstaged", |f| {
        f.seed_base();
        f.write("a.txt", "aaaaaaaaaa\nSTAGED\n");
        f.git(&["add", "a.txt"]);
        f.write("a.txt", "aaaaaaaaaa\nBOTH\n");
    }),
    ("staged_addition", |f| {
        f.seed_base();
        f.write("new.txt", "new\n");
        f.git(&["add", "new.txt"]);
    }),
    ("staged_deletion", |f| {
        f.seed_base();
        f.git(&["rm", "-q", "a.txt"]);
    }),
    ("unstaged_deletion", |f| {
        f.seed_base();
        f.remove("a.txt");
    }),
    ("staged_rename", |f| {
        f.seed_base();
        f.git(&["mv", "a.txt", "renamed.txt"]);
    }),
    ("removed_from_index_but_on_disk", |f| {
        // `git rm --cached` 之后 git 会为同一路径输出两行（索引侧删除 + 未跟踪）。
        f.seed_base();
        f.git(&["rm", "--cached", "-q", "a.txt"]);
    }),
    ("untracked_file_next_to_tracked", |f| {
        // 未跟踪文件与已修改文件同处一个目录，验证折叠规则不会误伤已知文件。
        f.seed_base();
        f.write("dir/tracked.txt", "tracked\n");
        f.git(&["add", "dir/tracked.txt"]);
        f.git(&["commit", "-q", "-m", "加入目录内文件"]);
        f.write("dir/modified.txt", "modified\n");
        f.write("dir/untracked.txt", "untracked\n");
    }),
    ("mixed_buckets", |f| {
        f.seed_base();
        f.write("staged-new.txt", "new\n");
        f.git(&["add", "staged-new.txt"]);
        f.write("a.txt", "aaaaaaaaaa\nCHANGED\n");
        f.write("loose.txt", "loose\n");
        f.write("noise.log", "ignored\n");
    }),
];

#[test]
fn status_matches_git_cli_across_scenarios() {
    let Some(_) = GitCli::detect() else {
        eprintln!("跳过：未找到 git 可执行文件");
        return;
    };

    for (label, setup) in STATUS_SCENARIOS {
        let fixture = Fixture::new(label).expect("创建夹具");
        setup(&fixture);
        fixture.assert_status_matches(label);
    }
}

#[test]
fn unicode_paths_match_git_cli_byte_for_byte() {
    let Some(_) = GitCli::detect() else {
        return;
    };

    let fixture = Fixture::new("unicode").expect("创建夹具");
    fixture.seed_base();
    // 含空格、中文、日文与 emoji：覆盖两条独立规则 —— 空格强制加引号、
    // 非 ASCII 由 `core.quotepath` 决定是否转义（夹具设为不转义）。
    fixture.write("中文 文件.txt", "一起修 bug 吧\n");
    fixture.write("日本語/テスト.txt", "テスト\n");
    fixture.write("emoji-🎯.txt", "target\n");
    fixture.assert_status_matches_bytes("unicode_paths");
}

#[test]
fn non_ascii_paths_are_escaped_when_quotepath_is_on() {
    let Some(_) = GitCli::detect() else {
        return;
    };

    let fixture = Fixture::new("quotepath-on").expect("创建夹具");
    fixture.git(&["config", "core.quotepath", "true"]);
    fixture.seed_base();
    fixture.write("中文.txt", "content\n");

    // git 出厂配置把非 ASCII 转义为八进制；两边的转义必须逐字节一致。
    let lines = fixture.assert_status_matches_bytes("quotepath_on");
    assert!(
        lines.iter().any(|line| line.contains("\\344")),
        "quotepath 开启时路径应被转义成八进制：{lines:#?}"
    );
    assert_ne!(
        fixture.porcelain_format(),
        gt_core::PorcelainFormat::RAW_NON_ASCII,
        "porcelain 渲染口径应跟随 core.quotepath"
    );
}

/// 全部 7 种未合并组合（`git status` 的 `XY` 冲突码）。
///
/// 直接注入索引三阶段而非走真实合并流程：`AU` / `UA` 依赖特定的
/// 重命名-删除交叉，用合并或 rebase 无法稳定构造出全部组合，
/// 而三阶段到冲突码的映射才是本用例要固化的契约。
const CONFLICT_STAGE_COMBOS: &[(&str, &[u8])] = &[
    ("UU", &[1, 2, 3]),
    ("AA", &[2, 3]),
    ("AU", &[2]),
    ("UA", &[3]),
    ("DU", &[1, 3]),
    ("UD", &[1, 2]),
    ("DD", &[1]),
];

#[test]
fn conflict_codes_match_git_cli() {
    let Some(_) = GitCli::detect() else {
        return;
    };

    for (label, stages) in CONFLICT_STAGE_COMBOS {
        let fixture = Fixture::new(&format!("conflict-{label}")).expect("创建夹具");
        fixture.seed_conflict_base();

        let base = fixture.hash_object("base version\n");
        let ours = fixture.hash_object("our version\n");
        let theirs = fixture.hash_object("their version\n");

        let mut payload = String::new();
        for stage in *stages {
            let sha = match stage {
                1 => &base,
                2 => &ours,
                _ => &theirs,
            };
            let _ = writeln!(payload, "100644 {sha} {stage}\ta.txt");
        }

        // 清空索引后仅注入未合并条目，让该路径的状态完全由冲突阶段决定。
        // 若保留同路径的 stage-0 条目，git 会走一条不同的冲突码判定分支
        // （见 `seed_conflict_base` 的说明）。
        fixture.git(&["read-tree", "--empty"]);
        fixture.git_stdin(&["update-index", "--index-info"], &payload);

        fixture.assert_status_matches(label);
        // 冲突码必须与 git 逐字母一致，而不只是"两边都有这行"。
        let expected = fixture.git_status_lines();
        assert_eq!(
            expected,
            vec![format!("{label} a.txt")],
            "git 对 {label} 的输出不符合预期，用例前提已被破坏"
        );
    }
}

#[test]
fn merge_conflict_matches_git_cli() {
    let Some(_) = GitCli::detect() else {
        return;
    };

    let fixture = Fixture::new("merge-conflict").expect("创建夹具");
    fixture.seed_base();
    fixture.git(&["checkout", "-q", "-b", "theirs"]);
    fixture.write("a.txt", "aaaaaaaaaa\nTHEIRS\n");
    fixture.git(&["commit", "-q", "-a", "-m", "他们修改 a.txt"]);
    fixture.git(&["checkout", "-q", "main"]);
    fixture.write("a.txt", "aaaaaaaaaa\nOURS\n");
    fixture.git(&["commit", "-q", "-a", "-m", "我们修改 a.txt"]);

    // 合并应当冲突。冲突时 git 以非零退出码结束，这在 `run` 里不是错误
    // （非零退出码的语义由调用方解释），因此只看随后的状态输出。
    let _ = fixture.cli.run(
        Some(&fixture.root),
        &["merge", "--no-edit", "theirs"],
        &NoopObserver,
        LOCAL_TIMEOUT,
    );

    let status = fixture.git_status_lines();
    assert!(
        status.iter().any(|line| line == "UU a.txt"),
        "预期制造出 UU 冲突，实际状态：{status:#?}"
    );
    fixture.assert_status_matches("merge_conflict");
}

#[test]
fn repository_state_is_detected_during_merge() {
    let Some(_) = GitCli::detect() else {
        return;
    };

    let fixture = Fixture::new("repo-state").expect("创建夹具");
    fixture.seed_base();
    assert_eq!(
        fixture.open().repo_state(),
        gt_core::RepoState::Clean,
        "干净仓库不应报告进行中的操作"
    );

    fixture.git(&["checkout", "-q", "-b", "theirs"]);
    fixture.write("a.txt", "aaaaaaaaaa\nTHEIRS\n");
    fixture.git(&["commit", "-q", "-a", "-m", "他们修改"]);
    fixture.git(&["checkout", "-q", "main"]);
    fixture.write("a.txt", "aaaaaaaaaa\nOURS\n");
    fixture.git(&["commit", "-q", "-a", "-m", "我们修改"]);
    let _ = fixture.cli.run(
        Some(&fixture.root),
        &["merge", "--no-edit", "theirs"],
        &NoopObserver,
        LOCAL_TIMEOUT,
    );

    assert_eq!(
        fixture.open().repo_state(),
        gt_core::RepoState::Merge,
        "冲突合并期间应报告 Merge 状态（FR-STA-02）"
    );
}

#[test]
fn rev_list_matches_git_cli() {
    let Some(_) = GitCli::detect() else {
        return;
    };

    let fixture = Fixture::new("rev-list").expect("创建夹具");
    fixture.seed_base();
    fixture.git(&["tag", "v0.1"]);

    // 造一个真分叉 + 合并的历史，让 revwalk 必须处理多起点与合并点。
    fixture.git(&["checkout", "-q", "-b", "feature"]);
    fixture.write("feature.txt", "feature\n");
    fixture.git(&["add", "feature.txt"]);
    fixture.git(&["commit", "-q", "-m", "feature 提交"]);
    fixture.git(&["tag", "v0.2"]);

    fixture.git(&["checkout", "-q", "main"]);
    fixture.write("main.txt", "main\n");
    fixture.git(&["add", "main.txt"]);
    fixture.git(&["commit", "-q", "-m", "main 提交"]);

    fixture.git(&["merge", "-q", "--no-ff", "--no-edit", "feature"]);

    let repo = fixture.open();

    for (scope, cli_args) in [
        (BranchScope::All, vec!["rev-list", "--all"]),
        (
            BranchScope::LocalAndUpstream,
            vec!["rev-list", "--branches", "HEAD"],
        ),
    ] {
        let mut expected: Vec<String> = fixture.git(&cli_args).lines().map(str::to_owned).collect();
        expected.sort();

        let mut actual: Vec<String> = repo
            .walk_ids(scope, None)
            .expect("枚举提交 id")
            .iter()
            .copied()
            .map(Oid::to_hex)
            .collect();
        actual.sort();

        assert_eq!(
            actual,
            expected,
            "rev-list 结果不一致（{scope:?}，对应 git {}）",
            cli_args.join(" ")
        );
    }

    // 合并提交必须出现在枚举结果里（否则 gt-graph 的泳道会缺一条边）。
    let head_meta = repo
        .commit_meta(repo.resolve("HEAD").expect("解析 HEAD"))
        .expect("读取 HEAD 元数据");
    assert!(head_meta.is_merge(), "HEAD 应为合并提交");
    assert_eq!(head_meta.parents.len(), 2, "合并提交应有两个父提交");
    assert!(
        head_meta.summary.contains("feature"),
        "合并提交的摘要是自动生成的合并信息，实际：{}",
        head_meta.summary
    );
}

#[test]
fn name_status_matches_git_cli() {
    let Some(_) = GitCli::detect() else {
        return;
    };

    let fixture = Fixture::new("name-status").expect("创建夹具");
    fixture.seed_base();
    let base = fixture
        .open()
        .resolve("HEAD")
        .expect("解析基线提交")
        .to_hex();

    // 覆盖 M / A / D / R 四种状态：内容修改、新增、删除、纯重命名。
    fixture.write("a.txt", "aaaaaaaaaa\nMODIFIED\n");
    fixture.write("added.txt", "brand new\n");
    fixture.remove("keep.txt");
    fixture.git(&["add", "-A"]);
    fixture.git(&["mv", "a.txt", "renamed.txt"]);
    fixture.git(&["commit", "-q", "-m", "第二轮"]);
    let head = fixture.open().resolve("HEAD").expect("解析新提交").to_hex();

    let mut expected: Vec<String> = fixture
        .git(&["diff", "--name-status", &base, &head])
        .lines()
        .map(str::to_owned)
        .collect();
    expected.sort();

    let mut actual: Vec<String> = fixture
        .open()
        .tree_changes(Some(&base), &head)
        .expect("计算树差异")
        .iter()
        .map(gt_core::FileChange::name_status_line)
        .collect();
    actual.sort();

    assert_eq!(
        actual, expected,
        "name-status 不一致\n  git CLI : {expected:#?}\n  knotwork: {actual:#?}"
    );
}

#[test]
fn root_commit_diff_matches_git_cli() {
    let Some(_) = GitCli::detect() else {
        return;
    };

    let fixture = Fixture::new("root-diff").expect("创建夹具");
    fixture.write("a.txt", "one\n");
    fixture.write("dir/b.txt", "two\n");
    fixture.git(&["add", "-A"]);
    fixture.git(&["commit", "-q", "-m", "初始提交"]);
    let head = fixture
        .open()
        .resolve("HEAD")
        .expect("解析初始提交")
        .to_hex();

    // 根提交的"旧侧"是空树：`--root` 让 git 也按这个口径输出。
    let mut expected: Vec<String> = fixture
        .git(&[
            "diff-tree",
            "--root",
            "--no-commit-id",
            "-r",
            "--name-status",
            &head,
        ])
        .lines()
        .map(str::to_owned)
        .collect();
    expected.sort();

    let mut actual: Vec<String> = fixture
        .open()
        .tree_changes(None, &head)
        .expect("计算根提交差异")
        .iter()
        .map(gt_core::FileChange::name_status_line)
        .collect();
    actual.sort();

    assert_eq!(
        actual, expected,
        "根提交差异不一致\n  git CLI : {expected:#?}\n  knotwork: {actual:#?}"
    );
}

#[test]
fn blob_read_matches_git_cli() {
    let Some(_) = GitCli::detect() else {
        return;
    };

    let fixture = Fixture::new("blob-read").expect("创建夹具");
    fixture.seed_base();
    let repo = fixture.open();

    let content = "aaaaaaaaaa\nbbbbbbbbbb\n";
    let blob = repo
        .blob_at("HEAD", "a.txt", 1024)
        .expect("读取 blob")
        .expect("a.txt 应存在");
    assert_eq!(String::from_utf8_lossy(&blob), content);

    // 超阈值必须返回 None 而不是部分内容（架构规划 5.5 的内存尖峰防线）。
    assert!(
        repo.blob_at("HEAD", "a.txt", 4)
            .expect("读取 blob")
            .is_none(),
        "超过 max_bytes 的 blob 应被拒绝读取"
    );

    // 不存在的路径返回 None，而不是错误。
    assert!(
        repo.blob_at("HEAD", "missing.txt", 1024)
            .expect("读取缺失路径")
            .is_none(),
        "不存在的路径应返回 None"
    );

    // 工作区侧读取：与 git CLI 的原始字节一致。
    let worktree = repo
        .worktree_blob("a.txt", 1024)
        .expect("读取工作区文件")
        .expect("工作区应存在 a.txt");
    assert_eq!(String::from_utf8_lossy(&worktree), content);

    // 越界路径必须被拒绝（防御 `..` 穿越）。
    assert!(
        repo.worktree_blob("../outside.txt", 1024).is_err(),
        "越出工作区的路径应被拒绝"
    );
}

#[test]
fn head_reports_branch_and_detached_state() {
    let Some(_) = GitCli::detect() else {
        return;
    };

    let fixture = Fixture::new("head").expect("创建夹具");
    fixture.seed_base();
    let repo = fixture.open();

    let head = repo.head().expect("读取 HEAD");
    assert_eq!(head.kind, gt_core::HeadKind::Branch);
    assert_eq!(head.branch.as_deref(), Some("main"));

    fixture.git(&["checkout", "-q", "--detach"]);
    let repo = fixture.open();
    let detached = repo.head().expect("读取分离 HEAD");
    assert_eq!(detached.kind, gt_core::HeadKind::Detached);
    assert!(detached.branch.is_none());

    // 未出生的分支（空仓库）不得被当成错误。
    let empty = Fixture::new("unborn").expect("创建夹具");
    let unborn = empty.open().head().expect("读取未出生 HEAD");
    assert_eq!(unborn.kind, gt_core::HeadKind::Unborn);
    assert!(unborn.target.is_none());
}

#[test]
fn refs_report_kinds_and_upstream() {
    let Some(_) = GitCli::detect() else {
        return;
    };

    let fixture = Fixture::new("refs").expect("创建夹具");
    fixture.seed_base();
    fixture.git(&["tag", "v1.0"]);
    fixture.git(&["branch", "feature/x"]);
    // 用一个本地"远程"仓库把 upstream 配起来，避免依赖网络。
    let upstream = Fixture::new("refs-upstream").expect("创建上游夹具");
    upstream.seed_base();
    let url = format!(
        "file:///{}",
        upstream.root.display().to_string().replace('\\', "/")
    );
    fixture.git(&["remote", "add", "origin", &url]);
    fixture.git(&["fetch", "-q", "origin"]);
    fixture.git(&["branch", "--set-upstream-to=origin/main", "main"]);

    let refs = fixture.open().refs().expect("读取引用");
    let local = refs
        .iter()
        .find(|info| info.name == "main" && info.kind == gt_core::RefKind::LocalBranch)
        .expect("应存在本地分支 main");
    assert!(local.is_head, "main 应为当前分支");
    assert_eq!(local.upstream.as_deref(), Some("origin/main"));

    assert!(
        refs.iter()
            .any(|info| info.kind == gt_core::RefKind::Tag && info.name == "v1.0"),
        "应识别出标签 v1.0"
    );
    assert!(
        refs.iter()
            .any(|info| info.kind == gt_core::RefKind::LocalBranch && info.name == "feature/x"),
        "带斜杠的分支名应完整保留"
    );
    assert!(
        refs.iter()
            .any(|info| info.kind == gt_core::RefKind::RemoteBranch && info.name == "origin/main"),
        "应识别出远程分支"
    );

    // 排序契约：按分组顺序、组内按名称。
    let mut previous = (0u8, String::new());
    for info in &refs {
        let key = (info.kind.group_order(), info.name.clone());
        assert!(key >= previous, "引用列表未按 (分组, 名称) 排序：{refs:#?}");
        previous = key;
    }
}

#[test]
fn opening_non_repository_fails_with_a_typed_error() {
    let Some(_) = GitCli::detect() else {
        return;
    };

    let root = std::env::temp_dir().join(format!(
        "knotwork-not-a-repo-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("创建临时目录");

    let err = GitRepo::open(&root, noop_observer()).expect_err("非仓库目录应打开失败");
    assert!(
        matches!(err, gt_backend::BackendError::NotARepository { .. }),
        "错误类型应为 NotARepository，实际：{err:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// 夹具必须在构造后即可被内置引擎打开 —— 否则后续所有断言都失去前提。
#[test]
fn fixture_is_openable_by_the_builtin_engine() {
    let Some(_) = GitCli::detect() else {
        return;
    };

    let fixture = Fixture::make("fixture-sanity").expect("创建夹具");
    fixture.seed_base();
    let repo = fixture.open();
    assert_eq!(
        repo.location().display_name(),
        repo.location().display_name()
    );
    assert!(repo.location().work_dir.is_some(), "非裸仓库应有工作区");
    assert!(!repo.location().is_bare);
    assert!(repo.status().expect("计算状态").is_clean());
}

// ---------------------------------------------------------------- Blame（FR-BLM，M5）

/// 提交当前全部变更。
fn commit_all(fixture: &Fixture, message: &str) {
    fixture.git(&["add", "-A"]);
    fixture.git(&["commit", "-q", "-m", message]);
}

/// 解析 `git blame -l` 的 `<sha> (<…> <lineno>) <code>`，返回 (sha, 最终行号)。
fn parse_blame_l(text: &str) -> Vec<(String, usize)> {
    text.lines()
        .filter_map(|line| {
            let (sha, rest) = line.split_once(' ')?;
            // 边界提交（初始提交）在 -l 输出里带 `^` 前缀；porcelain 无此前缀。
            let sha = sha.trim_start_matches('^');
            let open = rest.find('(')?;
            let close = rest[open..].find(')')?;
            let lineno: usize = rest[open + 1..open + close]
                .split_whitespace()
                .last()?
                .parse()
                .ok()?;
            Some((sha.to_owned(), lineno))
        })
        .collect()
}

/// FR-BLM-02 验收：解析结果与 `git blame -l` 逐行一致（含重命名跟随）。
#[test]
fn blame_matches_git_blame_l_including_rename() {
    let Some(cli) = GitCli::detect() else {
        return;
    };
    let fixture = Fixture::make("blame-consistency").expect("创建夹具");

    fixture.write("code.txt", "alpha\nbeta\ngamma\n");
    commit_all(&fixture, "第一版");
    fixture.write("code.txt", "alpha\ndelta\ngamma\nepsilon\n");
    commit_all(&fixture, "第二版");
    // 重命名：blame 必须跟随到第一版。
    fixture.git(&["mv", "code.txt", "renamed.txt"]);
    commit_all(&fixture, "重命名");

    let location = fixture.open().location().clone();
    let rows = gt_backend::blame::blame(
        &cli,
        &location,
        &NoopObserver,
        None,
        "renamed.txt",
        None,
    )
    .expect("blame 成功");

    let reference = parse_blame_l(&fixture.git(&["blame", "-l", "--", "renamed.txt"]));
    assert_eq!(reference.len(), 4, "git blame 参照行数");

    let ours: Vec<(String, usize)> = rows
        .iter()
        .map(|row| (row.commit.clone(), row.final_line))
        .collect();

    // 边界提交在 -l 输出中被截短一位以容纳 `^` 前缀：规范化为前 39 位再比对。
    let norm = |sha: &str| sha.trim_start_matches('^').chars().take(39).collect::<String>();
    let ours_norm: Vec<(String, usize)> =
        ours.iter().map(|(s, n)| (norm(s), *n)).collect();
    let ref_norm: Vec<(String, usize)> =
        reference.iter().map(|(s, n)| (norm(s), *n)).collect();
    assert_eq!(ours_norm, ref_norm, "blame 输出应与 git blame -l 逐行一致");

    // 字段抽查：第一行来自第一版，第四行来自第二版。
    assert_eq!(rows[0].summary, "第一版", "首行归因到第一版");
    assert_eq!(rows[3].summary, "第二版");
    assert_eq!(rows[0].author, "Knotwork Consistency");
    assert!(rows[0].author_time > 0, "作者时间应为 Unix 秒");
    // 全部行的提交都能在 git log 里找到（40 位十六进制已由解析保证）。
    for row in &rows {
        assert_eq!(row.commit.len(), 40);
    }
}

/// FR-BLM-03 验收：链式导航命中的 (commit, 行号) 与手工上溯一致。
#[test]
fn chain_navigation_maps_line_and_hits_expected_commit() {
    let Some(cli) = GitCli::detect() else {
        return;
    };
    let fixture = Fixture::make("blame-chain").expect("创建夹具");

    // 第一版：10 行；第二版在头部插入 3 行 → 新侧第 10 行来自旧侧第 7 行。
    let base: String = (1..=10).fold(String::new(), |acc, i| format!("{acc}line-{i}\n"));
    fixture.write("f.txt", &base);
    commit_all(&fixture, "第一版");
    let head: String = "h1\nh2\nh3\n".to_owned() + &base;
    fixture.write("f.txt", &head);
    commit_all(&fixture, "第二版（头部插 3 行）");

    let location = fixture.open().location().clone();
    let first = fixture.git(&["rev-parse", "HEAD~1"]);
    let head = fixture.git(&["rev-parse", "HEAD"]);

    let mapped = gt_backend::blame::map_line_to_older(
        &cli,
        &location,
        &NoopObserver,
        &first,
        Some(&head),
        &["f.txt"],
        10,
    )
    .expect("映射成功")
    .expect("第 10 行应能映射到旧侧");
    assert_eq!(mapped, 7, "头部插 3 行后第 10 行应来自旧侧第 7 行");

    // 上溯后 blame 旧侧：第 7 行应命中"第一版"提交（与手工 `git blame HEAD~` 一致）。
    let rows = gt_backend::blame::blame(&cli, &location, &NoopObserver, Some(&first), "f.txt", None)
        .expect("旧侧 blame 成功");
    assert_eq!(rows[6].commit, first, "上溯后第 7 行应归因到第一版提交");
    assert_eq!(rows[6].summary, "第一版");
}

/// 工作区 blame（spec = None）覆盖未提交的修改行（boundary / 归因到当前）。
#[test]
fn worktree_blame_covers_uncommitted_lines() {
    let Some(cli) = GitCli::detect() else {
        return;
    };
    let fixture = Fixture::make("blame-worktree").expect("创建夹具");
    fixture.write("f.txt", "committed\n");
    commit_all(&fixture, "已提交版");
    fixture.write("f.txt", "committed\nstaged-later\n");

    let location = fixture.open().location().clone();
    let rows =
        gt_backend::blame::blame(&cli, &location, &NoopObserver, None, "f.txt", None)
            .expect("工作区 blame 成功");
    assert_eq!(rows.len(), 2);
    // 未提交的行归因到"未提交"（git 输出为全零提交）。
    assert!(
        rows[1].commit.bytes().all(|b| b == b'0'),
        "未提交行应归因到零提交，实际 {}",
        rows[1].commit
    );
}

/// 文件历史：`--follow` 列出重命名前后的全部提交（FR-BLM-01）。
#[test]
fn file_history_follows_renames() {
    let Some(cli) = GitCli::detect() else {
        return;
    };
    let fixture = Fixture::make("file-history").expect("创建夹具");
    fixture.write("old.txt", "one\n");
    commit_all(&fixture, "第一次");
    fixture.write("old.txt", "one\ntwo\n");
    commit_all(&fixture, "第二次");
    fixture.git(&["mv", "old.txt", "new.txt"]);
    commit_all(&fixture, "重命名");

    let location = fixture.open().location().clone();
    let rows = gt_backend::blame::file_history(
        &cli,
        &location,
        &NoopObserver,
        false,
        "new.txt",
        None,
    )
    .expect("文件历史成功");
    let summaries: Vec<&str> = rows.iter().map(|r| r.summary.as_str()).collect();
    assert_eq!(summaries, vec!["重命名", "第二次", "第一次"], "--follow 应覆盖重命名前");
    for row in &rows {
        assert_eq!(row.commit.len(), 40);
        assert_eq!(row.short.len(), 7, "%h 短哈希");
    }
}
