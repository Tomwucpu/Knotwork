//! git 可执行文件探测与子进程执行器。
//!
//! 对应需求 FR-STA-04（启动探测 git；缺失时明确降级为只读）与
//! FR-RMT-05 / 架构规划 7.3（子进程必须有上限、超时可杀、输出流式读取）。
//!
//! M1 只用到版本探测与少量读命令；写路径与网络路径在 M2/M3 扩展，
//! 但进程管理（有界捕获、超时、强制终止）在此一次性做对。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::{BackendError, Result};
use crate::record::{CommandObserver, CommandRecord, CommandRoute, MAX_CAPTURE_BYTES};

/// 本地命令的默认超时（架构规划 7.3）。
pub const LOCAL_TIMEOUT: Duration = Duration::from_secs(30);

/// 网络命令的默认超时（架构规划 7.3）。
pub const NETWORK_TIMEOUT: Duration = Duration::from_secs(120);

/// 审计记录中保留的输出尾部字符数。
const TAIL_CHARS: usize = 4000;

/// 探测可执行文件所用的环境变量覆盖点。
const PROGRAM_ENV: &str = "KNOTWORK_GIT";

/// 一次成功运行的输出。
#[derive(Debug, Clone)]
pub struct CliOutput {
    /// 标准输出（已按 UTF-8 有损解码）。
    pub stdout: String,
    /// 标准错误（已按 UTF-8 有损解码）。
    pub stderr: String,
    /// 退出码；被信号终止时为 [`None`]。
    pub exit_code: Option<i32>,
    /// 输出是否被截断。
    pub truncated: bool,
}

impl CliOutput {
    /// 退出码为 0。
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.exit_code == Some(0)
    }

    /// 按行返回去掉空行的标准输出。
    #[must_use]
    pub fn lines(&self) -> Vec<&str> {
        self.stdout
            .lines()
            .filter(|line| !line.is_empty())
            .collect()
    }
}

/// 已验证可用的 git 可执行文件。
#[derive(Debug, Clone)]
pub struct GitCli {
    program: PathBuf,
    version: String,
}

impl GitCli {
    /// 探测可执行文件；不可用时返回 [`None`]。
    ///
    /// 查找顺序：`KNOTWORK_GIT` 环境变量 → `git`（PATH）。
    /// 探测本身失败（含超时）一律视为"缺失"，触发只读降级而非报错。
    #[must_use]
    pub fn detect() -> Option<Self> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(custom) = std::env::var_os(PROGRAM_ENV) {
            candidates.push(PathBuf::from(custom));
        }
        candidates.push(PathBuf::from("git"));

