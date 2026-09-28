// C# 对照基准服务端：DH.NCore 自研网络栈（HttpServer + WebSocket 回显）。
// 与 dhrust 完整实现在同一压测 harness（bench-net 的 http_load / ws_load）下同机对比。
// 用法: BenchNetServer [port]（默认 28100）

using System.Net;
using NewLife.Data;
using NewLife.Http;

var port = args.Length > 0 ? Int32.Parse(args[0]) : 28100;

var server = new HttpServer { Port = port };

// GET /ping —— 与 Rust 基准同字节数（16B JSON）
server.MapGet("/ping", (IHttpContext ctx) =>
    ctx.Response.SetResult("{\"code\":0,\"msg\":\"ok\"}", "application/json; charset=utf-8"));

// POST /echo —— 回显请求体
server.MapPost("/echo", (IHttpContext ctx) =>
{
    var body = ctx.Request.Body;
    ctx.Response.ContentType = "application/octet-stream";
    ctx.Response.Body = body == null ? new ArrayPacket(Array.Empty<Byte>()) : (ArrayPacket)body.ToArray();
});

// WebSocket 回显（文本原样回发）。
// 压测客户端（bench-net 的 ws_load）握手路径为 "/"，与 Rust 各基线服务端一致；
// DH.NCore 的 Map 为精确路径注册（Routes[path]），因此 "/" 与 "/ws" 各绑一次
// （此前只注册 "/ws" 导致 "/" 升级成功但没有消息处理器、压测端等不到回显而卡住）。
var echoHandler = new EchoWebSocketHandler();
server.Map("/", echoHandler);
server.Map("/ws", echoHandler);

server.Start();
Console.WriteLine($"bench-net C# server (DH.NCore) listening on 0.0.0.0:{port}");
Console.WriteLine($"  HTTP: GET /ping, POST /echo");
Console.WriteLine($"  WS:   / (echo), /ws (echo)");
Thread.Sleep(Timeout.Infinite);

/// <summary>WebSocket 回显处理器：文本消息原样回发（对齐 Rust 基准语义）。</summary>
internal sealed class EchoWebSocketHandler : WebSocketHandler
{
    public override void ProcessMessage(WebSocket socket, WebSocketMessage message)
    {
        if (message.Type == WebSocketMessageType.Text)
        {
            socket.Send(message.Payload!.ToStr());
        }
    }
}
