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
            var compiled = engine.Compile<InteropTemplate>(templateText);
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
            var compiled = engine.Compile<InteropTemplate>(templateText,
                builder => builder.IncludeDebuggingInfo());
            // Meta 为受保护成员：沿类型层次反射读取（仅诊断用途）
            var meta = FindMember(compiled, "Meta");
            var code = meta is null ? null : FindMember(meta, "GeneratedSourceCode") as string;
            WriteOutput(code ?? "（未取到生成源码）", null);
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

/// <summary>互操作模板基类：按 ASP.NET Core 原生语义转义输出，并提供 Raw 固定模式。</summary>
public class InteropTemplate : RazorEngineTemplateBase
{
    private static readonly HtmlEncoder Encoder = HtmlEncoder.Default;

    /// <summary>@Html.Raw(...) 固定模式的挂载点。</summary>
    public HtmlRawHelper Html { get; } = new();

    /// <summary>显式不转义（@Raw(...) 固定模式，F003）。</summary>
    public object Raw(object? value) => new RawContent(value);

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

/// <summary>@Html.Raw(...) 固定模式入口。</summary>
public sealed class HtmlRawHelper
{
    /// <summary>返回不转义内容。</summary>
    public object Raw(object? value) => new RawContent(value);
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
