//! 配置与 Cron 的互操作演示程序。
//!
//! 由 `scripts/interop.ps1` 与 C# 侧 `tools/csharp/DHRustDemo` 成对调用，
//! 验证双方读写同一份配置文件、Cron 求值结果完全一致。
//!
//! 用法：
//! ```text
//! config_interop read-setting  <文件路径>      # 读取配置并输出 key=value
//! config_interop write-setting <文件路径>      # 写入固定样例配置
//! config_interop cron-next     <表达式> <时间> # 输出下一次执行时间
//! config_interop cron-prev     <表达式> <时间> # 输出前一次执行时间
//! config_interop timer-demo                    # 定时器冒烟（触发数次后退出）
//! ```

use std::process::exit;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use chrono::NaiveDateTime;
use dhrust::config::Setting;
use dhrust::threading::{Cron, Timer, TimerScheduler};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: config_interop <command> [args...]");
        exit(2);
    }

    match args[0].as_str() {
        "read-setting" => {
            let setting = Setting::load_from(&args[1]).unwrap_or_else(|e| {
                eprintln!("读取配置失败: {e}");
                exit(1);
            });
            print_setting(&setting);
        }
        "write-setting" => {
            let setting = fixture();
            setting.save_to(&args[1]).unwrap_or_else(|e| {
                eprintln!("写入配置失败: {e}");
                exit(1);
            });
            println!("OK {}", args[1]);
        }
        "cron-next" => print_cron(&args[1], &args[2], true),
        "cron-prev" => print_cron(&args[1], &args[2], false),
        "timer-demo" => timer_demo(),
        other => {
            eprintln!("未知命令: {other}");
            exit(2);
        }
    }
}

/// 固定样例配置，与 C# `DHRustDemo write-setting` 保持一致。
fn fixture() -> Setting {
    let mut setting = Setting::default();
    setting.debug = false;
    setting.log_level = "Warn".to_string();
    setting.log_path = "Logs".to_string();
    setting.log_file_max_bytes = 20;
    setting.log_file_backups = 5;
    setting.network_log = "udp://127.0.0.1:5514".to_string();
    setting.data_path = "DataDir".to_string();
    setting.backup_path = "BackupDir".to_string();
    setting.plugin_path = "PluginDir".to_string();
    setting.plugin_server = "http://plugins.example/".to_string();
    setting.service_address = "http://localhost:8080".to_string();
    setting
}

/// 输出与 C# `read-setting` 完全一致的 key=value 行。
fn print_setting(s: &Setting) {
    println!("Debug={}", if s.debug { "true" } else { "false" });
    println!("LogLevel={}", s.log_level);
    println!("LogPath={}", s.log_path);
    println!("LogFileMaxBytes={}", s.log_file_max_bytes);
    println!("LogFileBackups={}", s.log_file_backups);
    println!("LogFileFormat={}", s.log_file_format);
    println!("LogLineFormat={}", s.log_line_format);
    println!("NetworkLog={}", s.network_log);
    println!("DataPath={}", s.data_path);
    println!("BackupPath={}", s.backup_path);
    println!("PluginPath={}", s.plugin_path);
    println!("PluginServer={}", s.plugin_server);
    println!("ServiceAddress={}", s.service_address);
}

fn print_cron(expression: &str, time: &str, next: bool) {
    let cron = Cron::parse(expression).unwrap_or_else(|| {
        eprintln!("非法 Cron 表达式: {expression}");
        exit(1);
    });
    let time = NaiveDateTime::parse_from_str(time, "%Y-%m-%d %H:%M:%S").unwrap_or_else(|e| {
        eprintln!("时间格式错误: {e}");
        exit(1);
    });

    let result = if next {
        cron.get_next(time)
    } else {
        cron.get_previous(time)
    };

    match result {
        // 与 C# 的 DateTime.MinValue 输出对齐
        None => println!("0001-01-01 00:00:00"),
        Some(dt) => println!("{}", dt.format("%Y-%m-%d %H:%M:%S")),
    }
}

/// 定时器冒烟：60ms 周期执行约 350ms，应触发至少 2 次。
fn timer_demo() {
    let scheduler = TimerScheduler::create("DHRustDemo");
    let count = Arc::new(AtomicI32::new(0));
    let c = count.clone();
    let timer = Timer::with_scheduler(scheduler.clone(), 50, 60, move |_| {
        c.fetch_add(1, Ordering::SeqCst);
    });

    thread::sleep(Duration::from_millis(350));
    let fired = count.load(Ordering::SeqCst);

    timer.cancel();
    scheduler.dispose();

    if fired >= 2 {
        println!("OK fired={fired}");
    } else {
        println!("FAIL fired={fired}");
        exit(1);
    }
}
