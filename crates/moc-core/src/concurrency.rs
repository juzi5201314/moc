//! 供应商请求和后台进程共享的并发许可；工作真正完成后才释放。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub(crate) struct Gate {
    active: AtomicUsize,
    changed: Notify,
}

pub(crate) struct Permit(Arc<Gate>);

impl Gate {
    pub(crate) fn try_acquire(self: &Arc<Self>, limit: usize) -> Option<Permit> {
        self.active
            .try_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (limit == 0 || active < limit).then_some(active + 1)
            })
            .ok()
            .map(|_| Permit(Arc::clone(self)))
    }

    pub(crate) async fn acquire(
        self: &Arc<Self>,
        limit: usize,
        cancel: &CancellationToken,
    ) -> Option<Permit> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            // 先注册再检查，免得许可在检查和 await 之间被释放而丢掉唤醒。
            changed.as_mut().enable();
            if cancel.is_cancelled() {
                return None;
            }
            if let Some(permit) = self.try_acquire(limit) {
                return Some(permit);
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return None,
                _ = changed => {}
            }
        }
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        let previous = self.0.active.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "并发许可计数不能为负");
        self.0.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn capacity_is_held_until_drop_and_waiters_can_cancel() {
        let gate = Arc::new(Gate::default());
        let first = gate.try_acquire(1).expect("首个请求获准");
        assert!(gate.try_acquire(1).is_none());
        let cancel = CancellationToken::new();
        let waiting = gate.acquire(1, &cancel);
        tokio::pin!(waiting);
        assert!(futures_util::poll!(&mut waiting).is_pending());
        cancel.cancel();
        assert!(waiting.await.is_none());
        assert!(
            gate.try_acquire(1).is_none(),
            "取消等待不能释放其他请求的许可"
        );
        drop(first);
        let next = gate
            .acquire(1, &CancellationToken::new())
            .await
            .expect("释放后可再次获准");
        assert!(gate.try_acquire(1).is_none());
        drop(next);
    }
}
