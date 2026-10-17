//! 凭据复用、失败诊断与密钥链存储（FR-RMT-04、NFR-SEC-01）。
//!
//! # 为什么"复用 credential helper"而不是自己实现认证
//!
//! 需求原文是"完全复用 git CLI credential 协议与系统 credential helper / SSH agent"。
//! 这不是实现偏好，而是唯一正确的选择：
//!
//! - 用户的既有配置（`credential.helper`、`~/.ssh/config`、SSH agent、企业 CA、
//!   代理）只对 git 本体生效。自建认证等于要求用户**再配一遍**，且永远追不上
//!   git 的边界情况（二次验证、OAuth device flow、企业 SSO）。
//! - 凭据协议是**拉取式**的：git 在需要时反向调用 helper，密码不经过我们的进程，
//!   因此也就不会被我们写进日志、配置文件或崩溃转储。
//!
//! 我们只在两处介入：
//!
//! 1. **不阻断**：设置 `GIT_TERMINAL_PROMPT=0`，让 git 在没有凭据时立即失败而不是
//!    挂在终端上等输入（GUI 里没有终端，挂住等于永久无响应）；
//! 2. **翻译失败**：把 git 的英文报错翻成"用户下一步该做什么"（[`diagnose`]）。
//!
//! # 可选的密钥链存储
//!
//! 没有配置 helper 的用户，可以在本工具里为该远端主机存一个 HTTPS token
//! （[`KeyringStore`]，走 Windows 凭据管理器）。存储值是**密文由操作系统保管**，
//! 我们盘上没有任何明文；使用时通过 `GIT_ASKPASS` 把它交给 git 子进程
//! （见 [`askpass_answer`] 与 [`ASKPASS_SECRET_ENV`]），生命周期只覆盖该子进程。
//!
//! 已知边界：子进程的环境变量理论上可被同用户的进程读取，这是 `GIT_ASKPASS`
//! 方案的固有代价（VS Code、GitHub Desktop 等同类工具采用同一机制）；
//! 明文不落盘这一条（NFR-SEC-01）由此满足。

use std::fmt::Write as _;
use std::sync::Mutex;

/// `GIT_ASKPASS` 子进程读取口令用的环境变量名。
pub const ASKPASS_SECRET_ENV: &str = "KNOTWORK_ASKPASS_SECRET";

/// `GIT_ASKPASS` 子进程读取用户名用的环境变量名。
pub const ASKPASS_USERNAME_ENV: &str = "KNOTWORK_ASKPASS_USERNAME";

/// 密钥链条目的服务名前缀（后接主机名）。
pub const KEYRING_SERVICE_PREFIX: &str = "knotwork-git";

/// 凭据相关失败的类别。
///
/// 分类的目的不是"给错误起名字"，而是决定**给用户哪条可执行的动作**：
/// 类别错了，建议就会指向错误的修法，比不给建议更糟。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    /// 远端要求认证，但本地没有任何可用凭据（无 helper、无 token）。
    AuthRequired,
    /// 凭据被远端拒绝（401/403、token 失效、密码错误）。
    AuthRejected,
    /// SSH 私钥不可用（未加载到 agent、文件权限、指纹不匹配）。
    SshKeyMissing,
    /// 主机密钥未在 `known_hosts` 中（首次连接或主机密钥变更）。
    HostKeyUnverified,
    /// 远端仓库策略拒绝（保护分支、pre-receive hook）。
    ServerPolicy,
    /// 网络不可达（DNS、超时、连接被拒）。
    Network,
    /// TLS/证书问题（企业根证书未安装、证书过期）。
    Tls,
    /// 代理配置问题。
    Proxy,
    /// 其他（无法归类时保持诚实，不硬套）。
    Unknown,
}

impl CredentialKind {
    /// 是否属于"凭据本身"的问题（决定界面是否引导去设置 token）。
    #[must_use]
    pub const fn is_credential_issue(self) -> bool {
        matches!(
            self,
            Self::AuthRequired | Self::AuthRejected | Self::SshKeyMissing | Self::HostKeyUnverified
        )
    }

