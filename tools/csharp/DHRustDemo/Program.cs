// DH.RustBase 互操作示例（C# 侧）。
// 与 Rust 侧 examples/config_interop.rs 成对使用，由 scripts/interop.ps1 驱动。
//
// 用法：
//   DHRustDemo read-setting  <文件路径>
//   DHRustDemo write-setting <文件路径>
//   DHRustDemo cron-next     <表达式> <时间>
//   DHRustDemo cron-prev     <表达式> <时间>
//   DHRustDemo timer-demo

using NewLife;
using NewLife.Configuration;
using NewLife.Log;
using NewLife.Threading;

var cmd = args.Length > 0 ? args[0] : "help";

switch (cmd)
{
    case "read-setting":
        {
            var file = args[1];
            UseProvider(file);
            var s = Setting.Current;
            PrintSetting(s, file);
            break;
        }
    case "write-setting":
        {
            var file = args[1];
            UseProvider(file);
            var s = Setting.Current;
            ApplyFixture(s);
            s.Save();
            Console.WriteLine("OK " + file);
            break;
        }
    case "cron-next":
        {
            var cron = new Cron(args[1]);
            var time = DateTime.Parse(args[2]);
            Console.WriteLine(cron.GetNext(time).ToString("yyyy-MM-dd HH:mm:ss"));
            break;
        }
    case "cron-prev":
        {
            var cron = new Cron(args[1]);
            var time = DateTime.Parse(args[2]);
            Console.WriteLine(cron.GetPrevious(time).ToString("yyyy-MM-dd HH:mm:ss"));
            break;
        }
    case "timer-demo":
        {
            var count = 0;
            using var timer = new TimerX(s => Interlocked.Increment(ref count), null, 50, 60, "DHRustDemo");
            Thread.Sleep(350);
            Console.WriteLine(count >= 2 ? $"OK fired={count}" : $"FAIL fired={count}");
            if (count < 2) Environment.Exit(1);
            break;
        }
    default:
        Console.Error.WriteLine("未知命令: " + cmd);
        Environment.Exit(2);
        break;
}

// 按扩展名选择与 Rust 侧一致的配置提供者（XML/JSON）
static void UseProvider(string file)
{
    IConfigProvider prv = file.EndsWith(".json", StringComparison.OrdinalIgnoreCase)
        ? new JsonConfigProvider { FileName = file }
        : new XmlConfigProvider { FileName = file };
    Config<Setting>.Provider = prv;
}

// 固定样例配置，与 Rust 侧 fixture 保持一致
static void ApplyFixture(Setting s)
{
    s.Debug = false;
    s.LogLevel = LogLevel.Warn;
    s.LogPath = "Logs";
    s.LogFileMaxBytes = 20;
    s.LogFileBackups = 5;
    s.NetworkLog = "udp://127.0.0.1:5514";
    s.DataPath = "DataDir";
    s.BackupPath = "BackupDir";
    s.PluginPath = "PluginDir";
    s.PluginServer = "http://plugins.example/";
    s.ServiceAddress = "http://localhost:8080";
}

// 输出与 Rust 侧 print_setting 完全一致的 key=value 行
static void PrintSetting(Setting s, string file)
{
    Console.WriteLine($"# source={file}");
    Console.WriteLine($"Debug={(s.Debug ? "true" : "false")}");
    Console.WriteLine($"LogLevel={s.LogLevel}");
    Console.WriteLine($"LogPath={s.LogPath}");
    Console.WriteLine($"LogFileMaxBytes={s.LogFileMaxBytes}");
    Console.WriteLine($"LogFileBackups={s.LogFileBackups}");
    Console.WriteLine($"LogFileFormat={s.LogFileFormat}");
    Console.WriteLine($"LogLineFormat={s.LogLineFormat}");
    Console.WriteLine($"NetworkLog={s.NetworkLog}");
    Console.WriteLine($"DataPath={s.DataPath}");
    Console.WriteLine($"BackupPath={s.BackupPath}");
    Console.WriteLine($"PluginPath={s.PluginPath}");
    Console.WriteLine($"PluginServer={s.PluginServer}");
    Console.WriteLine($"ServiceAddress={s.ServiceAddress}");
}
