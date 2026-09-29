#!/usr/bin/env node
/**
 * STUN 服务连通性自查工具（RFC 5389 Binding 请求）
 *
 * 来源：PekSendToMo（随 STUN 收编进入 dhrust，2026-09-29）；
 * 适用于任何部署了 dhrust::net::stun（或其他标准 STUN 服务）的服务器。
 *
 * 用法:
 *   node stun-check.js <主机> [端口] [超时毫秒]
 * 示例:
 *   node stun-check.js example.com 3478
 *
 * 说明：
 * - 收到响应且解析出「映射地址」=> 服务器的 UDP STUN 端口已开放且服务正常；
 * - 超时无响应 => 端口未放行 / 服务未运行 / 被防火墙拦截（三者需逐项排查）；
 * - 注意：STUN 是 UDP 协议，浏览器/curl 无法直接“访问”该端口做测试。
 */
const dgram = require('dgram');

const host = process.argv[2] || '127.0.0.1';
const port = Number(process.argv[3] || 3478);
const timeoutMs = Number(process.argv[4] || 3000);

const socket = dgram.createSocket('udp4');

// 构造 Binding Request：类型 0x0001 + 长度 0 + magic cookie + 随机 transaction id
const transactionId = Buffer.alloc(12);
for (let i = 0; i < 12; i++) {
  transactionId[i] = Math.floor(Math.random() * 256);
}
const request = Buffer.concat([
  Buffer.from([0x00, 0x01, 0x00, 0x00]),
  Buffer.from([0x21, 0x12, 0xa4, 0x42]),
  transactionId,
]);

let done = false;

socket.on('message', (message) => {
  done = true;
  const type = message.readUInt16BE(0).toString(16).padStart(4, '0');
  console.log(`[响应] ${message.length} 字节, 消息类型=0x${type}（0x0101 为 Binding Success）`);

  // 解析属性：查找 XOR-MAPPED-ADDRESS (0x0020)
  let offset = 20;
  while (offset + 4 <= message.length) {
    const attrType = message.readUInt16BE(offset);
    const attrLen = message.readUInt16BE(offset + 2);
    if (attrType === 0x0020 && attrLen >= 8 && offset + 12 <= message.length) {
      const family = message[offset + 5];
      if (family === 1) {
        const xorPort = message.readUInt16BE(offset + 6) ^ 0x2112;
        const xorIp = message.readUInt32BE(offset + 8) ^ 0x2112a442;
        const ip = [xorIp >>> 24, (xorIp >>> 16) & 255, (xorIp >>> 8) & 255, xorIp & 255].join('.');
        console.log(`[映射地址] ${ip}:${xorPort}  <- 服务端视角下你的公网地址`);
        console.log('[结论] STUN 服务正常，此端口可用于 WebRTC 公网地址发现');
      }
    }
    offset += 4 + attrLen + ((4 - (attrLen % 4)) % 4); // 属性按 4 字节对齐
  }
  socket.close();
});

socket.on('error', (err) => {
  console.log(`[错误] ${err.message}`);
  socket.close();
});

socket.send(request, port, host, (err) => {
  if (err) {
    console.log(`[发送失败] ${err.message}`);
    socket.close();
    return;
  }
  console.log(`已向 ${host}:${port} 发送 STUN Binding 请求...`);
});

setTimeout(() => {
  if (!done) {
    console.log(`[超时] ${timeoutMs}ms 内无响应`);
    console.log('[结论] 端口未放行 / STUN 服务未运行 / 被防火墙拦截（逐项排查）');
    socket.close();
  }
}, timeoutMs);
