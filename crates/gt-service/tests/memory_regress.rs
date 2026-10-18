//! 内存回归基准（NFR-MEM-03，M5；L4 层）。
//!
//! # 测什么
//!
//! "大仓库操作后 RSS 回落"的直接断言受分配器策略影响（glibc / Windows 堆
//! 都可能把释放的页留在进程里），在 CI 上天然抖动。这里改为测泄漏语义的
//! 等价形式：**同一组读操作重复多轮，RSS 增量必须收敛**——
//! 缓存（日志索引等）是有界的，第一轮之后不应再增长；若某个组件
//! 每轮都在积累数据（典型的"缓存无淘汰"或"模型重复物化"），RSS 会
//! 随轮数线性上涨，正是要拦的缺陷。
//!
//! # 基准仓库
//!
//! 用 git 管道命令（`hash-object` / `mktree` / `commit-tree` / `update-ref`）
//! 快速造一个 400 提交的单分支仓库，全程无需触碰 UI。
//!
//! # 判据
//!
//! - 6 轮操作，每轮 = 2 个日志页 + 工作区状态 + 树差异 + 全量提交元数据；
//! - `rss(第 6 轮) - rss(第 2 轮) < 20MB`（首轮含缓存与代码页冷启动，不计）。

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use gt_backend::{noop_observer, GitCli};
use gt_core::LogPageRequest;
use gt_service::{GitService, Outcome, RequestKind};

/// 每轮操作允许的 RSS 增量上限（字节）：20MB。
const MAX_ROUND_GROWTH: i64 = 20 * 1024 * 1024;

/// 基准仓库的提交数。
const COMMITS: usize = 400;

/// 重复轮数。
const ROUNDS: usize = 6;

// ------------------------------------------------------------ RSS 采样

// 工作区全域 deny(unsafe_code)，因此 RSS 采样不直接调 Win32 API，
// 而是起 PowerShell 子进程读工作集/私有内存。每轮一次、共 6 次，
// 开销可忽略；UNIX 直接读 /proc。

