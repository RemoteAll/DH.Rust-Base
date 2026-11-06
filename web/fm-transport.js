/* 文件管理：传输内核（分片断点上传 / Range 断点下载 / 传输任务面板 / 批量分批提交 / 剪贴板模型）。
 *
 * Pek.RAgent（web/app.js）与 Pek.RPanlServer（web/index.html）共用这一份实现——
 * 服务端两边都走 `dhrust::fileman`，协议必须一致，逻辑就别抄两遍。
 *
 * 设计约束：**不假设宿主是某个特定面板**，第三方面板（多平台/多项目）也能直接接入：
 *   - 只依赖 `globalThis` / `document` / `XMLHttpRequest` / `fetch` / `Blob` / `URL`，无框架、无构建；
 *   - 所有路径、文案、鉴权、存储都通过 `configure()` 注入，源码里不出现任何面板专有的 id/函数名；
 *   - 注入项缺省时都有安全回退（内存存储、简易提示、自动判断路径分隔符）；
 *   - 同一页面重复加载本文件不会报错（幂等定义 `globalThis.FmClient`）；
 *   - 打包器（Vite/webpack/ESBuild）场景：`import './fm-transport.js'` 只为触发挂载，
 *     之后用 `globalThis.FmClient`；CommonJS 下 `require('./fm-transport.js')` 直接返回实例；
 *   - 桌面端（Tauri/Electron）可把 `filePicker` 设为 false，走内存分片下载。
 *
 * configure 可注入项（全部可选）：
 *   endpoints     接口路径；键：mkdirs/upload/uploadChunk/uploadStatus/uploadAbort/write/download/copy/move/remove
 *   api           JSON 请求函数 `(path, init) => Promise<{code,message,data}>`（沿用宿主封装，负责 401/428 语义）
 *   token         会话令牌：字符串或取值函数（便于登录后刷新）
 *   onAuthFail    登录失效回调
 *   join/joinRel  路径拼接（缺省按 `dir` 里是否出现反斜杠判断 Windows/Unix）
 *   bytes/toast/esc 展示层小工具
 *   onUploadDone  上传结束后的刷新回调
 *   storage       `{get,set,remove}` 三件套（缺省 localStorage，不可用时自动降级为内存）
 *   storagePrefix localStorage 键前缀（剪贴板 / 下载 ETag 记忆互不干扰）
 *   jobsContainer 传输任务面板容器 id（缺省 fm-jobs）
 *   filePicker    false = 不调用 showSaveFilePicker（内嵌 WebView / 非 Chrome 环境）
 *   clipText      `{copy,cut,count,tip,clear,saved}` 文案覆盖（多语言/多面板语气）
 */
