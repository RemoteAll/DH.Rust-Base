// RazorInterop：Razor 子集双端互操作工具（C# 侧，免宿主编译）。
// 与 Rust 侧 examples/razor_render.rs 成对使用，由 scripts/razor_interop.ps1 驱动。
//
// 语义基线：ASP.NET Core 原生 Razor（.NET 10）。
//  - 模板经 RazorEngineCore 运行时编译（RazorEngineCore 基类默认不转义，由本工具覆写）；
//  - @ 表达式输出使用 HtmlEncoder.Default（与 ASP.NET Core RazorPageBase 相同）；
//  - @Raw(...) / @Html.Raw(...) 固定模式输出不转义（F003 子集合同）。
//
// 用法：
//   RazorInterop render       <template.cshtml> <data.json> [-o out.html]
//   RazorInterop probe-encode <文本|@文件>                # 输出 HtmlEncoder.Default 编码（诊断用）
//
// 注意：数据 JSON 将转换为 ExpandoObject/List 动态模型，与 Rust 侧 Value 树对应。

using System.Dynamic;
using System.Text;
using System.Text.Encodings.Web;
using System.Text.Json;
using Microsoft.AspNetCore.Razor.Language.Extensions;
using RazorEngineCore;

var cmd = args.Length > 0 ? args[0] : "help";

