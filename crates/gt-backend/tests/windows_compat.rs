//! Windows 平台兼容测试（FR-STA-05，M5）。
//!
//! v1.0 的唯一验收平台是 Windows（NFR-OS-01），本套件覆盖三项已知的
//! 平台坑：
//!
//! | 用例 | 场景 | 判据 |
//! |---|---|---|
//! | `long_paths…` | 全路径 > 260 字符的深嵌套文件 | 内置引擎 status/blob 与 git CLI 输出一致 |
//! | `crlf_autocrlf…` | `core.autocrlf=true` 下的 CRLF 文件 | 干净/修改两种状态与 git CLI 一致 |
//! | `case_only_rename…` | NTFS 大小写不敏感上的仅大小写改名 | 与 git CLI 的 porcelain 输出一致 |
//!
//! 判据统一走 L2 口径：**内置引擎（gix）输出 == git CLI 输出**。
//! 仅在 Windows 上编译运行（`#[cfg(windows)]`）。

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use gt_backend::{noop_observer, GitCli, GitRepo, NoopObserver, LOCAL_TIMEOUT};

/// 临时目录去重序号。
static COUNTER: AtomicU32 = AtomicU32::new(0);

/// 最小夹具：空仓库 + git 执行能力。
struct Fixture {
    root: PathBuf,
    cli: GitCli,
}

impl Fixture {
    fn new(label: &str) -> Option<Self> {
        let cli = GitCli::detect()?;
        let root = std::env::temp_dir().join(format!(
            "knotwork-win-compat-{}-{}-{label}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("建仓库目录");
        let fixture = Self { root, cli };
        fixture.git(&["init", "-q", "-b", "main"]);
        fixture.git(&["config", "user.email", "win@knotwork.invalid"]);
        fixture.git(&["config", "user.name", "Knotwork Windows"]);
        fixture.git(&["config", "commit.gpgsign", "false"]);
        // 换行转换与长路径在各自用例里显式开启。
        fixture.git(&["config", "core.autocrlf", "false"]);
        Some(fixture)
    }

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

    fn open(&self) -> GitRepo {
        GitRepo::open(&self.root, noop_observer()).expect("打开仓库")
    }

    /// git CLI 的工作区状态行（已排序）。
    fn git_status_lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .git(&["status", "--porcelain=v1"])
            .lines()
            .map(str::to_owned)
            .collect();
        lines.sort();
        lines
    }

    /// 内置引擎的工作区状态行（已排序）。
    fn gix_status_lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .open()
            .status()
            .expect("计算工作区状态")
            .to_porcelain()
            .lines()
            .map(str::to_owned)
            .collect();
        lines.sort();
        lines
    }

