//! 线程与定时调度，对应 DH.NCore `NewLife.Threading`。
//!
//! - [`Cron`]：轻量级 Cron 表达式，语义对齐 C# `NewLife.Threading.Cron`
//! - [`Timer`] / [`TimerScheduler`]：不可重入定时器与调度器，语义对齐 C# `TimerX` / `TimerScheduler`

mod cron;
mod scheduler;

pub use cron::Cron;
pub use scheduler::{set_global_time_source, tick_count64, Timer, TimerScheduler};
