//! 请求代次与令牌发放（架构规划 4.3）。
//!
//! 每个"视图通道"（Log 列表、工作区状态、Diff 面板…）持有一个 [`RequestGate`]。
//! 发起请求时调用 [`RequestGate::begin`]：它作废上一代（取消旧令牌）并发出
//! `(代次, 新令牌)`；结果回传前用 [`RequestGate::is_current`] 校验，
//! 过期结果直接丢弃。
//!
//! # 代次与"取消"的分工
//!
//! - **取消令牌**回答"还要不要继续算"——由工作线程在分片边界检查，用来省电、省 IO；
//! - **代次号**回答"算出来的还算不算数"——即使任务一秒前就跑完了，
//!   只要代次已过期，结果也不进视图。
//!
//! 两者缺一不可：只有代次，过期任务会白烧 CPU；只有取消，边界之外的
//! "已完成但未投递"的结果仍可能污染视图。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::cancel::CancellationToken;

/// 单调递增的请求代次号。
///
/// 从 1 开始；`0` 保留给"从未发起过请求"的初始状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Generation(u64);

impl Generation {
    /// 初始代次（尚未发起任何请求）。
    pub const INITIAL: Self = Self(0);

    /// 原始数值。
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for Generation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "g{}", self.0)
    }
}

/// 一条视图通道的代次闸门。
///
/// 内部只有一个 `AtomicU64` 与一个"当前令牌"槽位，因此可以放在
/// `Arc` 里被 UI 线程与工作线程同时访问，且没有任何阻塞式等待。
#[derive(Debug)]
pub struct RequestGate {
    generation: AtomicU64,
    current: Mutex<CancellationToken>,
}

impl Default for RequestGate {
    fn default() -> Self {
        Self::new()
    }
}

impl RequestGate {
    /// 新建闸门（当前代次为 [`Generation::INITIAL`]，当前令牌为未取消的新令牌）。
    #[must_use]
    pub fn new() -> Self {
        Self {
            generation: AtomicU64::new(Generation::INITIAL.raw()),
            current: Mutex::new(CancellationToken::new()),
        }
    }

    /// 发起一次新请求：作废上一代并返回 `(新代次, 新令牌)`。
    ///
    /// 上一代的令牌会被取消 —— 若那个任务还在跑，它会在下一个分片边界退出。
    pub fn begin(&self) -> (Generation, CancellationToken) {
        let next = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let mut slot = self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slot.cancel();
        let token = CancellationToken::new();
        *slot = token.clone();
        (Generation(next), token)
    }

    /// 指定代次是否仍然是当前代次（即结果应当被采纳）。
    #[must_use]
    pub fn is_current(&self, generation: Generation) -> bool {
        self.generation.load(Ordering::Acquire) == generation.raw()
    }

    /// 当前代次。
    #[must_use]
    pub fn current(&self) -> Generation {
        Generation(self.generation.load(Ordering::Acquire))
    }

    /// 当前代次的令牌（用于让"外部原因"也能取消，例如窗口关闭）。
    #[must_use]
    pub fn current_token(&self) -> CancellationToken {
        self.current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// 取消当前代次的任务，但**不**推进代次号。
    ///
    /// 用于"窗口关闭"这类场景：结果不再需要，但也没有新请求要发起。
    pub fn cancel_current(&self) {
        self.current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::{Generation, RequestGate};

    #[test]
    fn begin_advances_generation_and_cancels_the_previous_token() {
        let gate = RequestGate::new();
        assert_eq!(gate.current(), Generation::INITIAL);

        let (first, first_token) = gate.begin();
        assert_eq!(first.raw(), 1);
        assert!(gate.is_current(first));
        assert!(!first_token.is_cancelled());

        let (second, second_token) = gate.begin();
        assert_eq!(second.raw(), 2);
        assert!(gate.is_current(second));
        assert!(first_token.is_cancelled(), "发起新请求必须作废上一代任务");
        assert!(!second_token.is_cancelled());
        assert!(!gate.is_current(first), "过期代次不得再被采纳");
    }

    #[test]
    fn cancel_current_stops_the_task_without_advancing() {
        let gate = RequestGate::new();
        let (generation, token) = gate.begin();

        gate.cancel_current();
        assert!(token.is_cancelled());
        assert!(
            gate.is_current(generation),
            "取消不是作废：代次不变，因此已完成的结果仍可采纳"
        );
        assert_eq!(gate.current(), generation);
    }

    #[test]
    fn current_token_tracks_the_latest_generation() {
        let gate = RequestGate::new();
        let (_, first) = gate.begin();
        let (_, second) = gate.begin();

        assert!(first.is_cancelled());
        assert!(!second.is_cancelled());
        assert!(gate.current_token().shares_flag_with(&second));
    }

    #[test]
    fn generations_are_ordered_for_staleness_checks() {
        let gate = RequestGate::new();
        let early = gate.begin().0;
        let late = gate.begin().0;
        assert!(early < late);
        assert_eq!(format!("{late}"), "g2");
    }
}