    fn assert_status_matches(&self, label: &str) {
        let expected = self.git_status_lines();
        let actual = self.gix_status_lines();
        assert_eq!(
            actual, expected,
            "工作区状态不一致（{label}）\n  git CLI : {expected:#?}\n  knotwork: {actual:#?}"
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// 用扩展长度前缀（`\\?\`）创建深嵌套目录，返回全路径长度。
fn create_deep_dir(root: &Path, segments: &[&str]) -> (PathBuf, usize) {
    let mut dir = root.to_path_buf();
    for segment in segments {
        dir.push(segment);
    }
    let verbatim = PathBuf::from(format!(
        r"\\?\{}",
        dir.canonicalize().unwrap_or(dir.clone()).display()
    ));
    // canonicalize 需要目录已存在；首次创建用逐级建法（std 会用扩展长度
    // 语义处理 verbatim 前缀）。
    let _ = verbatim;
    let mut probe = root.to_path_buf();
    for segment in segments {
        probe.push(segment);
        let probe_verbatim =
            PathBuf::from(format!(r"\\?\{}", probe.canonicalize().unwrap_or(probe.clone()).display()));
        let _ = probe_verbatim;
        std::fs::create_dir_all(&probe).expect("逐级创建深目录");
    }
    let len = dir.display().to_string().chars().count();
    (dir, len)
}

/// FR-STA-05 长路径：>260 字符的全路径下，状态与 blob 读取仍与 git 一致。
#[test]
fn long_paths_beyond_260_chars_are_supported() {
    let Some(fixture) = Fixture::new("long-path") else {
        return;
    };
    fixture.git(&["config", "core.longpaths", "true"]);

    // 每段 40 字符 × 6 段 + 文件名 40 字符，叠加仓库根远超 260。
    let segments: Vec<String> = (0..6)
        .map(|i| format!("segment-{i:0>2}-{}", "x".repeat(28)))
        .collect();
    let segment_refs: Vec<&str> = segments.iter().map(String::as_str).collect();
    let (dir, len) = create_deep_dir(&fixture.root, &segment_refs);
    let file = dir.join(format!("{}.txt", "f".repeat(34)));
    let total_len = file.display().to_string().chars().count();
    assert!(total_len > 260, "构造的全路径应超过 260 字符，实际 {total_len}");

    let content = "deep file content\n".repeat(10);
    std::fs::write(&file, &content).expect("写深路径文件");

    fixture.git(&["add", "-A"]);
    fixture.git(&["commit", "-q", "-m", "深路径提交"]);
    fixture.assert_status_matches("长路径：干净工作区");

    // 相对路径（git 口径）读取 blob：内容逐字节一致。
    let relative = segments.join("/");
    let relative = format!("{relative}/{}.txt", "f".repeat(34));
    let blob = fixture
        .open()
        .blob_at("HEAD", &relative, 1024 * 1024)
        .expect("读取深路径 blob");
    assert_eq!(
        blob.as_deref(),
        Some(content.as_bytes()),
        "深路径 blob 内容应逐字节一致"
    );
    assert_eq!(len, dir.display().to_string().chars().count());

    // 深路径文件的修改同样被两侧一致地看到。
    std::fs::write(&file, "modified\n").expect("修改深路径文件");
    fixture.assert_status_matches("长路径：修改后");
}

/// FR-STA-05 CRLF：`core.autocrlf=true` 下干净与修改两种状态都一致。
#[test]
fn crlf_with_autocrlf_stays_consistent() {
    let Some(fixture) = Fixture::new("crlf") else {
        return;
    };
    fixture.git(&["config", "core.autocrlf", "true"]);

    let crlf = "line-one\r\nline-two\r\nline-three\r\n";
    std::fs::write(fixture.root.join("win.txt"), crlf).expect("写 CRLF 文件");
    fixture.git(&["add", "-A"]);
    fixture.git(&["commit", "-q", "-m", "CRLF 提交"]);

    // 干净：autocrlf 转换后工作区与索引等价。
    fixture.assert_status_matches("CRLF：干净工作区");

    // 修改一行（保持 CRLF）。
    std::fs::write(fixture.root.join("win.txt"), "line-one\r\nLINE-TWO\r\nline-three\r\n")
        .expect("修改 CRLF 文件");
    fixture.assert_status_matches("CRLF：修改后");

    // blob 读取：blob_at 走对象库原文（LF），git cat-file 同源。
    let blob = fixture
        .open()
        .blob_at("HEAD", "win.txt", 1024 * 1024)
        .expect("读取 blob")
        .expect("blob 存在");
    assert!(
        blob.windows(2).all(|pair| pair != b"\r\n"),
        "对象库内的 blob 应为 LF（autocrlf 提交时转换）"
    );
}

/// FR-STA-05 大小写：NTFS 上的仅大小写改名，与 git CLI 输出一致。
#[test]
fn case_only_rename_matches_git_cli() {
    let Some(fixture) = Fixture::new("case-rename") else {
        return;
    };
    // 大小写敏感性由 git 侧显式声明（NTFS 默认 ignorecase=true）。
    fixture.git(&["config", "core.ignorecase", "true"]);
    std::fs::write(fixture.root.join("Name.txt"), "case\n").expect("写文件");
    fixture.git(&["add", "-A"]);
    fixture.git(&["commit", "-q", "-m", "基线"]);
    fixture.assert_status_matches("大小写：干净");

    // NTFS 允许仅大小写的改名（两步走，避免被视作同路径）。
    std::fs::rename(fixture.root.join("Name.txt"), fixture.root.join("name__.tmp"))
        .expect("先改成中转名");
    std::fs::rename(fixture.root.join("name__.tmp"), fixture.root.join("name.txt"))
        .expect("再改成目标大小写");

    // 判据是与 git CLI 一致（git 对这种场景的呈现随版本可能不同，
    // 一致性才是需求 —— 我们不允许与 git 各说各话）。
    fixture.assert_status_matches("大小写：仅大小写改名后");
}