switch (cmd)
{
    case "render":
        {
            if (args.Length < 3)
            {
                Console.Error.WriteLine("用法：render <template.cshtml> <data.json> [-o out.html]");
                Environment.Exit(2);
            }
            var templateText = File.ReadAllText(args[1]);
            var model = LoadModel(File.ReadAllText(args[2]));
            var outPath = GetOption(args, "-o");

            var engine = new RazorEngine();
            var compiled = CompileWith(engine, templateText);
            var html = compiled.Run(instance => instance.Model = model);

            WriteOutput(html, outPath);
            break;
        }
    case "probe-encode":
        {
            if (args.Length < 2)
            {
                Console.Error.WriteLine("用法：probe-encode <文本|@文件>");
                Environment.Exit(2);
            }
            var text = args[1].StartsWith('@') ? File.ReadAllText(args[1][1..]) : args[1];
            WriteOutput(HtmlEncoder.Default.Encode(text), GetOption(args, "-o"));
            break;
        }
    case "dump-code":
        {
            if (args.Length < 2)
            {
                Console.Error.WriteLine("用法：dump-code <template.cshtml>");
                Environment.Exit(2);
            }
            var templateText = File.ReadAllText(args[1]);
            var engine = new RazorEngine();
            var compiled = CompileWith(engine, templateText, includeDebug: true);
            // Meta 为受保护成员：沿类型层次反射读取（仅诊断用途）
            var meta = FindMember(compiled, "Meta");
            var code = meta is null ? null : FindMember(meta, "GeneratedSourceCode") as string;
            WriteOutput(code ?? "（未取到生成源码）", GetOption(args, "-o"));
            break;
        }
    case "bench":
        {
            // bench <template.cshtml> <data.json> [iterations] [warmup]
            if (args.Length < 3)
            {
                Console.Error.WriteLine("用法：bench <template.cshtml> <data.json> [iterations] [warmup]");
                Environment.Exit(2);
            }
            var templateText = File.ReadAllText(args[1]);
            var model = LoadModel(File.ReadAllText(args[2]));
            var iters = args.Length > 3 ? int.Parse(args[3]) : 20000;
            var warmup = args.Length > 4 ? int.Parse(args[4]) : 1000;

            var engine = new RazorEngine();
            var compiled = CompileWith(engine, templateText);

            ulong checksum = 0;
            for (var i = 0; i < warmup; i++)
            {
                checksum ^= (ulong)compiled.Run(instance => instance.Model = model).Length;
            }

            var samples = new double[iters];
            var watch = System.Diagnostics.Stopwatch.StartNew();
            for (var i = 0; i < iters; i++)
            {
                var t0 = System.Diagnostics.Stopwatch.GetTimestamp();
                var html = compiled.Run(instance => instance.Model = model);
                var dt = System.Diagnostics.Stopwatch.GetTimestamp() - t0;
                samples[i] = dt * 1_000_000_000.0 / System.Diagnostics.Stopwatch.Frequency;
                checksum += (ulong)html.Length;
            }
            watch.Stop();

            Array.Sort(samples);
            double Pct(double p) => samples[(int)((samples.Length - 1) * p)] / 1000.0;
            Console.WriteLine("engine=csharp");
            Console.WriteLine($"iterations={iters}");
            Console.WriteLine($"total_ms={watch.Elapsed.TotalMilliseconds:F1}");
            Console.WriteLine($"ops_per_sec={iters / watch.Elapsed.TotalSeconds:F0}");
            Console.WriteLine($"mean_us={samples.Average() / 1000.0:F2}");
            Console.WriteLine($"p50_us={Pct(0.50):F2}");
            Console.WriteLine($"p90_us={Pct(0.90):F2}");
            Console.WriteLine($"p99_us={Pct(0.99):F2}");
            Console.WriteLine($"checksum={checksum}");
            break;
        }
    case "find-type":
        {
            // 在 Razor 编译器程序集（强制加载）中按子串查找类型（能力探测，诊断用）
            var needle = args.Length > 1 ? args[1] : "Section";
            System.Reflection.Assembly.Load("Microsoft.AspNetCore.Razor.Language");
            foreach (var asm in AppDomain.CurrentDomain.GetAssemblies())
            {
                if (!asm.FullName!.Contains("Razor", StringComparison.OrdinalIgnoreCase))
                {
                    continue;
                }
                System.Type[] types;
                try
                {
                    types = asm.GetTypes();
                }
                catch (System.Reflection.ReflectionTypeLoadException ex)
                {
                    types = ex.Types.Where(x => x != null).ToArray()!;
                }
                foreach (var t in types)
                {
                    if (t.FullName != null && t.FullName.Contains(needle, StringComparison.OrdinalIgnoreCase))
                    {
                        Console.WriteLine($"{t.FullName}  @ {asm.GetName().Name}");
                    }
                }
            }
            break;
        }
    case "render-page":
        {
            // render-page <caseDir> <data.json> [-o out.html]
            // 约定：caseDir/template.cshtml 为页面入口；同目录 *.cshtml 可按名称引用（布局/Partial）
            if (args.Length < 3)
            {
                Console.Error.WriteLine("用法：render-page <caseDir> <data.json> [-o out.html]");
                Environment.Exit(2);
            }
            var dir = args[1];
            var pageModel = LoadModel(File.ReadAllText(args[2]));
            var pageOut = GetOption(args, "-o");
            try
            {
                var host = new PageHost(dir);
                var html = host.RenderPage("template", pageModel);
                WriteOutput(html, pageOut);
            }
            catch (Exception ex)
            {
                Console.Error.WriteLine("渲染失败：" + ex.GetBaseException().Message);
                Environment.Exit(1);
            }
            break;
        }
    case "members":
        {
            // 反射导出 Razor 相关类型成员（能力探测，诊断用）：
            //   members [类型名]（默认 RazorEngineTemplateBase；跨 Razor 程序集查找）
            var typeName = args.Length > 1 ? args[1] : "RazorEngineTemplateBase";
            System.Reflection.Assembly.Load("Microsoft.AspNetCore.Razor.Language");
            System.Type? t = null;
            foreach (var asm in AppDomain.CurrentDomain.GetAssemblies())
            {
                if (!asm.FullName!.Contains("Razor", StringComparison.OrdinalIgnoreCase))
                {
                    continue;
                }
                System.Type[] types;
                try
                {
                    types = asm.GetTypes();
                }
                catch (System.Reflection.ReflectionTypeLoadException ex)
                {
                    types = ex.Types.Where(x => x != null).ToArray()!;
                }
                t = types.FirstOrDefault(x => x.Name == typeName || x.FullName == typeName);
                if (t != null)
                {
                    break;
                }
            }
            if (t == null)
            {
                Console.Error.WriteLine($"未找到类型：{typeName}");
                Environment.Exit(2);
            }
            var sb = new StringBuilder();
            sb.AppendLine($"type={t.FullName}");
            for (var cur = t; cur != null; cur = cur.BaseType)
            {
                sb.AppendLine($"-- {cur.Name}");
                foreach (var m in cur.GetMembers(System.Reflection.BindingFlags.Public
                    | System.Reflection.BindingFlags.NonPublic | System.Reflection.BindingFlags.Instance
                    | System.Reflection.BindingFlags.Static | System.Reflection.BindingFlags.DeclaredOnly))
                {
                    var vis = m switch
                    {
                        System.Reflection.MethodBase mb => mb.IsPublic ? "public"
                            : mb.IsFamily ? "protected" : mb.IsPrivate ? "private" : "internal",
                        System.Reflection.PropertyInfo p => (p.GetMethod?.IsPublic ?? p.SetMethod?.IsPublic ?? false)
                            ? "public" : "protected",
                        System.Reflection.FieldInfo f => f.IsPublic ? "public" : "protected",
                        _ => "?",
                    };
                    sb.AppendLine($"  [{vis}] {m.MemberType} {m}");
                }
            }
            WriteOutput(sb.ToString(), GetOption(args, "-o"));
            break;
        }
    case "bench-page":
        {
            // bench-page <caseDir> <data.json> [iterations] [warmup]（F008/F009 页面模式）
            if (args.Length < 3)
            {
                Console.Error.WriteLine("用法：bench-page <caseDir> <data.json> [iterations] [warmup]");
                Environment.Exit(2);
            }
            var pageDir = args[1];
            var pageBenchModel = LoadModel(File.ReadAllText(args[2]));
            var pageIters = args.Length > 3 ? int.Parse(args[3]) : 20000;
            var pageWarmup = args.Length > 4 ? int.Parse(args[4]) : 1000;
            var pageHost = new PageHost(pageDir);

            ulong pageChecksum = 0;
            for (var i = 0; i < pageWarmup; i++)
            {
                pageChecksum ^= (ulong)pageHost.RenderPage("template", pageBenchModel).Length;
            }
            var pageSamples = new double[pageIters];
            var pageWatch = System.Diagnostics.Stopwatch.StartNew();
            for (var i = 0; i < pageIters; i++)
            {
                var t0 = System.Diagnostics.Stopwatch.GetTimestamp();
                var html = pageHost.RenderPage("template", pageBenchModel);
                var dt = System.Diagnostics.Stopwatch.GetTimestamp() - t0;
                pageSamples[i] = dt * 1_000_000_000.0 / System.Diagnostics.Stopwatch.Frequency;
                pageChecksum += (ulong)html.Length;
            }
            pageWatch.Stop();
            Array.Sort(pageSamples);
            double PctPage(double p) => pageSamples[(int)((pageSamples.Length - 1) * p)] / 1000.0;
            Console.WriteLine("engine=csharp-page");
            Console.WriteLine($"iterations={pageIters}");
            Console.WriteLine($"total_ms={pageWatch.Elapsed.TotalMilliseconds:F1}");
            Console.WriteLine($"ops_per_sec={pageIters / pageWatch.Elapsed.TotalSeconds:F0}");
            Console.WriteLine($"mean_us={pageSamples.Average() / 1000.0:F2}");
            Console.WriteLine($"p50_us={PctPage(0.50):F2}");
            Console.WriteLine($"p90_us={PctPage(0.90):F2}");
            Console.WriteLine($"p99_us={PctPage(0.99):F2}");
            Console.WriteLine($"checksum={pageChecksum}");
            break;
        }
    default:
        Console.Error.WriteLine("用法：render <template.cshtml> <data.json> [-o out.html] | probe-encode <文本|@文件>");
        Environment.Exit(2);
        break;
}

