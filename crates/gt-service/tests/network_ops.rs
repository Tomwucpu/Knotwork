//! 网络操作等价测试（需求文档第 9 节第 2 条，L3 层；FR-RMT 验收）。
//!
//! # 测什么
//!
//! A/B 双仓库对，**内容与提交 id 完全相同**（`theirs` 是 `ours` 的逐字节副本，
//! 因此两侧的提交对象哈希一致）：A 走 [`RemoteOps`]，B 走等价的 `git` 命令，
//! 然后比较**git 自己报出来的远端状态**：
//!
//! - 裸仓库的引用（`rev-parse refs/heads/main`）是否一致；
//! - 远程跟踪引用与领先/落后计数是否一致；
//! - 拒绝推送时两侧是否同样被拒；
//! - 远端配置（`git remote -v`）是否一致。
//!
//! 关键点：提交 id 相同，所以"引用值相同"是可比对的事实，而不是"看起来差不多"。
//! 比较的基准永远是 git 的输出，不会出现"用我们的实现验证我们的实现"。
//!
//! # 为什么用本地裸仓库当远端
//!
//! 网络操作的全部语义（refspec、快进判定、拒绝原因、跟踪引用更新）都在
//! git 传输层之上，本地路径与 HTTPS/SSH 走的是同一套代码路径。
//! 用本地裸仓库把测试从"网络是否可达"这类外部因素里彻底剥离出来。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use gt_backend::{
    noop_observer, BranchProtection, FetchRequest, GitCli, MemoryTokenStore, PullRequest,
    PullStrategy, PushRequest, PushTarget, RemoteEdit, RemoteOps, TokenStore,
};
use gt_service::{GitService, NetworkOutcome, NetworkRequest, Outcome, RequestKind};
use std::time::Duration;

