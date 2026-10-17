//! git 可执行文件探测与子进程执行器。
//!
//! 对应需求 FR-STA-04（启动探测 git；缺失时明确降级为只读）与
//! FR-RMT-05 / 架构规划 3.3（子进程必须有上限、超时可杀、输出流式读取）。
//!
//! # 三条硬约束及其实现
//!
//! 1. **并发子进程 ≤ 4**（[`MAX_CONCURRENT_PROCESSES`]）：进程级信号量，
//!    跨仓库、跨请求共享。少了它，一次"拉取所有分支"就可能瞬间起十几个 git
//!    把内存吃光 —— 这与本工具的卖点直接冲突。
//! 2. **超时可杀且杀干净**：本地 30s、网络 120s（[`LOCAL_TIMEOUT`]、
//!    [`NETWORK_TIMEOUT`]）。终止时先杀**进程树**：git 会派生 `ssh`、
//!    `git-remote-https`、`git-upload-pack` 等子进程，只 kill 直接子进程
//!    会留下一串孤儿（NFR-COR-02 的验收项就是"取消后无残留 git 进程"）。
//! 3. **输出流式处理**：捕获有硬上限（超限截断但继续读，避免子进程因管道写满
//!    而阻塞）；网络操作额外把 stderr 进度实时转给调用方（[`ProgressSink`]）。
//!
//! # 审计与脱敏
//!
//! 每条命令都会产出一条审计记录（FR-CON-01）。网络命令额外打上
//! `network` 标记，并且**在记录落地前脱敏**（[`crate::credential::redact`]）：
//! git 的错误信息里经常原样回显带 token 的 URL，Console 会被导出、贴进 issue，
//! 因此脱敏必须发生在写入环形缓冲之前，而不是"展示时再处理"。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::credential::redact;
use crate::error::{BackendError, Result};
use crate::record::{CommandObserver, CommandRecord, CommandRoute, MAX_CAPTURE_BYTES};

/// 本地命令的默认超时（架构规划 7.3）。
pub const LOCAL_TIMEOUT: Duration = Duration::from_secs(30);

/// 网络命令的默认超时（架构规划 7.3）。
pub const NETWORK_TIMEOUT: Duration = Duration::from_secs(120);

/// 同时运行的 git 子进程上限（架构规划 3.3）。
pub const MAX_CONCURRENT_PROCESSES: usize = 4;

/// 等待并发槽位的最长时间。
///
/// 超过即报错而不是无限等待：四个槽位都被长时网络操作占住时，
/// 再排队 120s 的操作既没有意义（用户早走了），也会让界面显示"卡住"。
const LIMIT_WAIT: Duration = Duration::from_secs(30);

/// 审计记录中保留的输出尾部字符数。
const TAIL_CHARS: usize = 4000;

/// 探测可执行文件所用的环境变量覆盖点。
const PROGRAM_ENV: &str = "KNOTWORK_GIT";

/// 取消探测：调用方（服务层）的取消令牌只需实现这一个方法。
///
/// 用 trait 而不是把 `gt-service` 的令牌类型搬进来，是为了保持依赖方向
/// `gt-service → gt-backend` 单向：后端不认识调度层的类型。
pub trait CancelProbe {
    /// 是否已被取消。
    fn is_cancelled(&self) -> bool;
}

impl<F> CancelProbe for F
where
    F: Fn() -> bool,
{
    fn is_cancelled(&self) -> bool {
        self()
    }
}

/// 永不取消的探针，用于无需取消的操作。
struct NeverCancel;

impl CancelProbe for NeverCancel {
    fn is_cancelled(&self) -> bool {
        false
    }
}

/// 进度回调。
///
/// 网络命令（fetch/pull/push）的 stderr 是**进度流**而不是一次性输出：
/// git 用 `\r` 反复覆盖同一行（"Receiving objects: 45% (1234/2700)"）。
/// 实现方必须足够轻量（它在读取线程上被同步调用），且应把最新一行存进
/// 某个共享状态，由 UI 线程按帧读取 —— 不要在这里做界面更新。
pub trait ProgressSink: Send + Sync {
    /// 收到一行进度文本（已去掉 `\r` / `\n`）。
    fn progress(&self, line: &str);
}