(function (root) {
  'use strict';

  if (root.FmClient && root.FmClient.VERSION) return; // 幂等：重复 <script> 不重复定义

  var VERSION = '1.1.0';

  var DEFAULTS = {
    endpoints: {
      mkdirs: '/star/fileMkdirs',
      upload: '/star/fileUpload',
      uploadChunk: '/star/fileUploadChunk',
      uploadStatus: '/star/fileUploadStatus',
      uploadAbort: '/star/fileUploadAbort',
      write: '/star/fileWrite',
      download: '/star/fileDownload',
      copy: '/star/fileCopy',
      move: '/star/fileMove',
      remove: '/star/fileDelete'
    },
    clipText: {
      copy: '📋 复制',
      cut: '✂ 剪切',
      count: '项',
      tip: '→ 进入目标目录后点「📥 粘贴」',
      clear: '清空',
      saved: '已保存：'
    }
  };

  var CHUNK = 8 * 1024 * 1024;          // 分片上传片大小
  var DL_CHUNK = 8 * 1024 * 1024;       // 分片下载片大小
  var DL_RESUME_MIN = 32 * 1024 * 1024; // ≥32MB 才走 Range 断点续传

  var cfg = null;
  var dlParts = new Map();  // 无「另存为」能力时的内存兜底：路径 → Blob[]
  var dlMeta = new Map();   // 下载断点元信息：路径 → {size}
  var jobs = [];
  var memStore = {};        // localStorage 不可用时的降级存储

  function ep(key) { return ((cfg && cfg.endpoints) || DEFAULTS.endpoints)[key]; }
  function text(key) { return ((cfg && cfg.clipText) || DEFAULTS.clipText)[key]; }
  function bytes(n) { return cfg.bytes ? cfg.bytes(n) : (n || 0) + ' B'; }
  function toast(msg, err) { if (cfg.toast) cfg.toast(msg, err); else console.log('[fm] ' + msg); }
  function esc(s) {
    if (cfg.esc) return cfg.esc(s);
    return String(s == null ? '' : s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
  }
  function sleep(ms) { return new Promise(function (r) { setTimeout(r, ms); }); }
  function rate(n, ms) { return (!ms || ms < 300) ? '…' : bytes(n * 1000 / ms) + '/s'; }
  // token 既可直接给字符串，也可给取值函数（面板的令牌会在登录/刷新后变化）
  function tokenOf() { return typeof cfg.token === 'function' ? cfg.token() : cfg.token; }
  function authHeaders(extra) {
    var h = extra || {};
    var tk = tokenOf();
    if (tk) h['Authorization'] = 'Bearer ' + tk;
    return h;
  }
  function onAuthFail() { if (cfg.onAuthFail) cfg.onAuthFail(); }
  function sepOf(dir) {
    var s = String(dir || '');
    return (s.indexOf('\\') >= 0 && s.indexOf('/') < 0) ? '\\' : '/';
  }
  function join(dir, name) {
    if (cfg.join) return cfg.join(dir, name);
    var d = String(dir || '').replace(/[\\/]+$/, '');
    return d ? d + sepOf(d) + name : String(name || '');
  }
  function joinRel(dir, rel) {
    if (cfg.joinRel) return cfg.joinRel(dir, rel);
    var sep = sepOf(dir);
    var d = String(dir || '').replace(/[\\/]+$/, '');
    if (!rel) return d;
    return (d ? d + sep : '') + String(rel).split('/').join(sep);
  }
  function store() {
    if (cfg.storage && typeof cfg.storage.get === 'function') return cfg.storage;
    return {
      get: function (k) {
        try { return root.localStorage ? root.localStorage.getItem(k) : (memStore[k] || null); }
        catch (e) { return memStore[k] || null; }
      },
      set: function (k, v) {
        try { if (root.localStorage) { root.localStorage.setItem(k, v); return; } } catch (e) { }
        memStore[k] = v;
      },
      remove: function (k) {
        try { if (root.localStorage) { root.localStorage.removeItem(k); return; } } catch (e) { }
        delete memStore[k];
      }
    };
  }
  function fetchOrFail(url, init) {
    if (typeof root.fetch !== 'function') throw new Error('当前环境不支持 fetch（请升级浏览器或改用桌面客户端）');
    return root.fetch(url, init);
  }

  /* ── 传输任务面板（上传/下载进度、取消、续传） ── */
  function jobRender() {
    if (typeof document === 'undefined') return; // 无 DOM 环境（脚本/自动化）只保留任务状态
    var id = cfg.jobsContainer || 'fm-jobs';
    var box = document.getElementById(id);
    if (!jobs.length) { if (box) box.remove(); return; }
    if (!box) {
      box = document.createElement('div');
      box.id = id;
      box.className = 'fm-jobs';
      document.body.appendChild(box);
    }
    var running = jobs.filter(function (j) { return !(j.pct >= 100); }).length;
    box.innerHTML = '<div class="fm-jobs-head"><span>传输任务' + (running ? '（' + running + ' 进行中）' : '') +
      '</span><button title="清空列表" onclick="FmClient.jobs.clear()">✕</button></div>' +
      jobs.map(function (j, i) {
        return '<div class="fm-job">' +
          '<div class="fm-job-title" title="' + esc(j.title) + '">' + esc(j.title) + '</div>' +
          '<div class="fm-job-bar"><i style="width:' + Math.max(0, Math.min(100, j.pct || 0)).toFixed(1) + '%"></i></div>' +
          '<div class="fm-job-meta"><span title="' + esc(j.status || '') + '">' + esc(j.status || '') + '</span>' +
          '<span style="white-space:nowrap">' +
          (j.onAction ? '<button onclick="FmClient.jobs.act(' + i + ')">续传</button>' : '') +
          (j.onCancel ? '<button onclick="FmClient.jobs.cancel(' + i + ')">取消</button>' : '') +
          '</span></div></div>';
      }).join('');
  }
  function jobAdd(title) {
    var job = { title: title, pct: 0, status: '准备中', onAction: null, onCancel: null };
    jobs.push(job);
    if (jobs.length > 40) jobs = jobs.slice(-40);
    jobRender();
    return job;
  }
  function jobUpdate(job, patch) { if (!job) return; Object.assign(job, patch || {}); jobRender(); }

  /* ── 上传 ── */
  /** 上传会话 ID：由「目录+文件名+大小+修改时间」定死 → 刷新/重试/重开页面都能接着传 */
  function uploadId(dir, name, size, mtime) {
    var s = dir + '|' + name + '|' + size + '|' + (mtime || 0);
    var h = 2166136261 >>> 0, h2 = 5381;
    for (var i = 0; i < s.length; i++) {
      var ch = s.charCodeAt(i);
      h ^= ch; h = Math.imul(h, 16777619) >>> 0;
      h2 = ((h2 * 33) ^ ch) >>> 0;
    }
    var id = 'u' + h.toString(36) + h2.toString(36);
    if (id.length < 8) id = (id + '00000000').slice(0, 8);
    return id.slice(0, 64);
  }
  /** 二进制 POST（不能走 JSON 封装的 api()：它会强制 Content-Type: application/json） */
  function postBinary(url, body, onProgress) {
    return new Promise(function (resolve) {
      var xhr = new XMLHttpRequest();
      xhr.open('POST', url);
      xhr.setRequestHeader('Content-Type', 'application/octet-stream');
      var tk = tokenOf();
      if (tk) xhr.setRequestHeader('Authorization', 'Bearer ' + tk);
      if (onProgress && xhr.upload) xhr.upload.onprogress = function (e) { if (e.lengthComputable) onProgress(e.loaded); };
      xhr.onload = function () { var j = null; try { j = JSON.parse(xhr.responseText); } catch (e) { } resolve(j); };
      xhr.onerror = function () { resolve(null); };
      xhr.onabort = function () { resolve(null); };
      xhr.send(body);
    });
  }
  /** 单文件上传（0 字节 → 在线写空内容；≤8MB 整块；否则分片断点续传） */
  function uploadFile(t, overwrite, job) {
    var f = t.file;
    if (!f.size) return uploadEmpty(t);
    if (f.size <= CHUNK) return uploadWhole(t, overwrite, job);
    return uploadChunked(t, overwrite, job);
  }
  // 0 字节文件：服务端拒收空请求体/空分片，改用在线写空内容
  function uploadEmpty(t) {
    return cfg.api(ep('write'), { method: 'POST', body: JSON.stringify({ path: t.path, content: '' }) })
      .then(function (r) { return r && r.code === 0 ? { ok: true } : { ok: false, message: (r && r.message) || '写入失败' }; },
        function () { return { ok: false, message: '网络错误' }; });
  }
  function uploadWhole(t, overwrite, job) {
    return new Promise(function (resolve) {
      var xhr = new XMLHttpRequest();
      xhr.open('POST', ep('upload') + '?path=' + encodeURIComponent(t.dir) + '&name=' + encodeURIComponent(t.name) + '&overwrite=' + (overwrite ? 1 : 0));
      var tk = tokenOf();
      if (tk) xhr.setRequestHeader('Authorization', 'Bearer ' + tk);
      var started = Date.now();
      xhr.upload.onprogress = function (e) {
        if (!e.lengthComputable) return;
        var pct = e.loaded / e.total * 100;
        jobUpdate(job, { pct: pct, status: pct.toFixed(0) + '% · ' + bytes(e.loaded) + ' / ' + bytes(e.total) + ' · ' + rate(e.loaded, Date.now() - started) });
      };
      xhr.onload = function () {
        var j = null; try { j = JSON.parse(xhr.responseText); } catch (e) { }
        if (xhr.status === 401 || (j && j.code === 401)) { onAuthFail(); return resolve({ ok: false, message: '登录已过期' }); }
        if (j && j.code === 0) return resolve({ ok: true });
        resolve({ ok: false, message: (j && j.message) || ('HTTP ' + xhr.status) });
      };
      xhr.onerror = function () { resolve({ ok: false, message: '网络错误' }); };
      xhr.onabort = function () { resolve({ ok: false, message: '已取消' }); };
      xhr.send(t.file);
    });
  }
  function uploadChunked(t, overwrite, job) {
    var f = t.file;
    var id = uploadId(t.dir, t.name, f.size, f.lastModified);
    var q = 'path=' + encodeURIComponent(t.dir) + '&name=' + encodeURIComponent(t.name) + '&id=' + encodeURIComponent(id);
    var chunks = Math.max(1, Math.ceil(f.size / CHUNK));
    var offset = 0;
    return cfg.api(ep('uploadStatus') + '?' + q, {}).then(function (st) {
      if (st && st.code === 0) offset = st.data.received || 0;
    }, function () { }).then(function () {
      // 服务端已收字节超过文件大小（换了同名的更小文件）：先弃掉旧会话
      if (offset > f.size) {
        return cfg.api(ep('uploadAbort') + '?' + q, { method: 'POST' }).then(function () { offset = 0; }, function () { offset = 0; });
      }
    }).then(function () {
      if (offset > 0) jobUpdate(job, { pct: offset / f.size * 100, status: '断点续传：已有 ' + bytes(offset) + ' / ' + bytes(f.size) });
      function next() {
        if (offset >= f.size) return { ok: true };
        var start = offset;
        var end = Math.min(start + CHUNK, f.size);
        var chunk = f.slice(start, end);
        var lastErr = '网络错误';
        function attempt(n) {
          var started = Date.now();
          return postBinary(
            ep('uploadChunk') + '?' + q + '&offset=' + start + '&total=' + f.size + '&overwrite=' + (overwrite ? 1 : 0),
            chunk,
            function (loaded) {
              var done = start + loaded;
              jobUpdate(job, {
                pct: done / f.size * 100,
                status: (done / f.size * 100).toFixed(0) + '% · ' + bytes(done) + ' / ' + bytes(f.size) + ' · ' +
                  rate(done - start, Date.now() - started) + '（分片 ' + (Math.floor(start / CHUNK) + 1) + '/' + chunks + '）'
              });
            }
          ).then(function (r) {
            if (r && r.code === 0) { offset = end; return next(); }
            lastErr = (r && r.message) || '网络错误';
            if (/已存在/.test(lastErr)) return { ok: false, message: lastErr };
            // 响应丢失但服务端其实已收下：对齐已收字节后继续（真续传）
            return cfg.api(ep('uploadStatus') + '?' + q, {}).then(function (st) {
              var got = st && st.code === 0 ? (st.data.received || 0) : start;
              if (got > start) { offset = Math.min(got, f.size); return next(); }
              if (n < 3) return sleep(500 * n).then(function () { return attempt(n + 1); });
              return { ok: false, message: lastErr };
            }, function () {
              if (n < 3) return sleep(500 * n).then(function () { return attempt(n + 1); });
              return { ok: false, message: lastErr };
            });
          });
        }
        return attempt(1);
      }
      return next();
    });
  }
  /** 批量上传：先建目录树 → 逐文件传（同名覆盖确认由面板在调用前完成） */
  async function uploadAll(targets, dirs, dir) {
    if (!dir) { toast('请先进入具体目录', true); return; }
    if (dirs && dirs.length) {
      var r = await cfg.api(ep('mkdirs'), { method: 'POST', body: JSON.stringify({ path: dir, dirs: dirs }) });
      if (r && r.code !== 0) { toast(r.message || '创建目录失败', true); return; }
    }
    if (!targets || !targets.length) return;
    var ok = 0, fail = 0, firstErr = '';
    for (var i = 0; i < targets.length; i++) {
      var t = targets[i];
      var job = jobAdd('⬆ ' + (t.rel ? t.rel + '/' : '') + t.name);
      jobUpdate(job, { pct: 0, status: (i + 1) + '/' + targets.length + ' · 准备中' });
      var res;
      try { res = await uploadFile(t, t.overwrite, job); } catch (e) { res = { ok: false, message: (e && e.message) || '上传失败' }; }
      if (res && res.ok) { ok++; jobUpdate(job, { pct: 100, status: '完成', onAction: null }); }
      else {
        fail++; firstErr = firstErr || ((res && res.message) || '上传失败');
        jobUpdate(job, {
          status: '失败：' + ((res && res.message) || '上传失败'),
          onAction: (function (tt, jj) { return function () { return retryUpload(tt, tt.overwrite, jj); }; })(t, job)
        });
      }
    }
    if (ok) toast('上传完成：成功 ' + ok + ' 个' + (fail ? '，失败 ' + fail + ' 个（可在任务面板续传）' : ''), fail > 0);
    else if (fail) toast('上传失败 ' + fail + ' 个：' + firstErr, true);
    if (cfg.onUploadDone) cfg.onUploadDone();
  }
  async function retryUpload(t, overwrite, job) {
    jobUpdate(job, { status: '重试中…', onAction: null });
    var res;
    try { res = await uploadFile(t, overwrite, job); } catch (e) { res = { ok: false, message: (e && e.message) || '上传失败' }; }
    if (res && res.ok) { jobUpdate(job, { pct: 100, status: '完成（断点续传）' }); if (cfg.onUploadDone) cfg.onUploadDone(); }
    else jobUpdate(job, {
      status: '失败：' + ((res && res.message) || '上传失败'),
      onAction: function () { return retryUpload(t, overwrite, job); }
    });
  }

  /* ── 下载 ── */
  function download(path, name, it) {
    it = it || {};
    if (!(it.size >= DL_RESUME_MIN)) return downloadDirect(path, name);
    return downloadResume(path, name, it);
  }
  async function downloadDirect(path, name) {
    var r = await fetchOrFail(ep('download') + '?path=' + encodeURIComponent(path), { headers: authHeaders({}) });
    if (r.status === 401) { onAuthFail(); return; }
    var ct = r.headers.get('content-type') || '';
    if (!r.ok || ct.indexOf('application/json') >= 0) {
      var msg = '下载失败';
      try { var j = await r.json(); msg = j.message || msg; } catch (e) { }
      toast(msg, true); return;
    }
    saveBlob(await r.blob(), name);
  }
  function saveBlob(blob, name) {
    if (typeof document === 'undefined') { toast('当前环境无法自动保存文件：' + name, true); return; }
    var a = document.createElement('a');
    a.href = URL.createObjectURL(blob); a.download = name;
    document.body.appendChild(a); a.click();
    setTimeout(function () { URL.revokeObjectURL(a.href); a.remove(); }, 1000);
  }
  function etagKey(path) { return (cfg.storagePrefix || 'fm') + '_dl_etag:' + path; }
  function etagGet(path) { return store().get(etagKey(path)) || ''; }
  function etagSet(path, v) { v ? store().set(etagKey(path), v) : store().remove(etagKey(path)); }
  /** 断点续传下载：有「另存为」句柄就落盘（跨次续传），否则内存兜底 */
  async function downloadResume(path, name, it) {
    var handle = null;
    if (cfg.filePicker !== false && typeof root.showSaveFilePicker === 'function') {
      try { handle = await root.showSaveFilePicker({ suggestedName: name }); }
      catch (e) { if (e && e.name === 'AbortError') return; handle = null; }
    }
    var job = jobAdd('⬇ ' + name);
    var ctrl = { cancelled: false };
    jobUpdate(job, { status: '准备中', onCancel: function () { ctrl.cancelled = true; jobUpdate(job, { status: '正在停止…' }); } });
    try {
      if (handle) await downloadToFile(handle, path, name, it, job, ctrl);
      else await downloadToMemory(path, name, it, job, ctrl);
    } catch (e) {
      jobUpdate(job, {
        status: '失败：' + ((e && e.message) || e), onCancel: null,
        onAction: function () { return downloadResume(path, name, it); }
      });
      throw e;
    }
  }
  async function downloadToFile(handle, path, name, it, job, ctrl) {
    var offset = 0;
    try { offset = (await handle.getFile()).size; } catch (e) { offset = 0; }
    var probe = await fetchRange(path, 0, 0, '', 1);
    var total = probe.total || it.size || 0;
    if (!total || offset > total) offset = 0;
    var stored = etagGet(path);
    if (offset > 0 && stored && probe.etag && stored !== probe.etag) offset = 0;
    var etag = probe.etag || '';
    var w = await handle.createWritable({ keepExistingData: offset > 0 });
    var done = offset;
    try {
      if (done > 0) jobUpdate(job, { pct: done / total * 100, status: '断点续传：已有 ' + bytes(done) + ' / ' + bytes(total) });
      while (done < total) {
        if (ctrl.cancelled) {
          await w.close();
          jobUpdate(job, { status: '已暂停（' + bytes(done) + ' / ' + bytes(total) + '），重新点击下载可续传', onCancel: null });
          return;
        }
        var start = done;
        var r = await fetchRange(path, start, Math.min(start + DL_CHUNK, total) - 1, etag, 3);
        if (r.total) total = r.total;
        if (r.status === 412) {
          await w.truncate(0); done = 0; etag = r.etag || etag; etagSet(path, etag);
          jobUpdate(job, { pct: 0, status: '源文件已更新，从头下载' });
          continue;
        }
        if (r.status === 200 && start > 0) {
          // 服务端忽略了 Range（不应发生）：整文件已返回，直接整体重写
          await w.truncate(0);
          await w.write({ type: 'write', position: 0, data: r.buf });
          done = r.buf.length; total = r.buf.length;
        } else {
          await w.write({ type: 'write', position: start, data: r.buf });
          done = start + r.buf.length;
        }
        if (etag) etagSet(path, etag);
        jobUpdate(job, { pct: total ? done / total * 100 : 0, status: bytes(done) + ' / ' + bytes(total) });
        if (!r.buf.length) break;
      }
      await w.close();
      etagSet(path, '');
      jobUpdate(job, { pct: 100, status: '完成', onCancel: null, onAction: null });
      toast('已保存：' + name);
    } catch (e) {
      try { await w.close(); } catch (_) { }
      throw e;
    }
  }
  async function downloadToMemory(path, name, it, job, ctrl) {
    var probe = await fetchRange(path, 0, 0, '', 1);
    var total = probe.total || it.size || 0;
    var etag = probe.etag || '';
    var parts = dlParts.get(path) || [];
    var done = parts.reduce(function (a, b) { return a + b.size; }, 0);
    var meta = dlMeta.get(path);
    if (!meta || meta.size !== total || done > total) { parts = []; done = 0; }
    dlParts.set(path, parts);
    dlMeta.set(path, { size: total });
    if (done > 0) jobUpdate(job, { pct: done / total * 100, status: '断点续传：已有 ' + bytes(done) + ' / ' + bytes(total) });
    while (done < total) {
      if (ctrl.cancelled) {
        jobUpdate(job, { status: '已暂停（' + bytes(done) + ' / ' + bytes(total) + '），重新点击下载可续传', onCancel: null });
        return;
      }
      var start = done;
      var r = await fetchRange(path, start, Math.min(start + DL_CHUNK, total) - 1, etag, 3);
      if (r.total) total = r.total;
      if (r.status === 412) {
        parts = []; done = 0; etag = r.etag || etag;
        dlParts.set(path, parts); dlMeta.set(path, { size: total });
        continue;
      }
      if (r.status === 200 && start > 0) { parts = []; done = 0; continue; }
      if (r.buf.length) parts.push(new Blob([r.buf]));
      done = start + r.buf.length;
      jobUpdate(job, { pct: total ? done / total * 100 : 0, status: bytes(done) + ' / ' + bytes(total) });
      if (!r.buf.length) break;
    }
    dlParts.delete(path); dlMeta.delete(path);
    saveBlob(new Blob(parts), name);
    jobUpdate(job, { pct: 100, status: '完成', onCancel: null, onAction: null });
    toast('已保存：' + name);
  }
  /** Range 取一片；412 = 源文件已变（调用方从头重下）；网络/5xx 自动重试 */
  async function fetchRange(path, start, end, etag, retries) {
    var tries = retries || 3;
    var last = null;
    for (var attempt = 1; attempt <= tries; attempt++) {
      try {
        var h = { 'Range': 'bytes=' + start + '-' + end };
        if (etag) h['If-Range'] = etag;
        var r = await fetchOrFail(ep('download') + '?path=' + encodeURIComponent(path), { headers: authHeaders(h) });
        if (r.status === 401) { onAuthFail(); throw new Error('登录已过期'); }
        var ct = r.headers.get('content-type') || '';
        var m = /\/(\d+)\s*$/.exec(r.headers.get('content-range') || '');
        var out = { status: r.status, total: m ? +m[1] : 0, etag: r.headers.get('etag') || '', buf: new Uint8Array(0) };
        // 防呆：错误响应是 JSON，绝不能当文件内容写盘
        if (ct.indexOf('application/json') >= 0) {
          if (r.status === 412) return out;
          var msg = '下载失败';
          try { var j = await r.json(); msg = j.message || msg; } catch (e) { }
          last = new Error(msg);
          if (r.status < 500) break;
        } else if (r.status === 200 || r.status === 206) {
          out.buf = new Uint8Array(await r.arrayBuffer());
          if (r.status === 200) out.total = out.buf.length;
          return out;
        } else {
          last = new Error('下载失败（HTTP ' + r.status + '）');
          if (r.status < 500) break;
        }
      } catch (e) { last = e; }
      if (attempt < tries) await sleep(500 * attempt);
    }
    throw last || new Error('下载失败');
  }

  /* ── 批量操作（单批 500；服务端面板层上限 5000，前端再兜一层） ── */
  async function postPathsChunked(url, paths, target, chunk) {
    var step = chunk || 500;
    var total = { files: 0, bytes: 0, deleted: 0, freedBytes: 0 };
    for (var i = 0; i < paths.length; i += step) {
      var part = paths.slice(i, i + step);
      var body = target ? { paths: part, target: target } : { paths: part };
      var j = await cfg.api(url, { method: 'POST', body: JSON.stringify(body) }).catch(function () { return { code: -1, message: '网络错误' }; });
      if (j.code !== 0) return { code: j.code, message: j.message, data: total, partial: true };
      var d = j.data || {};
      total.files += d.files || 0; total.bytes += d.bytes || 0;
      total.deleted += d.deleted || 0; total.freedBytes += d.freedBytes || 0;
    }
    return { code: 0, data: total };
  }

  /* ── 剪贴板模型（模式 + 绝对路径；localStorage 持久化，刷新不丢） ── */
  function clipKey() { return (cfg.storagePrefix || 'fm') + '_clip'; }
  function clipLoad() {
    try {
      var raw = store().get(clipKey());
      if (!raw) return null;
      var d = JSON.parse(raw);
      return (d && Array.isArray(d.paths) && d.paths.length && (d.mode === 'cut' || d.mode === 'copy')) ? d : null;
    } catch (e) { return null; }
  }
  function clipSave(d) {
    if (d) store().set(clipKey(), JSON.stringify(d));
    else store().remove(clipKey());
  }
  /** 剪贴板提示条的**内容**（不含外层 .fm-clip 容器；面板已有容器时用它，避免嵌套重复边框） */
  function clipInner(d) {
    d = d || clipLoad();
    if (!d) return '';
    var names = d.paths.map(function (x) { return String(x).split(/[\\/]/).pop(); }).join('、');
    return '<span class="fm-clip-badge">' + (d.mode === 'cut' ? text('cut') : text('copy')) + '</span>' +
      '<span class="fm-clip-count">' + d.paths.length + ' ' + text('count') + '</span>' +
      '<span class="fm-clip-src" title="' + esc(d.paths.join('\n')) + '">' + esc(names) + '</span>' +
      '<span class="fm-clip-tip">' + esc(text('tip')) + '</span>' +
      '<span style="flex:1"></span>' +
      '<button type="button" class="fm-clip-clear" data-fm-clip-clear="1">' + esc(text('clear')) + '</button>';
  }
  /** 剪贴板提示条的完整 HTML（含外层容器；面板把整段塞进自己的模板时用它） */
  function clipHtml(d) {
    var body = clipInner(d);
    return body ? '<div class="fm-clip">' + body + '</div>' : '';
  }
  /** 把剪贴板提示条渲染进容器（容器 id 或元素；空剪贴板 → 隐藏并清空） */
  function clipRender(target) {
    var el = typeof target === 'string'
      ? (typeof document === 'undefined' ? null : document.getElementById(target))
      : target;
    if (!el) return;
    var d = clipLoad();
    if (!d) { el.innerHTML = ''; el.hidden = true; if (el.classList) el.classList.add('hidden'); return; }
    el.hidden = false;
    if (el.classList) el.classList.remove('hidden');
    // 容器自身已是 .fm-clip 时只填内容，避免嵌套出双边框
    el.innerHTML = (el.classList && el.classList.contains('fm-clip')) ? clipInner(d) : clipHtml(d);
  }
  // 「清空」按钮统一走委托事件：面板无需暴露全局函数，也不与面板同名函数冲突
  if (typeof document !== 'undefined') {
    document.addEventListener('click', function (e) {
      var t = e.target;
      if (!t || !t.closest) return;
      if (!t.closest('[data-fm-clip-clear]')) return;
      e.preventDefault();
      clipSave(null);
      if (cfg.onClipChange) cfg.onClipChange(null);
    });
  }

  /* ── 拖拽文件夹：webkitGetAsEntry 递归（readEntries 每次最多 100 条，必须循环读到空） ── */
  function readAllEntries(reader) {
    return new Promise(function (resolve, reject) {
      var all = [];
      function step() {
        reader.readEntries(function (batch) {
          if (!batch || !batch.length) { resolve(all); return; }
          for (var i = 0; i < batch.length; i++) all.push(batch[i]);
          step();
        }, reject);
      }
      step();
    });
  }
  async function walkEntry(entry, prefix, files, dirs) {
    if (!entry) return;
    if (entry.isFile) {
      try {
        var file = await new Promise(function (res, rej) { entry.file(res, rej); });
        files.push({ file: file, rel: prefix });
      } catch (e) { }
      return;
    }
    if (!entry.isDirectory) return;
    var rel = prefix ? prefix + '/' + entry.name : entry.name;
    // dirs 允许是数组（面板上传用）或 Set（RAgent 旧签名用）
    if (dirs && dirs.add) dirs.add(rel); else if (dirs) dirs.push(rel);
    var kids = [];
    try { kids = await readAllEntries(entry.createReader()); } catch (e) { }
    for (var i = 0; i < kids.length; i++) await walkEntry(kids[i], rel, files, dirs);
  }

  function configure(opts) {
    cfg = Object.assign({}, opts || {});
    cfg.endpoints = Object.assign({}, DEFAULTS.endpoints, (opts && opts.endpoints) || {});
    return cfg;
  }

  root.FmClient = {
    VERSION: VERSION,
    configure: configure,
    constants: { chunk: CHUNK, dlChunk: DL_CHUNK, dlResumeMin: DL_RESUME_MIN },
    sleep: sleep,
    rate: rate,
    bytes: bytes,
    join: join,
    joinRel: joinRel,
    uploadId: uploadId,
    postBinary: postBinary,
    uploadFile: uploadFile,
    uploadAll: uploadAll,
    download: download,
    downloadDirect: downloadDirect,
    fetchRange: fetchRange,
    saveBlob: saveBlob,
    postPathsChunked: postPathsChunked,
    readAllEntries: readAllEntries,
    walkEntry: walkEntry,
    clip: { load: clipLoad, save: clipSave, html: clipHtml, inner: clipInner, render: clipRender },
    jobs: {
      list: function () { return jobs; },
      add: jobAdd,
      update: jobUpdate,
      render: jobRender,
      clear: function () { jobs = []; jobRender(); },
      act: function (i) { var j = jobs[i]; if (j && j.onAction) j.onAction(); },
      cancel: function (i) { var j = jobs[i]; if (j && j.onCancel) j.onCancel(); }
    }
  };
})(typeof globalThis !== 'undefined' ? globalThis : (typeof window !== 'undefined' ? window : this));

// CommonJS 互操作（Electron / 打包器）：require 时直接拿到实例
if (typeof module !== 'undefined' && module.exports && typeof globalThis !== 'undefined') {
  module.exports = globalThis.FmClient;
}
