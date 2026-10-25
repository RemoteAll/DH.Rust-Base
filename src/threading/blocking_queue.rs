//! 带容量上限的阻塞队列（`Mutex<VecDeque>` + `Condvar`）。
//!
//! 收编 Pek.RMail 的 `MailQueueProvider` 核心；提供：
//! - 可选容量上限（`try_enqueue` 满/关闭时返回原值，便于调用方背压）；
//! - 超时出队与停止谓词（停止通知与等待在同一把锁下判定，避免丢失唤醒）；
//! - `close()` 停止入队并唤醒所有等待者（已入队元素仍可取出）。
//!
//! 本类型**不做后台线程**，只提供队列语义；消费方自行决定线程模型。

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug)]
struct State<T> {
    queue: VecDeque<T>,
    closed: bool,
}

/// 带容量上限的阻塞队列。
#[derive(Debug)]
pub struct BlockingQueue<T> {
    capacity: Option<usize>,
    state: Mutex<State<T>>,
    ready: Condvar,
}

impl<T> Default for BlockingQueue<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> BlockingQueue<T> {
    /// 无容量上限。
    pub fn new() -> Self {
        Self {
            capacity: None,
            state: Mutex::new(State {
                queue: VecDeque::new(),
                closed: false,
            }),
            ready: Condvar::new(),
        }
    }

    /// 指定容量上限（0 按 1 处理）。
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity: Some(capacity.max(1)),
            state: Mutex::new(State {
                queue: VecDeque::new(),
                closed: false,
            }),
            ready: Condvar::new(),
        }
    }

    /// 容量上限（`None` = 无上限）。
    pub fn capacity(&self) -> Option<usize> {
        self.capacity
    }

    /// 当前元素数量。
    pub fn len(&self) -> usize {
        self.lock().queue.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.lock().queue.is_empty()
    }

    /// 是否已关闭（不再接受入队）。
    pub fn is_closed(&self) -> bool {
        self.lock().closed
    }

    /// 尝试入队；队列已满或已关闭时返回原值（不阻塞）。
    pub fn try_enqueue(&self, item: T) -> Result<(), T> {
        let mut state = self.lock();
        if state.closed || self.capacity.is_some_and(|max| state.queue.len() >= max) {
            return Err(item);
        }
        state.queue.push_back(item);
        drop(state);
        self.ready.notify_one();
        Ok(())
    }

    /// 尝试出队（不阻塞）。
    pub fn try_dequeue(&self) -> Option<T> {
        self.lock().queue.pop_front()
    }

    /// 等待出队，最多等待 `timeout`；超时返回 `None`。
    pub fn dequeue_timeout(&self, timeout: Duration) -> Option<T> {
        self.dequeue_timeout_while(timeout, || false)
    }

    /// 等待出队；`should_stop()` 为 true 或队列已关闭且为空时返回 `None`。
    ///
    /// 停止通知与等待在同一把锁下判定，避免“通知早于等待”的丢失唤醒。
    pub fn dequeue_timeout_while(
        &self,
        timeout: Duration,
        should_stop: impl Fn() -> bool,
    ) -> Option<T> {
        let deadline = Instant::now() + timeout;
        let mut state = self.lock();
        loop {
            if should_stop() || (state.closed && state.queue.is_empty()) {
                return None;
            }
            if let Some(item) = state.queue.pop_front() {
                return Some(item);
            }
            if timeout.is_zero() {
                return None;
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let (guard, _) = self
                .ready
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = guard;
        }
    }

    /// 关闭队列：拒绝后续入队并唤醒所有等待者（已入队元素仍可取出）。
    pub fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        drop(state);
        self.ready.notify_all();
    }

    /// 唤醒所有等待者（通常配合外部停止标志使用）。
    pub fn notify_all(&self) {
        self.ready.notify_all();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State<T>> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_rejects_overflow_and_preserves_order() {
        let queue = BlockingQueue::with_capacity(2);
        assert!(queue.try_enqueue(1).is_ok());
        assert!(queue.try_enqueue(2).is_ok());
        assert_eq!(queue.try_enqueue(3), Err(3));
        assert_eq!(queue.len(), 2);
        assert_eq!(queue.try_dequeue(), Some(1));
        assert_eq!(queue.try_dequeue(), Some(2));
        assert_eq!(queue.try_dequeue(), None);
    }

    #[test]
    fn close_wakes_waiter_and_rejects_enqueue() {
        let queue = std::sync::Arc::new(BlockingQueue::new());
        let waiter = std::sync::Arc::clone(&queue);
        let handle = std::thread::spawn(move || waiter.dequeue_timeout(Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(20));
        queue.close();
        assert_eq!(handle.join().unwrap(), None);
        assert_eq!(queue.try_enqueue(1), Err(1));
    }

    #[test]
    fn stop_predicate_returns_immediately() {
        let queue = BlockingQueue::<i32>::new();
        let start = Instant::now();
        let item = queue.dequeue_timeout_while(Duration::from_secs(5), || true);
        assert_eq!(item, None);
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
