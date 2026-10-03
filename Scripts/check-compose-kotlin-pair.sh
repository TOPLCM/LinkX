#!/usr/bin/env bash
# Compose 编译器与 Kotlin 版本必须一一对应（libs.versions.toml 单一源）
# 校验：Kotlin 2.x → Compose 编译器插件版本 == Kotlin 版本
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TOML="$ROOT/Platforms/Android/gradle/libs.versions.toml"

KOTLIN="$(grep -E '^kotlin = ' "$TOML" | head -1 | sed -E 's/.*"([^"]+)".*/\1/')"
[ -n "$KOTLIN" ] || { echo "❌ libs.versions.toml 缺 kotlin 版本"; exit 1; }

# Kotlin 2.0+：compose 插件版本必须绑定 kotlin 版本，两者不可各写一份
if [ "${KOTLIN%%.*}" -ge 2 ]; then
  COMPOSE_PLUGIN="$(grep -E 'kotlin-compose = .*version.ref = "kotlin"' "$TOML")"
  [ -n "$COMPOSE_PLUGIN" ] || { echo "❌ Kotlin 2.x 需 kotlin-compose 插件绑定 kotlin 版本"; exit 1; }
fi
echo "✅ compose/kotlin pair passed: kotlin=$KOTLIN"