//! 操作等价测试套件（需求文档第 9 节第 2 条，L3 层）。
//!
//! # 这一层测什么
//!
//! L2（`gt-backend/tests/consistency.rs`）比的是**读**：我们的输出 == `git` 的输出。
//! 本套件比的是**写**：经由写路径（界面写操作的实际执行者）改出来的仓库，
//! 与"手工在命令行敲等价命令"改出来的仓库，是否**可观察状态完全一致**。
//!
//! 做法：建一个仓库 A，逐字节复制成仓库 B；A 走 `RepoWriter` / `gt-diff` 补丁，
//! B 走 `git` 命令；然后比较两边 `git status --porcelain` / `git diff --cached` /
//! `git log` / 文件内容。**比较的是 git 自己的输出**，因此不会出现"用我们的实现
//! 验证我们的实现"这种自证。
//!
//! # 覆盖范围（M2）
//!
//! | 用例 | 需求 |
//! |---|---|
//! | 文件级暂存 / 反暂存 | FR-LC-02 |
//! | hunk 级暂存（含行级选择） | FR-LC-03、FR-DIF-05 |
//! | 丢弃修改 / 丢弃所选 hunk | FR-LC-02 |
//! | 提交与修订（含多行消息） | FR-CMT-01/02 |
//! | 忽略规则 | FR-LC-04 |
//! | 分支新建 / 重命名 / 删除 / 检出 | FR-BR-01/02 |

use std::path::{Path, PathBuf};
use std::process::Command;

use gt_backend::{
    noop_observer, BranchOp, CommitRequest, GitCli, GitRepo, IgnoreTarget, RepoWriter,
    LOCAL_TIMEOUT,
};
use gt_diff::{build_patch, diff_bytes, DiffOptions, HunkSelection, PatchPaths};

/// 一个测试用仓库对：`ours` 由我们操作，`theirs` 由 git 命令操作。
struct Pair {
    root: PathBuf,
    ours: PathBuf,
    theirs: PathBuf,
    cli: GitCli,
}

