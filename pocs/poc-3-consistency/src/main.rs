//! POC-3：gix 与 git CLI 输出一致性对比。
//!
//! 对应实施计划 M0 节 POC-3 与需求 NFR-COR-01：
//! 对基准仓库，`gix` 的输出须与 `git` CLI 的结构化输出一致。
//!
//! 对比项：
//!   A. 提交遍历（等价 `git rev-list --all`）
//!   B. 提交元数据（`%H|%an|%ae|%at`，抽样 2000 条）
//!   C. 工作区状态（等价 `git status --porcelain=v1`，在专用 fixture 仓库上）
//!   D. 树差异（等价 `git diff --name-status <a> <b>`）
//!
//! 用法：`poc3-consistency [基准仓库根目录]`，默认指向 `../../test-repos/repos`。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

/// 单项对比结果。
struct Check {
    suite: &'static str,
    repo: String,
    ok: bool,
    detail: String,
}

/// 执行 git 子进程并返回 stdout；非零退出码视为错误。
fn git(repo: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|e| format!("无法启动 git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {args:?} 退出码 {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `&BStr` → `String`（UTF-8 有损转换，规避 bstr 的 Display 依赖）。
fn bs(b: &gix::bstr::BStr) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// 收集仓库全部引用指向的提交 id。
fn gix_tips(repo: &gix::Repository) -> Result<Vec<gix::ObjectId>, String> {
    let platform = repo.references().map_err(|e| format!("读取引用失败: {e}"))?;
    let iter = platform.all().map_err(|e| format!("引用迭代失败: {e}"))?;
    let mut tips = Vec::new();
    for r in iter {
        let r = r.map_err(|e| format!("引用项错误: {e}"))?;
        tips.push(r.id().detach());
    }
    Ok(tips)
}

/// 读取某个版本标识对应的树对象。
fn tree_of<'r>(repo: &'r gix::Repository, spec: &str) -> Result<gix::Tree<'r>, String> {
    let id = repo
        .rev_parse_single(spec)
        .map_err(|e| format!("解析 {spec} 失败: {e}"))?;
    let obj = id.object().map_err(|e| format!("读取 {spec} 对象失败: {e}"))?;
    obj.peel_to_tree()
        .map_err(|e| format!("{spec} 剥离为树失败: {e}"))
}

// ---------------------------------------------------------------------------
// A. 提交遍历一致性
// ---------------------------------------------------------------------------
fn check_rev_list(path: &Path) -> Check {
    let repo = match gix::open(path) {
        Ok(r) => r,
        Err(e) => return fail("A.rev-list", path, format!("gix::open 失败: {e}")),
    };

    let gix_ids: BTreeSet<String> = match gix_tips(&repo).and_then(|tips| {
        let walk = repo
            .rev_walk(tips)
            .all()
            .map_err(|e| format!("rev_walk 失败: {e}"))?;
        let mut set = BTreeSet::new();
        for info in walk {
            let info = info.map_err(|e| format!("遍历出错: {e}"))?;
            set.insert(info.id.to_string());
        }
        Ok(set)
    }) {
        Ok(set) => set,
        Err(e) => return fail("A.rev-list", path, e),
    };

    let cli_ids: BTreeSet<String> = match git(path, &["rev-list", "--all"]) {
        Ok(out) => out.lines().map(str::to_owned).collect(),
        Err(e) => return fail("A.rev-list", path, e),
    };

    let only_gix: Vec<_> = gix_ids.difference(&cli_ids).take(3).cloned().collect();
    let only_cli: Vec<_> = cli_ids.difference(&gix_ids).take(3).cloned().collect();
    let ok = gix_ids == cli_ids;

    Check {
        suite: "A.rev-list",
        repo: label(path),
        ok,
        detail: if ok {
            format!("{} 个提交集合完全一致", gix_ids.len())
        } else {
            format!(
                "集合不一致：gix={} cli={}；仅 gix 有 {only_gix:?}；仅 cli 有 {only_cli:?}",
                gix_ids.len(),
                cli_ids.len()
            )
        },
    }
}

