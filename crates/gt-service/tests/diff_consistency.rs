//! 差异载荷的一致性测试（M0 待办 #5，NFR-COR-01）。
//!
//! `gt-backend` 的套件覆盖状态 / 历史 / 文件级变更，但 diff 的**行数统计**
//! 需要组合 `gt-diff`（架构规划 5.4 的 `numstat` 口径），因此放在这里 ——
//! `gt-service` 同时依赖内核与差异引擎，是唯一能同时看到两者的层。
//!
//! 比对口径是 `git diff --numstat`：它对每个文件给出"新增行数 / 删除行数"，
//! 是 IDEA 与命令行用户都会看的数字，也是"diff 算得对不对"最直接的证据。

use gt_backend::{GitCli, GitRepo, NoopObserver, LOCAL_TIMEOUT};
use gt_diff::DiffOptions;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Command;

/// 一致性用例里读取 blob 的上限（远大于测试内容，保证不会被阈值截断）。
const MAX_BLOB: usize = 8 * 1024 * 1024;

/// 一个临时仓库夹具。
struct Fixture {
    root: PathBuf,
    cli: GitCli,
}

impl Fixture {
    fn new(label: &str) -> Option<Self> {
        let cli = GitCli::detect()?;
        let root = std::env::temp_dir().join(format!(
            "knotwork-diff-consistency-{}-{label}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建临时仓库目录");

        let fixture = Self { root, cli };
        fixture.git(&["init", "-q", "-b", "main"]);
        fixture.git(&["config", "user.email", "diff@knotwork.invalid"]);
        fixture.git(&["config", "user.name", "Knotwork Diff"]);
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

    fn write(&self, rel: &str, content: &str) {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("创建工作区目录");
        }
        std::fs::write(&path, content).unwrap_or_else(|err| panic!("写入 {rel} 失败：{err}"));
    }

    fn remove(&self, rel: &str) {
        std::fs::remove_file(self.root.join(rel))
            .unwrap_or_else(|err| panic!("删除 {rel} 失败：{err}"));
    }

    fn commit_all(&self, message: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
    }

    fn open(&self) -> GitRepo {
        GitRepo::open(&self.root, gt_backend::noop_observer()).expect("打开临时仓库")
    }

    /// `git diff --numstat <old> <new>` 的输出行，已排序。
    fn git_numstat(&self, old: &str, new: &str) -> Vec<String> {
        sorted_lines(&self.git(&["diff", "--numstat", old, new]))
    }

    /// 用本项目的引擎算出同一范围的 numstat 行。
    fn our_numstat(&self, old: Option<&str>, new: &str) -> Vec<String> {
        let repo = self.open();
        let options = DiffOptions::default();
        let changes = repo.tree_changes(old, new).expect("计算文件级变更");

        let mut lines = Vec::with_capacity(changes.len());
        for change in &changes {
            // 重命名必须用**旧路径**去取旧侧内容，否则会被当成"新增了若干行"，
            // 与 CLI 的 `0\t0\t旧 => 新` 不符。
            let old_blob = match old {
                Some(spec) => repo
                    .blob_at(
                        spec,
                        change.old_path.as_deref().unwrap_or(&change.path),
                        MAX_BLOB,
                    )
                    .expect("读取旧侧 blob"),
                // 根提交的旧侧是空树。
                None => None,
            };
            let new_blob = repo
                .blob_at(new, &change.path, MAX_BLOB)
                .expect("读取新侧 blob");

            let diff = gt_diff::diff_bytes(
                change.old_path.as_deref().unwrap_or(&change.path),
                &change.path,
                old_blob.as_deref(),
                new_blob.as_deref(),
                &options,
            );
            // `gt-diff` 的 `numstat_line` 输出 `新增\t删除\t路径`，与 CLI 一致。
            lines.push(diff.numstat_line());
        }
        lines.sort();
        lines
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// 按行切分并排序（CLI 与我们的输出顺序可能不同，内容才是断言对象）。
fn sorted_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    lines.sort();
    lines
}

#[test]
fn numstat_matches_git_cli_across_change_kinds() {
    let Some(fixture) = Fixture::new("numstat") else {
        eprintln!("跳过：未找到 git 可执行文件");
        return;
    };

    // 基线：一个多行文件 + 一个将被重命名的文件。
    fixture.write("edited.txt", "line1\nline2\nline3\nline4\n");
    fixture.write("renamed.txt", "aaa\nbbb\nccc\n");
    fixture.write("deleted.txt", "gone\n");
    fixture.commit_all("基线");
    let base = fixture.open().resolve("HEAD").expect("解析基线").to_hex();

    // 第二轮：修改（增删混合）、新增、删除、纯重命名。
    fixture.write("edited.txt", "line1\nCHANGED\nline3\nline4\nline5\n");
    fixture.write("added.txt", "brand new\nsecond line\n");
    fixture.remove("deleted.txt");
    fixture.git(&["mv", "renamed.txt", "moved.txt"]);
    fixture.commit_all("第二轮");
    let head = fixture.open().resolve("HEAD").expect("解析新提交").to_hex();

    let expected = fixture.git_numstat(&base, &head);
    let actual = fixture.our_numstat(Some(&base), &head);
    assert_eq!(
        actual, expected,
        "numstat 不一致\n  git CLI : {expected:#?}\n  knotwork: {actual:#?}"
    );

    // 断言具体数字，避免"两边都为空"这种假通过。
    assert!(
        expected
            .iter()
            .any(|line| line.starts_with("2\t1\tedited.txt")),
        "修改文件的增删行数应为 2/1：{expected:#?}"
    );
    assert!(
        expected
            .iter()
            .any(|line| line.starts_with("2\t0\tadded.txt")),
        "新增文件应为 2/0：{expected:#?}"
    );
    assert!(
        expected
            .iter()
            .any(|line| line.starts_with("0\t1\tdeleted.txt")),
        "删除文件应为 0/1：{expected:#?}"
    );
}

#[test]
fn numstat_matches_git_cli_for_a_root_commit() {
    let Some(fixture) = Fixture::new("numstat-root") else {
        return;
    };

    fixture.write("a.txt", "one\ntwo\n");
    fixture.write("nested/b.txt", "three\n");
    fixture.commit_all("初始提交");
    let head = fixture
        .open()
        .resolve("HEAD")
        .expect("解析初始提交")
        .to_hex();

    // 根提交的旧侧是空树，CLI 用 `--root` 表示同一口径。
    let expected = sorted_lines(&fixture.git(&[
        "diff-tree",
        "--root",
        "--no-commit-id",
        "-r",
        "--numstat",
        &head,
    ]));
    let actual = fixture.our_numstat(None, &head);
    assert_eq!(
        actual, expected,
        "根提交 numstat 不一致\n  git CLI : {expected:#?}\n  knotwork: {actual:#?}"
    );
    assert!(expected.iter().all(|line| line.contains('\t')));
}

#[test]
fn numstat_flags_binary_files_like_git_cli() {
    let Some(fixture) = Fixture::new("numstat-binary") else {
        return;
    };

    fixture.write("text.txt", "hello\n");
    fixture.commit_all("基线");
    let base = fixture.open().resolve("HEAD").expect("解析基线").to_hex();

    // 含 NUL 字节 → git 判定为二进制，numstat 输出 `-\t-`。
    let binary: Vec<u8> = vec![0x00, 0x01, 0x02, 0x00, 0xff, 0xfe];
    std::fs::write(fixture.root.join("blob.bin"), &binary).expect("写二进制文件");
    fixture.commit_all("加入二进制文件");
    let head = fixture.open().resolve("HEAD").expect("解析新提交").to_hex();

    let expected = fixture.git_numstat(&base, &head);
    let actual = fixture.our_numstat(Some(&base), &head);
    assert_eq!(
        actual, expected,
        "二进制文件的 numstat 不一致\n  git CLI : {expected:#?}\n  knotwork: {actual:#?}"
    );
    assert!(
        expected
            .iter()
            .any(|line| line.starts_with("-\t-\tblob.bin")),
        "二进制文件应输出 `-\\t-`：{expected:#?}"
    );
}

#[test]
fn unified_hunk_headers_survive_a_round_of_edits() {
    let Some(fixture) = Fixture::new("unified") else {
        return;
    };

    // 单一文件、单处修改：hunk 头应当只出现一次且行号可核对。
    fixture.write("a.txt", "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n");
    fixture.commit_all("基线");
    let base = fixture.open().resolve("HEAD").expect("解析基线").to_hex();

    let mut edited = String::new();
    for index in 1..=10 {
        if index == 5 {
            writeln!(edited, "FIVE").expect("写入编辑内容");
        } else {
            writeln!(edited, "{index}").expect("写入编辑内容");
        }
    }
    fixture.write("a.txt", &edited);
    fixture.commit_all("改第 5 行");
    let head = fixture.open().resolve("HEAD").expect("解析新提交").to_hex();

    let repo = fixture.open();
    let pair = repo
        .blob_pair(Some(&base), Some(&head), "a.txt", 1024 * 1024)
        .expect("读取 blob 对");
    let diff = gt_diff::diff_bytes(
        "a.txt",
        "a.txt",
        pair.old.as_deref(),
        pair.new.as_deref(),
        &DiffOptions::default(),
    );

    let text = diff.unified_text();
    assert!(text.contains("@@ -2,7 +2,7 @@"), "hunk 头不符：\n{text}");
    assert!(text.contains("+FIVE"), "应包含新增行：\n{text}");
    assert!(text.contains("-5"), "应包含删除行：\n{text}");
    assert_eq!(diff.stats.added, 1);
    assert_eq!(diff.stats.removed, 1);

    // 与 CLI 的 hunk 头逐字比较：排除 `---`/`+++` 文件头后只剩 `@@` 行。
    let cli = fixture.git(&["diff", "--unified=3", &base, &head]);
    let cli_headers: Vec<&str> = cli.lines().filter(|line| line.starts_with("@@")).collect();
    assert_eq!(
        cli_headers,
        vec!["@@ -2,7 +2,7 @@"],
        "CLI 的 hunk 头与应用内应当一致"
    );
}

/// 让 `Command` 在未使用时不报"未使用导入"（部分平台上夹具会走 git 的绝对路径）。
#[test]
fn git_program_is_discoverable() {
    let Some(fixture) = Fixture::new("discover") else {
        return;
    };
    assert!(!fixture.cli.version().is_empty());
    let direct = Command::new(fixture.cli.program())
        .arg("--version")
        .output()
        .expect("直接调用 git");
    assert!(direct.status.success());
}
