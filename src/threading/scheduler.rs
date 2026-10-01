//! 不可重入的定时器与调度器，语义对齐 DH.NCore `NewLife.Threading.TimerX` / `TimerScheduler`。
//!
//! 与 C# 版本的关键语义保持一致：
//! - 基于单调时钟刻度的调度，无惧系统时间回拨；
//! - 不可重入：上一次回调未结束时不会再次触发；
//! - 执行完成后才开始计算下一次间隔（非绝对定时器）；
//! - 回调异常（Rust 中为 panic）被捕获，不会杀死调度线程；
//! - 周期为 0 或负数时只执行一次，执行完成后自动销毁；
//! - 禁止小于 10ms 的周期任务，检查到时直接销毁（与 C# 一致，避免占用过多 CPU）；
//! - 调度线程在第一个定时器加入时启动，长时间空闲（60 秒）且无任务时自动退出；
//! - 绝对定时器与 Cron 定时器从调度器的时间源获取“当前时间”。
//!
//! # 与 C# 的差异
//!
//! - C# 使用弱引用关联委托宿主对象，被 GC 回收后自动移除定时器；Rust 无此机制，
//!   定时器由调度器持有强引用，需要显式调用 [`Timer::cancel`]（对应 C# `Dispose`）才能销毁。
//! - C# `Runtime.TickCount64` 是从系统开机起算的毫秒数；本实现从**进程启动**起算，
//!   仅要求单调性，不参与跨进程数据交换。
//!
//! # 示例
//!
//! ```no_run
//! use dhrust::threading::{Timer, TimerScheduler};
//! use std::sync::atomic::{AtomicI32, Ordering};
//! use std::sync::Arc;
//!
//! let count = Arc::new(AtomicI32::new(0));
//! let c = count.clone();
//! let timer = Timer::new(1000, 2000, move |_| {
//!     c.fetch_add(1, Ordering::SeqCst);
//! });
//!
//! // ... 业务运行 ...
//!
//! timer.cancel();
//! TimerScheduler::default_scheduler().dispose();
//! ```

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use chrono::{Local, NaiveDateTime};

use super::Cron;

/// 调度线程的默认等待周期（毫秒）。无定时器到期时，最长休眠该时长。
const DEFAULT_WAIT_MS: i64 = 60_000;

/// 最小周期（毫秒）。小于该值的周期任务会被直接销毁（与 C# 一致）。
const MIN_PERIOD_MS: i64 = 10;

/// 调度器销毁时等待调度线程退出的最长时间。
const DISPOSE_WAIT_MS: u64 = 5_000;

/// 异步执行线程的空闲存活时长；超时退出，后续任务到来时按需新建。
const ASYNC_IDLE_MS: u64 = 30_000;

/// 异步执行任务（调度器 + 定时器）。
type AsyncJob = (TimerScheduler, Timer);

type TimeSource = Arc<dyn Fn() -> NaiveDateTime + Send + Sync>;

static GLOBAL_TIME_SOURCE: Mutex<Option<TimeSource>> = Mutex::new(None);

/// 设置全局时间提供者。影响所有未单独设置时间源的调度器。
///
/// 对应 C# `TimerScheduler.GlobalTimeProvider`。
pub fn set_global_time_source<F>(source: F)
where
    F: Fn() -> NaiveDateTime + Send + Sync + 'static,
{
    *GLOBAL_TIME_SOURCE.lock().expect("global time source") = Some(Arc::new(source));
}

fn global_now() -> Option<NaiveDateTime> {
    let source = GLOBAL_TIME_SOURCE.lock().expect("global time source");
    source.as_ref().map(|f| f())
}

/// 进程启动以来的单调毫秒刻度。对应 C# `Runtime.TickCount64`（基准为进程启动）。
pub fn tick_count64() -> i64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as i64
}

/// 定时器调度器。
///
/// 同名调度器全局唯一（对应 C# `TimerScheduler.Create` 的缓存语义）。
#[derive(Clone)]
pub struct TimerScheduler {
    inner: Arc<SchedulerInner>,
}

struct SchedulerInner {
    name: String,
    state: Mutex<SchedulerState>,
    /// 等待/唤醒信号
    signal: Condvar,
    /// 当前等待周期（毫秒），可被检查与执行完成收缩
    wait_ms: AtomicI64,
    next_id: AtomicI32,
    disposing: AtomicBool,
    time_source: Mutex<Option<TimeSource>>,
    /// 空闲异步执行线程的发送端队列：优先复用空闲线程，全部繁忙时新建线程，
    /// 保持“每 tick 一条执行线程”的并行语义，仅在低负载时收敛线程数量。
    async_idle: Mutex<Vec<Sender<AsyncJob>>>,
}

struct SchedulerState {
    timers: Vec<Timer>,
    thread: Option<JoinHandle<()>>,
    /// 调度线程已退出（无线程运行时为 true）
    finished: bool,
    /// 变更代数，用于避免丢失唤醒
    generation: u64,
}

impl SchedulerState {
    fn bump(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }
}

static REGISTRY: OnceLock<Mutex<HashMap<String, TimerScheduler>>> = OnceLock::new();

fn registry() -> &'static Mutex<HashMap<String, TimerScheduler>> {
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

impl TimerScheduler {
    /// 创建（或获取）指定名称的调度器。
    pub fn create(name: &str) -> TimerScheduler {
        let mut map = registry().lock().expect("scheduler registry");
        if let Some(scheduler) = map.get(name) {
            return scheduler.clone();
        }

        let scheduler = TimerScheduler::new_named(name);
        map.insert(name.to_string(), scheduler.clone());
        scheduler
    }

    /// 默认调度器。
    pub fn default_scheduler() -> TimerScheduler {
        static DEFAULT: OnceLock<TimerScheduler> = OnceLock::new();
        DEFAULT
            .get_or_init(|| TimerScheduler::create("Default"))
            .clone()
    }

