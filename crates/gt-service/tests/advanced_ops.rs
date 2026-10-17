//! 高级操作等价测试（需求文档第 9 节第 2 条，L3 层；M4 验收）。
//!
//! # 这一层测什么
//!
//! L2 比的是**读**（我们的输出 == `git` 的输出）；L3 比的是**写**：
//! 经由 M4 的高级操作改出来的仓库，与"手工在命令行敲等价命令"改出来的仓库，
//! 是否**可观察状态完全一致**。
//!
//! 做法：建仓库 A，逐字节复制成仓库 B；A 走 [`AdvancedOps`]，B 走 `git` 命令；
//! 然后比较 `git log --format=%s`、`HEAD^{tree}`、`git status --porcelain`、
//! 引用名列表。**比较的永远是 git 自己的输出**，不存在"用我们的实现验证我们的实现"。
//!
//! 为什么比 `HEAD^{tree}` 与提交消息而不是提交 id：提交 id 包含 committer 时间，
//! 两侧执行时刻不同就必然不同；tree + 消息 + 顺序一致已经蕴含"内容与结构一致"。
//!
//! # 覆盖范围
//!
//! | 用例 | 需求 |
//! |---|---|
//! | 合并（no-ff / 冲突清单） | FR-MRG-01 |
//! | 变基：冲突 → continue | FR-MRG-02 |
//! | 交互式变基：squash / drop / reword | FR-MRG-03 |
//! | 拣选 / 回滚 / 重置 | FR-MRG-04/05/06 |
//! | 贮藏 apply / pop | FR-STH |
//! | 补丁生成 → 应用 | FR-PTC |
//! | 标签增删 | FR-BR-03 |
//! | 三路冲突解决（服务层全链路） | FR-CFL-01/03 |
//! | 高级操作槽位的释放 | 架构 3.3 |

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use gt_backend::{
    noop_observer, AdvancedOps, CherryPickRequest, GitCli, GitRepo, MergeRequest, MergeStrategy,
    PatchApplyRequest, PatchSource, RebaseAction, RebaseControl, RebaseRequest, ResetMode,
    ResetRequest, RevertRequest, StashOp, TagOp,
};
use gt_service::{Delivery, GitService, Outcome, RequestKind, WriteRequest};

/// A/B 仓库对：`ours` 由 [`AdvancedOps`]（或服务层）操作，`theirs` 由 git 命令操作。
struct Pair {
    root: PathBuf,
    ours: PathBuf,
    theirs: PathBuf,
    cli: GitCli,
}