    /// 诊断标签。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::AuthRequired => "缺少凭据",
            Self::AuthRejected => "凭据被拒",
            Self::SshKeyMissing => "SSH 密钥不可用",
            Self::HostKeyUnverified => "主机密钥未确认",
            Self::ServerPolicy => "远端策略拒绝",
            Self::Network => "网络不可达",
            Self::Tls => "证书校验失败",
            Self::Proxy => "代理配置问题",
            Self::Unknown => "未知原因",
        }
    }
}

/// 一次失败的诊断结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialDiagnosis {
    /// 失败类别。
    pub kind: CredentialKind,
    /// 一句话结论（面向用户）。
    pub summary: String,
    /// 建议动作，按推荐顺序排列。
    pub actions: Vec<String>,
}

impl CredentialDiagnosis {
    /// 是否值得提示"可为此主机保存 token"。
    ///
    /// 只有 HTTPS 凭据类问题才适用：SSH 密钥缺失或主机密钥未确认时，
    /// 建议用户去存 token 是错的方向（他们需要的是 `ssh-add` 或核对指纹）。
    #[must_use]
    pub fn suggests_token(&self) -> bool {
        matches!(
            self.kind,
            CredentialKind::AuthRequired | CredentialKind::AuthRejected
        )
    }

    /// 拼成可直接展示的多行文本（结论 + 建议 + 远端地址）。
    #[must_use]
    pub fn to_message(&self, remote_url: Option<&str>) -> String {
        let mut out = self.summary.clone();
        if let Some(url) = remote_url {
            out.push_str("\n远端：");
            out.push_str(&redact_url(url));
        }
        if !self.actions.is_empty() {
            out.push_str("\n建议：");
            for (index, action) in self.actions.iter().enumerate() {
                let _ = write!(out, "\n  {}. {action}", index + 1);
            }
        }
        out
    }
}