    /// 清理全部调度器（对应 C# `TimerScheduler.ClearAll`，宿主退出时调用）。
    pub fn clear_all() {
        let schedulers: Vec<TimerScheduler> = {
            let mut map = registry().lock().expect("scheduler registry");
            map.drain().map(|(_, v)| v).collect()
        };
        for scheduler in schedulers {
            scheduler.dispose();
        }
    }

    fn new_named(name: &str) -> TimerScheduler {
        TimerScheduler {
            inner: Arc::new(SchedulerInner {
                name: name.to_string(),
                state: Mutex::new(SchedulerState {
                    timers: Vec::new(),
                    thread: None,
                    finished: true,
                    generation: 0,
                }),
                signal: Condvar::new(),
                wait_ms: AtomicI64::new(DEFAULT_WAIT_MS),
                next_id: AtomicI32::new(0),
                disposing: AtomicBool::new(false),
                time_source: Mutex::new(None),
                async_idle: Mutex::new(Vec::new()),
            }),
        }
    }

    /// 名称。
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// 定时器个数。
    pub fn count(&self) -> usize {
        self.inner
            .state
            .lock()
            .expect("scheduler state")
            .timers
            .len()
    }

    /// 设置时间提供者。该调度器下所有绝对定时器与 Cron 定时器均从此获取当前时间。
    pub fn set_time_source<F>(&self, source: F)
    where
        F: Fn() -> NaiveDateTime + Send + Sync + 'static,
    {
        *self
            .inner
            .time_source
            .lock()
            .expect("scheduler time source") = Some(Arc::new(source));
    }

    /// 获取当前时间（本地时间）。
    ///
    /// 优先使用调度器自己的时间源，其次全局时间源，最后取系统本地时间。
    pub fn now(&self) -> NaiveDateTime {
        let local = self
            .inner
            .time_source
            .lock()
            .expect("scheduler time source");
        if let Some(source) = local.as_ref() {
            return source();
        }
        drop(local);

        if let Some(now) = global_now() {
            return now;
        }

        Local::now().naive_local()
    }

    /// 唤醒调度线程。
    pub fn wake(&self) {
        let mut state = self.inner.state.lock().expect("scheduler state");
        state.bump();
        self.inner.signal.notify_all();
        drop(state);
    }

    /// 销毁调度器：停止调度线程并移除全部定时器。
    pub fn dispose(&self) {
        if self.inner.disposing.swap(true, Ordering::SeqCst) {
            return;
        }

        // 唤醒调度线程，等待其退出（最多 5 秒）
        self.wake();
        let mut state = self.inner.state.lock().expect("scheduler state");
        let deadline = Instant::now() + Duration::from_millis(DISPOSE_WAIT_MS);
        while !state.finished {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let (next, _) = self
                .inner
                .signal
                .wait_timeout(state, deadline - now)
                .expect("scheduler state");
            state = next;
        }

        // 清理全部定时器
        for timer in state.timers.drain(..) {
            timer.mark_cancelled();
        }
    }

    fn ensure_thread(&self, state: &mut SchedulerState) {
        if state.thread.is_some() {
            return;
        }

        let inner = Arc::clone(&self.inner);
        let name = if self.inner.name == "Default" {
            "T".to_string()
        } else {
            self.inner.name.clone()
        };
        let handle = thread::Builder::new()
            .name(name)
            .spawn(move || process(inner))
            .expect("spawn timer scheduler thread");
        state.thread = Some(handle);
        state.finished = false;
    }

    fn add(&self, timer: &Timer) {
        assert!(
            !self.inner.disposing.load(Ordering::SeqCst),
            "TimerScheduler has been disposed"
        );

        timer.set_id(self.inner.next_id.fetch_add(1, Ordering::SeqCst) + 1);

        let mut state = self.inner.state.lock().expect("scheduler state");
        if !state
            .timers
            .iter()
            .any(|t| Arc::ptr_eq(&t.inner, &timer.inner))
        {
            state.timers.push(timer.clone());
        }
        state.bump();
        self.ensure_thread(&mut state);
        self.inner.signal.notify_all();
        drop(state);
    }

    fn remove(&self, timer: &Timer) {
        if timer.id() == 0 {
            return;
        }

        timer.set_id(0);

        let mut state = self.inner.state.lock().expect("scheduler state");
        if let Some(pos) = state
            .timers
            .iter()
            .position(|t| Arc::ptr_eq(&t.inner, &timer.inner))
        {
            state.timers.remove(pos);
            state.bump();
        }
        self.inner.signal.notify_all();
        drop(state);
    }

    /// 执行完成后的下一次等待周期收缩（对应 C# `OnFinish` 中的 `_period` 更新）。
    ///
    /// 异步任务在调度线程已经开始等待后才完成收缩时，必须唤醒调度线程，
    /// 否则会按收缩前的更长周期休眠（最坏可达一整轮空闲周期）。
    fn shrink_wait(&self, period_ms: i64) {
        if period_ms <= 0 {
            return;
        }

        let prev = self.inner.wait_ms.fetch_min(period_ms, Ordering::SeqCst);
        if period_ms < prev {
            self.inner.signal.notify_all();
        }
    }

    /// 调度线程是否仍在运行（测试与自省用）。
    #[cfg(test)]
    fn worker_running(&self) -> bool {
        self.inner
            .state
            .lock()
            .expect("scheduler state")
            .thread
            .is_some()
    }

    fn upgrade(inner: &Weak<SchedulerInner>) -> Option<TimerScheduler> {
        inner.upgrade().map(|inner| TimerScheduler { inner })
    }
}

impl fmt::Display for TimerScheduler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.inner.name)
    }
}