static object? LoadModel(string json)
{
    using var doc = JsonDocument.Parse(json);
    return ConvertElement(doc.RootElement);
}

/// <summary>统一编译入口：注册 SectionDirective（F008）等固定子集所需的 Razor 扩展。</summary>
static IRazorEngineCompiledTemplate<InteropTemplate> CompileWith(
    RazorEngine engine, string templateText, bool includeDebug = false)
{
    return engine.Compile<InteropTemplate>(templateText, builder =>
    {
        if (includeDebug)
        {
            builder.IncludeDebuggingInfo();
        }
        if (builder is RazorEngineCompilationOptionsBuilder concrete)
        {
            concrete.ConfigureRazorEngineProject(SectionDirective.Register);
        }
    });
}

static object? ConvertElement(JsonElement e) => e.ValueKind switch
{
    JsonValueKind.Object => ConvertObject(e),
    JsonValueKind.Array => ConvertArray(e),
    JsonValueKind.String => e.GetString(),
    JsonValueKind.Number => ConvertNumber(e),
    JsonValueKind.True => true,
    JsonValueKind.False => false,
    _ => null,
};

// 注意：不能用三元表达式返回 long/double（会因公共类型推断被提升为 double，
// 导致整数运算变成浮点运算，与 Rust 侧整数语义不一致）
static object ConvertNumber(JsonElement e)
{
    if (e.TryGetInt64(out var l))
    {
        return l;
    }
    return e.GetDouble();
}

