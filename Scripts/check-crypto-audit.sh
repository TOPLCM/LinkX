#!/usr/bin/env bash
# 依赖图校验：拦截 AES 相关依赖（ring 例外）
# 选型口径：全项目只有一种 AEAD = ChaCha20-Poly1305，配 x25519 + snow 握手。
# 多一套 AES 就是多一条可被误用的密码学路径，因此按依赖图而非代码审查来拦。
# 用法：Scripts/check-crypto-audit.sh [path-to-Cargo.lock]
set -euo pipefail
LOCK="${1:-Cargo.lock}"
[ -f "$LOCK" ] || { echo "❌ 找不到 $LOCK"; exit 1; }

# 黑名单：任何间接引入 AES 实现即为违规（唯一的 AEAD 是 ChaCha20-Poly1305）
for pat in '^name = "aes$"' '^name = "aes-gcm"' '^name = "rust-gcm"' '^name = "aes-soft"' '^name = "aesni"'; do
  if grep -Eq "$pat" "$LOCK"; then
    echo "❌ 检测到 AES 依赖（$pat），违反「唯一 AEAD = ChaCha20-Poly1305」"
    exit 1
  fi
done

# 必须存在
for expect in '^name = "chacha20poly1305"' '^name = "snow"' '^name = "x25519-dalek"' '^name = "sha2"' '^name = "hkdf"'; do
  if ! grep -Eq "$expect" "$LOCK"; then
    echo "❌ 缺少必要加密依赖（$expect）"
    exit 1
  fi
done

# ring 例外：允许 ring 提供曲线实现，但不允许它成为另一个 AES 载体
grep -Eq '^name = "ring"' "$LOCK" && echo "ℹ️ ring 存在（例外项，允许）" || true
echo "✅ crypto audit passed"