/// 从 git 的 stderr 与远端地址推断失败原因。
///
/// 匹配顺序即优先级：git 在失败时常常把多段信息叠在一起
/// （例如 "Permission denied (publickey)\nfatal: Could not read from remote repository"），
/// 必须让最具体的那条先命中，否则所有 SSH 问题都会被归到泛化的"无法读取远端"。
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn diagnose(stderr: &str, remote_url: Option<&str>) -> CredentialDiagnosis {
    let lower = stderr.to_ascii_lowercase();
    let ssh = remote_url.is_some_and(is_ssh_url);

    // ---- SSH 侧 ----
    if lower.contains("host key verification failed")
        || lower.contains("remote host identification has changed")
    {
        return CredentialDiagnosis {
            kind: CredentialKind::HostKeyUnverified,
            summary: "SSH 主机密钥未被本机信任（首次连接或对端密钥已变更）".to_owned(),
            actions: ssh_host_key_actions(),
        };
    }
    if lower.contains("permission denied (publickey)")
        || lower.contains("no supported authentication methods available")
        || lower.contains("load key")
    {
        return CredentialDiagnosis {
            kind: CredentialKind::SshKeyMissing,
            summary: "SSH 私钥不可用：远端拒绝了本机提供的密钥".to_owned(),
            actions: ssh_key_actions(),
        };
    }

    // ---- 凭据协议：git 说"没有可用的凭据" ----
    if lower.contains("terminal prompts disabled")
        || lower.contains("could not read username")
        || lower.contains("could not read password")
        || lower.contains("authentication required")
    {
        return CredentialDiagnosis {
            kind: CredentialKind::AuthRequired,
            summary: "远端需要认证，但本机没有可用的凭据".to_owned(),
            actions: auth_required_actions(ssh),
        };
    }

    // ---- 凭据被拒 ----
    if lower.contains("authentication failed")
        || lower.contains("invalid username or password")
        || lower.contains("401")
        || lower.contains("403")
        || lower.contains("the requested url returned error: 403")
        || lower.contains("support for password authentication was removed")
    {
        let forbidden = lower.contains("403");
        return CredentialDiagnosis {
            kind: CredentialKind::AuthRejected,
            summary: if forbidden {
                "远端返回 403：凭据有效但无权访问该仓库（或 token 权限不足）".to_owned()
            } else {
                "远端拒绝了本次认证（token 失效、密码错误或需要二次验证）".to_owned()
            },
            actions: auth_rejected_actions(forbidden),
        };
    }

    // ---- 远端策略 ----
    if lower.contains("pre-receive hook declined")
        || lower.contains("protected branch")
        || lower.contains("remote rejected")
        || lower.contains("gh00")
    {
        return CredentialDiagnosis {
            kind: CredentialKind::ServerPolicy,
            summary: "远端策略拒绝了本次推送（保护分支 / 服务端 hook）".to_owned(),
            actions: vec![
                "确认目标分支是否为受保护分支；若是，请改用推送新分支并通过合并请求合入。"
                    .to_owned(),
                "若远端配置了 pre-receive 检查（提交规范、签名、体积），按其提示修正后重试。"
                    .to_owned(),
                "详情见 Console 中该条记录的完整输出。".to_owned(),
            ],
        };
    }

    // ---- 传输层 ----
    if lower.contains("certificate")
        || lower.contains("ssl")
        || lower.contains("tls")
        || lower.contains("unable to get local issuer")
    {
        return CredentialDiagnosis {
            kind: CredentialKind::Tls,
            summary: "TLS 证书校验失败：远端证书不被本机信任".to_owned(),
            actions: vec![
                "若使用企业自建 Git 服务，请把其根证书导入系统信任存储后重试。".to_owned(),
                "确认系统时间正确（证书有效期判断依赖本地时钟）。".to_owned(),
                "不建议用 http.sslVerify=false 绕过：那会让整条链路的中间人攻击不可见。".to_owned(),
            ],
        };
    }
    if lower.contains("proxy") {
        return CredentialDiagnosis {
            kind: CredentialKind::Proxy,
            summary: "通过代理访问远端失败".to_owned(),
            actions: vec![
                "检查 git 的 proxy 配置（`git config --get http.proxy`）与系统代理是否一致。"
                    .to_owned(),
                "若代理需要认证，请把凭据交给代理自身的配置，而不是写进远端 URL。".to_owned(),
            ],
        };
    }
    if lower.contains("could not resolve host")
        || lower.contains("connection refused")
        || lower.contains("connection timed out")
        || lower.contains("failed to connect")
        || lower.contains("unable to access")
        || lower.contains("network is unreachable")
        || lower.contains("early eof")
    {
        return CredentialDiagnosis {
            kind: CredentialKind::Network,
            summary: "无法连接到远端（DNS 解析、端口不通或网络中断）".to_owned(),
            actions: vec![
                "确认远端地址拼写正确（见消息中的远端一行）。".to_owned(),
                "在终端里执行同一条命令，可快速区分是本工具的问题还是网络问题。".to_owned(),
                "企业内网通常需要 VPN 或代理，确认当前网络环境是否可访问该主机。".to_owned(),
            ],
        };
    }

    CredentialDiagnosis {
        kind: CredentialKind::Unknown,
        summary: "远端操作失败（未识别的错误）".to_owned(),
        actions: vec![
            "在 Console 中查看该条记录的完整输出与退出码。".to_owned(),
            "在终端执行「复现」一行给出的命令，可得到与 git 完全一致的行为。".to_owned(),
        ],
    }
}

fn ssh_host_key_actions() -> Vec<String> {
    vec![
        "首次连接：确认真实性后执行一次 `ssh -T <主机>` 把主机密钥写入 known_hosts。".to_owned(),
        "密钥已变更：先向管理员核实是服务端正常变更，再删除 known_hosts 中的旧条目。".to_owned(),
        "不要直接关闭 StrictHostKeyChecking —— 那会让中间人攻击不可见。".to_owned(),
    ]
}

fn ssh_key_actions() -> Vec<String> {
    vec![
        "确认私钥已加载：`ssh-add -l` 能看到对应指纹。".to_owned(),
        "确认公钥已添加到远端账号（GitHub/GitLab 的 SSH keys 页面）。".to_owned(),
        "若私钥有口令，启动 ssh-agent 并 `ssh-add` 后再重试。".to_owned(),
    ]
}