impl Pair {
    /// 建一对内容完全相同的仓库。
    fn new(label: &str) -> Option<Self> {
        let cli = GitCli::detect()?;
        let root =
            std::env::temp_dir().join(format!("knotwork-equiv-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let ours = root.join("ours");
        std::fs::create_dir_all(&ours).expect("建仓库目录");

        let git = |dir: &Path, args: &[&str]| {
            let output = Command::new(cli.program())
                .args(args)
                .current_dir(dir)
                .output()
                .expect("执行 git");
            assert!(
                output.status.success(),
                "git {args:?} 失败：{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).into_owned()
        };

        git(&ours, &["init", "-q", "-b", "main"]);
        git(&ours, &["config", "user.email", "equiv@knotwork.invalid"]);
        git(&ours, &["config", "user.name", "Knotwork Equiv"]);
        git(&ours, &["config", "core.autocrlf", "false"]);

        std::fs::write(ours.join("a.txt"), baseline()).expect("写基线文件");
        std::fs::write(ours.join("b.txt"), "beta\n").expect("写基线文件");
        git(&ours, &["add", "-A"]);
        git(&ours, &["commit", "-q", "-m", "基线提交"]);

        let theirs = root.join("theirs");
        copy_dir(&ours, &theirs).expect("复制仓库");

        Some(Self {
            root,
            ours,
            theirs,
            cli,
        })
    }

    /// 在 `ours` 上执行一次写操作（走 `RepoWriter`）。
    fn writer(&self) -> RepoWriter {
        let repo = GitRepo::open(&self.ours, noop_observer()).expect("打开仓库");
        RepoWriter::new(repo.location(), self.cli.clone(), repo.observer())
    }

    /// 打开 `ours` 的 gix 句柄（用于取索引内容）。
    fn repo(&self) -> GitRepo {
        GitRepo::open(&self.ours, noop_observer()).expect("打开仓库")
    }

    /// 在 `theirs` 上跑 git 命令。
    fn git(&self, args: &[&str]) {
        let output = self
            .cli
            .run(
                Some(&self.theirs),
                args,
                &gt_backend::NoopObserver,
                LOCAL_TIMEOUT,
            )
            .expect("执行 git");
        assert!(output.is_success(), "git {args:?} 失败：{}", output.stderr);
    }

    /// 断言两边输出一致（失败时带上双方输出，便于定位）。
    fn assert_same(&self, args: &[&str]) {
        if let Err(detail) = self.compare(args) {
            panic!("{detail}");
        }
    }

    /// 两边在同一个命令下的输出必须一致。
    fn compare(&self, args: &[&str]) -> Result<(), String> {
        let ours = run_git_raw(&self.cli, &self.ours, args);
        let theirs = run_git_raw(&self.cli, &self.theirs, args);
        if ours == theirs {
            Ok(())
        } else {
            Err(format!(
                "`git {}` 输出不一致\n--- ours ---\n{ours}\n--- theirs ---\n{theirs}",
                args.join(" ")
            ))
        }
    }

    /// "索引 vs 工作区"的差异（与界面展示的完全同源）。
    fn worktree_diff(&self) -> gt_diff::FileDiff {
        let repo = self.repo();
        let old = repo
            .index_blob("a.txt", 1 << 20)
            .expect("读索引")
            .expect("索引中应有该文件");
        let new = std::fs::read(self.ours.join("a.txt")).expect("读工作区");
        diff_bytes(
            "a.txt",
            "a.txt",
            Some(&old),
            Some(&new),
            &DiffOptions::default(),
        )
    }

    fn cleanup(self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// 20 行基线内容（足够让"相隔较远的两处修改"落进两个 hunk）。
fn baseline() -> String {
    numbered(1..=20, |_| None)
}

/// 生成 20 行内容，`replace(n)` 返回 `Some(text)` 时替换第 n 行。
fn numbered(
    range: std::ops::RangeInclusive<usize>,
    replace: impl Fn(usize) -> Option<String>,
) -> String {
    range.fold(String::new(), |mut acc, n| {
        use std::fmt::Write as _;
        match replace(n) {
            Some(text) => {
                let _ = writeln!(acc, "{text}");
            }
            None => {
                let _ = writeln!(acc, "line{n}");
            }
        }
        acc
    })
}

/// 第 2 行与第 18 行各改一处 → 两个独立 hunk。
fn two_hunks_modified() -> String {
    numbered(1..=20, |n| match n {
        2 => Some("CHANGED2".to_owned()),
        18 => Some("CHANGED18".to_owned()),
        _ => None,
    })
}

fn run_git_raw(cli: &GitCli, dir: &Path, args: &[&str]) -> String {
    let output = cli
        .run(Some(dir), args, &gt_backend::NoopObserver, LOCAL_TIMEOUT)
        .expect("执行 git");
    // 统一行尾再比较：Windows 上的 git 输出可能带 CRLF。
    format!("{}{}", output.stdout, output.stderr).replace("\r\n", "\n")
}

/// 递归复制目录（测试目录很小，够用即可）。
fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[test]
fn file_staging_matches_the_cli() {
    let Some(pair) = Pair::new("stage") else {
        eprintln!("未检测到 git，跳过");
        return;
    };
    for dir in [&pair.ours, &pair.theirs] {
        std::fs::write(dir.join("a.txt"), "one\nCHANGED\n").expect("改文件");
        std::fs::write(dir.join("new.txt"), "brand new\n").expect("新文件");
    }

    pair.writer()
        .stage_paths(&["a.txt".to_owned(), "new.txt".to_owned()])
        .expect("暂存");
    pair.git(&["add", "-A", "--", "a.txt", "new.txt"]);

    pair.assert_same(&["status", "--porcelain=v1"]);
    pair.assert_same(&["diff", "--cached", "--name-status"]);
    pair.assert_same(&["diff", "--cached", "-U3"]);

    // 反暂存也必须等价。
    pair.writer()
        .unstage_paths(&["a.txt".to_owned(), "new.txt".to_owned()])
        .expect("反暂存");
    pair.git(&["restore", "--staged", "--", "a.txt", "new.txt"]);
    pair.assert_same(&["status", "--porcelain=v1"]);

    pair.cleanup();
}

#[test]
fn hunk_staging_matches_git_add_p() {
    let Some(pair) = Pair::new("hunk") else {
        eprintln!("未检测到 git，跳过");
        return;
    };
    for dir in [&pair.ours, &pair.theirs] {
        std::fs::write(dir.join("a.txt"), two_hunks_modified()).expect("改文件");
    }

    let diff = pair.worktree_diff();
    assert!(
        diff.payload.hunks().is_some_and(|hunks| hunks.len() >= 2),
        "应有两个独立 hunk"
    );
    let patch = build_patch(&diff, PatchPaths::same("a.txt"), &[HunkSelection::whole(0)])
        .expect("应生成补丁");
    pair.writer()
        .apply_patch_cached(&patch, false)
        .expect("应用补丁到索引");

    // 对照：给 theirs 一份"只有第一处改动"的内容并整文件暂存。
    // 这与 hunk 级暂存的可观察结果相同（索引内容一致），正是本用例要证明的等价性。
    std::fs::write(
        pair.theirs.join("a.txt"),
        numbered(1..=20, |n| (n == 2).then(|| "CHANGED2".to_owned())),
    )
    .expect("写对照内容");
    pair.git(&["add", "--", "a.txt"]);

    pair.assert_same(&["diff", "--cached", "-U0"]);
    // 工作区仍保留两处改动（hunk 暂存不碰工作区）。
    let worktree = std::fs::read_to_string(pair.ours.join("a.txt")).expect("读工作区");
    assert!(
        worktree.contains("CHANGED2") && worktree.contains("CHANGED18"),
        "暂存 hunk 不应改动工作区"
    );

    pair.cleanup();
}

#[test]
fn line_level_staging_matches_a_hand_edited_index() {
    let Some(pair) = Pair::new("line") else {
        eprintln!("未检测到 git，跳过");
        return;
    };
    // 相邻两行新增（落进同一个 hunk），只暂存其中第一行。
    let worktree = numbered(1..=20, |n| match n {
        4 => Some("A_INSERTED\nB_INSERTED".to_owned()),
        _ => None,
    });
    for dir in [&pair.ours, &pair.theirs] {
        std::fs::write(dir.join("a.txt"), &worktree).expect("改文件");
    }

    let diff = pair.worktree_diff();
    let hunks = diff.payload.hunks().expect("应有 hunk");
    assert_eq!(hunks.len(), 1, "相邻改动应合成一个 hunk");

    // 动态定位 `+A_INSERTED` 在 hunk 内的下标：写死下标会让用例依赖
    // diff 算法的对齐细节，换个算法实现就红。
    let target = hunks[0]
        .lines
        .iter()
        .position(|line| line.kind == gt_diff::LineKind::Added && line.text == "A_INSERTED")
        .expect("hunk 内应有 A_INSERTED 新增行");

    let patch = build_patch(
        &diff,
        PatchPaths::same("a.txt"),
        &[HunkSelection::lines(0, vec![target])],
    )
    .expect("应生成补丁");
    pair.writer()
        .apply_patch_cached(&patch, false)
        .expect("应用补丁");

    // 对照：索引里只在第 4 行之后多一行 A_INSERTED。
    // 注意是"插入"而不是"替换" —— 未选中的 -line4 在补丁里降级为上下文，
    // 因此它仍然留在索引里（这正是行级暂存的语义）。
    let mut expected: Vec<String> = baseline().lines().map(str::to_owned).collect();
    expected.insert(4, "A_INSERTED".to_owned());
    std::fs::write(
        pair.theirs.join("a.txt"),
        format!("{}\n", expected.join("\n")),
    )
    .expect("写对照");
    pair.git(&["add", "--", "a.txt"]);
    // 工作区恢复成"两行都还在"的原始状态：这样 status 的 MM/M 也就可比了
    // （我们的实现本来就只改索引，不动工作区）。
    std::fs::write(pair.theirs.join("a.txt"), &worktree).expect("恢复对照工作区");

    pair.assert_same(&["diff", "--cached", "-U0"]);
    pair.assert_same(&["status", "--porcelain=v1"]);
    pair.cleanup();
}

#[test]
fn discard_matches_git_restore() {
    let Some(pair) = Pair::new("discard") else {
        eprintln!("未检测到 git，跳过");
        return;
    };
    for dir in [&pair.ours, &pair.theirs] {
        std::fs::write(dir.join("a.txt"), "garbage everywhere\n").expect("改文件");
    }

    pair.writer()
        .discard_tracked(&["a.txt".to_owned()])
        .expect("丢弃");
    pair.git(&["restore", "--worktree", "--", "a.txt"]);

    assert_eq!(
        std::fs::read_to_string(pair.ours.join("a.txt")).expect("读 ours"),
        std::fs::read_to_string(pair.theirs.join("a.txt")).expect("读 theirs")
    );
    pair.assert_same(&["status", "--porcelain=v1"]);

    pair.cleanup();
}

#[test]
fn commit_matches_the_cli_tree_and_message() {
    let Some(pair) = Pair::new("commit") else {
        eprintln!("未检测到 git，跳过");
        return;
    };
    for dir in [&pair.ours, &pair.theirs] {
        std::fs::write(dir.join("a.txt"), "one\ncommitted by both\n").expect("改文件");
    }

    let message = "feat: 双方都应写出同样的消息\n\n第二段正文";
    pair.writer()
        .stage_paths(&["a.txt".to_owned()])
        .expect("暂存");
    pair.git(&["add", "-A"]);
    pair.writer()
        .commit(&CommitRequest {
            message: message.to_owned(),
            ..CommitRequest::default()
        })
        .expect("提交");

    let theirs = pair
        .cli
        .run_with_input(
            Some(&pair.theirs),
            &["commit", "-q", "--file=-", "--cleanup=strip"],
            message.as_bytes(),
            &gt_backend::NoopObserver,
            LOCAL_TIMEOUT,
        )
        .expect("对照提交");
    assert!(theirs.is_success(), "{}", theirs.stderr);

    // 提交时间不同，因此只比较"树内容、消息、作者"这三项可比较的部分。
    pair.assert_same(&["rev-parse", "HEAD^{tree}"]);
    pair.assert_same(&["log", "-1", "--format=%s%n%b"]);
    pair.assert_same(&["log", "-1", "--format=%an <%ae>"]);
    pair.assert_same(&["status", "--porcelain=v1"]);

    pair.cleanup();
}

#[test]
fn amend_matches_the_cli() {
    let Some(pair) = Pair::new("amend") else {
        eprintln!("未检测到 git，跳过");
        return;
    };
    let message = "chore: 改过的消息";
    pair.writer()
        .commit(&CommitRequest {
            message: message.to_owned(),
            amend: true,
            ..CommitRequest::default()
        })
        .expect("修订");

    let theirs = pair
        .cli
        .run_with_input(
            Some(&pair.theirs),
            &["commit", "--amend", "-q", "--file=-", "--cleanup=strip"],
            message.as_bytes(),
            &gt_backend::NoopObserver,
            LOCAL_TIMEOUT,
        )
        .expect("对照修订");
    assert!(theirs.is_success(), "{}", theirs.stderr);

    pair.assert_same(&["log", "-1", "--format=%s"]);
    pair.assert_same(&["rev-list", "--count", "HEAD"]);
    pair.cleanup();
}

#[test]
fn branch_operations_match_the_cli() {
    let Some(pair) = Pair::new("branch") else {
        eprintln!("未检测到 git，跳过");
        return;
    };

    pair.writer()
        .branch(&BranchOp::Create {
            name: "feature/x".to_owned(),
            start: None,
            checkout: true,
        })
        .expect("新建并检出");
    pair.git(&["switch", "-c", "feature/x"]);

    pair.writer()
        .branch(&BranchOp::Rename {
            from: "feature/x".to_owned(),
            to: "feature/y".to_owned(),
        })
        .expect("重命名");
    pair.git(&["branch", "-m", "feature/x", "feature/y"]);

    pair.assert_same(&["branch", "--format=%(refname:short)"]);
    pair.assert_same(&["rev-parse", "--abbrev-ref", "HEAD"]);

    pair.writer()
        .branch(&BranchOp::Checkout {
            name: "main".to_owned(),
        })
        .expect("检出 main");
    pair.git(&["switch", "main"]);

    pair.writer()
        .branch(&BranchOp::Delete {
            name: "feature/y".to_owned(),
            force: false,
        })
        .expect("删除已合并分支");
    pair.git(&["branch", "-d", "feature/y"]);

    pair.assert_same(&["branch", "--format=%(refname:short)"]);
    pair.cleanup();
}

#[test]
fn ignore_rules_match_the_cli() {
    let Some(pair) = Pair::new("ignore") else {
        eprintln!("未检测到 git，跳过");
        return;
    };
    for dir in [&pair.ours, &pair.theirs] {
        std::fs::write(dir.join("scratch.log"), "x\n").expect("写文件");
    }

    pair.writer()
        .append_ignore(IgnoreTarget::Gitignore, "scratch.log")
        .expect("写忽略");
    std::fs::write(pair.theirs.join(".gitignore"), "scratch.log\n").expect("写对照忽略");

    assert_eq!(
        std::fs::read_to_string(pair.ours.join(".gitignore")).expect("读 ours"),
        std::fs::read_to_string(pair.theirs.join(".gitignore")).expect("读 theirs")
    );
    // 忽略后该文件不再出现在 `git status` 的未跟踪分区，两边一致。
    pair.assert_same(&["status", "--porcelain=v1"]);
    pair.cleanup();
}

#[test]
fn discard_of_a_selected_hunk_matches_manual_editing() {
    let Some(pair) = Pair::new("discard-hunk") else {
        eprintln!("未检测到 git，跳过");
        return;
    };
    for dir in [&pair.ours, &pair.theirs] {
        std::fs::write(dir.join("a.txt"), two_hunks_modified()).expect("改文件");
    }

    let diff = pair.worktree_diff();
    let patch = build_patch(&diff, PatchPaths::same("a.txt"), &[HunkSelection::whole(0)])
        .expect("应生成补丁");
    pair.writer()
        .apply_patch_worktree(&patch)
        .expect("丢弃第一个 hunk");

    // 对照：手工把第一处改动改回去。
    std::fs::write(
        pair.theirs.join("a.txt"),
        numbered(1..=20, |n| (n == 18).then(|| "CHANGED18".to_owned())),
    )
    .expect("写对照");

    assert_eq!(
        std::fs::read_to_string(pair.ours.join("a.txt")).expect("读 ours"),
        std::fs::read_to_string(pair.theirs.join("a.txt")).expect("读 theirs"),
        "只应丢弃被选中的 hunk"
    );
    pair.assert_same(&["status", "--porcelain=v1"]);
    pair.cleanup();
}