impl Pair {
    /// 建一个带基线提交的仓库对。
    fn new(label: &str) -> Option<Self> {
        let cli = GitCli::detect()?;
        let root =
            std::env::temp_dir().join(format!("knotwork-adv-equiv-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let ours = root.join("ours");
        std::fs::create_dir_all(&ours).expect("建仓库目录");

        git(&ours, &["init", "-q", "-b", "main"]);
        git(&ours, &["config", "user.email", "adv@knotwork.invalid"]);
        git(&ours, &["config", "user.name", "Knotwork Advanced Equiv"]);
        git(&ours, &["config", "core.autocrlf", "false"]);
        std::fs::write(ours.join("a.txt"), "one\ntwo\nthree\n").expect("写文件");
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

    /// 在 `ours` 上准备场景后，把它整体复制给 `theirs`，让两侧从同一状态出发。
    fn sync(&self) {
        let _ = std::fs::remove_dir_all(&self.theirs);
        copy_dir(&self.ours, &self.theirs).expect("同步仓库");
    }

    fn ops(&self) -> AdvancedOps {
        let repo = GitRepo::open(&self.ours, noop_observer()).expect("打开仓库");
        AdvancedOps::new(repo.location(), self.cli.clone(), repo.observer())
    }

    /// 断言两边在同一条 git 命令下的输出一致。
    fn assert_same(&self, args: &[&str]) {
        let ours = git_raw(&self.ours, args);
        let theirs = git_raw(&self.theirs, args);
        assert_eq!(
            ours,
            theirs,
            "`git {}` 不一致\n--- ours（我们的操作）---\n{ours}\n--- theirs（CLI）---\n{theirs}",
            args.join(" ")
        );
    }

    /// 断言操作后的可观察状态一致：内容树、提交消息序列、工作区状态、引用名。
    fn assert_equivalent(&self) {
        self.assert_same(&["rev-parse", "HEAD^{tree}"]);
        self.assert_same(&["log", "--format=%s"]);
        self.assert_same(&["status", "--porcelain"]);
        self.assert_same(&["for-each-ref", "--format=%(refname)"]);
    }

    fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("执行 git");
    assert!(
        output.status.success(),
        "git {args:?} 失败：{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// 允许失败的命令（冲突场景下 git 的退出码就是 1）。
fn git_raw(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("执行 git");
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .trim()
    .to_owned()
}

/// 带额外环境变量执行（`rebase --continue` 需要 `GIT_EDITOR=true` 才不会等编辑器）。
fn git_env(dir: &Path, args: &[&str], envs: &[(&str, &str)]) {
    let mut command = Command::new("git");
    command.args(args).current_dir(dir);
    for (key, value) in envs {
        command.env(key, value);
    }
    let output = command.output().expect("执行 git");
    assert!(
        output.status.success(),
        "git {args:?} 失败：{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// 用预置的待办列表跑一次交互式变基（CLI 侧的"手工等价操作"）。
fn cli_rebase_with_todo(dir: &Path, base: &str, todo: &str) {
    // 待办列表必须写在**仓库之外**：写进工作区会变成未跟踪文件，
    // 让 `git status --porcelain` 的两侧比较失真。
    let todo_path = dir.parent().expect("仓库应有父目录").join("cli-todo");
    std::fs::write(&todo_path, todo).expect("写待办列表");
    let editor = format!(
        "cp '{}'",
        todo_path.display().to_string().replace('\\', "/")
    );
    git_env(
        dir,
        &["rebase", "-i", "--no-autosquash", "-q", base],
        &[
            ("GIT_SEQUENCE_EDITOR", editor.as_str()),
            ("GIT_EDITOR", "true"),
        ],
    );
}

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

/// 造一个"feature 分支改了 a.txt，main 也改了 a.txt"的冲突场景。
fn setup_conflicting_branches(pair: &Pair) {
    git(&pair.ours, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(pair.ours.join("a.txt"), "feature 一侧\n").expect("写文件");
    git(&pair.ours, &["add", "-A"]);
    git(&pair.ours, &["commit", "-q", "-m", "feature 改 a.txt"]);
    git(&pair.ours, &["checkout", "-q", "main"]);
    std::fs::write(pair.ours.join("a.txt"), "main 一侧\n").expect("写文件");
    git(&pair.ours, &["add", "-A"]);
    git(&pair.ours, &["commit", "-q", "-m", "main 改 a.txt"]);
    git(&pair.ours, &["checkout", "-q", "feature"]);
}

// ---------------------------------------------------------------- 合并

#[test]
fn merge_no_ff_matches_the_cli() {
    let Some(pair) = Pair::new("merge") else {
        return;
    };
    git(&pair.ours, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(pair.ours.join("f.txt"), "feature\n").expect("写文件");
    git(&pair.ours, &["add", "-A"]);
    git(&pair.ours, &["commit", "-q", "-m", "feature 提交"]);
    git(&pair.ours, &["checkout", "-q", "main"]);
    pair.sync();

    pair.ops()
        .merge(&MergeRequest {
            rev: "feature".to_owned(),
            strategy: MergeStrategy::NoFastForward,
            message: Some("合并 feature".to_owned()),
        })
        .expect("我们的合并");

    git(
        &pair.theirs,
        &["merge", "--no-ff", "-m", "合并 feature", "feature"],
    );
    pair.assert_equivalent();
    pair.cleanup();
}

#[test]
fn merge_conflict_lists_the_same_files_as_the_cli() {
    let Some(pair) = Pair::new("merge-conflict") else {
        return;
    };
    setup_conflicting_branches(&pair);
    git(&pair.ours, &["checkout", "-q", "main"]);
    pair.sync();

    let outcome = pair
        .ops()
        .merge(&MergeRequest {
            rev: "feature".to_owned(),
            strategy: MergeStrategy::Default,
            message: None,
        })
        .expect("冲突不是 Err");
    assert!(outcome.is_conflicted());

    // CLI 侧的冲突文件清单。
    git_raw(&pair.theirs, &["merge", "feature"]);
    let theirs = git(&pair.theirs, &["diff", "--name-only", "--diff-filter=U"]);
    assert_eq!(
        outcome.conflicts.join("\n"),
        theirs,
        "冲突文件清单必须与 CLI 一致"
    );
    pair.assert_equivalent();
    pair.cleanup();
}

// ---------------------------------------------------------------- 变基

#[test]
fn rebase_conflict_then_continue_matches_the_cli() {
    let Some(pair) = Pair::new("rebase-continue") else {
        return;
    };
    setup_conflicting_branches(&pair);
    pair.sync();

    // A：我们的变基 → 冲突 → 写解决结果 → continue。
    let ops = pair.ops();
    let outcome = ops
        .rebase(&RebaseRequest {
            onto: "main".to_owned(),
        })
        .expect("冲突不是 Err");
    assert!(
        outcome.conflicts.contains(&"a.txt".to_owned()),
        "{outcome:?}"
    );
    std::fs::write(pair.ours.join("a.txt"), "resolved\n").expect("写解决结果");
    git(&pair.ours, &["add", "a.txt"]);
    ops.rebase_control(RebaseControl::Continue)
        .expect("继续变基");

    // B：手工等价操作。
    git_raw(&pair.theirs, &["rebase", "main"]);
    std::fs::write(pair.theirs.join("a.txt"), "resolved\n").expect("写解决结果");
    git(&pair.theirs, &["add", "a.txt"]);
    git_env(
        &pair.theirs,
        &["rebase", "--continue"],
        &[("GIT_EDITOR", "true")],
    );

    pair.assert_equivalent();
    pair.cleanup();
}

#[test]
fn interactive_rebase_squash_matches_a_cli_todo() {
    let Some(pair) = Pair::new("rebase-squash") else {
        return;
    };
    // 三个 WIP 提交，把后两个压缩进第一个。
    for index in 0..3 {
        std::fs::write(pair.ours.join(format!("w{index}.txt")), "x\n").expect("写文件");
        git(&pair.ours, &["add", "-A"]);
        git(&pair.ours, &["commit", "-q", "-m", &format!("WIP {index}")]);
    }
    let base = git(&pair.ours, &["rev-parse", "HEAD~2"]);
    pair.sync();

    let mut plan = pair
        .ops()
        .plan_from_base(&base, &base)
        .expect("生成变基计划");
    assert_eq!(plan.steps.len(), 2, "{plan:?}");
    plan.steps[1].action = RebaseAction::Squash;
    pair.ops()
        .interactive_rebase(&plan, None)
        .expect("交互式变基");

    let todo = format!(
        "pick {} WIP 1\nsquash {} WIP 2\n",
        plan.steps[0].short(),
        plan.steps[1].short()
    );
    cli_rebase_with_todo(&pair.theirs, &base, &todo);

    pair.assert_equivalent();
    pair.cleanup();
}

#[test]
fn interactive_rebase_drop_matches_a_cli_todo() {
    let Some(pair) = Pair::new("rebase-drop") else {
        return;
    };
    for index in 0..3 {
        std::fs::write(pair.ours.join(format!("d{index}.txt")), "x\n").expect("写文件");
        git(&pair.ours, &["add", "-A"]);
        git(
            &pair.ours,
            &["commit", "-q", "-m", &format!("待处理 {index}")],
        );
    }
    let base = git(&pair.ours, &["rev-parse", "HEAD~2"]);
    pair.sync();

    let mut plan = pair
        .ops()
        .plan_from_base(&base, &base)
        .expect("生成变基计划");
    plan.steps[0].action = RebaseAction::Drop;
    pair.ops()
        .interactive_rebase(&plan, None)
        .expect("交互式变基");

    let todo = format!(
        "drop {} 待处理 1\npick {} 待处理 2\n",
        plan.steps[0].short(),
        plan.steps[1].short()
    );
    cli_rebase_with_todo(&pair.theirs, &base, &todo);

    pair.assert_equivalent();
    pair.cleanup();
}

#[test]
fn interactive_rebase_reword_matches_a_manual_amend() {
    let Some(pair) = Pair::new("rebase-reword") else {
        return;
    };
    std::fs::write(pair.ours.join("r.txt"), "r\n").expect("写文件");
    git(&pair.ours, &["add", "-A"]);
    git(&pair.ours, &["commit", "-q", "-m", "旧消息"]);
    let base = git(&pair.ours, &["rev-parse", "HEAD~1"]);
    pair.sync();

    let mut plan = pair
        .ops()
        .plan_from_base(&base, &base)
        .expect("生成变基计划");
    plan.steps[0].action = RebaseAction::Reword;
    plan.steps[0].message = Some("新消息\n".to_owned());
    pair.ops()
        .interactive_rebase(&plan, None)
        .expect("交互式变基");

    // CLI 等价：pick 之后 amend 消息（这正是 reword 的手工做法）。
    let todo = format!("pick {} 旧消息\n", plan.steps[0].short());
    cli_rebase_with_todo(&pair.theirs, &base, &todo);
    let message_file = pair.root.join("msg.txt");
    std::fs::write(&message_file, "新消息\n").expect("写消息");
    git(
        &pair.theirs,
        &[
            "commit",
            "--amend",
            "-F",
            message_file.to_str().expect("路径"),
        ],
    );

    pair.assert_equivalent();
    assert_eq!(git(&pair.ours, &["log", "-1", "--format=%s"]), "新消息");
    pair.cleanup();
}

// ---------------------------------------------- 拣选 / 回滚 / 重置

#[test]
fn cherry_pick_matches_the_cli() {
    let Some(pair) = Pair::new("cherry-pick") else {
        return;
    };
    git(&pair.ours, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(pair.ours.join("p.txt"), "picked\n").expect("写文件");
    git(&pair.ours, &["add", "-A"]);
    git(&pair.ours, &["commit", "-q", "-m", "待拣选"]);
    let rev = git(&pair.ours, &["rev-parse", "HEAD"]);
    git(&pair.ours, &["checkout", "-q", "main"]);
    pair.sync();

    pair.ops()
        .cherry_pick(&CherryPickRequest {
            revs: vec![rev.clone()],
            record_source: true,
            no_commit: false,
        })
        .expect("拣选");
    git(&pair.theirs, &["cherry-pick", "-x", &rev]);

    pair.assert_equivalent();
    pair.cleanup();
}

#[test]
fn revert_matches_the_cli() {
    let Some(pair) = Pair::new("revert") else {
        return;
    };
    std::fs::write(pair.ours.join("a.txt"), "one\ntwo\nthree\nfour\n").expect("改文件");
    git(&pair.ours, &["add", "-A"]);
    git(&pair.ours, &["commit", "-q", "-m", "加一行"]);
    pair.sync();

    pair.ops()
        .revert(&RevertRequest {
            revs: vec!["HEAD".to_owned()],
            no_commit: false,
        })
        .expect("回滚");
    git(&pair.theirs, &["revert", "--no-edit", "HEAD"]);

    // 回滚提交的消息由 git 生成，两侧一致；内容树也应一致。
    pair.assert_equivalent();
    assert_eq!(
        std::fs::read_to_string(pair.ours.join("a.txt")).expect("读回"),
        "one\ntwo\nthree\n"
    );
    pair.cleanup();
}

#[test]
fn reset_modes_match_the_cli() {
    for (mode, cli_flag) in [
        (ResetMode::Soft, "--soft"),
        (ResetMode::Mixed, "--mixed"),
        (ResetMode::Hard, "--hard"),
    ] {
        let Some(pair) = Pair::new(&format!("reset-{cli_flag}")) else {
            return;
        };
        std::fs::write(pair.ours.join("b.txt"), "b\n").expect("写文件");
        git(&pair.ours, &["add", "-A"]);
        git(&pair.ours, &["commit", "-q", "-m", "第二个提交"]);
        // 再造一处未提交的改动，验证三种模式对工作区的差别。
        std::fs::write(pair.ours.join("a.txt"), "未提交的改动\n").expect("改文件");
        let target = git(&pair.ours, &["rev-parse", "HEAD~1"]);
        pair.sync();

        pair.ops()
            .reset(&ResetRequest {
                target: target.clone(),
                mode,
            })
            .expect("重置");
        git(&pair.theirs, &["reset", cli_flag, &target]);

        pair.assert_equivalent();
        pair.cleanup();
    }
}

// ---------------------------------------------------------------- 贮藏

#[test]
fn stash_apply_and_pop_match_the_cli() {
    let Some(pair) = Pair::new("stash") else {
        return;
    };
    std::fs::write(pair.ours.join("a.txt"), "stashed\n").expect("改文件");
    std::fs::write(pair.ours.join("u.txt"), "untracked\n").expect("写未跟踪文件");
    pair.sync();

    pair.ops()
        .stash(&StashOp::Create {
            message: "WIP".to_owned(),
            include_untracked: true,
        })
        .expect("创建贮藏");
    git(&pair.theirs, &["stash", "push", "-u", "-m", "WIP"]);
    pair.assert_equivalent();

    pair.ops()
        .stash(&StashOp::Apply { index: 0 })
        .expect("应用");
    git(&pair.theirs, &["stash", "apply", "stash@{0}"]);
    pair.assert_equivalent();

    // apply 之后工作区已有改动，直接 pop 会被 git 拒绝（"would be overwritten"）。
    // 手工等价操作同样要先清理工作区，因此这里两侧都清。
    for dir in [&pair.ours, &pair.theirs] {
        git(dir, &["checkout", "--", "a.txt"]);
        let _ = std::fs::remove_file(dir.join("u.txt"));
    }

    pair.ops().stash(&StashOp::Pop { index: 0 }).expect("弹出");
    git(&pair.theirs, &["stash", "pop", "stash@{0}"]);
    pair.assert_equivalent();
    assert!(
        pair.ops().stash_list().expect("列表").is_empty(),
        "pop 之后两边都不该再有贮藏"
    );
    pair.cleanup();
}

// ---------------------------------------------------------------- 补丁

#[test]
fn patch_generated_here_applies_like_the_cli() {
    let Some(pair) = Pair::new("patch") else {
        return;
    };
    std::fs::write(pair.ours.join("a.txt"), "one\ntwo\nthree\nFOUR\n").expect("改文件");
    git(&pair.ours, &["add", "-A"]);
    git(&pair.ours, &["commit", "-q", "-m", "加一行"]);
    let rev = git(&pair.ours, &["rev-parse", "HEAD"]);
    pair.sync();

    // 两侧都回到父提交，再各自把自己的补丁应用回去。
    git(&pair.ours, &["reset", "--hard", "HEAD~1"]);
    git(&pair.theirs, &["reset", "--hard", "HEAD~1"]);

    let patch_file = pair.root.join("out.patch");
    pair.ops()
        .create_patch(&PatchSource::Commit { rev: rev.clone() }, &patch_file)
        .expect("生成补丁");

    pair.ops()
        .apply_patch(&PatchApplyRequest {
            file: patch_file.clone(),
            three_way: false,
            reverse: false,
        })
        .expect("应用补丁");

    // CLI 侧用同一个补丁文件应用 —— 这正是"补丁可用"的定义。
    git(&pair.theirs, &["apply", patch_file.to_str().expect("路径")]);

    pair.assert_same(&["status", "--porcelain"]);
    assert_eq!(
        std::fs::read_to_string(pair.ours.join("a.txt")).expect("读回"),
        std::fs::read_to_string(pair.theirs.join("a.txt")).expect("读回"),
    );
    pair.cleanup();
}

// ---------------------------------------------------------------- 标签

#[test]
fn tag_operations_match_the_cli() {
    let Some(pair) = Pair::new("tag") else {
        return;
    };
    pair.ops()
        .tag(&TagOp::Create {
            name: "v1.0".to_owned(),
            message: Some("首个版本".to_owned()),
            target: None,
        })
        .expect("附注标签");
    git(&pair.theirs, &["tag", "-a", "v1.0", "-m", "首个版本"]);

    // 附注标签对象的类型与指向必须一致。
    pair.assert_same(&["cat-file", "-t", "v1.0"]);
    pair.assert_same(&["rev-parse", "v1.0^{commit}"]);

    pair.ops()
        .tag(&TagOp::Delete {
            name: "v1.0".to_owned(),
        })
        .expect("删除");
    git(&pair.theirs, &["tag", "-d", "v1.0"]);
    pair.assert_same(&["tag"]);
    pair.cleanup();
}

// ------------------------------------------------------------ 服务层全链路

/// 投递一个请求并等它的结果（服务层的"一次往返"）。
fn round_trip(
    service: &GitService,
    id: gt_core::RepoId,
    sender: &crossbeam_channel::Sender<Delivery>,
    receiver: &crossbeam_channel::Receiver<Delivery>,
    kind: RequestKind,
) -> Outcome {
    service.request(id, kind, sender.clone()).expect("投递请求");
    receiver
        .recv_timeout(Duration::from_secs(60))
        .expect("等待结果超时")
        .outcome
}

#[test]
fn conflict_flow_through_the_service_clears_unmerged_entries() {
    let Some(pair) = Pair::new("svc-conflict") else {
        return;
    };
    setup_conflicting_branches(&pair);
    git(&pair.ours, &["checkout", "-q", "main"]);
    let root = pair.ours.clone();

    let service = Arc::new(
        GitService::with_token_store(
            noop_observer(),
            Arc::new(gt_backend::MemoryTokenStore::new()),
        )
        .expect("建服务"),
    );
    let (sender, receiver) = crossbeam_channel::unbounded();
    let handle = service.open_repo(&root).expect("打开仓库");
    let id = handle.id();

    // 1) 合并 → 冲突。
    let merged = round_trip(
        &service,
        id,
        &sender,
        &receiver,
        RequestKind::Write(Box::new(WriteRequest::Merge(MergeRequest {
            rev: "feature".to_owned(),
            strategy: MergeStrategy::Default,
            message: None,
        }))),
    );
    match &merged {
        Outcome::Advanced(result) => assert!(result.is_conflicted(), "{result:?}"),
        other => panic!("期望高级操作结果，实际 {other:?}"),
    }

    // 2) 冲突清单。
    let conflicts = round_trip(&service, id, &sender, &receiver, RequestKind::Conflicts);
    let Outcome::Conflicts(entries) = &conflicts else {
        panic!("期望冲突清单，实际 {conflicts:?}");
    };
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0].path, "a.txt");
    assert_eq!(entries[0].hunks, 1, "应统计出 1 处冲突");

    // 3) 解析冲突文件 → 逐块取"合入分支" → 写回。
    let file = round_trip(
        &service,
        id,
        &sender,
        &receiver,
        RequestKind::ConflictFile {
            path: "a.txt".to_owned(),
        },
    );
    let Outcome::ConflictFile(parsed) = &file else {
        panic!("期望冲突文件，实际 {file:?}");
    };
    assert_eq!(parsed.hunk_count(), 1);
    let solved = parsed
        .solve(&[gt_diff::Resolution::Theirs])
        .expect("逐块求解");
    assert!(!solved.contains("<<<<<<<"), "{solved}");

    let resolved = round_trip(
        &service,
        id,
        &sender,
        &receiver,
        RequestKind::Write(Box::new(WriteRequest::ResolveConflict {
            path: "a.txt".to_owned(),
            content: solved.into_bytes(),
        })),
    );
    assert!(resolved.is_ok(), "{resolved:?}");

    // 4) `git ls-files -u` 必须为空（需求文档 5.8 的验收口径）。
    let unmerged = git(&root, &["ls-files", "-u"]);
    assert!(unmerged.is_empty(), "仍有未合并条目：{unmerged}");

    // 5) 完成合并提交后状态回到 Clean。
    let committed = round_trip(
        &service,
        id,
        &sender,
        &receiver,
        RequestKind::Write(Box::new(WriteRequest::Commit(gt_backend::CommitRequest {
            message: "合并 feature".to_owned(),
            ..gt_backend::CommitRequest::default()
        }))),
    );
    assert!(committed.is_ok(), "{committed:?}");
    assert!(
        git(&root, &["status", "--porcelain"]).is_empty(),
        "合并提交后工作区应干净"
    );

    service.close_repo(id);
    pair.cleanup();
}

#[test]
fn a_failed_advanced_operation_releases_the_slot() {
    let Some(pair) = Pair::new("svc-slot") else {
        return;
    };
    let root = pair.ours.clone();
    let service = Arc::new(
        GitService::with_token_store(
            noop_observer(),
            Arc::new(gt_backend::MemoryTokenStore::new()),
        )
        .expect("建服务"),
    );
    let (sender, receiver) = crossbeam_channel::unbounded();
    let handle = service.open_repo(&root).expect("打开仓库");
    let id = handle.id();

    // 第一次：合并一个不存在的修订 → 失败。
    let failed = round_trip(
        &service,
        id,
        &sender,
        &receiver,
        RequestKind::Write(Box::new(WriteRequest::Merge(MergeRequest {
            rev: "不存在的分支".to_owned(),
            strategy: MergeStrategy::Default,
            message: None,
        }))),
    );
    assert!(!failed.is_ok(), "{failed:?}");

    // 第二次：即使前一次失败，槽位也必须已经释放。
    assert!(
        handle.advanced_label().is_none(),
        "失败的合并没有释放高级操作槽位"
    );
    let second = round_trip(
        &service,
        id,
        &sender,
        &receiver,
        RequestKind::Write(Box::new(WriteRequest::Stash(StashOp::Create {
            message: "空贮藏".to_owned(),
            include_untracked: false,
        }))),
    );
    assert!(second.is_ok(), "{second:?}");

    service.close_repo(id);
    pair.cleanup();
}