fn auth_required_actions(ssh: bool) -> Vec<String> {
    if ssh {
        return ssh_key_actions();
    }
    vec![
        "配置一个 credential helper（例如 `git config --global credential.helper manager`），之后由它保管密码。".to_owned(),
        "或在本窗口的远端管理里为此主机保存访问 token（存入系统密钥链，明文不落盘）。".to_owned(),
        "token 需要仓库读取权限；推送还需要写入权限。".to_owned(),
    ]
}

fn auth_rejected_actions(forbidden: bool) -> Vec<String> {
    if forbidden {
        return vec![
            "确认账号对该仓库有权限（403 通常不是密码错误，而是权限不足）。".to_owned(),
            "若使用细粒度 token，检查它是否包含该仓库的 Contents: Read and write 权限。".to_owned(),
            "企业 SSO 组织的 token 需要先授权给该组织。".to_owned(),
        ];
    }
    vec![
        "更新本机保存的凭据：Windows 凭据管理器中删除该主机条目后重新认证。".to_owned(),
        "若开启了二次验证，请使用访问 token 而不是账号密码。".to_owned(),
        "确认 token 未过期，且未因异地登录而被撤销。".to_owned(),
    ]
}

/// 是否为 SSH 形式的远端地址（`ssh://` 或 `user@host:path` 简写）。
#[must_use]
pub fn is_ssh_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("ssh://") || lower.starts_with("git+ssh://") {
        return true;
    }
    // `git@github.com:owner/repo.git`：冒号出现在第一个 `/` 之前。
    match (url.find('@'), url.find(':')) {
        (Some(at), Some(colon)) => at < colon && !lower.contains("://"),
        _ => false,
    }
}

/// 是否为 HTTPS 形式的远端地址。
#[must_use]
pub fn is_https_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.starts_with("https://") || lower.starts_with("http://")
}

/// 取远端地址的主机名（用于密钥链条目命名）。
///
/// 解析失败的输入返回 [`None`]：宁可不保存 token，也不要把它存到一个
/// 猜出来的条目名下（那会让用户以为存好了，实际永远不会被取用）。
#[must_use]
pub fn host_of(url: &str) -> Option<String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(rest) = trimmed.split_once("://").map(|(_, rest)| rest) {
        // scheme://[user[:pass]@]host[:port]/path
        let authority = rest.split('/').next().unwrap_or(rest);
        let host_port = authority.rsplit('@').next().unwrap_or(authority);
        let host = host_port.split(':').next().unwrap_or(host_port);
        return non_empty(host);
    }
    // scp 风格：user@host:path
    if let Some((before, after)) = trimmed.split_once(':') {
        if let Some((_, host)) = before.rsplit_once('@') {
            return non_empty(host);
        }
        // `E:/repos/x`、`C://repos//x`：盘符后面紧跟路径分隔符，这是本地路径而不是主机。
        if is_windows_drive(before) && after.starts_with(['/', '\\']) {
            return None;
        }
        return non_empty(before);
    }
    // 既没有协议头也没有冒号：本地路径（`/srv/repo`、`../repo`）没有主机。
    None
}

/// 单个 ASCII 字母（Windows 盘符）。
fn is_windows_drive(text: &str) -> bool {
    text.len() == 1 && text.as_bytes()[0].is_ascii_alphabetic()
}

fn non_empty(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// 掩码 URL 中的凭据部分。
///
/// `https://user:token@host/repo` → `https://user:***@host/repo`；
/// `https://token@host/repo` → `https://***@host/repo`。
/// 主机与路径保持原样 —— 用户要靠它们定位问题。
#[must_use]
pub fn redact_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        // scp 风格里没有口令，原样返回。
        return url.to_owned();
    };
    let (authority, tail) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    let Some((userinfo, host)) = authority.rsplit_once('@') else {
        return url.to_owned();
    };
    let masked = match userinfo.split_once(':') {
        Some((user, _)) if !user.is_empty() => format!("{user}:***"),
        _ => "***".to_owned(),
    };
    format!("{scheme}://{masked}@{host}{tail}")
}