/// 调度主循环。对应 C# `TimerScheduler.Process`。
fn process(inner: Arc<SchedulerInner>) {
    let scheduler = TimerScheduler {
        inner: Arc::clone(&inner),
    };

    // 是否因空闲而销毁线程（该路径已在空列表检查的同一临界区内更新过 thread/finished）
    let mut idle_exited = false;

    loop {
        if inner.disposing.load(Ordering::SeqCst) {
            break;
        }

        // 准备好定时器列表与变更代数
        let (timers, generation) = {
            let state = inner.state.lock().expect("scheduler state");
            (state.timers.clone(), state.generation)
        };

        // 没有任务时，等待一个完整周期后销毁线程（对应 C# 空任务销毁逻辑）。
        // 注意：thread/finished 必须与“空列表”检查在同一临界区内更新；
        // 否则并发 Add 可能落在“检查通过、线程置空之前”的窗口里，
        // 看到旧线程句柄而不启动新线程，最终定时器无人调度（悬挂）
        if timers.is_empty() && inner.wait_ms.load(Ordering::SeqCst) >= DEFAULT_WAIT_MS {
            let mut state = inner.state.lock().expect("scheduler state");
            if state.timers.is_empty() && inner.wait_ms.load(Ordering::SeqCst) >= DEFAULT_WAIT_MS {
                state.thread = None;
                state.finished = true;
                inner.signal.notify_all();
                idle_exited = true;
                break;
            }
        }

        let now = tick_count64();

        // 设置一个较大的间隔，内部会根据处理情况调整该值为最合理值
        inner.wait_ms.store(DEFAULT_WAIT_MS, Ordering::SeqCst);
        for timer in &timers {
            if inner.disposing.load(Ordering::SeqCst) {
                break;
            }

            if timer.calling() {
                continue;
            }
            if !check_time(&scheduler, timer, now) {
                continue;
            }

            // 必须在主线程设置状态，异步线程还没执行前，主线程不能开启新的一轮调度
            timer.set_calling(true);
            if timer.is_async() {
                dispatch_async(&inner, scheduler.clone(), timer.clone());
            } else {
                execute(&scheduler, timer);
            }
        }

        if inner.disposing.load(Ordering::SeqCst) {
            break;
        }

        // 等待唤醒或超时。
        // - 代数变化（新增/移除/唤醒）→ 立即重新扫描；
        // - 周期被收缩（异步任务完成）→ 按最新周期继续等待；
        // - 本轮周期耗尽 → 重新扫描。
        let mut state = inner.state.lock().expect("scheduler state");
        loop {
            if state.generation != generation {
                break;
            }

            let wait = inner.wait_ms.load(Ordering::SeqCst);
            if wait <= 0 {
                break;
            }

            let (next, timeout) = inner
                .signal
                .wait_timeout(state, Duration::from_millis(wait as u64))
                .expect("scheduler state");
            state = next;

            if timeout.timed_out() {
                break;
            }
        }
    }

    // 统一退出路径：非空闲退出（dispose）时标记线程退出并唤醒等待者。
    // 空闲销毁路径已在空列表检查的同一临界区内更新过状态，不能再写：
    // 两次写之间可能有新的 Add 启动了新线程，重复置空会让后续 Add 误判并重复拉起调度线程
    if !idle_exited {
        let mut state = inner.state.lock().expect("scheduler state");
        state.thread = None;
        state.finished = true;
        inner.signal.notify_all();
    }
}

/// 检查定时器是否到期。对应 C# `TimerScheduler.CheckTime`。
fn check_time(scheduler: &TimerScheduler, timer: &Timer, now: i64) -> bool {
    // 删除过期的，为了避免占用过多CPU资源，TimerX禁止小于10ms的任务调度
    let p = timer.period();
    if p > 0 && p < MIN_PERIOD_MS {
        // 为了避免占用过多CPU资源，关闭周期小于 10ms 的任务
        scheduler.remove(timer);
        return false;
    }

    let ts = timer.next_tick() - now;
    if ts > 0 {
        // 缩小间隔，便于快速调用
        scheduler.shrink_wait(ts);
        return false;
    }

    true
}

/// 派发异步定时器任务：优先复用空闲的异步执行线程，池空则新建线程。
/// 保持与“每 tick 一线程”相同的并行语义（繁忙任务不排队），仅复用空闲线程。
fn dispatch_async(inner: &Arc<SchedulerInner>, scheduler: TimerScheduler, timer: Timer) {
    // 先尝试复用：send 失败说明该线程已退出（空闲超时），丢弃后继续找
    {
        let mut idle = inner.async_idle.lock().expect("async idle pool");
        while let Some(tx) = idle.pop() {
            if tx.send((scheduler.clone(), timer.clone())).is_ok() {
                return;
            }
        }
    }

    let (tx, rx) = mpsc::channel::<AsyncJob>();
    let pool_inner = Arc::clone(inner);
    let worker_tx = tx.clone();
    let _ = thread::Builder::new()
        .name("timer:Async".to_string())
        .spawn(move || async_worker(pool_inner, worker_tx, rx));
    let _ = tx.send((scheduler, timer));
}

/// 异步执行线程主体：循环执行任务；空闲超过 `ASYNC_IDLE_MS` 后退出。
/// 线程仅在被派发前注册进空闲池（池中线程均为空闲），执行期间不在池中，
/// 从而“池里取到即复用、池空即新建”，与“每 tick 一线程”的并行度一致。
fn async_worker(inner: Arc<SchedulerInner>, my_tx: Sender<AsyncJob>, rx: Receiver<AsyncJob>) {
    loop {
        // 注册为空闲等待复用；被派发时其 tx 会从池中取出，任务经 channel 送达
        inner
            .async_idle
            .lock()
            .expect("async idle pool")
            .push(my_tx.clone());

        match rx.recv_timeout(Duration::from_millis(ASYNC_IDLE_MS)) {
            Ok((scheduler, timer)) => execute(&scheduler, &timer),
            // 空闲超时或通道关闭：退出线程（下次派发时按需新建）
            Err(_) => break,
        }
    }
}