impl<F> ProgressSink for F
where
    F: Fn(&str) + Send + Sync,
{
    fn progress(&self, line: &str) {
        self(line);
    }
}

/// 网络命令的执行选项。
///
/// 单列一个结构体而不是继续给 `run_*` 加参数：网络命令需要的
/// "取消 + 进度 + 额外环境变量"三件套在本地命令里都不存在，
/// 混在一个签名里会让本地调用点塞满 `None`。
#[derive(Default)]
pub struct NetworkOptions<'a> {
    /// 取消探针；[`None`] 表示不可取消。
    pub cancel: Option<&'a dyn CancelProbe>,
    /// 进度回调；[`None`] 表示只捕获不流式。
    pub progress: Option<Arc<dyn ProgressSink>>,
    /// 追加给子进程的环境变量（例如 askpass 的凭据注入）。
    pub env: Vec<(String, String)>,
}

impl std::fmt::Debug for NetworkOptions<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetworkOptions")
            .field("cancellable", &self.cancel.is_some())
            .field("streaming", &self.progress.is_some())
            // 只报变量名：值可能是口令。
            .field(
                "env",
                &self.env.iter().map(|(key, _)| key).collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// 一次成功运行的输出。
#[derive(Debug, Clone)]
pub struct CliOutput {
    /// 标准输出（已按 UTF-8 有损解码）。
    pub stdout: String,
    /// 标准输出的**原始字节**。
    ///
    /// 只有 `git show :2:path` 这类"读对象内容"的命令需要它：有损解码会把
    /// 非 UTF-8 的 blob（图片、GBK 文本）改坏，而冲突解决要求字节保真。
    pub stdout_bytes: Vec<u8>,
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

    /// 合并 stdout 与 stderr（先 stdout 后 stderr，去掉空行）。
    ///
    /// git 的失败信息几乎全在 stderr，成功信息常在 stdout；用户要的是
    /// "能贴进 issue 的完整输出"，因此这里刻意合并两路。
    #[must_use]
    pub fn merged(&self) -> String {
        let mut out = String::new();
        for chunk in [self.stdout.trim_end(), self.stderr.trim_end()] {
            if chunk.is_empty() {
                continue;
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(chunk);
        }
        out
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
    /// - 超时后强制终止进程树并返回 [`BackendError::ProcessTimeout`]；
    /// - 无论成功失败都会向 `observer` 投递一条审计记录（已脱敏）。
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
        self.run_full(
            cwd,
            args,
            None,
            observer,
            &ExecOptions {
                timeout,
                cancel: None,
                progress: None,
                env: Vec::new(),
                network: false,
            },
        )
    }

    /// 从标准输入喂入数据后执行（提交消息、补丁都用这条路径）。
    ///
    /// 用 stdin 而不是命令行参数传消息与补丁，原因有二：参数长度有平台上限，
    /// 且多行文本经 shell 转义后极易出错（提交消息里的引号、CRLF）。
    pub fn run_with_input(
        &self,
        cwd: Option<&Path>,
        args: &[&str],
        input: &[u8],
        observer: &dyn CommandObserver,
        timeout: Duration,
    ) -> Result<CliOutput> {
        self.run_full(
            cwd,
            args,
            Some(input),
            observer,
            &ExecOptions {
                timeout,
                cancel: None,
                progress: None,
                env: Vec::new(),
                network: false,
            },
        )
    }

    /// 可取消的执行（NFR-COR-02：被取消的操作不留子进程）。
    ///
    /// # Errors
    /// 取消时返回 [`BackendError::Cancelled`]，超时见 [`BackendError::ProcessTimeout`]。
    pub fn run_cancellable(
        &self,
        cwd: Option<&Path>,
        args: &[&str],
        input: Option<&[u8]>,
        observer: &dyn CommandObserver,
        timeout: Duration,
        cancel: &dyn CancelProbe,
    ) -> Result<CliOutput> {
        self.run_full(
            cwd,
            args,
            input,
            observer,
            &ExecOptions {
                timeout,
                cancel: Some(cancel),
                progress: None,
                env: Vec::new(),
                network: false,
            },
        )
    }

    /// 执行一次网络命令（FR-RMT-05）。
    ///
    /// 与 [`GitCli::run`] 的差别有四处，每一处都对应一个真实的坑：
    ///
    /// 1. 超时用 [`NETWORK_TIMEOUT`]（120s）—— 30s 对冷克隆/大仓库 fetch 不够；
    /// 2. 默认 `GIT_TERMINAL_PROMPT=0`：没有终端可交互，git 一旦尝试读输入
    ///    就会挂到超时，用户看到的是"卡死 120 秒"；
    /// 3. `LC_ALL=C`：我们按英文关键字分类失败（[`crate::credential::diagnose`]），
    ///    若 git 输出被本地化，分类会集体失效；
    /// 4. stderr 实时流式转给 `progress`，否则进度条只能等命令结束才动。
    ///
    /// # Errors
    /// 取消 → [`BackendError::Cancelled`]；超时 → [`BackendError::ProcessTimeout`]。
    pub fn run_network(
        &self,
        cwd: Option<&Path>,
        args: &[&str],
        observer: &dyn CommandObserver,
        options: NetworkOptions<'_>,
    ) -> Result<CliOutput> {
        let mut env = vec![
            ("GIT_TERMINAL_PROMPT".to_owned(), "0".to_owned()),
            ("LC_ALL".to_owned(), "C".to_owned()),
            ("LANG".to_owned(), "C".to_owned()),
        ];
        env.extend(options.env);
        self.run_full(
            cwd,
            args,
            None,
            observer,
            &ExecOptions {
                timeout: NETWORK_TIMEOUT,
                cancel: options.cancel,
                progress: options.progress,
                env,
                network: true,
            },
        )
    }

    /// 可取消、可注入环境变量的本地执行（M4：交互式变基的编辑器注入）。
    ///
    /// 与 [`GitCli::run_network`] 的区别：不写死网络相关的三个变量、超时由调用方
    /// 决定。M4 只用来注入 `GIT_SEQUENCE_EDITOR` / `GIT_EDITOR`，以及把
    /// `GIT_EDITOR` 固定成 `true` 以避免"没有终端却等编辑器"的死锁。
    ///
    /// # Errors
    /// 取消 → [`BackendError::Cancelled`]；超时 → [`BackendError::ProcessTimeout`]。
    pub fn run_with_env(
        &self,
        cwd: Option<&Path>,
        args: &[&str],
        env: Vec<(String, String)>,
        observer: &dyn CommandObserver,
        timeout: Duration,
        cancel: Option<&dyn CancelProbe>,
    ) -> Result<CliOutput> {
        self.run_full(
            cwd,
            args,
            None,
            observer,
            &ExecOptions {
                timeout,
                cancel,
                progress: None,
                env,
                network: false,
            },
        )
    }

    /// 执行的核心实现。
    ///
    /// 长度换来的是一条路径只做一次：spawn → 写入 stdin → 双路读取 → 等待 →
    /// 收尾 → 审计。把它拆成若干小函数会让"取消时到底有没有 kill、有没有
    /// 释放并发槽位"变得难以确认，而这正是本模块最容易出错的地方。
    #[allow(clippy::too_many_lines)]
    fn run_full(
        &self,
        cwd: Option<&Path>,
        args: &[&str],
        input: Option<&[u8]>,
        observer: &dyn CommandObserver,
        options: &ExecOptions<'_>,
    ) -> Result<CliOutput> {
        let began = Instant::now();
        let probe = options.cancel.unwrap_or(&NeverCancel);

        // 并发槽位必须在 spawn **之前**拿到：先起进程再排队等于没有上限。
        let _permit = acquire_slot(probe)?;

        let mut child = self.spawn(cwd, args, input.is_some(), observer, began, options)?;

        // 输入在线程里写：补丁可能远大于管道缓冲，同步写会与"读输出"互锁。
        let stdin_writer = match (input, child.stdin.take()) {
            (Some(bytes), Some(mut pipe)) => {
                let owned = bytes.to_vec();
                Some(std::thread::spawn(move || {
                    use std::io::Write as _;
                    let _ = pipe.write_all(&owned);
                    // 关闭管道：git 靠 EOF 判定输入结束。
                }))
            }
            _ => None,
        };

        let stdout = child.stdout.take().expect("已配置 piped stdout");
        let stderr = child.stderr.take().expect("已配置 piped stderr");
        let stdout_reader = std::thread::spawn(move || read_capped(stdout));
        let progress = options.progress.clone();
        let stderr_reader =
            std::thread::spawn(move || read_capped_streaming(stderr, progress.as_ref()));

        let waited = match wait_with_timeout(&mut child, options.timeout, probe) {
            Ok(outcome) => outcome,
            Err(err) => {
                // 先终止进程树再收管道，否则读取线程可能永远等不到 EOF。
                kill_tree(&mut child);
                // 收尾用有界等待：若进程树没被完全终止（例如 taskkill 失败，
                // 或子进程把管道继承给了孙进程），无限 join 会让"取消"变成
                // "等到命令自己结束"—— 那正是用户最不能接受的。
                let (stdout_bytes, stdout_truncated) = join_reader_quiet(stdout_reader);
                let (stderr_bytes, stderr_truncated, _) = join_stream_reader_quiet(stderr_reader);
                let error_text = err.to_string();
                let stdout_text = String::from_utf8_lossy(&stdout_bytes).into_owned();
                let stderr_text = String::from_utf8_lossy(&stderr_bytes).into_owned();
                observer.observe(&self.record(
                    cwd,
                    args,
                    &RunTrace {
                        duration: began.elapsed(),
                        exit_code: None,
                        stdout: Some(&stdout_text),
                        stderr: Some(&stderr_text),
                        truncated: stdout_truncated || stderr_truncated,
                        error: Some(&error_text),
                    },
                    options.network,
                ));
                return Err(BackendError::Process(error_text));
            }
        };

        // 必须先收完管道再 join（读取线程持有管道读端）。
        // 正常结束时无限等待是正确的；被终止时改用有界等待（见下方 abort 分支）。
        let (stdout_bytes, stdout_truncated) = if waited == WaitOutcome::Exited {
            join_reader(stdout_reader)
        } else {
            join_reader_quiet(stdout_reader)
        };
        let (stderr_bytes, stderr_truncated, _last_line) = if waited == WaitOutcome::Exited {
            join_stream_reader(stderr_reader)
        } else {
            join_stream_reader_quiet(stderr_reader)
        };
        if let Some(writer) = stdin_writer {
            // 子进程已退出，写端可能已被打断；这里的错误无关紧要。
            let _ = writer.join();
        }

        let duration = began.elapsed();
        let exit_code = child.wait().ok().and_then(|status| status.code());
        let stdout_text = String::from_utf8_lossy(&stdout_bytes).into_owned();
        let stderr_text = String::from_utf8_lossy(&stderr_bytes).into_owned();
        let truncated = stdout_truncated || stderr_truncated;

        // 取消与超时都走"先记录再报错"的同一收尾路径：否则 Console 里会缺一条
        // 用户实际触发过的操作记录，审计就断了。
        let abort_reason = match waited {
            WaitOutcome::Exited => None,
            WaitOutcome::TimedOut => Some(format!("超时 {}s", options.timeout.as_secs())),
            WaitOutcome::Cancelled => Some("已取消".to_owned()),
        };

        if let Some(reason) = &abort_reason {
            observer.observe(&self.record(
                cwd,
                args,
                &RunTrace {
                    duration,
                    exit_code,
                    stdout: Some(&stdout_text),
                    stderr: Some(&stderr_text),
                    truncated,
                    error: Some(reason),
                },
                options.network,
            ));
            return Err(match waited {
                WaitOutcome::Cancelled => BackendError::Cancelled,
                _ => BackendError::ProcessTimeout {
                    seconds: options.timeout.as_secs(),
                    program: self.program.display().to_string(),
                    args: args.iter().map(|arg| (*arg).to_owned()).collect(),
                },
            });
        }

        observer.observe(&self.record(
            cwd,
            args,
            &RunTrace {
                duration,
                exit_code,
                stdout: Some(&stdout_text),
                stderr: Some(&stderr_text),
                truncated,
                error: None,
            },
            options.network,
        ));

        Ok(CliOutput {
            stdout: stdout_text,
            stdout_bytes,
            stderr: stderr_text,
            exit_code,
            truncated,
        })
    }

    /// 启动子进程；失败时投递一条审计记录并返回 [`BackendError::Process`]。
    fn spawn(
        &self,
        cwd: Option<&Path>,
        args: &[&str],
        with_stdin: bool,
        observer: &dyn CommandObserver,
        began: Instant,
        options: &ExecOptions<'_>,
    ) -> Result<std::process::Child> {
        let mut command = Command::new(&self.program);
        command
            .args(args)
            .stdin(if with_stdin {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = cwd {
            command.current_dir(dir);
        }
        for (key, value) in &options.env {
            command.env(key, value);
        }

        command.spawn().map_err(|err| {
            let error_text = err.to_string();
            observer.observe(&self.record(
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
                options.network,
            ));
            BackendError::Process(error_text)
        })
    }

    /// 组装一条审计记录（含脱敏）。
    fn record(
        &self,
        cwd: Option<&Path>,
        args: &[&str],
        trace: &RunTrace<'_>,
        network: bool,
    ) -> CommandRecord {
        let program = self.program.display().to_string();
        let mut command_line = program.clone();
        for arg in args {
            command_line.push(' ');
            command_line.push_str(arg);
        }

        CommandRecord {
            route: CommandRoute::Cli,
            program: Some(program),
            // 参数与命令行都过一遍脱敏：远端 URL 里的 token 常常就写在参数里。
            args: args.iter().map(|arg| redact(arg)).collect(),
            command_line: redact(&command_line),
            cwd: cwd.map(Path::to_path_buf),
            exit_code: trace.exit_code,
            duration: trace.duration,
            stdout_tail: trace.stdout.map(|text| tail(&redact(text))),
            stderr_tail: trace.stderr.map(|text| tail(&redact(text))),
            truncated: trace.truncated,
            error: trace.error.map(redact),
            network,
        }
    }
}

/// 一次执行的可选参数（内部用）。
struct ExecOptions<'a> {
    timeout: Duration,
    cancel: Option<&'a dyn CancelProbe>,
    progress: Option<Arc<dyn ProgressSink>>,
    env: Vec<(String, String)>,
    network: bool,
}

/// 等待结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitOutcome {
    /// 正常结束。
    Exited,
    /// 超时后被强制终止。
    TimedOut,
    /// 被取消令牌终止。
    Cancelled,
}

/// 等待子进程结束；超时或被取消则强制终止进程树。
///
/// **不**在此处收敛退出码：Windows 上 `try_wait` 已收割过状态，随后
/// `Child::wait()` 会直接取回缓存结果。
fn wait_with_timeout(
    child: &mut std::process::Child,
    timeout: Duration,
    cancel: &dyn CancelProbe,
) -> std::io::Result<WaitOutcome> {
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            return Ok(WaitOutcome::Exited);
        }
        if cancel.is_cancelled() {
            kill_tree(child);
            return Ok(WaitOutcome::Cancelled);
        }
        if Instant::now() >= deadline {
            kill_tree(child);
            return Ok(WaitOutcome::TimedOut);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// 终止子进程及其全部后代（NFR-COR-02）。
///
/// 为什么不能只 `Child::kill()`：git 的网络操作几乎总是派生子进程
/// （`git-remote-https`、`ssh`、`git-upload-pack`）。只杀直接子进程会留下
/// 孤儿，而孤儿仍然占着文件句柄、连接与内存 —— 用户看到的就是"取消了但
/// 网络还在跑"。
///
/// Windows 用 `taskkill /T`（进程树）是唯一不需要 `unsafe` 的手段；
/// 其他平台尽力而为（`kill` 直接子进程），与 NFR-OS-01"仅承诺 Windows"一致。
fn kill_tree(child: &mut std::process::Child) {
    let pid = child.id();
    if cfg!(windows) {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    } else {
        // 负号表示进程组；spawn 时未单独建组，退化为杀直接子进程。
        let _ = Command::new("kill")
            .args(["-9", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
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

/// 读取 stderr，既捕获又有选择地流式推送进度。
///
/// 返回值第三项是最后一行未终结的文本：git 的进度行以 `\r` 结尾，
/// 循环结束时缓冲区里通常还剩最后一段（它已经是完整的一行，只是没有换行符）。
fn read_capped_streaming(
    mut reader: impl Read,
    sink: Option<&Arc<dyn ProgressSink>>,
) -> (Vec<u8>, bool, String) {
    let mut captured: Vec<u8> = Vec::new();
    let mut truncated = false;
    let mut line: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 16 * 1024];

    loop {
        match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let room = MAX_CAPTURE_BYTES.saturating_sub(captured.len());
                if room == 0 {
                    truncated = true;
                } else {
                    let take = room.min(read);
                    captured.extend_from_slice(&chunk[..take]);
                    if take < read {
                        truncated = true;
                    }
                }

                if let Some(sink) = sink.as_ref() {
                    for &byte in &chunk[..read] {
                        if byte == b'\r' || byte == b'\n' {
                            emit_progress(sink.as_ref(), &line);
                            line.clear();
                        } else {
                            // 进度行的长度有界；异常长的一行直接截断，避免无界增长。
                            if line.len() < 4096 {
                                line.push(byte);
                            }
                        }
                    }
                }
            }
        }
    }

    let last = String::from_utf8_lossy(&line).into_owned();
    if let Some(sink) = sink.as_ref() {
        emit_progress(sink.as_ref(), &line);
    }
    (captured, truncated, last)
}

fn emit_progress(sink: &dyn ProgressSink, raw: &[u8]) {
    let text = String::from_utf8_lossy(raw);
    let trimmed = text.trim();
    if !trimmed.is_empty() {
        sink.progress(trimmed);
    }
}

fn join_reader(handle: std::thread::JoinHandle<(Vec<u8>, bool)>) -> (Vec<u8>, bool) {
    handle.join().unwrap_or_default()
}

/// 终止**之后**的收尾等待上限。
///
/// 为什么需要它：管道读端会被子进程的后代继承。若进程树没被杀干净，
/// 读取线程要等到最后一个持有者退出才收到 EOF —— 取消一个 `fetch` 就会变成
/// "等到它自己跑完"。1.5s 足够收完已缓冲的输出，也足够让用户感觉"立刻停了"。
const ABORT_DRAIN: Duration = Duration::from_millis(1500);

/// 有界 join：超预算就放弃该线程（它会在孤儿退出后自然结束）。
fn join_reader_quiet(handle: std::thread::JoinHandle<(Vec<u8>, bool)>) -> (Vec<u8>, bool) {
    let deadline = Instant::now() + ABORT_DRAIN;
    while Instant::now() < deadline {
        if handle.is_finished() {
            return handle.join().unwrap_or_default();
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    tracing::warn!("子进程输出管道仍有写端未关闭，已放弃等待（进程树可能未完全终止）");
    (Vec::new(), false)
}

fn join_stream_reader_quiet(
    handle: std::thread::JoinHandle<(Vec<u8>, bool, String)>,
) -> (Vec<u8>, bool, String) {
    let deadline = Instant::now() + ABORT_DRAIN;
    while Instant::now() < deadline {
        if handle.is_finished() {
            return join_stream_reader(handle);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    tracing::warn!("子进程 stderr 仍有写端未关闭，已放弃等待（进程树可能未完全终止）");
    (Vec::new(), false, String::new())
}

fn join_stream_reader(
    handle: std::thread::JoinHandle<(Vec<u8>, bool, String)>,
) -> (Vec<u8>, bool, String) {
    handle
        .join()
        .unwrap_or_else(|_| (Vec::new(), false, String::new()))
}

// ---------------------------------------------------------------- 并发槽位

/// 进程级并发计数（架构规划 3.3：子进程数 ≤ 4）。
///
/// 用 `Mutex<usize> + Condvar` 而不是再引一个信号量 crate：这里只服务
/// 一个进程内的四个槽位，实现二十行就够，多一个依赖就多一份供应链风险。
static ACTIVE: AtomicUsize = AtomicUsize::new(0);
static SLOT_LOCK: OnceLock<(Mutex<usize>, Condvar)> = OnceLock::new();

fn slot_pair() -> &'static (Mutex<usize>, Condvar) {
    SLOT_LOCK.get_or_init(|| (Mutex::new(0), Condvar::new()))
}

/// 槽位守卫：drop 时归还。
struct Slot;

impl Drop for Slot {
    fn drop(&mut self) {
        let (lock, condvar) = slot_pair();
        let mut active = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *active = active.saturating_sub(1);
        ACTIVE.store(*active, Ordering::Release);
        condvar.notify_one();
    }
}

/// 申请一个并发槽位。
fn acquire_slot(cancel: &dyn CancelProbe) -> Result<Slot> {
    let (lock, condvar) = slot_pair();
    let deadline = Instant::now() + LIMIT_WAIT;
    let mut active = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        if *active < MAX_CONCURRENT_PROCESSES {
            *active += 1;
            ACTIVE.store(*active, Ordering::Release);
            return Ok(Slot);
        }
        if cancel.is_cancelled() {
            return Err(BackendError::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(BackendError::Process(format!(
                "同时运行的 git 子进程已达上限（{MAX_CONCURRENT_PROCESSES}），请稍后重试"
            )));
        }
        let (guard, _timeout) = condvar
            .wait_timeout(active, Duration::from_millis(50))
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active = guard;
    }
}

/// 当前在飞的 git 子进程数（诊断与测试用）。
#[must_use]
pub fn active_processes() -> usize {
    ACTIVE.load(Ordering::Acquire)
}

/// 一次运行的收尾信息，用于组装审计记录。
///
/// 打包成结构体是为了让 [`GitCli::record`] 保持在 clippy 的参数个数上限内，
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
    use super::{
        acquire_slot, active_processes, redact, tail, GitCli, LOCAL_TIMEOUT,
        MAX_CONCURRENT_PROCESSES, NETWORK_TIMEOUT, TAIL_CHARS,
    };
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
        assert_eq!(MAX_CONCURRENT_PROCESSES, 4);
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
        assert!(!record.network, "本地命令不应带网络标记");
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

    #[test]
    fn network_command_is_flagged_and_redacted() {
        let Some(cli) = GitCli::detect() else {
            eprintln!("未检测到 git，跳过执行用例");
            return;
        };

        let collector = Arc::new(Collector::default());
        // 指向一个必然失败的本地地址：只要命令真的跑了，审计就必须落地。
        let _ = cli.run_network(
            None,
            &["ls-remote", "https://alice:sekrit@127.0.0.1:1/repo.git"],
            collector.as_ref(),
            super::NetworkOptions {
                cancel: None,
                progress: None,
                env: Vec::new(),
            },
        );

        let records = collector.records.lock().expect("锁未被污染");
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert!(record.network, "网络命令必须带网络标记");
        assert!(
            record.route_label().contains("网络"),
            "Console 标签应能区分网络操作：{}",
            record.route_label()
        );
        let serialized = format!("{} {:?}", record.command_line, record.args);
        assert!(
            !serialized.contains("sekrit"),
            "审计记录必须脱敏：{serialized}"
        );
        assert!(serialized.contains("127.0.0.1"), "主机必须保留");
    }

    #[test]
    fn cancellation_aborts_a_long_network_command() {
        let Some(cli) = GitCli::detect() else {
            eprintln!("未检测到 git，跳过执行用例");
            return;
        };

        // 用 git alias 起一个必然长跑的命令（`!sleep` 由 git 自带的 sh 执行）。
        // 目的不是测 sleep，而是测"等待循环在取消时能杀掉进程树并立刻返回"。
        let began = std::time::Instant::now();
        let cancel = move || began.elapsed() > Duration::from_millis(300);
        let result = cli.run_network(
            Some(&std::env::temp_dir()),
            &["-c", "alias.knotwork-stall=!sleep 20", "knotwork-stall"],
            &crate::record::NoopObserver,
            super::NetworkOptions {
                cancel: Some(&cancel),
                progress: None,
                env: Vec::new(),
            },
        );

        assert!(
            matches!(result, Err(crate::error::BackendError::Cancelled)),
            "取消必须返回 Cancelled：{result:?}"
        );
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "取消后应立即返回（实际 {:?}，说明等待循环没在取消时退出）",
            began.elapsed()
        );
    }

    #[test]
    fn concurrency_slots_are_bounded_and_released() {
        // 占满全部槽位。
        let mut permits = Vec::new();
        for _ in 0..MAX_CONCURRENT_PROCESSES {
            permits.push(acquire_slot(&|| false).expect("槽位应可获取"));
        }
        assert_eq!(active_processes(), MAX_CONCURRENT_PROCESSES);

        // 第 5 个必须被拒绝（等待超时会返回错误），而不是静默超发。
        let blocked = std::thread::spawn(|| acquire_slot(&|| true).is_err());
        assert!(blocked.join().expect("线程未 panic"), "取消应立即放弃排队");

        // 释放一个后应立刻可再获取。
        permits.pop();
        let again = acquire_slot(&|| false).expect("释放后应可获取");
        drop(again);
        drop(permits);
        assert_eq!(active_processes(), 0, "守卫全部 drop 后计数必须归零");
    }

    #[test]
    fn progress_lines_are_emitted_for_carriage_return_updates() {
        use std::sync::Mutex as StdMutex;
        let seen: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let sink_seen = Arc::clone(&seen);
        let sink = move |line: &str| {
            sink_seen.lock().expect("锁未被污染").push(line.to_owned());
        };

        let raw = b"Receiving objects: 10%\rReceiving objects: 55%\nDone\r";
        let reader = std::io::Cursor::new(raw.to_vec());
        let sink: Arc<dyn super::ProgressSink> = Arc::new(sink);
        let (captured, truncated, _) = super::read_capped_streaming(reader, Some(&sink));
        assert!(!truncated);
        assert_eq!(captured, raw);

        let lines = seen.lock().expect("锁未被污染").clone();
        assert_eq!(
            lines,
            vec!["Receiving objects: 10%", "Receiving objects: 55%", "Done"],
            "\\r 与 \\n 都应切分为进度行，且空行被忽略"
        );
    }

    #[test]
    fn redaction_applies_to_audit_payloads() {
        assert_eq!(
            redact("git push https://u:p@h/r main"),
            "git push https://u:***@h/r main"
        );
        // 未含凭据的文本不被破坏。
        assert_eq!(redact("git status --porcelain"), "git status --porcelain");
    }
}
