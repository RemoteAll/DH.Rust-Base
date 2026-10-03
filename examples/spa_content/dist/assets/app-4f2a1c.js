// SPA 演示脚本（嵌入自 dist/assets/app-4f2a1c.js）
// 1) 显示当前路径，便于观察 history 深链接回退行为；
// 2) 真实调用后端 JSON API（/api/time）——演示“前后端分离”形态下的接口联动。
document.getElementById("path").textContent = location.pathname;

(async function () {
  var el = document.getElementById("api-time");
  try {
    var resp = await fetch("/api/time");
    var data = await resp.json();
    el.textContent = "GET /api/time → " + JSON.stringify(data);
  } catch (e) {
    el.textContent = "调用 /api/time 失败：" + e;
  }
})();

console.log("dhrust spa_server：SPA 资源已加载");