        for program in candidates {
            if let Some(cli) = Self::probe(program) {
                return Some(cli);
            }
        }
        None
    }

    fn probe(program: PathBuf) -> Option<Self> {
        let output = Command::new(&program)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .ok()?;

        if !output.status.success() {
            return None;
        }

        let text = String::from_utf8_lossy(&output.stdout);
        let version = text
            .trim()
            .strip_prefix("git version ")
            .unwrap_or(text.trim())
            .to_owned();

        Some(Self { program, version })
    }

    /// 可执行文件路径。
    #[must_use]
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// `git --version` 报告出来的版本号。
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// 在 `cwd` 下执行 `git <args>` 并返回捕获到的输出。
    ///
    /// - 输出流式读取并有硬上限（超限截断，不断开管道以免子进程阻塞）；
    /// - 超时后强制终止并返回 [`BackendError::ProcessTimeout`]；
    /// - 无论成功失败都会向 `observer` 投递一条审计记录。
    ///
    /// 非零退出码**不**视为本函数的错误 —— 调用方依据 [`CliOutput::exit_code`]
    /// 决定语义（例如 `git check-ignore` 用退出码表达布尔结果）。
    pub fn run(
        &self,
        cwd: Option<&Path>,
        args: &[&str],
        observer: &dyn CommandObserver,
        timeout: Duration,
    ) -> Result<CliOutput> {
        let began = Instant::now();
        let mut child = self.spawn(cwd, args, observer, began)?;

        let stdout = child.stdout.take().expect("已配置 piped stdout");
        let stderr = child.stderr.take().expect("已配置 piped stderr");
        let stdout_reader = std::thread::spawn(move || read_capped(stdout));
        let stderr_reader = std::thread::spawn(move || read_capped(stderr));

        let timed_out = match wait_with_timeout(&mut child, timeout) {
            Ok(timed_out) => timed_out,
            Err(err) => {
                // 先终止进程再收管道，否则读取线程可能永远等不到 EOF。
                let _ = child.kill();
                let _ = child.wait();
                let (_, _) = join_reader(stdout_reader);
                let (_, _) = join_reader(stderr_reader);
                let error_text = err.to_string();
                observer.observe(&cli_record(
                    self,
                    cwd,
                    args,
                    &RunTrace {
                        duration: began.elapsed(),
                        exit_code: None,
                        stdout: None,
                        stderr: None,
                        truncated: false,
                        error: Some(&error_text),
                    },
                ));
                return Err(BackendError::Process(error_text));
            }
        };

        // 必须先收完管道再 join（读取线程持有管道读端）。
        let (stdout_bytes, stdout_truncated) = join_reader(stdout_reader);
        let (stderr_bytes, stderr_truncated) = join_reader(stderr_reader);

        let duration = began.elapsed();
        let exit_code = child.wait().ok().and_then(|status| status.code());
        let stdout_text = String::from_utf8_lossy(&stdout_bytes).into_owned();
        let stderr_text = String::from_utf8_lossy(&stderr_bytes).into_owned();

        if timed_out {
            let timeout_note = format!("超时 {}s", timeout.as_secs());
            observer.observe(&cli_record(
                self,
                cwd,
                args,
                &RunTrace {
                    duration,
                    exit_code,
                    stdout: Some(&stdout_text),
                    stderr: Some(&stderr_text),
                    truncated: stdout_truncated || stderr_truncated,
                    error: Some(&timeout_note),
                },
            ));
            return Err(BackendError::ProcessTimeout {
                seconds: timeout.as_secs(),
                program: self.program.display().to_string(),
                args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            });
        }

        observer.observe(&cli_record(
            self,
            cwd,
            args,
            &RunTrace {
                duration,
                exit_code,
                stdout: Some(&stdout_text),
                stderr: Some(&stderr_text),
                truncated: stdout_truncated || stderr_truncated,
                error: None,
            },
        ));

        Ok(CliOutput {
            stdout: stdout_text,
            stderr: stderr_text,
            exit_code,
            truncated: stdout_truncated || stderr_truncated,
        })
    }

    /// 启动子进程；失败时投递一条审计记录并返回 [`BackendError::Process`]。
    fn spawn(
        &self,
        cwd: Option<&Path>,
        args: &[&str],
        observer: &dyn CommandObserver,
        began: Instant,
    ) -> Result<std::process::Child> {
        let mut command = Command::new(&self.program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = cwd {
            command.current_dir(dir);
        }

        command.spawn().map_err(|err| {
            let error_text = err.to_string();
            observer.observe(&cli_record(
                self,
                cwd,
                args,
                &RunTrace {
                    duration: began.elapsed(),
                    exit_code: None,
                    stdout: None,
                    stderr: None,
                    truncated: false,
                    error: Some(&error_text),
                },
            ));
            BackendError::Process(error_text)
        })
    }
}

/// 等待子进程结束；超时则强制终止。
///
/// 返回 `true` 表示发生了超时。**不**在此处收敛退出码：Windows 上 `try_wait`
/// 已收割过状态，随后 `Child::wait()` 会直接取回缓存结果。
fn wait_with_timeout(child: &mut std::process::Child, timeout: Duration) -> std::io::Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            return Ok(false);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// 读取管道直到 EOF，最多保留 [`MAX_CAPTURE_BYTES`] 字节。
///
/// 即使超出上限也继续读取（丢弃多余数据），否则子进程会因管道写满而阻塞。
fn read_capped(mut reader: impl Read) -> (Vec<u8>, bool) {
    let mut captured: Vec<u8> = Vec::new();
    let mut truncated = false;
    let mut chunk = [0u8; 16 * 1024];

    loop {
        match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let room = MAX_CAPTURE_BYTES.saturating_sub(captured.len());
                if room == 0 {
                    truncated = true;
                    continue;
                }
                let take = room.min(read);
                captured.extend_from_slice(&chunk[..take]);
                if take < read {
                    truncated = true;
                }
            }
        }
    }

    (captured, truncated)
}

fn join_reader(handle: std::thread::JoinHandle<(Vec<u8>, bool)>) -> (Vec<u8>, bool) {
    handle.join().unwrap_or_default()
}

/// 一次运行的收尾信息，用于组装审计记录。
///
/// 打包成结构体是为了让 [`cli_record`] 保持在 clippy 的参数个数上限内，
/// 同时避免调用点出现一长串 `None`。
struct RunTrace<'a> {
    /// 从发起到收尾的耗时。
    duration: Duration,
    /// 退出码；被信号终止或未启动时为 [`None`]。
    exit_code: Option<i32>,
    /// 标准输出（已解码）。
    stdout: Option<&'a str>,
    /// 标准错误（已解码）。
    stderr: Option<&'a str>,
    /// 输出是否被硬上限截断。
    truncated: bool,
    /// 失败原因。
    error: Option<&'a str>,
}

