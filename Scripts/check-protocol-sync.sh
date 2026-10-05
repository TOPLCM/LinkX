#!/usr/bin/env bash
# 协议一致性校验：单源是 `Proto/`，两端常量必须名值都对得上。
#  1) Rust 侧 TLV 常量与 Proto 定义的消息类型抽查
#  2) Kotlin 镜像 Tlv.kt 与 Proto/linkx/v1/tlv.rs 常量一致性
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TLS_RS="$ROOT/Proto/linkx/v1/tlv.rs"
TLV_KT="$ROOT/Platforms/Android/app/src/main/java/com/linkx/app/Tlv.kt"
PROTO_DIR="$ROOT/Proto/linkx/v1"

# 1) tlv.rs 与 tlv_codec 期望的常量存在
for tag in TAG_ADVERT_NAME TAG_OS TAG_VERSION TAG_CIPHERTEXT TAG_SAS TAG_PING TAG_PONG TAG_NONCE_TCP TAG_DEVICE_ID TAG_FINGERPRINT TAG_BIND_TAG TAG_FILE_ID TAG_RESUME_FROM; do
  grep -q "pub const $tag" "$TLS_RS" || { echo "❌ tlv.rs 缺少 $tag"; exit 1; }
done

# 2) Kotlin 镜像逐项对齐（仅比对常量名，Tlv.Ble.* 单独比对）
for tag in MSG_HELLO MSG_CHALLENGE MSG_REPLY MSG_PAIR_CONFIRM MSG_PAIR_DONE MSG_CHANNEL_BIND MSG_RESUME MSG_HEARTBEAT \
           TAG_ADVERT_NAME TAG_OS TAG_VERSION TAG_CIPHERTEXT TAG_SAS TAG_PING TAG_PONG \
           TAG_NONCE_TCP TAG_DEVICE_ID TAG_FINGERPRINT TAG_BIND_TAG TAG_FILE_ID TAG_RESUME_FROM OS_ANDROID OS_WINDOWS; do
  grep -q "const val $tag" "$TLV_KT" || { echo "❌ Tlv.kt 缺少 $tag（与 tlv.rs 不同步）"; exit 1; }
done
for uuid in SERVICE CHAR_TX CHAR_EVT; do
  grep -q "const val $uuid" "$TLV_KT" || { echo "❌ Tlv.kt 缺少 Ble.$uuid"; exit 1; }
  grep -q "pub const $uuid" "$ROOT/Crates/lan/src/transport.rs" || { echo "❌ transport.rs 缺少 $uuid"; exit 1; }
done

# 2b) 名值对必须相等。只查"名字在不在"不够：`const val MSG_HELLO = 0x99` 一样能过存在性检查，
#     而两端拿不同的数字当同一个消息类型 = 静默协议故障。顺带能抓出单边漏加。
# python3 与 python 两个命令名都试，取第一个存在的
PY="$(command -v python3 || command -v python)"
[ -n "$PY" ] || { echo "❌ 找不到 python，无法比对名值对"; exit 1; }
"$PY" - "$TLS_RS" "$TLV_KT" <<'PY' || exit 1
import re, sys
rs, kt = sys.argv[1], sys.argv[2]
pat_rs = re.compile(r'pub const ([A-Z][A-Z0-9_]*)\s*:\s*u8\s*=\s*(0x[0-9A-Fa-f]+|\d+)')
pat_kt = re.compile(r'const val ([A-Z][A-Z0-9_]*)\s*=\s*(0x[0-9A-Fa-f]+|\d+)')

def load(path, pat):
    out = {}
    for line in open(path, encoding='utf-8'):
        m = pat.search(line)
        if m:
            out[m.group(1)] = int(m.group(2), 0)
    return out

r, k = load(rs, pat_rs), load(kt, pat_kt)
bad = [n for n in sorted(set(r) & set(k)) if r[n] != k[n]]
for n in bad:
    print(f"❌ {n} 两端数值不同：tlv.rs={r[n]:#04x} vs Tlv.kt={k[n]:#04x}")
# 只比"两边都有的名字"。不查"Rust 有而 Kotlin 没有"——MSG_IDENTITY / MSG_CONFIG_SYNC 这类
# 全程在 Rust 引擎里处理，Kotlin 镜像本来就不需要它们，报出来只会促使镜像塞进用不到的死常量。
sys.exit(1 if bad else 0)
PY
echo "  ✓ tlv.rs / Tlv.kt 名值对逐项一致（$(grep -c 'pub const' "$TLS_RS") 项已比对）"