/// 执行一次回调。对应 C# `TimerScheduler.Execute`（含异常隔离与 `OnExecuted`）。
fn execute(scheduler: &TimerScheduler, timer: &Timer) {
    Timer::set_current(Some(timer.clone()));

    timer.set_has_set_next(false);

    let started = Instant::now();
    // 用户回调 panic 时捕获，避免调度线程被杀死（对应 C# catch Exception）
    let callback = timer.callback();
    let _ = catch_unwind(AssertUnwindSafe(|| callback(timer)));
    let cost = started.elapsed().as_millis() as i32;

    on_executed(scheduler, timer, cost);

    Timer::set_current(None);
}

/// 执行完成后收尾。对应 C# `TimerScheduler.OnExecuted` + `OnFinish`。
fn on_executed(scheduler: &TimerScheduler, timer: &Timer, ms: i32) {
    let cost = timer.cost();
    timer.set_cost(if cost == 0 { ms } else { (cost + ms) / 2 });

    timer.inc_timers();

    // 如果内部设置了下一次时间，则不再递加周期
    let p = set_and_get_next_time(timer);

    // 清理一次性定时器
    if p <= 0 {
        scheduler.remove(timer);
        timer.mark_cancelled();
    } else {
        scheduler.shrink_wait(p);
    }

    timer.set_calling(false);
}

/// 设置下一次执行时间，并获取间隔。对应 C# `TimerX.SetAndGetNextTime`。
fn set_and_get_next_time(timer: &Timer) -> i64 {
    let period = timer.period();
    let now_tick = tick_count64();
    if timer.has_set_next() {
        let ts = timer.next_tick() - now_tick;
        return if ts > 0 { ts } else { period };
    }

    if timer.is_absolutely() {
        let now = timer.now();

        let next = if !timer.crons().is_empty() {
            // Cron以当前时间开始计算下一次
            let first = Cron::get_next_multi(timer.crons(), now);
            match first {
                Some(mut next) => {
                    // 如果cron计算得到的下一次时间过近，则需要重新计算
                    if (next - now).num_milliseconds() < 1000 {
                        if let Some(re) = Cron::get_next_multi(timer.crons(), next) {
                            next = re;
                        }
                    }
                    next
                }
                // 一年内无匹配：退化为按周期重试（C# 此处会得到 MinValue 并发生整数溢出，这里做了防御）
                None => {
                    timer.set_next_tick(period);
                    return period;
                }
            }
        } else {
            // 能够处理基准时间变大，但不能处理基准时间变小
            let mut next = timer.absolutely_next();
            while next < now {
                next += chrono::Duration::milliseconds(period);
            }
            next
        };

        // 即使基准时间改变，也不影响绝对时间定时器的执行时刻
        timer.set_absolutely_next(next);
        let ts = ((next - now).num_milliseconds() as f64).round() as i64;
        timer.set_next_tick(ts);

        return if ts > 0 { ts } else { period };
    }

    timer.set_next_tick(period);

    period
}

/// 不可重入的定时器，对应 C# `TimerX`。
#[derive(Clone)]
pub struct Timer {
    inner: Arc<TimerInner>,
}

struct TimerInner {
    scheduler: Weak<SchedulerInner>,
    id: AtomicI32,
    period: AtomicI64,
    async_flag: AtomicBool,
    calling: AtomicBool,
    absolutely: AtomicBool,
    has_set_next: AtomicBool,
    next_tick: AtomicI64,
    base_time: Mutex<NaiveDateTime>,
    absolutely_next: Mutex<NaiveDateTime>,
    crons: Vec<Cron>,
    timers_count: AtomicI32,
    cost: AtomicI32,
    name: Mutex<String>,
    callback: Box<dyn Fn(&Timer) + Send + Sync>,
}

thread_local! {
    static CURRENT_TIMER: RefCell<Option<Timer>> = const { RefCell::new(None) };
}

impl Timer {
    /// 实例化一个不可重入的定时器，使用默认调度器。
    ///
    /// - `due_time_ms`：多久之后开始（毫秒），必须大于等于 0
    /// - `period_ms`：间隔周期（毫秒），设为 0 或负数则只调用一次
    ///
    /// # Panics
    /// 当 `due_time_ms` 小于 0，或默认调度器已销毁时。
    pub fn new<F>(due_time_ms: i64, period_ms: i64, callback: F) -> Timer
    where
        F: Fn(&Timer) + Send + Sync + 'static,
    {
        Timer::with_scheduler(
            TimerScheduler::default_scheduler(),
            due_time_ms,
            period_ms,
            callback,
        )
    }

    /// 实例化一个不可重入的定时器，指定调度器。
    ///
    /// # Panics
    /// 当 `due_time_ms` 小于 0，或调度器已销毁时。
    pub fn with_scheduler<F>(
        scheduler: TimerScheduler,
        due_time_ms: i64,
        period_ms: i64,
        callback: F,
    ) -> Timer
    where
        F: Fn(&Timer) + Send + Sync + 'static,
    {
        assert!(due_time_ms >= 0, "due_time_ms must be >= 0");

        let timer = Timer::build(scheduler.clone(), period_ms, Vec::new(), callback);
        timer.set_next_tick(due_time_ms);
        scheduler.add(&timer);

        timer
    }

    /// 实例化一个绝对定时器，指定时刻执行，跟当前时间和 SetNext 无关。
    ///
    /// - `start_time`：绝对开始时间
    /// - `period_ms`：间隔周期（毫秒），必须大于 0
    ///
    /// # Panics
    /// 当 `period_ms` 小于等于 0，或调度器已销毁时。
    pub fn new_at<F>(start_time: NaiveDateTime, period_ms: i64, callback: F) -> Timer
    where
        F: Fn(&Timer) + Send + Sync + 'static,
    {
        Timer::new_at_with_scheduler(
            TimerScheduler::default_scheduler(),
            start_time,
            period_ms,
            callback,
        )
    }

