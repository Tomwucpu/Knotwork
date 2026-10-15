//! 取消令牌（架构规划 4.3 第 2、6 点）。
//!
//! 每一次 Git 操作都带一个令牌。用户切换仓库、改筛选条件、关闭窗口时，
//! UI 线程调用 [`CancellationToken::cancel`]，工作线程在**分片边界**检查
//! [`CancellationToken::is_cancelled`] 后提前返回。
//!
//! # 为什么是"分片边界"而不是"立即中断"
//!
//! 单次 `gix` 调用（一次 revwalk、一次 tree diff）是同步的，无法从外部打断。
//! 强行打断只能靠进程/线程级暴力手段，代价远高于收益。因此令牌的语义是
//! **"别再往下做了"**：已开始的那一小步会跑完（毫秒级），但后续分片立刻放弃。
//! 这正是"快速在多个仓库窗口间切换不会产生结果错乱"所需要的粒度。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// 操作已被取消。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("操作已取消")]
pub struct Cancelled;

/// 可克隆的取消令牌；任一副本被取消，全部副本可见。
#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    flag: Arc<AtomicBool>,
}

impl CancellationToken {
    /// 新建一个未取消的令牌。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 取消该令牌（幂等；已取消时是空操作）。
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Release);
    }

    /// 是否已取消。
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    /// 分片边界检查：已取消则返回 [`Cancelled`]。
    ///
    /// # Errors
    /// 令牌已被取消时返回 [`Cancelled`]。
    pub fn check(&self) -> Result<(), Cancelled> {
        if self.is_cancelled() {
            Err(Cancelled)
        } else {
            Ok(())
        }
    }

    /// 是否与另一个令牌共享同一面标志（用于测试与断言）。
    #[must_use]
    pub fn shares_flag_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.flag, &other.flag)
    }
}

#[cfg(test)]
mod tests {
    use super::CancellationToken;

    #[test]
    fn clones_share_the_flag() {
        let token = CancellationToken::new();
        let clone = token.clone();
        assert!(token.shares_flag_with(&clone));
        assert!(!token.is_cancelled());

        clone.cancel();
        assert!(token.is_cancelled(), "任一副本取消应立即对全部副本可见");
        assert!(token.check().is_err());
    }

    #[test]
    fn cancel_is_idempotent_and_manual_tokens_are_independent() {
        let first = CancellationToken::new();
        let second = CancellationToken::new();
        assert!(!first.shares_flag_with(&second));

        first.cancel();
        first.cancel();
        assert!(first.is_cancelled());
        assert!(!second.is_cancelled(), "不同令牌不应互相影响");
        assert!(second.check().is_ok());
    }
}