# 3) 消息类型抽查
grep -q "pub const NOTIFY_PUSH: u8 = 0x10" "$ROOT/Crates/protocol/src/lib.rs"
grep -q "pub const NOTIFY_REPLY: u8 = 0x11" "$ROOT/Crates/protocol/src/lib.rs"     || { echo "❌ 缺少 NOTIFY_REPLY 消息类型（电脑回复通知）"; exit 1; }
grep -q "pub const NOTIFY_REPLY_ACK: u8 = 0x13" "$ROOT/Crates/protocol/src/lib.rs" || { echo "❌ 缺少 NOTIFY_REPLY_ACK 消息类型（回复回执）"; exit 1; }
grep -q "pub const NOTIFY_DISMISS: u8 = 0x12" "$ROOT/Crates/protocol/src/lib.rs"   || { echo "❌ 缺少 NOTIFY_DISMISS 消息类型（通知已消失）"; exit 1; }
grep -q "pub const MEDIA_STATE: u8 = 0x80" "$ROOT/Crates/protocol/src/lib.rs"   || { echo "❌ 缺少 MEDIA_STATE 消息类型（播放状态）"; exit 1; }
grep -q "pub const DEVICE_STATUS: u8 = 0x82" "$ROOT/Crates/protocol/src/lib.rs" || { echo "❌ 缺少 DEVICE_STATUS 消息类型（电量上报）"; exit 1; }

# 4) proto 编译产物有源码（Rust 侧由 build.rs 生成，检查定义存在）
# media = 播放状态/控制指令；漏登记会让 build.rs 编不出来却没人报警
for f in common notify clipboard file heartbeat config media device album; do
  [ -f "$PROTO_DIR/$f.proto" ] || { echo "❌ 缺少 proto $f.proto"; exit 1; }
done

# 5) 文件传输摘要口径：FILE_META 可留空（边发边算），摘要改由 FILE_DONE 交付。
#    少了 FileDone.sha256 就等于把整文件校验退回"先发摘要后发块"，大文件要预扫几十秒。
grep -Eq '^  bytes sha256 = 4;' "$PROTO_DIR/file.proto" \
  || { echo "❌ file.proto 的 FileDone 缺少 sha256 字段（流式摘要交付位）"; exit 1; }
# Kotlin 镜像的事件 9 必须解出这段摘要尾段，否则手机收不到新端的校验值
grep -q 'LinkxEvent.FileDone(fileId, ok != 0, error, sha)' \
  "$ROOT/Platforms/Android/app/src/main/java/com/linkx/app/NativeCore.kt" \
  || { echo "❌ NativeCore.kt 未解析 FileDone 的摘要尾段（与 file.proto 不同步）"; exit 1; }

# 6) 通知回复口径：回复定位三元组必须在 proto 里，且手机侧事件流要解出请求（kind 21）。
#    少任何一处，电脑上的"回复"按钮就变成一个点了没反应的死入口。
grep -q '^  bool can_reply = 9;' "$PROTO_DIR/notify.proto" \
  || { echo "❌ notify.proto 的 NotificationPush 缺少 can_reply（回复入口判据）"; exit 1; }
grep -q '^message NotificationReply ' "$PROTO_DIR/notify.proto" \
  || { echo "❌ notify.proto 缺少 NotificationReply（回复请求本体）"; exit 1; }
grep -q '^message NotificationReplyAck ' "$PROTO_DIR/notify.proto" \
  || { echo "❌ notify.proto 缺少 NotificationReplyAck（回复回执本体）"; exit 1; }
grep -q 'LinkxEvent.NotifyReplyRequested(' \
  "$ROOT/Platforms/Android/app/src/main/java/com/linkx/app/NativeCore.kt" \
  || { echo "❌ NativeCore.kt 未解析事件 21（回复请求），手机侧不会执行回复"; exit 1; }
# 「通知已消失」两端都要有：proto 本体 + Kotlin 声明（Rust 侧导出同名函数，缺一处就是静默丢事件）
grep -q '^message NotificationDismiss ' "$PROTO_DIR/notify.proto" \
  || { echo "❌ notify.proto 缺少 NotificationDismiss（通知消失上报）"; exit 1; }
grep -q 'external fun nativeSendNotifyDismiss(' \
  "$ROOT/Platforms/Android/app/src/main/java/com/linkx/app/NativeCore.kt" \
  || { echo "❌ NativeCore.kt 缺少 nativeSendNotifyDismiss，手机不会上报通知消失"; exit 1; }

echo "✅ protocol sync passed"