    /// 实例化一个绝对定时器，指定调度器与绝对开始时间。
    pub fn new_at_with_scheduler<F>(
        scheduler: TimerScheduler,
        start_time: NaiveDateTime,
        period_ms: i64,
        callback: F,
    ) -> Timer
    where
        F: Fn(&Timer) + Send + Sync + 'static,
    {
        assert!(period_ms > 0, "period_ms must be > 0 for absolute timer");

        let timer = Timer::build(scheduler.clone(), period_ms, Vec::new(), callback);
        timer.set_absolutely(true);

        let now = scheduler.now();
        let mut next = start_time;
        while next < now {
            next += chrono::Duration::milliseconds(period_ms);
        }

        let ms = (next - now).num_milliseconds();
        timer.set_absolutely_next(next);
        timer.set_next_tick(ms);
        scheduler.add(&timer);

        timer
    }

    /// 实例化一个 Cron 定时器。支持多个表达式，分号分隔。
    ///
    /// 表达式非法时返回错误（对应 C# 构造函数抛出 `ArgumentException`）。
    pub fn new_cron<F>(cron_expression: &str, callback: F) -> Result<Timer, String>
    where
        F: Fn(&Timer) + Send + Sync + 'static,
    {
        Timer::new_cron_with_scheduler(
            TimerScheduler::default_scheduler(),
            cron_expression,
            callback,
        )
    }

    /// 实例化一个 Cron 定时器，指定调度器。
    pub fn new_cron_with_scheduler<F>(
        scheduler: TimerScheduler,
        cron_expression: &str,
        callback: F,
    ) -> Result<Timer, String>
    where
        F: Fn(&Timer) + Send + Sync + 'static,
    {
        let mut crons = Vec::new();
        for item in cron_expression.split(';') {
            let cron =
                Cron::parse(item).ok_or_else(|| format!("Invalid Cron expression[{item}]"))?;
            crons.push(cron);
        }
        if crons.is_empty() {
            return Err("Invalid Cron expression".to_string());
        }

        let timer = Timer::build(scheduler.clone(), 0, crons, callback);
        timer.set_absolutely(true);

        let now = scheduler.now();
        let next = Cron::get_next_multi(timer.crons(), now)
            .ok_or_else(|| format!("Cron expression[{cron_expression}] has no next time"))?;
        let ms = (next - now).num_milliseconds();
        timer.set_absolutely_next(next);
        timer.set_next_tick(ms);
        scheduler.add(&timer);

        Ok(timer)
    }

    /// 延迟执行一个委托。对应 C# `TimerX.Delay`（异步 + 只执行一次）。
    ///
    /// # Panics
    /// 当 `delay_ms` 小于 0，或默认调度器已销毁时。
    pub fn delay<F>(delay_ms: i64, callback: F) -> Timer
    where
        F: Fn(&Timer) + Send + Sync + 'static,
    {
        let timer = Timer::new(delay_ms, 0, callback);
        timer.set_async(true);
        timer
    }

    fn build<F>(scheduler: TimerScheduler, period_ms: i64, crons: Vec<Cron>, callback: F) -> Timer
    where
        F: Fn(&Timer) + Send + Sync + 'static,
    {
        Timer {
            inner: Arc::new(TimerInner {
                scheduler: Arc::downgrade(&scheduler.inner),
                id: AtomicI32::new(0),
                period: AtomicI64::new(period_ms),
                async_flag: AtomicBool::new(false),
                calling: AtomicBool::new(false),
                absolutely: AtomicBool::new(false),
                has_set_next: AtomicBool::new(false),
                next_tick: AtomicI64::new(tick_count64()),
                base_time: Mutex::new(scheduler.now()),
                absolutely_next: Mutex::new(NaiveDateTime::MIN),
                crons,
                timers_count: AtomicI32::new(0),
                cost: AtomicI32::new(0),
                name: Mutex::new("timer".to_string()),
                callback: Box::new(callback),
            }),
        }
    }

    /// 当前定时器。在回调执行期间可获取当前定时器实例。
    pub fn current() -> Option<Timer> {
        CURRENT_TIMER.with(|c| c.borrow().clone())
    }

    fn set_current(timer: Option<Timer>) {
        CURRENT_TIMER.with(|c| *c.borrow_mut() = timer);
    }

    /// 编号。定时器唯一标识，销毁后为 0。
    pub fn id(&self) -> i32 {
        self.inner.id.load(Ordering::SeqCst)
    }

    fn set_id(&self, id: i32) {
        self.inner.id.store(id, Ordering::SeqCst);
    }

    /// 所属调度器。
    pub fn scheduler(&self) -> Option<TimerScheduler> {
        TimerScheduler::upgrade(&self.inner.scheduler)
    }

    /// 间隔周期。毫秒，设为 0 或负数则只调用一次。
    pub fn period(&self) -> i64 {
        self.inner.period.load(Ordering::SeqCst)
    }

    /// 设置间隔周期。毫秒。
    pub fn set_period(&self, period_ms: i64) {
        self.inner.period.store(period_ms, Ordering::SeqCst);
    }

    /// 异步执行任务。默认 false（在调度线程中同步执行）。
    pub fn is_async(&self) -> bool {
        self.inner.async_flag.load(Ordering::SeqCst)
    }

    /// 设置异步执行任务。
    pub fn set_async(&self, value: bool) {
        self.inner.async_flag.store(value, Ordering::SeqCst);
    }

    /// 绝对精确时间执行。
    pub fn is_absolutely(&self) -> bool {
        self.inner.absolutely.load(Ordering::SeqCst)
    }