/// 组装一条 CLI 审计记录。
fn cli_record(
    cli: &GitCli,
    cwd: Option<&Path>,
    args: &[&str],
    trace: &RunTrace<'_>,
) -> CommandRecord {
    let program = cli.program.display().to_string();
    let mut command_line = program.clone();
    for arg in args {
        command_line.push(' ');
        command_line.push_str(arg);
    }

    CommandRecord {
        route: CommandRoute::Cli,
        program: Some(program),
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        command_line,
        cwd: cwd.map(Path::to_path_buf),
        exit_code: trace.exit_code,
        duration: trace.duration,
        stdout_tail: trace.stdout.map(tail),
        stderr_tail: trace.stderr.map(tail),
        truncated: trace.truncated,
        error: trace.error.map(str::to_owned),
    }
}

/// 取文本尾部，避免审计记录持有整段输出。
fn tail(text: &str) -> String {
    let trimmed = text.trim_end();
    if trimmed.chars().count() <= TAIL_CHARS {
        return trimmed.to_owned();
    }
    let skip = trimmed.chars().count() - TAIL_CHARS;
    let tail: String = trimmed.chars().skip(skip).collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::{tail, GitCli, LOCAL_TIMEOUT, NETWORK_TIMEOUT, TAIL_CHARS};
    use crate::record::{CommandObserver, CommandRecord};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// 收集记录的测试观察者。
    #[derive(Default)]
    struct Collector {
        records: Mutex<Vec<CommandRecord>>,
    }

    impl CommandObserver for Collector {
        fn observe(&self, record: &CommandRecord) {
            self.records
                .lock()
                .expect("锁未被污染")
                .push(record.clone());
        }
    }

    #[test]
    fn timeouts_follow_architecture_constraints() {
        assert_eq!(LOCAL_TIMEOUT.as_secs(), 30);
        assert_eq!(NETWORK_TIMEOUT.as_secs(), 120);
    }

    #[test]
    fn tail_truncates_long_text() {
        let short = "abc";
        assert_eq!(tail(short), "abc");

        let long = "x".repeat(TAIL_CHARS + 100);
        let result = tail(&long);
        assert!(result.starts_with('…'));
        assert_eq!(result.chars().count(), TAIL_CHARS + 1);
    }

    #[test]
    fn tail_keeps_trailing_content() {
        let mut text = "y".repeat(TAIL_CHARS);
        text.push_str("MARKER");
        let result = tail(&text);
        assert!(result.ends_with("MARKER"));
    }

    #[test]
    fn detection_matches_environment() {
        // 本机开发环境已安装 git（阶段 0 已确认 2.55）；若缺失则跳过断言。
        match GitCli::detect() {
            Some(cli) => {
                assert!(!cli.version().is_empty(), "版本号不应为空");
                assert!(!cli.program().as_os_str().is_empty());
            }
            None => eprintln!("未检测到 git，跳过版本断言"),
        }
    }

    #[test]
    fn run_records_audit_entry() {
        let Some(cli) = GitCli::detect() else {
            eprintln!("未检测到 git，跳过执行用例");
            return;
        };

        let collector = Arc::new(Collector::default());
        let output = cli
            .run(
                None,
                &["--version"],
                collector.as_ref(),
                Duration::from_secs(20),
            )
            .expect("git --version 应执行成功");

        assert!(output.is_success());
        assert!(output.stdout.contains("git version"));
        assert!(!output.truncated);

        let records = collector.records.lock().expect("锁未被污染");
        assert_eq!(records.len(), 1, "每次执行都应留下一条审计记录");
        let record = &records[0];
        assert_eq!(record.exit_code, Some(0));
        assert_eq!(
            record.program.as_deref(),
            Some(cli.program().display().to_string().as_str())
        );
        assert!(record.summary_line().contains("git --version"));
        assert!(record.reproducible_line().starts_with("git --version"));
    }

    #[test]
    fn failing_command_still_records_and_returns_output() {
        let Some(cli) = GitCli::detect() else {
            eprintln!("未检测到 git，跳过执行用例");
            return;
        };

        let collector = Arc::new(Collector::default());
        let output = cli
            .run(
                None,
                &["rev-parse", "--verify", "refs/heads/绝不存在"],
                collector.as_ref(),
                Duration::from_secs(20),
            )
            .expect("非零退出码不应成为函数级错误");

        assert!(!output.is_success());
        let records = collector.records.lock().expect("锁未被污染");
        assert_eq!(records.len(), 1);
        assert!(!records[0].is_success());
        assert!(records[0].stderr_tail.is_some());
    }
}