static ExpandoObject ConvertObject(JsonElement e)
{
    var expando = new ExpandoObject();
    var dict = (IDictionary<string, object?>)expando;
    foreach (var property in e.EnumerateObject())
    {
        dict[property.Name] = ConvertElement(property.Value);
    }
    return expando;
}

static List<object?> ConvertArray(JsonElement e)
{
    var list = new List<object?>();
    foreach (var item in e.EnumerateArray())
    {
        list.Add(ConvertElement(item));
    }
    return list;
}

static string? GetOption(string[] args, string name)
{
    for (var i = 0; i < args.Length - 1; i++)
    {
        if (args[i] == name)
        {
            return args[i + 1];
        }
    }
    return null;
}

// 沿类型层次反射查找字段/属性（用于读取第三方库的受保护成员，仅诊断用途）
static object? FindMember(object obj, string name)
{
    const System.Reflection.BindingFlags flags = System.Reflection.BindingFlags.NonPublic
        | System.Reflection.BindingFlags.Public | System.Reflection.BindingFlags.Instance
        | System.Reflection.BindingFlags.DeclaredOnly;
    for (var t = obj.GetType(); t != null; t = t.BaseType)
    {
        var field = t.GetField(name, flags);
        if (field != null)
        {
            return field.GetValue(obj);
        }
        var property = t.GetProperty(name, flags);
        if (property != null)
        {
            return property.GetValue(obj);
        }
    }
    return null;
}

static void WriteOutput(string text, string? outPath)
{
    if (outPath != null)
    {
        // UTF-8 无 BOM，保证与 Rust 侧写出的字节流可比
        File.WriteAllText(outPath, text, new UTF8Encoding(false));
        return;
    }
    using var stdout = Console.OpenStandardOutput();
    var bytes = Encoding.UTF8.GetBytes(text);
    stdout.Write(bytes);
}

/// <summary>互操作模板基类：按 ASP.NET Core 原生语义转义输出，并提供 Raw / 布局 / 分区 / Partial 固定模式。</summary>
public class InteropTemplate : RazorEngineTemplateBase
{
    private static readonly HtmlEncoder Encoder = HtmlEncoder.Default;

    /// <summary>@Html.Raw(...) / @await Html.PartialAsync(...) 固定模式的挂载点。</summary>
    public HtmlRawHelper Html { get; } = new();

    /// <summary>布局名（页面/布局模板通过 @{ Layout = "..."; } 设置；F008）。</summary>
    public string? Layout { get; set; }

    /// <summary>当前渲染上下文（由宿主在 Run 前设置）。</summary>
    public RenderPageContext? PageContext { get; set; }

    /// <summary>页面渲染期收集的分区（F008）。</summary>
    public Dictionary<string, string> Sections { get; } = new(StringComparer.Ordinal);

    /// <summary>分区体捕获缓冲（非空时 WriteLiteral 重定向到此处）。</summary>
    private StringBuilder? _sectionCapture;

    /// <summary>显式不转义（@Raw(...) 固定模式，F003）。</summary>
    public object Raw(object? value) => new RawContent(value);

    /// <summary>@section X { } 的展开目标（F008）：立即渲染分区体并收集（隔离输出）。</summary>
    public void DefineSection(string name, Func<Task> section)
    {
        if (PageContext is null || PageContext.Mode != RenderMode.Page)
        {
            throw new InvalidOperationException("分区只能在页面模板中定义");
        }
        if (_sectionCapture != null)
        {
            throw new InvalidOperationException("分区不能嵌套定义");
        }
        var capture = new StringBuilder();
        _sectionCapture = capture;
        try
        {
            section().GetAwaiter().GetResult();
        }
        finally
        {
            _sectionCapture = null;
        }
        if (Sections.ContainsKey(name))
        {
            throw new InvalidOperationException($"section 重复定义：{name}");
        }
        Sections[name] = capture.ToString();
    }