    fn set_absolutely(&self, value: bool) {
        self.inner.absolutely.store(value, Ordering::SeqCst);
    }

    /// 是否正在执行。由执行线程写入、调度线程读取，防止重复触发。
    pub fn calling(&self) -> bool {
        self.inner.calling.load(Ordering::SeqCst)
    }

    fn set_calling(&self, value: bool) {
        self.inner.calling.store(value, Ordering::SeqCst);
    }

    /// 调用次数。
    pub fn timers_count(&self) -> i32 {
        self.inner.timers_count.load(Ordering::SeqCst)
    }

    fn inc_timers(&self) {
        self.inner.timers_count.fetch_add(1, Ordering::SeqCst);
    }

    /// 平均耗时。毫秒。
    pub fn cost(&self) -> i32 {
        self.inner.cost.load(Ordering::SeqCst)
    }

    fn set_cost(&self, cost: i32) {
        self.inner.cost.store(cost, Ordering::SeqCst);
    }

    /// 下一次执行时间。进程启动以来的毫秒刻度。
    pub fn next_tick(&self) -> i64 {
        self.inner.next_tick.load(Ordering::SeqCst)
    }

    /// 下一次调用时间。
    pub fn next_time(&self) -> NaiveDateTime {
        let base = *self.inner.base_time.lock().expect("timer base time");
        base + chrono::Duration::milliseconds(self.next_tick())
    }

    /// Cron 表达式集合。
    pub fn crons(&self) -> &[Cron] {
        &self.inner.crons
    }

    /// 调试名称。默认 `timer`。
    pub fn name(&self) -> String {
        self.inner.name.lock().expect("timer name").clone()
    }

    /// 设置调试名称，用于日志与 [`fmt::Display`] 输出。
    pub fn with_name(self, name: &str) -> Timer {
        *self.inner.name.lock().expect("timer name") = name.to_string();
        self
    }

    fn set_has_set_next(&self, value: bool) {
        self.inner.has_set_next.store(value, Ordering::SeqCst);
    }

    fn has_set_next(&self) -> bool {
        self.inner.has_set_next.load(Ordering::SeqCst)
    }

    fn set_next_tick(&self, ms: i64) {
        // 使用开机滴答来做定时调度，无惧时间回拨，每次修正时间基准
        let tick = tick_count64();
        let now = self.now();
        *self.inner.base_time.lock().expect("timer base time") =
            now - chrono::Duration::milliseconds(tick);
        self.inner.next_tick.store(tick + ms, Ordering::SeqCst);
    }

    fn set_absolutely_next(&self, next: NaiveDateTime) {
        *self
            .inner
            .absolutely_next
            .lock()
            .expect("timer absolutely next") = next;
    }

    fn absolutely_next(&self) -> NaiveDateTime {
        *self
            .inner
            .absolutely_next
            .lock()
            .expect("timer absolutely next")
    }

    fn now(&self) -> NaiveDateTime {
        self.scheduler()
            .map(|s| s.now())
            .unwrap_or_else(|| Local::now().naive_local())
    }

    fn callback(&self) -> &(dyn Fn(&Timer) + Send + Sync) {
        self.inner.callback.as_ref()
    }

    /// 设置下一次运行时间。
    ///
    /// # 参数
    /// - `ms`：延迟毫秒数。小于等于 0 表示马上调度
    pub fn set_next(&self, ms: i64) {
        self.set_next_tick(ms);
        self.set_has_set_next(true);

        if let Some(scheduler) = self.scheduler() {
            scheduler.wake();
        }
    }

    /// 销毁定时器（对应 C# `TimerX.Dispose`）。幂等。
    pub fn cancel(&self) {
        if let Some(scheduler) = self.scheduler() {
            scheduler.remove(self);
        }
        self.mark_cancelled();
    }

    fn mark_cancelled(&self) {
        self.set_id(0);
    }
}