/// 掩码自由文本中的凭据。
///
/// 覆盖三类高风险位置：URL 里的 userinfo、`key=value` 形式的 token/password、
/// `Authorization: Bearer xxx` 头。其余文本原样保留 —— 过度掩码会让诊断信息
/// 失去价值（用户看不出是哪个主机、哪个分支出的问题）。
#[must_use]
pub fn redact(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }

    // 小写副本只用于定位：ASCII 小写化不改变字节长度，因此下标可以直接切原文。
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    while cursor < text.len() {
        let Some(start) = find_url_start(&lower, cursor) else {
            out.push_str(&text[cursor..]);
            break;
        };
        out.push_str(&text[cursor..start]);
        let end = lower[start..]
            .find([' ', '\n', '\r', '\t', '"', '\'', ')', '>'])
            .map_or(text.len(), |offset| start + offset);
        let url = &text[start..end];
        if url.contains('@') && url.contains("://") {
            out.push_str(&redact_url(url));
        } else {
            out.push_str(url);
        }
        cursor = end;
    }

    redact_assignments(&out)
}

/// 从 `from` 起找最早出现的 URL 协议头。
fn find_url_start(lower: &str, from: usize) -> Option<usize> {
    ["https://", "http://", "ssh://", "git+ssh://"]
        .iter()
        .filter_map(|scheme| lower[from..].find(scheme).map(|index| from + index))
        .min()
}

/// 掩码 `password=xxx` / `token: xxx` / `Bearer xxx` 形式的值。
fn redact_assignments(text: &str) -> String {
    const KEYS: [&str; 5] = ["password", "passwd", "token", "secret", "authorization"];
    let lowered = text.to_ascii_lowercase();
    let bytes = text.as_bytes();

    let mut masks: Vec<(usize, usize)> = Vec::new();
    for key in KEYS {
        let mut from = 0;
        while let Some(found) = lowered[from..].find(key) {
            let start = from + found;
            from = start + key.len();
            // 键名之后允许空白，然后是 `=` 或 `:`。
            let mut cursor = start + key.len();
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            if cursor >= bytes.len() || (bytes[cursor] != b'=' && bytes[cursor] != b':') {
                continue;
            }
            cursor += 1;
            while cursor < bytes.len() && (bytes[cursor] == b' ' || bytes[cursor] == b'\t') {
                cursor += 1;
            }
            let mut value_start = cursor;
            // Bearer 前缀保留：让用户看出用的是哪种认证方案，同时不泄露口令本体。
            if lowered[cursor..].starts_with("bearer ") {
                cursor += 7;
                value_start = cursor;
            }
            // 值在空白与常见分隔符处结束：`token=abc&user=bob` 里的 `&` 必须
            // 截断掩码范围，否则会把后面的非敏感参数一起吃掉。
            while cursor < bytes.len()
                && !bytes[cursor].is_ascii_whitespace()
                && !matches!(
                    bytes[cursor],
                    b'&' | b';' | b',' | b'"' | b'\'' | b')' | b'}' | b']' | b'>' | b'<'
                )
            {
                cursor += 1;
            }
            if cursor > value_start {
                masks.push((value_start, cursor));
            }
        }
    }

    if masks.is_empty() {
        return text.to_owned();
    }
    masks.sort_unstable();
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for (start, end) in masks {
        if start < cursor {
            continue;
        }
        out.push_str(&text[cursor..start]);
        out.push_str("***");
        cursor = end;
    }
    out.push_str(&text[cursor..]);
    out
}

// ---------------------------------------------------------------- 密钥链

/// token 存储抽象。
///
/// 抽成 trait 是为了让测试不触碰真实的系统密钥链 —— 单测在 CI 上写
/// Windows 凭据管理器会污染机器，也无法并行。
pub trait TokenStore: Send + Sync {
    /// 取某主机的 token。
    fn get(&self, host: &str) -> Option<String>;

