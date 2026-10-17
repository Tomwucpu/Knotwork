//! 后端错误类型。

use std::path::PathBuf;

use thiserror::Error;

/// `gt-backend` 的统一错误类型。
#[derive(Debug, Error)]
pub enum BackendError {
    /// 路径不是 Git 仓库，或 `.git` 缺失 / 不是有效目录。
    #[error("不是有效的 Git 仓库：{path}（{reason}）")]
    NotARepository {
        /// 被检查的路径。
        path: PathBuf,
        /// 具体原因。
        reason: String,
    },

    /// 仓库使用了本版本不支持的哈希算法（v1.0 只支持 SHA-1）。
    #[error("仓库使用 {kind:?} 哈希算法；v1.0 仅支持 SHA-1 仓库")]
    UnsupportedObjectHash {
        /// 仓库声明的哈希类型。
        kind: String,
    },

    /// 对象引用（分支名、标签、提交号）无法解析。
    #[error("无法解析版本标识：{spec}")]
    UnknownRevision {
        /// 原始标识。
        spec: String,
    },

    /// 读取对象或遍历历史失败。
    #[error("Git 对象读取失败：{0}")]
    Object(String),

    /// 引用读取失败。
    #[error("引用读取失败：{0}")]
    Reference(String),

    /// 工作区状态计算失败。
    #[error("工作区状态计算失败：{0}")]
    Status(String),

    /// 树比较失败。
    #[error("树比较失败：{0}")]
    Diff(String),

    /// 路径不属于仓库。
    #[error("路径不在仓库工作区内：{path}")]
    PathOutsideWorktree {
        /// 越界路径。
        path: PathBuf,
    },

    /// 工作区文件读取失败。
    #[error("文件读取失败：{path}（{reason}）")]
    Io {
        /// 被读取的路径。
        path: PathBuf,
        /// 具体原因。
        reason: String,
    },

    /// git 可执行文件缺失（FR-STA-04 的降级触发条件）。
    #[error("未找到 git 可执行文件：{0}")]
    GitNotFound(String),

    /// 子进程执行失败。
    #[error("git 子进程执行失败：{0}")]
    Process(String),

    /// 子进程超时（本地操作 30s，网络操作 120s，架构规划 7.3）。
    #[error("git 子进程超时（{seconds}s）：{program} {args:?}")]
    ProcessTimeout {
        /// 已等待的秒数。
        seconds: u64,
        /// 可执行文件。
        program: String,
        /// 参数列表。
        args: Vec<String>,
    },

    /// 输出无法按预期解析。
    #[error("命令输出解析失败：{0}")]
    Parse(String),

    /// 该操作依赖 git 可执行文件，但启动时未探测到（FR-STA-04 的写侧降级）。
    #[error("该操作需要 git 可执行文件支持：{operation}（当前未检测到 git）")]
    CliRequired {
        /// 操作的中文名称。
        operation: &'static str,
    },

    /// 仓库以只读模式打开（写锁被其他实例持有，FR-MR-04）。
    #[error("仓库以只读模式打开：{reason}")]
    ReadOnly {
        /// 只读原因（含占用者 PID）。
        reason: String,
    },

    /// 操作被仓库侧规则拒绝（hook 失败、分支受保护、非快进等）。
    #[error("{summary}")]
    Rejected {
        /// 面向用户的结论。
        summary: String,
        /// git 的原始输出（可能为空），供 Console 与详情展示。
        detail: String,
    },

    /// 操作被取消（取消令牌在子进程等待期间被触发）。
    #[error("操作已取消")]
    Cancelled,
}

/// 便捷别名。
pub type Result<T, E = BackendError> = std::result::Result<T, E>;