impl fmt::Display for Timer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let target = if self.inner.crons.is_empty() {
            format!("{}ms", self.period())
        } else {
            self.inner
                .crons
                .iter()
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join(";")
        };
        write!(f, "[{}]{} ({})", self.id(), self.name(), target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicI32;

    fn wait_until(condition: impl Fn() -> bool, timeout_ms: u64) -> bool {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        condition()
    }

    #[test]
    fn timer_runs_and_counts() {
        let scheduler = TimerScheduler::create("test-runs");
        let count = Arc::new(AtomicI32::new(0));
        let c = count.clone();
        let timer = Timer::with_scheduler(scheduler.clone(), 30, 60, move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        });

        assert!(timer.id() > 0);
        assert_eq!(timer.scheduler().unwrap().name(), "test-runs");
        assert_eq!(timer.period(), 60);
        assert!(!timer.is_async());
        assert!(!timer.is_absolutely());
        assert_eq!(timer.timers_count(), 0);

        assert!(
            wait_until(|| count.load(Ordering::SeqCst) >= 3, 3000),
            "定时器未按期执行，count={}",
            count.load(Ordering::SeqCst)
        );
        assert!(timer.timers_count() >= 3);

        timer.cancel();
        let after = count.load(Ordering::SeqCst);
        thread::sleep(Duration::from_millis(200));
        assert_eq!(count.load(Ordering::SeqCst), after);
        assert_eq!(timer.id(), 0);

        scheduler.dispose();
    }

    #[test]
    fn one_shot_timer_stops() {
        let scheduler = TimerScheduler::create("test-once");
        let count = Arc::new(AtomicI32::new(0));
        let c = count.clone();
        let timer = Timer::with_scheduler(scheduler.clone(), 30, 0, move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        });

        assert!(wait_until(|| count.load(Ordering::SeqCst) >= 1, 2000));
        thread::sleep(Duration::from_millis(300));
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(scheduler.count(), 0, "一次性定时器应自动移除");
        assert_eq!(timer.id(), 0);

        scheduler.dispose();
    }

    #[test]
    fn set_next_reschedules() {
        let scheduler = TimerScheduler::create("test-setnext");
        let count = Arc::new(AtomicI32::new(0));
        let c = count.clone();
        // 首次延迟很长，通过 SetNext 提前触发
        let timer = Timer::with_scheduler(scheduler.clone(), 60_000, 0, move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        });

        timer.set_next(30);
        assert!(wait_until(|| count.load(Ordering::SeqCst) >= 1, 2000));

        scheduler.dispose();
    }

    #[test]
    fn non_reentrant() {
        let scheduler = TimerScheduler::create("test-nonreentrant");
        let running = Arc::new(AtomicBool::new(false));
        let violation = Arc::new(AtomicBool::new(false));
        let count = Arc::new(AtomicI32::new(0));

        let r = running.clone();
        let v = violation.clone();
        let c = count.clone();
        let timer = Timer::with_scheduler(scheduler.clone(), 10, 20, move |_| {
            if r.swap(true, Ordering::SeqCst) {
                v.store(true, Ordering::SeqCst);
            }
            c.fetch_add(1, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(40));
            r.store(false, Ordering::SeqCst);
        });

        assert!(wait_until(|| count.load(Ordering::SeqCst) >= 3, 3000));
        assert!(!violation.load(Ordering::SeqCst), "回调发生了重入");

        timer.cancel();
        scheduler.dispose();
    }

    #[test]
    fn async_timer_runs() {
        let scheduler = TimerScheduler::create("test-async");
        let count = Arc::new(AtomicI32::new(0));
        let c = count.clone();
        let timer = Timer::with_scheduler(scheduler.clone(), 20, 40, move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        });
        timer.set_async(true);

        // 异步任务不应阻塞调度线程：同一调度器上的同步任务照常执行
        let count2 = Arc::new(AtomicI32::new(0));
        let c2 = count2.clone();
        let timer2 = Timer::with_scheduler(scheduler.clone(), 20, 40, move |_| {
            c2.fetch_add(1, Ordering::SeqCst);
        });

        assert!(wait_until(
            || count.load(Ordering::SeqCst) >= 2 && count2.load(Ordering::SeqCst) >= 2,
            3000
        ));

        timer.cancel();
        timer2.cancel();
        scheduler.dispose();
    }

    #[test]
    fn async_workers_are_reused_when_idle() {
        // 线程复用：连续多次异步触发后，空闲执行线程应留在池中等待复用
        //（旧实现为每 tick 新建线程，空闲池恒为空）
        let scheduler = TimerScheduler::create("test-async-reuse");
        let count = Arc::new(AtomicI32::new(0));
        let c = count.clone();
        let timer = Timer::with_scheduler(scheduler.clone(), 0, 30, move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        });
        timer.set_async(true);

        assert!(
            wait_until(|| count.load(Ordering::SeqCst) >= 4, 3000),
            "异步定时器应按时重复触发，count={}",
            count.load(Ordering::SeqCst)
        );
        // 池非空 = 存在空闲执行线程等待复用（复用失效时为每 tick 新建且立即退出）
        assert!(
            wait_until(|| !scheduler.inner.async_idle.lock().expect("pool").is_empty(), 2000),
            "空闲异步执行线程应被复用（池为空说明仍按 tick 新建线程）"
        );
        // 多次触发只应留下极少的空闲线程（复用而非线性增长）
        let idle = scheduler.inner.async_idle.lock().expect("pool").len();
        assert!(
            idle <= 4,
            "空闲线程数应远小于触发次数，实际 {idle}（count={}）",
            count.load(Ordering::SeqCst)
        );

        timer.cancel();
        scheduler.dispose();
    }

    #[test]
    fn async_timer_zero_due_fires_repeatedly() {
        // 回归：单定时器（没有其它定时器为异步完成争取时间窗口）时，
        // 异步任务完成后的周期收缩必须能唤醒调度线程，
        // 否则调度线程会按收缩前的空闲周期（60s）休眠
        let scheduler = TimerScheduler::create("test-async-zero-due");
        let count = Arc::new(AtomicI32::new(0));
        let c = count.clone();
        let timer = Timer::with_scheduler(scheduler.clone(), 0, 200, move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        });
        timer.set_async(true);

        assert!(
            wait_until(|| count.load(Ordering::SeqCst) >= 3, 3000),
            "异步定时器未按期重复触发，count={}",
            count.load(Ordering::SeqCst)
        );

        timer.cancel();
        scheduler.dispose();
    }

    #[test]
    fn panic_in_callback_does_not_kill_scheduler() {
        let scheduler = TimerScheduler::create("test-panic");
        let count = Arc::new(AtomicI32::new(0));
        let c = count.clone();
        let timer = Timer::with_scheduler(scheduler.clone(), 20, 40, move |_| {
            let n = c.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                panic!("第一次调用故意 panic");
            }
        });

        assert!(
            wait_until(|| count.load(Ordering::SeqCst) >= 3, 3000),
            "回调 panic 后调度器应继续运行，count={}",
            count.load(Ordering::SeqCst)
        );

        timer.cancel();
        scheduler.dispose();
    }

    #[test]
    fn min_period_timer_is_disposed() {
        let scheduler = TimerScheduler::create("test-minperiod");
        let count = Arc::new(AtomicI32::new(0));
        let c = count.clone();
        let timer = Timer::with_scheduler(scheduler.clone(), 5, 5, move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        });

        // 小于 10ms 的周期任务会被直接销毁
        assert!(wait_until(|| scheduler.count() == 0, 1000));
        thread::sleep(Duration::from_millis(100));
        assert_eq!(count.load(Ordering::SeqCst), 0);
        assert_eq!(timer.id(), 0);

        scheduler.dispose();
    }

    #[test]
    fn cron_timer_fires() {
        let scheduler = TimerScheduler::create("test-cron");
        let count = Arc::new(AtomicI32::new(0));
        let c = count.clone();
        // 每秒一次
        let timer = Timer::new_cron_with_scheduler(scheduler.clone(), "*/1 * * * * *", move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();

        assert!(timer.is_absolutely());
        assert_eq!(timer.crons().len(), 1);
        // 下一次执行时间应在 2 秒内（带亚秒的起点会向前对齐）
        let now = scheduler.now();
        assert!((timer.next_time() - now).num_milliseconds() <= 2000);

        assert!(
            wait_until(|| count.load(Ordering::SeqCst) >= 2, 4000),
            "Cron 定时器未按期执行，count={}",
            count.load(Ordering::SeqCst)
        );

        timer.cancel();
        scheduler.dispose();
    }

    #[test]
    fn cron_timer_invalid_expression() {
        let scheduler = TimerScheduler::create("test-cron-invalid");
        let result = Timer::new_cron_with_scheduler(scheduler.clone(), "not a cron", |_| {});
        assert!(result.is_err());
        scheduler.dispose();
    }

    #[test]
    fn absolute_timer_schedules_next_time() {
        let scheduler = TimerScheduler::create("test-absolute");
        let start = scheduler.now() + chrono::Duration::milliseconds(500);
        let timer = Timer::new_at_with_scheduler(scheduler.clone(), start, 1000, |_| {});

        assert!(timer.is_absolutely());
        // 下一次时间应紧邻指定的绝对时刻（容忍 100ms 计算误差）
        let delta = (timer.next_time() - start).num_milliseconds().abs();
        assert!(delta <= 100, "next_time 与绝对开始时间相差 {delta}ms");

        timer.cancel();
        scheduler.dispose();
    }

    #[test]
    fn delay_runs_once_async() {
        let count = Arc::new(AtomicI32::new(0));
        let c = count.clone();
        let timer = Timer::delay(30, move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        });

        assert!(timer.is_async());
        assert_eq!(timer.period(), 0);
        assert_eq!(timer.scheduler().unwrap().name(), "Default");
        assert!(wait_until(|| count.load(Ordering::SeqCst) >= 1, 2000));
        thread::sleep(Duration::from_millis(200));
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn current_timer_is_visible_in_callback() {
        let scheduler = TimerScheduler::create("test-current");
        let seen = Arc::new(Mutex::new(None::<i32>));
        let s = seen.clone();
        let timer = Timer::with_scheduler(scheduler.clone(), 10, 0, move |t| {
            *s.lock().unwrap() = Some(t.id());
        });

        assert!(wait_until(|| seen.lock().unwrap().is_some(), 2000));
        // 回调期间可见当前定时器，其 id 必然大于 0（一次性定时器结束后 id 归零）
        assert!(seen.lock().unwrap().unwrap() > 0);
        let _ = timer;

        scheduler.dispose();
    }

    #[test]
    fn scheduler_dispose_is_idempotent() {
        let scheduler = TimerScheduler::create("test-dispose");
        let timer = Timer::with_scheduler(scheduler.clone(), 1000, 1000, |_| {});
        assert_eq!(scheduler.count(), 1);

        scheduler.dispose();
        scheduler.dispose();

        // 销毁后不能继续添加定时器
        let result = catch_unwind(AssertUnwindSafe(|| {
            Timer::with_scheduler(scheduler.clone(), 100, 100, |_| {})
        }));
        assert!(result.is_err());

        timer.cancel();
    }

    #[test]
    fn idle_thread_exits_and_restarts_on_next_add() {
        let scheduler = TimerScheduler::create("test-idle-restart");

        // 加入一个远期定时器再取消：列表清空且等待周期为默认值，工作线程会立即退出
        let long = Timer::with_scheduler(scheduler.clone(), 60_000, 0, |_| {});
        assert!(wait_until(|| scheduler.worker_running(), 1000));
        long.cancel();
        assert!(
            wait_until(|| !scheduler.worker_running(), 2000),
            "空任务时调度线程应退出"
        );

        // 线程退出后再次加入定时器：必须自动重启线程并按时触发
        let count = Arc::new(AtomicI32::new(0));
        let c = count.clone();
        let timer = Timer::with_scheduler(scheduler.clone(), 10, 200, move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        });

        assert!(
            wait_until(|| count.load(Ordering::SeqCst) >= 2, 3000),
            "线程重启后定时器未触发，count={}",
            count.load(Ordering::SeqCst)
        );

        timer.cancel();
        scheduler.dispose();
    }

    #[test]
    fn concurrent_add_cancel_stress() {
        let scheduler = TimerScheduler::create("test-stress");
        let fired = Arc::new(AtomicI32::new(0));

        let mut handles = Vec::new();
        for worker in 0..4i64 {
            let scheduler = scheduler.clone();
            let fired = fired.clone();
            handles.push(thread::spawn(move || {
                for seq in 0..25i64 {
                    let c = fired.clone();
                    let timer = Timer::with_scheduler(
                        scheduler.clone(),
                        (worker + seq) % 20,
                        0,
                        move |_| {
                            c.fetch_add(1, Ordering::SeqCst);
                        },
                    );
                    // 一半立即取消；取消与触发并发发生也不允许挂起或崩溃
                    if seq % 2 == 0 {
                        timer.cancel();
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        // 未取消的一次性定时器共 4×12=48 个，都应在数秒内完成触发（取消的即使多触发一次也无妨）
        assert!(
            wait_until(|| fired.load(Ordering::SeqCst) >= 48, 5000),
            "并发压力下定时器未全部触发，fired={}",
            fired.load(Ordering::SeqCst)
        );

        scheduler.dispose();
    }
}