    /// 写入/覆盖某主机的 token。
    ///
    /// # Errors
    /// 系统密钥链不可用时返回可读错误。
    fn set(&self, host: &str, username: &str, token: &str) -> Result<(), String>;

    /// 删除某主机的 token；不存在不算错误。
    ///
    /// # Errors
    /// 系统密钥链不可用时返回可读错误。
    fn delete(&self, host: &str) -> Result<(), String>;

    /// 当前平台的密钥链是否可用。
    fn is_available(&self) -> bool;
}

/// 基于系统密钥链的实现（Windows 凭据管理器）。
#[derive(Debug, Clone)]
pub struct KeyringStore {
    service: String,
}

impl Default for KeyringStore {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyringStore {
    /// 使用默认服务名。
    #[must_use]
    pub fn new() -> Self {
        Self {
            service: KEYRING_SERVICE_PREFIX.to_owned(),
        }
    }

    /// 允许调用方覆盖服务名（测试与多版本并存时使用）。
    #[must_use]
    pub fn with_service(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    fn entry(&self, host: &str) -> Result<keyring::Entry, String> {
        keyring::Entry::new(&self.service, host).map_err(|err| err.to_string())
    }
}

impl TokenStore for KeyringStore {
    fn get(&self, host: &str) -> Option<String> {
        let entry = self.entry(host).ok()?;
        // `NoEntry` 是正常情况（没存过），不该当成错误刷日志。
        entry.get_password().ok()
    }

    fn set(&self, host: &str, username: &str, token: &str) -> Result<(), String> {
        self.entry(host)?
            .set_password(token)
            .map_err(|err| err.to_string())?;

        // 用户名单独存一份：askpass 需要它回答 "Username for ..." 提示。
        // 存失败不算致命（取用时回退到远端 URL 里的用户名）。
        if username.trim().is_empty() {
            return Ok(());
        }
        let user_entry = keyring::Entry::new(&format!("{}-user", self.service), host)
            .map_err(|err| err.to_string())?;
        user_entry
            .set_password(username)
            .map_err(|err| err.to_string())
    }

    fn delete(&self, host: &str) -> Result<(), String> {
        if let Ok(entry) = self.entry(host) {
            let _ = entry.delete_credential();
        }
        if let Ok(entry) = keyring::Entry::new(&format!("{}-user", self.service), host) {
            let _ = entry.delete_credential();
        }
        Ok(())
    }

    fn is_available(&self) -> bool {
        // 能否创建条目即可说明后端是否可用；不写入任何值。
        self.entry("knotwork-probe").is_ok()
    }
}

/// 内存实现（测试与"不留痕"模式使用）。
#[derive(Debug, Default)]
pub struct MemoryTokenStore {
    entries: Mutex<Vec<(String, String, String)>>,
}

impl MemoryTokenStore {
    /// 新建空存储。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前保存的条目数（测试断言用）。
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<(String, String, String)>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl TokenStore for MemoryTokenStore {
    fn get(&self, host: &str) -> Option<String> {
        self.lock()
            .iter()
            .find(|(entry_host, _, _)| entry_host == host)
            .map(|(_, _, token)| token.clone())
    }

    fn set(&self, host: &str, username: &str, token: &str) -> Result<(), String> {
        let mut guard = self.lock();
        match guard
            .iter_mut()
            .find(|(entry_host, _, _)| entry_host == host)
        {
            Some(entry) => {
                username.clone_into(&mut entry.1);
                token.clone_into(&mut entry.2);
            }
            None => guard.push((host.to_owned(), username.to_owned(), token.to_owned())),
        }
        Ok(())
    }

    fn delete(&self, host: &str) -> Result<(), String> {
        self.lock().retain(|(entry_host, _, _)| entry_host != host);
        Ok(())
    }

    fn is_available(&self) -> bool {
        true
    }
}

/// `GIT_ASKPASS` 的应答逻辑。
///
/// git 会以提示文本作为唯一参数调用 askpass 程序；我们对两类提示分别作答：
/// 含 "username" 的返回用户名，其余（密码 / token）返回口令。
/// 无法识别的提示也返回口令 —— 让 git 拿到一个值总比挂住好，
/// 失败会以正常的认证错误回传（此时用户能从诊断里看到原因）。
#[must_use]
pub fn askpass_answer(prompt: &str, username: &str, secret: &str) -> String {
    let lower = prompt.to_ascii_lowercase();
    if lower.contains("username") || lower.contains("user name") {
        username.to_owned()
    } else {
        secret.to_owned()
    }
}

/// 读取 `GIT_ASKPASS` 模式所需的环境变量。
///
/// 由应用的 `--askpass` 入口调用：它不碰网络、不碰配置，只把环境里的值打到
/// stdout —— 这是 `GIT_ASKPASS` 协议要求的全部行为。
#[must_use]
pub fn askpass_from_env(prompt: &str) -> Option<String> {
    let secret = std::env::var(ASKPASS_SECRET_ENV).ok()?;
    let username = std::env::var(ASKPASS_USERNAME_ENV).unwrap_or_default();
    Some(askpass_answer(prompt, &username, &secret))
}

#[cfg(test)]
mod tests {
    use super::{
        askpass_answer, diagnose, host_of, is_https_url, is_ssh_url, redact, redact_url,
        CredentialKind, KeyringStore, MemoryTokenStore, TokenStore,
    };