#[cfg(windows)]
fn current_rss_bytes() -> u64 {
    let script = format!(
        "$p=Get-Process -Id {}; [string]([math]::Max($p.WorkingSet64, $p.PrivateMemorySize64))",
        std::process::id()
    );
    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .expect("运行采样子进程");
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

#[cfg(unix)]
fn current_rss_bytes() -> u64 {
    // /proc/self/statm 的第二个字段是常驻页数（页大小 4KB）。
    let statm = std::fs::read_to_string("/proc/self/statm").expect("读取 statm");
    let pages: u64 = statm
        .split_whitespace()
        .nth(1)
        .and_then(|count| count.parse().ok())
        .unwrap_or(0);
    pages * 4096
}

#[cfg(not(any(windows, unix)))]
fn current_rss_bytes() -> u64 {
    0
}

// ------------------------------------------------------------ 基准仓库

/// 用 `git fast-import` 一次性造 N 提交的单分支仓库（秒级，CI 友好）。
fn build_fixture(label: &str, commits: usize) -> Option<PathBuf> {
    let cli = GitCli::detect()?;
    let root = std::env::temp_dir().join(format!(
        "knotwork-mem-regress-{}-{label}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("建仓库目录");

    let run = |args: &[&str]| -> String {
        let output = Command::new(cli.program())
            .args(args)
            .current_dir(&root)
            .output()
            .unwrap_or_else(|err| panic!("启动 git {args:?} 失败：{err}"));
        assert!(
            output.status.success(),
            "git {args:?} 失败：{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    };

    run(&["init", "-q", "-b", "main"]);

    // fast-import 流：每个提交引用一个新的 blob（内容逐次变化，保证
    // 每次树差异都有实际载荷）。
    let mut stream = String::new();
    let mut blob_mark = 1usize;
    let mut commit_mark = 2usize;
    for index in 0..commits {
        let content = format!("line-{index}\n");
        let message = format!("第 {index} 次提交");
        // fmt::Write 追加（clippy 不喜欢 push_str(&format!(..))）。
        let _ = write!(
            stream,
            "blob\nmark :{blob_mark}\ndata {}\n{}\n",
            content.len(),
            content
        );
        let _ = write!(
            stream,
            "commit refs/heads/main\nmark :{commit_mark}\n\
             author Knotwork Mem <mem@knotwork.invalid> 1700000000 +0800\n\
             committer Knotwork Mem <mem@knotwork.invalid> 1700000000 +0800\n\
             data {}\n{}\n\
             M 100644 :{blob_mark} f.txt\n\n",
            message.len(),
            message
        );
        blob_mark += 2;
        commit_mark += 2;
    }

    let mut child = Command::new(cli.program())
        .args(["fast-import", "--quiet"])
        .current_dir(&root)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("启动 git fast-import");
    child
        .stdin
        .as_mut()
        .expect("stdin piped")
        .write_all(stream.as_bytes())
        .expect("写入 fast-import 流");
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("等待 fast-import");
    assert!(
        output.status.success(),
        "fast-import 失败：{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // 工作区与 HEAD 同步，让 status 是干净的。
    run(&["checkout", "-q", "-f", "main"]);
    Some(root)
}

// ------------------------------------------------------------ 用例

#[test]
fn repeated_read_operations_do_not_grow_rss() {
    let Some(root) = build_fixture("service", COMMITS) else {
        return; // git 不可用：跳过
    };

    let mut service = GitService::new(noop_observer()).expect("建服务");
    let (sender, receiver) = crossbeam_channel::unbounded();
    let handle = service.open_repo(&root).expect("打开仓库");
    let id = handle.id();

    let round_trip = |kind: RequestKind| -> Outcome {
        service.request(id, kind, sender.clone()).expect("投递请求");
        receiver
            .recv_timeout(Duration::from_secs(120))
            .expect("等待结果超时")
            .outcome
    };

    let mut samples: Vec<u64> = Vec::new();
    for round in 0..ROUNDS {
        // 每轮：两个日志页 + 状态 + 树差异（读路径全覆盖）。
        let first = round_trip(RequestKind::LogPage(LogPageRequest::default()));
        assert!(first.is_ok(), "日志页失败：{first:?}");
        let second = LogPageRequest {
            offset: 200,
            ..LogPageRequest::default()
        };
        let paged = round_trip(RequestKind::LogPage(second));
        assert!(paged.is_ok(), "翻页失败：{paged:?}");
        let status = round_trip(RequestKind::Status);
        assert!(status.is_ok(), "状态失败：{status:?}");
        let changes = round_trip(RequestKind::TreeChanges {
            old: Some("HEAD~1".to_owned()),
            new: "HEAD".to_owned(),
        });
        assert!(changes.is_ok(), "树差异失败：{changes:?}");

        samples.push(current_rss_bytes());
        let mut note = String::from("轮次 RSS：");
        note.push_str(&(samples[round] / 1024).to_string());
        note.push_str(" KB");
        tracing_free_the_reader(&note);
    }

    service.shutdown();
    let _ = std::fs::remove_dir_all(&root);

    // 判据：第 2 轮（缓存已建）到最后一轮之间不线性增长。
    let baseline = samples[1];
    let last = samples[ROUNDS - 1];
    let growth = last.saturating_sub(baseline);
    let max: u64 = MAX_ROUND_GROWTH.max(0) as u64;
    assert!(
        growth < max,
        "重复读操作使 RSS 增长 {} MB（> {} MB 上限），疑似无界缓存或模型泄漏；\
         各轮采样：{:?}",
        growth / (1024 * 1024),
        max / (1024 * 1024),
        samples.iter().map(|s| s / (1024 * 1024)).collect::<Vec<_>>()
    );
}

/// 打一条诊断日志（本地跑的时候能直接看到曲线）。
fn tracing_free_the_reader(message: &str) {
    eprintln!("[memory-regress] {message}");
}
