//! 线程与定时调度，对应 DH.NCore `NewLife.Threading`。
//!
//! - [`Cron`]：轻量级 Cron 表达式，语义对齐 C# `NewLife.Threading.Cron`
//! - [`Timer`] / [`TimerScheduler`]：不可重入定时器与调度器，语义对齐 C# `TimerX` / `TimerScheduler`

mod blocking_queue;
mod cron;
mod scheduler;

pub use blocking_queue::BlockingQueue;
pub use cron::Cron;
pub use scheduler::{set_global_time_source, tick_count64, Timer, TimerScheduler};

/// 启动命名线程（线程名便于排查；创建失败时 panic，与 `std::thread::spawn` 一致）。
pub fn spawn_named<F, T>(name: &str, f: F) -> std::thread::JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(f)
        .unwrap_or_else(|e| panic!("创建线程 {name} 失败：{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_named_sets_thread_name() {
        let handle = spawn_named("dhrust-test", || {
            std::thread::current().name().map(str::to_string)
        });
        assert_eq!(handle.join().unwrap().as_deref(), Some("dhrust-test"));
    }
}