    /// <summary>@RenderBody()（F008）：布局中输出页面体。</summary>
    public object RenderBody()
    {
        if (PageContext is null || PageContext.Mode != RenderMode.Layout)
        {
            throw new InvalidOperationException("RenderBody 仅在布局模板中可用");
        }
        return new RawContent(PageContext.Body);
    }

    /// <summary>@await RenderSectionAsync("X", false) 的展开目标（F008）。</summary>
    public Task<object> RenderSectionAsync(string name, bool required = false)
    {
        if (PageContext is null || PageContext.Mode != RenderMode.Layout)
        {
            throw new InvalidOperationException("RenderSection 仅在布局模板中可用");
        }
        if (PageContext.Sections != null && PageContext.Sections.TryGetValue(name, out var html))
        {
            return Task.FromResult<object>(new RawContent(html));
        }
        if (required)
        {
            throw new InvalidOperationException($"缺少必需的分区：{name}");
        }
        return Task.FromResult<object>(new RawContent(""));
    }

    /// <summary>@ 表达式输出：与 ASP.NET Core RazorPageBase 一致使用 HtmlEncoder.Default。</summary>
    public override void Write(object? obj = null)
    {
        if (obj == null)
        {
            return;
        }
        if (obj is RawContent raw)
        {
            WriteLiteral(raw.Text);
            return;
        }
        WriteLiteral(Encoder.Encode(obj.ToString() ?? string.Empty));
    }

    /// <summary>字面量输出：分区捕获期间重定向到隔离缓冲。</summary>
    public override void WriteLiteral(string literal)
    {
        if (_sectionCapture != null)
        {
            _sectionCapture.Append(literal);
            return;
        }
        base.WriteLiteral(literal);
    }

    /// <summary>属性值输出：字面量片段原样、动态值按表达式语义转义。</summary>
    public override void WriteAttributeValue(string prefix, int prefixOffset, object? value,
        int valueOffset, int valueLength, bool isLiteral)
    {
        WriteLiteral(prefix);
        if (value == null)
        {
            return;
        }
        if (isLiteral)
        {
            WriteLiteral(value.ToString());
        }
        else
        {
            Write(value);
        }
    }
}

/// <summary>渲染模式（与 Rust 侧 rt::RenderMode 一致）。</summary>
public enum RenderMode
{
    /// <summary>独立渲染（无页面上下文：分区/布局/Partial 均不可用）。</summary>
    Standalone = 0,

    /// <summary>页面渲染（收集分区与 Layout）。</summary>
    Page = 1,

    /// <summary>布局渲染（提供 Body 与分区）。</summary>
    Layout = 2,

    /// <summary>Partial 渲染（无权定义分区/设置 Layout）。</summary>
    Partial = 3,
}

/// <summary>渲染上下文（由宿主提供；页面/布局/Partial 共享同一协议）。</summary>
public sealed class RenderPageContext
{
    /// <summary>当前渲染模式。</summary>
    public RenderMode Mode { get; init; } = RenderMode.Standalone;

    /// <summary>页面体（仅布局模式）。</summary>
    public string? Body { get; init; }

    /// <summary>页面收集的分区（仅布局模式）。</summary>
    public IReadOnlyDictionary<string, string>? Sections { get; init; }

    /// <summary>Partial 渲染回调：名称 + 子模型 → HTML（由宿主实现）。</summary>
    public Func<string, object?, string>? PartialRenderer { get; init; }
}

/// <summary>@Html.Raw(...) / @await Html.PartialAsync(...) 固定模式入口。</summary>
public sealed class HtmlRawHelper
{
    /// <summary>Partial 渲染回调（由宿主在每个实例 Run 前设置）。</summary>
    public Func<string, object?, string>? PartialRenderer { get; set; }

    /// <summary>返回不转义内容。</summary>
    public object Raw(object? value) => new RawContent(value);

    /// <summary>@await Html.PartialAsync("Name", model) 的展开目标（F009）。</summary>
    public Task<object> PartialAsync(string name, object? model = null)
    {
        if (PartialRenderer is null)
        {
            throw new InvalidOperationException("Partial 需要页面渲染上下文（用 render-page）");
        }
        return Task.FromResult<object>(new RawContent(PartialRenderer(name, model)));
    }
}

/// <summary>不转义内容包装（由 Write 覆写识别并原样输出）。</summary>
public sealed class RawContent
{
    /// <summary>按字符串语义取文本（null 为空串，对齐 C# 输出）。</summary>
    public RawContent(object? value)
    {
        Text = value?.ToString() ?? string.Empty;
    }