// ---------------------------------------------------------------------------
// B. 提交元数据一致性（单次 git 调用取基准，逐条比对）
// ---------------------------------------------------------------------------
fn check_log_metadata(path: &Path, sample: usize) -> Check {
    let repo = match gix::open(path) {
        Ok(r) => r,
        Err(e) => return fail("B.log-meta", path, format!("gix::open 失败: {e}")),
    };

    let cli = match git(
        path,
        &[
            "log",
            "--all",
            &format!("-n{sample}"),
            "--format=%H|%an|%ae|%at",
        ],
    ) {
        Ok(out) => out,
        Err(e) => return fail("B.log-meta", path, e),
    };

    let cli_map: BTreeMap<String, String> = cli
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|l| l.split_once('|').map(|(id, _)| (id.to_owned(), l.to_owned())))
        .collect();

    let mut mismatches = Vec::new();
    for (id, cli_line) in &cli_map {
        let Ok(oid) = gix::ObjectId::from_hex(id.as_bytes()) else {
            mismatches.push(format!("{id}: 非法 oid"));
            continue;
        };
        let Ok(commit) = repo.find_commit(oid) else {
            mismatches.push(format!("{id}: gix 找不到该提交"));
            continue;
        };
        let (Ok(author), Ok(time)) = (commit.author(), commit.time()) else {
            mismatches.push(format!("{id}: gix 元数据解码失败"));
            continue;
        };
        let gix_line = format!(
            "{}|{}|{}|{}",
            oid,
            bs(author.name),
            bs(author.email),
            time.seconds
        );
        if gix_line != *cli_line {
            mismatches.push(format!("gix={gix_line} cli={cli_line}"));
        }
    }

    Check {
        suite: "B.log-meta",
        repo: label(path),
        ok: mismatches.is_empty(),
        detail: if mismatches.is_empty() {
            format!("抽样 {} 条提交元数据（作者/邮箱/时间）逐条一致", cli_map.len())
        } else {
            format!("{} 条不一致，样例：{}", mismatches.len(), mismatches[0])
        },
    }
}

// ---------------------------------------------------------------------------
// C. 工作区状态一致性（fixture 仓库）
// ---------------------------------------------------------------------------
fn check_status(fixture: &Path) -> Check {
    let repo = match gix::open(fixture) {
        Ok(r) => r,
        Err(e) => return fail("C.status", fixture, format!("gix::open 失败: {e}")),
    };

    let cli = match git(fixture, &["status", "--porcelain=v1", "--untracked-files=all"]) {
        Ok(s) => s,
        Err(e) => return fail("C.status", fixture, e),
    };
    let cli_map: BTreeMap<String, String> = cli
        .lines()
        .filter_map(|l| {
            if l.len() < 4 {
                return None;
            }
            let code = l[..2].to_owned();
            let path = l[3..].trim_matches('"').replace('\\', "/");
            Some((path, code))
        })
        .collect();

    let platform = match repo.status(gix::progress::Discard) {
        Ok(p) => p.untracked_files(gix::status::UntrackedFiles::Files),
        Err(e) => return fail("C.status", fixture, format!("gix status 初始化失败: {e}")),
    };
    let iter = match platform.into_iter(std::iter::empty::<gix::bstr::BString>()) {
        Ok(it) => it,
        Err(e) => return fail("C.status", fixture, format!("gix status 迭代失败: {e}")),
    };

    let mut gix_paths: BTreeSet<String> = BTreeSet::new();
    for item in iter {
        match item {
            Ok(i) => {
                gix_paths.insert(bs(i.location()));
            }
            Err(e) => return fail("C.status", fixture, format!("gix status 项错误: {e}")),
        }
    }

    let cli_paths: BTreeSet<String> = cli_map.keys().cloned().collect();
    let ok = cli_paths == gix_paths;

    let detail = if ok {
        let mut lines: Vec<String> = cli_map
            .iter()
            .map(|(p, c)| format!("{c} {p}"))
            .collect();
        lines.sort();
        format!("{} 个变更路径一致：{}", cli_paths.len(), lines.join(" ; "))
    } else {
        format!("路径集合不一致：cli={cli_paths:?} gix={gix_paths:?}")
    };

    Check {
        suite: "C.status",
        repo: label(fixture),
        ok,
        detail,
    }
}