    #[test]
    fn urls_are_recognized_by_transport() {
        assert!(is_ssh_url("git@github.com:owner/repo.git"));
        assert!(is_ssh_url("ssh://git@host:22/repo"));
        assert!(!is_ssh_url("https://github.com/owner/repo"));
        assert!(is_https_url("https://github.com/owner/repo"));
        assert!(!is_ssh_url("C:/repos/demo"));
    }

    #[test]
    fn host_extraction_handles_all_supported_forms() {
        assert_eq!(
            host_of("https://github.com/owner/repo.git").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            host_of("https://user:token@git.example.com:8443/r").as_deref(),
            Some("git.example.com")
        );
        assert_eq!(
            host_of("git@github.com:owner/repo.git").as_deref(),
            Some("github.com")
        );
        assert_eq!(host_of("  "), None);
    }

    #[test]
    fn urls_lose_their_secrets_but_keep_their_location() {
        assert_eq!(
            redact_url("https://alice:ghp_secret@github.com/o/r.git"),
            "https://alice:***@github.com/o/r.git"
        );
        assert_eq!(
            redact_url("https://ghp_secret@github.com/o/r.git"),
            "https://***@github.com/o/r.git"
        );
        // 无凭据的 URL 与 scp 形式原样保留。
        assert_eq!(
            redact_url("https://github.com/o/r.git"),
            "https://github.com/o/r.git"
        );
        assert_eq!(
            redact_url("git@github.com:o/r.git"),
            "git@github.com:o/r.git"
        );
    }

    #[test]
    fn free_text_redaction_covers_the_common_leaks() {
        let text = "fatal: unable to access 'https://bob:tok123@example.com/r.git/': 403";
        let masked = redact(text);
        assert!(!masked.contains("tok123"), "口令必须被抹掉：{masked}");
        assert!(masked.contains("example.com"), "主机必须保留：{masked}");
        assert!(masked.contains("403"));

        let assigned = redact("GET https://x.dev?a=1\nAuthorization: Bearer abc.def.ghi\n");
        assert!(!assigned.contains("abc.def.ghi"), "{assigned}");
        assert!(assigned.contains("Authorization: Bearer ***"), "{assigned}");

        let tokens = redact("token=abcd1234&user=bob");
        assert!(!tokens.contains("abcd1234"), "{tokens}");
        assert!(tokens.contains("user=bob"), "非敏感参数应保留：{tokens}");
    }

    #[test]
    fn redaction_is_idempotent_and_total() {
        for sample in ["", "no secrets here", "https://a:b@c/d"] {
            let once = redact(sample);
            assert_eq!(redact(&once), once, "二次掩码不应继续变化：{sample}");
        }
    }

