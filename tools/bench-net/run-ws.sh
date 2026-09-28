#!/usr/bin/env bash
# WebSocket 性能对比编排（Linux 版，与 run-ws.ps1 等价）
# 用法: ./run-ws.sh [conns] [msgs] [payload_size] [loaders]
# 优先使用 zigbuild 交叉编译产物（x86_64/aarch64 musl），否则回退本机 cargo 构建。
set -u
cd "$(dirname "$0")"

CONNS="${1:-64}"; MSGS="${2:-2000}"; PAYLOAD="${3:-256}"; LOADERS="${4:-1}"

case "$(uname -m)" in
  x86_64) TD="target/x86_64-unknown-linux-musl/release" ;;
  aarch64|arm64) TD="target/aarch64-unknown-linux-musl/release" ;;
  *) TD="" ;;
esac
if [ -z "$TD" ] || [ ! -x "$TD/ws_raw" ]; then
  cargo build --release
  TD="target/release"
fi

run_server() {
  name="$1"; port="$2"
  echo ""
  echo "== $name @ 127.0.0.1:$port =="
  "$TD/$name" "127.0.0.1:$port" &
  pid=$!
  for _ in $(seq 1 50); do
    if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then exec 3>&-; break; fi
    sleep 0.1
  done
  sleep 0.3

  sum=0
  for j in $(seq 1 "$LOADERS"); do
    "$TD/ws_load" "127.0.0.1:$port" "$CONNS" "$MSGS" "$PAYLOAD" > "load-$name-$j.txt" &
    eval "lp$j=$!"
  done
  for j in $(seq 1 "$LOADERS"); do
    eval "p=\$lp$j"; wait "$p"
  done
  for j in $(seq 1 "$LOADERS"); do
    f="load-$name-$j.txt"
    cat "$f"
    m=$(sed -n 's/.*mps=\([0-9]*\).*/\1/p' "$f")
    sum=$((sum + ${m:-0}))
  done
  if [ "$LOADERS" -gt 1 ]; then echo "aggregate mps = $sum"; fi

  kill "$pid" 2>/dev/null
  wait "$pid" 2>/dev/null
}

run_server ws_raw 18091
run_server ws_tungstenite 18092
run_server ws_fast 18093