    /// <summary>原始文本。</summary>
    public string Text { get; }
}

/// <summary>页面宿主：解析布局链、收集分区、按名称加载 Partial（与 Rust 侧 ViewEngine 同协议）。</summary>
public sealed class PageHost
{
    private const int MaxLayoutDepth = 8;
    private const int MaxPartialDepth = 8;

    private readonly string _root;
    private readonly RazorEngine _engine = new();
    private readonly Dictionary<string, IRazorEngineCompiledTemplate<InteropTemplate>> _cache
        = new(StringComparer.Ordinal);

    /// <summary>以目录为模板根创建宿主。</summary>
    public PageHost(string root)
    {
        _root = Path.GetFullPath(root);
    }

    /// <summary>渲染页面：页面 → 布局链（逐级包裹）→ 最终 HTML。</summary>
    public string RenderPage(string pageName, object? model)
    {
        InteropTemplate? pageInstance = null;
        var body = GetCompiled(pageName).Run(instance =>
        {
            instance.Model = model;
            instance.PageContext = new RenderPageContext { Mode = RenderMode.Page };
            instance.Html.PartialRenderer = (n, m) => RenderPartial(n, m, 1);
            pageInstance = instance;
        });
        var layoutName = pageInstance!.Layout;
        var sections = pageInstance.Sections;
        var depth = 0;
        while (layoutName != null)
        {
            if (++depth > MaxLayoutDepth)
            {
                throw new InvalidOperationException($"布局嵌套过深（上限 {MaxLayoutDepth}）");
            }
            InteropTemplate? layoutInstance = null;
            var context = new RenderPageContext
            {
                Mode = RenderMode.Layout,
                Body = body,
                Sections = sections,
            };
            body = GetCompiled(layoutName).Run(instance =>
            {
                instance.Model = model;
                instance.PageContext = context;
                instance.Html.PartialRenderer = (n, m) => RenderPartial(n, m, 1);
                layoutInstance = instance;
            });
            layoutName = layoutInstance!.Layout;
        }
        return body;
    }

    /// <summary>渲染 Partial（递归；深度守卫；禁止 Layout/分区）。</summary>
    public string RenderPartial(string name, object? model, int depth)
    {
        if (depth > MaxPartialDepth)
        {
            throw new InvalidOperationException($"Partial 嵌套过深（上限 {MaxPartialDepth}）");
        }
        InteropTemplate? instance = null;
        var html = GetCompiled(name).Run(instance2 =>
        {
            instance2.Model = model;
            instance2.PageContext = new RenderPageContext { Mode = RenderMode.Partial };
            instance2.Html.PartialRenderer = (n, m) => RenderPartial(n, m, depth + 1);
            instance = instance2;
        });
        if (instance!.Layout != null)
        {
            throw new InvalidOperationException("Partial 不支持 Layout");
        }
        return html;
    }

    private IRazorEngineCompiledTemplate<InteropTemplate> GetCompiled(string name)
    {
        if (_cache.TryGetValue(name, out var cached))
        {
            return cached;
        }
        var text = File.ReadAllText(ResolvePath(name));
        var compiled = CompileView(text);
        _cache[name] = compiled;
        return compiled;
    }

    /// <summary>编译视图（注册 SectionDirective，与顶层 CompileWith 同口径）。</summary>
    private IRazorEngineCompiledTemplate<InteropTemplate> CompileView(string templateText)
    {
        return _engine.Compile<InteropTemplate>(templateText, builder =>
        {
            if (builder is RazorEngineCompilationOptionsBuilder concrete)
            {
                concrete.ConfigureRazorEngineProject(SectionDirective.Register);
            }
        });
    }

    private string ResolvePath(string name)
    {
        if (string.IsNullOrEmpty(name) || name.Contains("..") || Path.IsPathRooted(name))
        {
            throw new InvalidOperationException($"非法的模板名：{name}");
        }
        var full = Path.GetFullPath(Path.Combine(_root, name + ".cshtml"));
        if (!full.StartsWith(_root, StringComparison.OrdinalIgnoreCase))
        {
            throw new InvalidOperationException($"模板越出根目录：{name}");
        }
        if (!File.Exists(full))
        {
            throw new FileNotFoundException($"模板不存在：{name}");
        }
        return full;
    }
}