    #[test]
    fn ssh_permission_denied_is_not_mistaken_for_a_network_problem() {
        let stderr = "git@github.com: Permission denied (publickey).\n\
                      fatal: Could not read from remote repository.";
        let diagnosis = diagnose(stderr, Some("git@github.com:o/r.git"));
        assert_eq!(diagnosis.kind, CredentialKind::SshKeyMissing);
        assert!(diagnosis.summary.contains("SSH"));
        assert!(
            !diagnosis.suggests_token(),
            "SSH 问题不该引导去存 HTTPS token"
        );
    }

    #[test]
    fn missing_credentials_are_reported_as_such() {
        let stderr = "fatal: could not read Username for 'https://github.com': \
                      terminal prompts disabled";
        let diagnosis = diagnose(stderr, Some("https://github.com/o/r.git"));
        assert_eq!(diagnosis.kind, CredentialKind::AuthRequired);
        assert!(diagnosis.suggests_token());
        assert!(!diagnosis.actions.is_empty());
    }

    #[test]
    fn forbidden_is_distinguished_from_bad_password() {
        let stderr = "remote: HTTP Basic: Access denied\n\
                      fatal: Authentication failed for 'https://gitlab.com/o/r.git/'";
        assert_eq!(
            diagnose(stderr, Some("https://gitlab.com/o/r.git")).kind,
            CredentialKind::AuthRejected
        );

        let forbidden = "The requested URL returned error: 403";
        let diagnosis = diagnose(forbidden, Some("https://gitlab.com/o/r.git"));
        assert_eq!(diagnosis.kind, CredentialKind::AuthRejected);
        assert!(diagnosis.summary.contains("403"));
    }

    #[test]
    fn server_policy_and_network_are_separated() {
        let policy = "remote: GitLab: You are not allowed to push code to protected branches\n\
                      ! [remote rejected] main -> main (pre-receive hook declined)";
        assert_eq!(
            diagnose(policy, Some("https://gitlab.com/o/r.git")).kind,
            CredentialKind::ServerPolicy
        );

        let network = "fatal: unable to access 'https://example.com/r.git/': \
                       Could not resolve host: example.com";
        assert_eq!(
            diagnose(network, Some("https://example.com/r.git")).kind,
            CredentialKind::Network
        );
    }

    #[test]
    fn diagnosis_message_never_leaks_the_token() {
        let diagnosis = diagnose(
            "fatal: Authentication failed for 'https://bob:tok@h/r'",
            Some("https://bob:tok@h/r.git"),
        );
        let message = diagnosis.to_message(Some("https://bob:tok@h/r.git"));
        assert!(!message.contains("tok@"), "{message}");
        assert!(message.contains("建议"), "{message}");
    }

    #[test]
    fn memory_store_round_trips_and_forgets() {
        let store = MemoryTokenStore::new();
        assert!(store.is_available());
        assert_eq!(store.get("github.com"), None);

        store.set("github.com", "bob", "tok").expect("写入内存存储");
        assert_eq!(store.get("github.com").as_deref(), Some("tok"));
        assert_eq!(store.len(), 1);

        store.set("github.com", "bob", "tok2").expect("覆盖");
        assert_eq!(store.get("github.com").as_deref(), Some("tok2"));
        assert_eq!(store.len(), 1, "同一主机只应有一条");

        store.delete("github.com").expect("删除");
        assert!(store.is_empty());
    }

    #[test]
    fn keyring_probe_does_not_write_anything() {
        // 只验证可用性判定不会 panic；不实际写入系统密钥链。
        let store = KeyringStore::new();
        let _ = store.is_available();
    }

    #[test]
    fn askpass_answers_the_two_prompt_shapes() {
        assert_eq!(
            askpass_answer("Username for 'https://github.com': ", "bob", "tok"),
            "bob"
        );
        assert_eq!(
            askpass_answer("Password for 'https://bob@github.com': ", "bob", "tok"),
            "tok"
        );
        // 无法识别的提示给出口令，保证 git 不会挂住。
        assert_eq!(askpass_answer("Token:", "bob", "tok"), "tok");
    }
}
