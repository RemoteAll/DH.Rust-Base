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
    case "password-hash":
        {
            // 口令散列（随机盐）：NewLife.Security.SaltPasswordProvider
            var prv = new NewLife.Security.SaltPasswordProvider { Algorithm = args[1] };
            Console.WriteLine(prv.Hash(args[2]));
            break;
        }
    case "password-hash-fixed":
        {
            // 固定盐散列：用 NewLife 扩展方法逐行复刻 SaltPasswordProvider.Hash 的算法
            Console.WriteLine(HashWithSalt(args[1], args[2], args[3]));
            break;
        }
    case "password-verify":
        {
            var prv = new NewLife.Security.SaltPasswordProvider();
            Console.WriteLine(prv.Verify(args[1], args[2]) ? "OK" : "FAIL");
            break;
        }
    case "password-verify-alg":
        {
            var prv = new NewLife.Security.SaltPasswordProvider { Algorithm = args[1] };
            Console.WriteLine(prv.Verify(args[2], args[3]) ? "OK" : "FAIL");
            break;
        }
    case "password-legacy-md5":
        {
            // C# `String.MD5()` 口径（大写十六进制）
            Console.WriteLine(args[1].MD5());
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

// 固定盐口令散列（逐行复刻 SaltPasswordProvider.Hash 的算法，盐由调用方给定以便互通比对）
static string HashWithSalt(string alg, string password, string salt)
{
    var hash = alg switch
    {
        "md5" => (password.MD5() + salt).MD5(),
        "sha1" => password.GetBytes().SHA1(salt.GetBytes()).ToBase64(),
        "sha512" => password.GetBytes().SHA512(salt.GetBytes()).ToBase64(),
        "md5+sha1" => password.GetBytes().MD5().SHA1(salt.GetBytes()).ToBase64(),
        "md5+sha512" => password.GetBytes().MD5().SHA512(salt.GetBytes()).ToBase64(),
        _ => throw new NotSupportedException($"unsupported algorithm [{alg}]"),
    };

    return $"${alg}${salt}${hash}";
}