fn git_ok(dir: &Path, args: &[&str]) -> String {
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

/// 以固定作者/提交时间执行 git，使两侧的提交 id 完全一致。
///
/// 只设 `--date` 不够：它只改作者时间，committer 时间仍取当前时钟，
/// 于是 A/B 两侧的同一条提交会得到不同的哈希，"引用相等"这个断言就失效了。
fn git_dated(dir: &Path, args: &[&str], date: &str) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .output()
        .expect("执行 git");
    assert!(
        output.status.success(),
        "git {args:?} 失败：{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn git_status(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// A/B 仓库对：各自有自己的裸远端，内容完全相同。
struct Pair {
    root: PathBuf,
    ours: PathBuf,
    theirs: PathBuf,
    bare_ours: PathBuf,
    bare_theirs: PathBuf,
    cli: GitCli,
}

impl Pair {
    fn new(label: &str) -> Option<Self> {
        let cli = GitCli::detect()?;
        let root =
            std::env::temp_dir().join(format!("knotwork-net-equiv-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("建临时目录");

        let ours = root.join("ours");
        let theirs = root.join("theirs");
        std::fs::create_dir_all(&ours).expect("建仓库目录");

        git_ok(
            &root,
            &[
                "init",
                "--bare",
                "-q",
                "--initial-branch=main",
                "bare-ours.git",
            ],
        );
        git_ok(
            &root,
            &[
                "init",
                "--bare",
                "-q",
                "--initial-branch=main",
                "bare-theirs.git",
            ],
        );

        git_ok(&ours, &["init", "-q", "--initial-branch=main"]);
        git_ok(
            &ours,
            &["config", "user.email", "net-equiv@knotwork.invalid"],
        );
        git_ok(&ours, &["config", "user.name", "Knotwork Net Equiv"]);
        git_ok(&ours, &["config", "core.autocrlf", "false"]);
        std::fs::write(ours.join("a.txt"), "alpha\n").expect("写文件");
        git_ok(&ours, &["add", "-A"]);
        // 固定作者与提交时间：两侧的提交 id 因此完全一致，引用才可比。
        git_dated(
            &ours,
            &["commit", "-q", "-m", "基线提交"],
            "2026-01-01T00:00:00 +0000",
        );
        git_ok(
            &ours,
            &["remote", "add", "origin", path(&root.join("bare-ours.git"))],
        );

        copy_dir(&ours, &theirs).expect("复制仓库");
        git_ok(
            &theirs,
            &[
                "remote",
                "set-url",
                "origin",
                path(&root.join("bare-theirs.git")),
            ],
        );

        let bare_ours = root.join("bare-ours.git");
        let bare_theirs = root.join("bare-theirs.git");
        Some(Self {
            root,
            ours,
            theirs,
            bare_ours,
            bare_theirs,
            cli,
        })
    }

    fn ops(&self, dir: &Path) -> RemoteOps {
        let location = gt_backend::RepoLocation {
            git_dir: dir.join(".git"),
            work_dir: Some(dir.to_path_buf()),
            is_bare: false,
        };
        RemoteOps::new(&location, self.cli.clone(), noop_observer())
    }

    fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn path(path: &Path) -> &str {
    path.to_str().expect("路径可转字符串")
}

/// 递归复制目录（只用于测试资产）。
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

/// 只看 backup 远端的配置行（`origin` 指向各自的裸仓库，路径本就不同）。
fn backup_lines(dir: &Path) -> Vec<String> {
    git_ok(dir, &["remote", "-v"])
        .lines()
        .filter(|line| line.starts_with("backup"))
        .map(str::to_owned)
        .collect()
}

/// 头部引用（两侧必须相同，因为提交 id 相同）。
fn head_of(dir: &Path) -> String {
    git_ok(dir, &["rev-parse", "HEAD"])
}

#[test]
fn push_produces_the_same_remote_state_as_the_cli() {
    let Some(pair) = Pair::new("push") else {
        return;
    };

    // A：我们的推送。
    pair.ops(&pair.ours)
        .push(
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
        .expect("我们的推送应成功");

    // B：手工命令。
    git_ok(&pair.theirs, &["push", "-q", "-u", "origin", "main"]);

    // 比较两侧裸仓库的引用 —— 提交 id 相同，所以这是可严格比对的事实。
    let ours_ref = git_ok(&pair.bare_ours, &["rev-parse", "refs/heads/main"]);
    let theirs_ref = git_ok(&pair.bare_theirs, &["rev-parse", "refs/heads/main"]);
    assert_eq!(ours_ref, theirs_ref, "推送结果必须与 CLI 一致");
    assert_eq!(ours_ref, head_of(&pair.ours));

    // 远程跟踪引用也要一致（`-u` 之后本地应能看到 origin/main）。
    assert_eq!(
        git_ok(&pair.ours, &["rev-parse", "refs/remotes/origin/main"]),
        git_ok(&pair.theirs, &["rev-parse", "refs/remotes/origin/main"]),
        "跟踪引用的更新必须与 CLI 一致"
    );

    // 上游配置也要一致。
    assert_eq!(
        git_ok(&pair.ours, &["rev-parse", "--abbrev-ref", "main@{u}"]),
        git_ok(&pair.theirs, &["rev-parse", "--abbrev-ref", "main@{u}"])
    );

    pair.cleanup();
}

#[test]
fn fetch_and_pull_match_the_cli_after_the_remote_moves() {
    let Some(pair) = Pair::new("fetch") else {
        return;
    };

    // 两侧先各自推送，让远端有 main。
    pair.ops(&pair.ours)
        .push(
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
        .expect("推送 A");
    git_ok(&pair.theirs, &["push", "-q", "-u", "origin", "main"]);

    // 远端被第三个克隆推进（两侧远端各推一次，提交内容完全相同）。
    for bare in [&pair.bare_ours, &pair.bare_theirs] {
        let advance = pair.root.join(format!(
            "advance-{}",
            bare.file_name().expect("文件名").to_string_lossy()
        ));
        git_ok(&pair.root, &["clone", "-q", path(bare), path(&advance)]);
        git_ok(
            &advance,
            &["config", "user.email", "advance@knotwork.invalid"],
        );
        git_ok(&advance, &["config", "user.name", "Advance"]);
        std::fs::write(advance.join("remote.txt"), "from remote\n").expect("写文件");
        git_ok(&advance, &["add", "-A"]);
        git_dated(
            &advance,
            &["commit", "-q", "-m", "远端提交"],
            "2026-01-02T00:00:00 +0000",
        );
        git_ok(&advance, &["push", "-q", "origin", "main"]);
    }

    // A：我们的 fetch。
    pair.ops(&pair.ours)
        .fetch(
            &FetchRequest {
                remote: Some("origin".to_owned()),
                prune: true,
                tags: false,
            },
            &|| false,
            None,
        )
        .expect("我们的 fetch 应成功");
    // B：手工 fetch。
    git_ok(&pair.theirs, &["fetch", "-q", "--prune", "origin"]);

    assert_eq!(
        git_ok(&pair.ours, &["rev-parse", "refs/remotes/origin/main"]),
        git_ok(&pair.theirs, &["rev-parse", "refs/remotes/origin/main"]),
        "fetch 后的跟踪引用必须与 CLI 一致"
    );

    // 两侧再各自 pull（合并策略），比较工作区与 HEAD 的树。
    pair.ops(&pair.ours)
        .pull(
            &PullRequest {
                remote: "origin".to_owned(),
                branch: Some("main".to_owned()),
                strategy: PullStrategy::Merge,
            },
            &|| false,
            None,
        )
        .expect("我们的 pull 应成功");
    git_ok(
        &pair.theirs,
        &["pull", "-q", "--no-rebase", "--no-edit", "origin", "main"],
    );

    assert_eq!(
        git_ok(&pair.ours, &["rev-parse", "HEAD^{tree}"]),
        git_ok(&pair.theirs, &["rev-parse", "HEAD^{tree}"]),
        "pull 合并后的树必须与 CLI 一致"
    );
    assert_eq!(
        std::fs::read_to_string(pair.ours.join("remote.txt")).expect("读文件"),
        std::fs::read_to_string(pair.theirs.join("remote.txt")).expect("读文件"),
        "工作区内容必须一致"
    );
    assert_eq!(
        git_ok(&pair.ours, &["status", "--porcelain"]),
        git_ok(&pair.theirs, &["status", "--porcelain"]),
        "pull 后不应留下未预期的本地改动"
    );

    pair.cleanup();
}

#[test]
fn rejected_push_matches_the_cli_and_carries_guidance() {
    let Some(pair) = Pair::new("reject") else {
        return;
    };

    // 两侧先推送基线。
    pair.ops(&pair.ours)
        .push(
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
        .expect("推送 A");
    git_ok(&pair.theirs, &["push", "-q", "-u", "origin", "main"]);

    // 远端被推进，两侧本地各自新增一个提交（内容相同）。
    for (work, bare) in [
        (&pair.ours, &pair.bare_ours),
        (&pair.theirs, &pair.bare_theirs),
    ] {
        let advance = pair.root.join(format!(
            "adv-{}",
            bare.file_name().expect("文件名").to_string_lossy()
        ));
        git_ok(&pair.root, &["clone", "-q", path(bare), path(&advance)]);
        git_ok(&advance, &["config", "user.email", "adv@knotwork.invalid"]);
        git_ok(&advance, &["config", "user.name", "Adv"]);
        std::fs::write(advance.join("x.txt"), "x\n").expect("写文件");
        git_ok(&advance, &["add", "-A"]);
        git_dated(
            &advance,
            &["commit", "-q", "-m", "远端"],
            "2026-01-02T00:00:00 +0000",
        );
        git_ok(&advance, &["push", "-q", "origin", "main"]);

        std::fs::write(work.join("local.txt"), "local\n").expect("写文件");
        git_ok(work, &["add", "-A"]);
        git_dated(
            work,
            &["commit", "-q", "-m", "本地"],
            "2026-01-03T00:00:00 +0000",
        );
    }

    // A：我们的推送必须被拒，并给出可行动建议。
    let error = pair
        .ops(&pair.ours)
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
        .expect_err("非快进推送必须失败");
    let message = error.to_string();
    assert!(
        message.contains("Update") || message.contains("force-with-lease"),
        "拒绝必须给出下一步建议：{message}"
    );

    // B：CLI 同样被拒 —— 证明"拒绝"不是我们的一厢情愿。
    assert!(
        !git_status(&pair.theirs, &["push", "-q", "origin", "main"]),
        "CLI 在同样的场景下也必须被拒"
    );

    // 两侧的远端引用保持不动。
    assert_eq!(
        git_ok(&pair.bare_ours, &["rev-parse", "refs/heads/main"]),
        git_ok(&pair.bare_theirs, &["rev-parse", "refs/heads/main"]),
        "被拒的推送不应改变远端"
    );

    pair.cleanup();
}

#[test]
fn remote_configuration_matches_the_cli() {
    let Some(pair) = Pair::new("config") else {
        return;
    };

    pair.ops(&pair.ours)
        .edit_remote(&RemoteEdit::Add {
            name: "backup".to_owned(),
            url: "https://example.invalid/backup.git".to_owned(),
        })
        .expect("添加远端");
    git_ok(
        &pair.theirs,
        &[
            "remote",
            "add",
            "backup",
            "https://example.invalid/backup.git",
        ],
    );

    // `origin` 指向各自的裸仓库（路径必然不同），因此只比对新建的远端。
    assert_eq!(
        backup_lines(&pair.ours),
        backup_lines(&pair.theirs),
        "新增远端必须与 CLI 一致"
    );

    pair.ops(&pair.ours)
        .edit_remote(&RemoteEdit::Rename {
            from: "backup".to_owned(),
            to: "mirror".to_owned(),
        })
        .expect("重命名");
    git_ok(&pair.theirs, &["remote", "rename", "backup", "mirror"]);
    assert_eq!(
        git_ok(&pair.ours, &["remote", "-v"])
            .lines()
            .filter(|line| line.starts_with("mirror"))
            .collect::<Vec<_>>(),
        git_ok(&pair.theirs, &["remote", "-v"])
            .lines()
            .filter(|line| line.starts_with("mirror"))
            .collect::<Vec<_>>(),
        "重命名结果必须与 CLI 一致"
    );

    pair.cleanup();
}

#[test]
fn push_preview_matches_rev_list() {
    let Some(pair) = Pair::new("preview") else {
        return;
    };
    pair.ops(&pair.ours)
        .push(
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
        .expect("推送");

    // 新增两个提交后，预览的"待推提交"必须等于 git 自己的 rev-list。
    for message in ["第二", "第三"] {
        std::fs::write(pair.ours.join(format!("{message}.txt")), message).expect("写文件");
        git_ok(&pair.ours, &["add", "-A"]);
        git_ok(&pair.ours, &["commit", "-q", "-m", message]);
    }

    let preview = pair
        .ops(&pair.ours)
        .push_preview("origin", Some("main"), &BranchProtection::default())
        .expect("预览");
    let expected: usize = git_ok(&pair.ours, &["rev-list", "--count", "origin/main..HEAD"])
        .parse()
        .expect("计数");
    assert_eq!(preview.ahead, expected, "领先数必须与 rev-list 一致");
    assert_eq!(preview.commits.len(), expected);
    assert_eq!(preview.commits[0].subject, "第三", "预览应按时间倒序列出");
    assert!(!preview.creates_branch);
    assert_eq!(preview.behind, 0);

    pair.cleanup();
}

#[test]
fn service_level_push_updates_the_remote_and_console_audit() {
    let Some(pair) = Pair::new("service") else {
        return;
    };
    let store = Arc::new(MemoryTokenStore::new());
    let mut service =
        GitService::with_token_store(noop_observer(), Arc::clone(&store) as Arc<dyn TokenStore>)
            .expect("建服务");
    let handle = service.open_repo(&pair.ours).expect("打开仓库");
    let (sender, receiver) = crossbeam_channel::unbounded();

    service
        .request(
            handle.id(),
            RequestKind::Network(Box::new(NetworkRequest::Push {
                request: PushRequest {
                    remote: "origin".to_owned(),
                    targets: vec![PushTarget::same("main")],
                    force_with_lease: false,
                    set_upstream: true,
                    tags: false,
                },
                protected_patterns: Vec::new(),
            })),
            sender,
        )
        .expect("投递推送");

    let delivery = receiver
        .recv_timeout(Duration::from_secs(60))
        .expect("应在超时前收到结果");
    let Outcome::Network(outcome) = delivery.outcome else {
        panic!("期望网络结果，实际 {:?}", delivery.outcome);
    };
    assert!(
        matches!(*outcome, NetworkOutcome::Done(_)),
        "推送应成功：{outcome:?}"
    );

    // 远端真的收到了（以 git 自己为准）。
    assert_eq!(
        git_ok(&pair.bare_ours, &["rev-parse", "refs/heads/main"]),
        head_of(&pair.ours)
    );

    service.shutdown();
    pair.cleanup();
}
