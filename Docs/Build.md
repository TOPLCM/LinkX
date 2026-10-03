# 从源码构建

前置：Rust（版本由 `rust-toolchain.toml` 锁定，CI 与构建脚本三处一致）、
Windows 侧需 MinGW-w64（目标 `x86_64-pc-windows-gnu`，链接器写在 `.cargo/config.toml`）、
Android 侧需 JDK 21 + Android NDK（`ANDROID_NDK_HOME`）。构建目录统一在 `Target/`。

## Windows

```bash
# 核心 + 壳（交付目标是 gnu，不是 msvc）
bash Scripts/build-mingw-windows.sh        # → Target/x86_64-pc-windows-gnu/release/linkx.exe

# MSI → Release/Windows/（需本机 WiX 5.x）
bash Scripts/build-msi-windows.sh
```

**为什么必须是 `x86_64-pc-windows-gnu`**：`build.rs` 只在 windows-gnu 下调用 `windres`
编图标资源；msvc 目标那一步被跳过，产出的 exe 没有图标（资源管理器、任务栏、卸载面板
全是默认图标）。直接 `cargo build --release` 得到的就是这份没图标的产物 —— 别拿它交付。

## Android

```bash
# 核心 .so（release，不含调试面）→ 再打 APK
bash Scripts/build-android-core.sh
bash Scripts/build-android-apk.sh --release

# 调试变体：DEBUGD=1 必须同时给两个脚本
DEBUGD=1 bash Scripts/build-android-core.sh && DEBUGD=1 bash Scripts/build-android-apk.sh
```

只给其中一个，会产出一个"名字叫 debug、其实没有控制面"的包（脚本会校验 `.so` 里的
agent-debug 状态并强制重建，就是为了拦住这个）。

APK 用 `Tools/Keys/linkx-test.jks` 自签（缺失时脚本自动生成），**调试包与交付包同一把签名**，
覆盖安装不会丢配对。这是测试密钥不是发布密钥：仓库不含也不应含真实发布密钥，
正式分发请用你自己的 keystore 重签（重签后校验值会变，`Release/README.md` 的表随之更新）。

`Tools/` 的布局与配套的本地工具链持久化脚本是维护者本机方案，不在公开仓库里，
**普通贡献者不需要、也跑不通**，直接用上面的标准工具链即可。

## 改动后必过的自检

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p linkx-windows --target x86_64-pc-windows-gnu -- -D warnings   # 宿主 clippy 看不到 cfg(windows)
cargo test --workspace
for s in check-compose-kotlin-pair check-protocol-sync check-crypto-audit \
         check-version-sync check-comment-hygiene check-release-clean check-install-single; do
  bash Scripts/$s.sh
done
```

六道门禁各管什么：

| 脚本 | 拦的是什么 |
|---|---|
| `check-protocol-sync.sh` | Proto 与两端实现漂移 |
| `check-crypto-audit.sh` | 依赖图混进 AES 系实现、加密用法越界 |
| `check-version-sync.sh` | 版本号在 Cargo / Android / README / 安装器模板各处不一致 |
| `check-compose-kotlin-pair.sh` | Compose 与 Kotlin 版本不配对（"编译过但出包才炸"） |
| `check-comment-hygiene.sh` | 源码里出现内部编号、注释占比与连续块超长 |
| `check-release-clean.sh` | 交付产物里残留调试控制面（依赖图闸门 + 字节扫描） |

`check-release-clean.sh` 的用法与它为什么还要**反向断言产品符号必须存在**，
写在 [`Debug-Plane.md`](Debug-Plane.md) 第 7 节。

## 真机验收脚本

改完不是"编译过了"就算数。这些脚本用调试控制面驱动两台真设备，判据来自埋点与字节比对：

| 脚本 | 验的是 |
|---|---|
| `linkx-ctl.py` | 双端状态对照、配对、剪贴板往返（两端同一个入口） |
| `b3-file-transfer-check.py` | 电脑→手机：小文件/中文名/emoji 名/大文件逐字节 sha256 |
| `a7-phone-to-pc-check.py` | 手机→电脑：含"发送前必须不存在"前提与在途/丢包计数 |
| `verify-resume.py` | 断点续传四用例（故障注入 + 不注错的反向对照） |
| `soak-transfer.py` | 双向浸泡多轮，只认 `file.meta` + `file.done` 这一对与落盘字节数 |
| `b-bench-throughput.py` | 双向吞吐与电脑端工作集峰值（内存红线的实测口） |
| `stress-pair.py` | 反复配对的稳定性（进程死亡与系统崩溃记录） |
| `mem-map.py` / `measure-feature-memory.py` | 内存构成与功能开关收益实测 |
| `ui-shot.py` / `check-repaint.py` / `nav_preview.py` | 界面截图、重绘触发取证、图标肉眼验收 |

> 依赖面已经很小：2026-09-30 删掉 vendored LocalSend 与未接线的互操作层之后，
> `Cargo.lock` 从 287 个唯一 crate 降到 160 个，tokio / hyper / reqwest / rustls 全部不再出现。