// ---------------------------------------------------------------------------
// D. 树差异一致性（name-status）
// ---------------------------------------------------------------------------
fn check_diff(path: &Path, spec_a: &str, spec_b: &str) -> Check {
    use gix::diff::tree_with_rewrites::Change;

    let repo = match gix::open(path) {
        Ok(r) => r,
        Err(e) => return fail("D.diff", path, format!("gix::open 失败: {e}")),
    };

    let cli = match git(path, &["diff", "--name-status", spec_a, spec_b]) {
        Ok(s) => s,
        Err(e) => return fail("D.diff", path, e),
    };
    let cli_set: BTreeSet<String> = cli
        .lines()
        .filter_map(|l| {
            let mut it = l.split('\t');
            let code = it.next()?.chars().next()?;
            let dest = it.last()?;
            Some(format!("{code} {dest}"))
        })
        .collect();

    let (tree_a, tree_b) = match (tree_of(&repo, spec_a), tree_of(&repo, spec_b)) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => return fail("D.diff", path, e),
    };

    let changes = match repo.diff_tree_to_tree(Some(&tree_a), Some(&tree_b), None) {
        Ok(c) => c,
        Err(e) => return fail("D.diff", path, format!("gix diff 失败: {e}")),
    };

    let mut gix_set: BTreeSet<String> = BTreeSet::new();
    for ch in &changes {
        let code = match ch {
            Change::Addition { .. } => 'A',
            Change::Deletion { .. } => 'D',
            Change::Modification { .. } => 'M',
            Change::Rewrite { .. } => 'R',
        };
        gix_set.insert(format!("{code} {}", bs(ch.location())));
    }

    let ok = cli_set == gix_set;
    Check {
        suite: "D.diff",
        repo: label(path),
        ok,
        detail: if ok {
            format!("{spec_a}..{spec_b} 的 {} 个变更一致", cli_set.len())
        } else {
            let only_cli: Vec<_> = cli_set.difference(&gix_set).take(3).collect();
            let only_gix: Vec<_> = gix_set.difference(&cli_set).take(3).collect();
            format!("不一致：仅 cli 有 {only_cli:?}；仅 gix 有 {only_gix:?}")
        },
    }
}

fn label(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn fail(suite: &'static str, path: &Path, detail: String) -> Check {
    Check {
        suite,
        repo: label(path),
        ok: false,
        detail,
    }
}

// ---------------------------------------------------------------------------
// fixture：覆盖 staged / unstaged / untracked / deleted 的小仓库
// ---------------------------------------------------------------------------
fn build_fixture(dir: &Path) -> Result<(), String> {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;

    let run = |args: &[&str]| -> Result<(), String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "POC")
            .env("GIT_AUTHOR_EMAIL", "poc@example.com")
            .env("GIT_COMMITTER_NAME", "POC")
            .env("GIT_COMMITTER_EMAIL", "poc@example.com")
            .output()
            .map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    };

    run(&["init", "-q"])?;
    std::fs::write(dir.join("keep.txt"), "line1\nline2\nline3\n").map_err(|e| e.to_string())?;
    std::fs::write(dir.join("modify.txt"), "a\nb\nc\n").map_err(|e| e.to_string())?;
    std::fs::write(dir.join("delete.txt"), "gone soon\n").map_err(|e| e.to_string())?;
    run(&["add", "."])?;
    run(&["commit", "-q", "-m", "init"])?;

    std::fs::write(dir.join("modify.txt"), "a\nB\nc\n").map_err(|e| e.to_string())?;
    std::fs::write(dir.join("keep.txt"), "line1\nline2 changed\nline3\n").map_err(|e| e.to_string())?;
    run(&["add", "keep.txt"])?;
    std::fs::write(dir.join("staged_new.txt"), "new\n").map_err(|e| e.to_string())?;
    run(&["add", "staged_new.txt"])?;
    std::fs::remove_file(dir.join("delete.txt")).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("untracked.txt"), "u\n").map_err(|e| e.to_string())?;

    Ok(())
}

fn main() {
    let base = std::env::args().nth(1).map_or_else(
        || {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../test-repos/repos")
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from("../../test-repos/repos"))
        },
        PathBuf::from,
    );

    println!("=== POC-3 gix 与 git CLI 一致性对比 ===");
    println!("基准仓库根目录: {}\n", base.display());

    let mut results = Vec::new();

    for name in ["small", "medium", "large"] {
        let path = base.join(name);
        if !path.join(".git").exists() {
            continue;
        }
        results.push(check_rev_list(&path));
        results.push(check_log_metadata(&path, 2000));
    }

    let small = base.join("small");
    if small.join(".git").exists() {
        results.push(check_diff(&small, "HEAD~3", "HEAD"));
        results.push(check_diff(&small, "HEAD~1", "HEAD"));
    }

    let fixture = std::env::temp_dir().join("knotwork-poc3-fixture");
    match build_fixture(&fixture) {
        Ok(()) => results.push(check_status(&fixture)),
        Err(e) => results.push(fail("C.status", &fixture, format!("构造 fixture 失败: {e}"))),
    }

    println!("{:<12} {:<10} {:<6} {}", "对比项", "仓库", "结果", "说明");
    println!("{}", "-".repeat(110));
    let mut pass = 0;
    for r in &results {
        if r.ok {
            pass += 1;
        }
        println!(
            "{:<12} {:<10} {:<6} {}",
            r.suite,
            r.repo,
            if r.ok { "PASS" } else { "FAIL" },
            r.detail
        );
    }
    println!("{}", "-".repeat(110));
    println!("合计：{}/{} 通过", pass, results.len());
}
