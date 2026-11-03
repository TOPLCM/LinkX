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

**Windows 宿主上的 WiX 路线还要有一个 Util 扩展**：安装器在卸载/升级前先结束正在运行的程序，用的是
`util:CloseApplication`，它只存在于 `WixToolset.Util.wixext` 里。构建脚本优先用本机已有的那份扩展，
没有时 WiX 会从 NuGet 取一次（因此这一步需要联网，取到后缓存在本机用户目录）。
Linux 宿主的 wixl 链路没有这个扩展，打出来的包不带这道保护。

**为什么必须是 `x86_64-pc-windows-gnu`**：`build.rs` 只在 windows-gnu 下调用 `windres`
编图标资源；msvc 目标那一步被跳过，产出的 exe 没有图标（资源管理器、任务栏、卸载面板
全是默认图标）。直接 `cargo build --release` 得到的就是这份没图标的产物 —— 别拿它交付。

## Windows ARM64（原生包）

```bash
# 只能在 Windows 宿主上编：要 MSVC 的 ARM64 交叉工具链 + Windows SDK 的 ARM64 库
bash Scripts/build-msvc-windows.sh                  # → Target/aarch64-pc-windows-msvc/release/linkx.exe
ARCH=arm64 bash Scripts/build-msi-windows.sh        # → Release/Windows/LinkX-<版本>-arm64.msi
```

上面那句"必须 gnu"的前提在 0.5.3 已经改掉了一半：`build.rs` 现在给 msvc 目标调用 Windows SDK 的
`rc.exe` 编同一份 `linkx.rc`，**找不到 rc.exe 就直接失败**，不再静默产出一个没图标的 exe
（要指定路径用 `LINKX_RC=<rc.exe 全路径>`）。所以两条链各自成立：x64 走 MinGW（任何宿主都能交叉编，
CI 就靠这条），ARM64 走 MSVC（Linux 上交叉不过去，只能 Windows 宿主）。

工具链装法（工作区内自建一份也可以）：

```powershell
vs_buildtools.exe --quiet --wait --norestart --installPath "<仓库目录>\Tools\Toolchain\VSBuildTools" `
  --add Microsoft.VisualStudio.Workload.VCTools `
  --add Microsoft.VisualStudio.Component.VC.Tools.ARM64 `
  --add Microsoft.VisualStudio.Component.Windows11SDK.22621
```

`build-msvc-windows.sh` 除了编译还做两件断言：PE 机器码必须是 `0xAA64`（ARM64），
以及 exe 里必须有非空的 `.rsrc` 段。前者拦"目标是 ARM64 但链出来还是 x64"，
后者拦"图标资源静默没编进去"。安装包侧不需要额外准备：WiX 的 util 扩展会按包的目标架构
挑对应的原生自定义动作（arm64 包里落的是 `Wix4UtilCA_A64`），"卸载/升级前先结束运行中的程序"
那道保护没掉。

⚠ **运行期验证状态**：原生 ARM64 包在没有 ARM 硬件的机器上只能验到"产物形态 + 安装包表级判据"，
装得上吗、蓝牙能不能连、传输正不正常都没有实测数据。发布与文档都必须照实写这一句。

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

**一个 APK 带两套 ABI**（`arm64-v8a` + `armeabi-v7a`）：`build-android-core.sh` 给两个 Rust 目标
各出一份 `liblinkx_core.so`，Gradle 的 `abiFilters` 把两份都收进 `lib/<abi>/`，装机时由系统按设备自动取用。
只发 arm64 的话 32 位老机装不上；只发 v7a 的话，Android 13+ 上那些已经砍掉 32 位支持的 64 位机型
反而装不上 —— 所以两套同时在包里，台账仍然算一个 Android 产物。代价是每个 ABI 一份 `.so`
（arm64 那份约 0.87 MB、32 位那份约 0.55 MB，两套合计让上架的包从 2.31 MB 涨到 2.86 MB）。

APK 的签名密钥由出包脚本在**你自己的机器上**生成并复用（默认落在本机的 `LOCAL_KEYS/` 下，
这个目录被 `.gitignore` 排除 —— **密钥不入库**：仓库里没有这把钥匙，也没有任何发布密钥）。
调试包与交付包**用同一把签名**，覆盖安装因此不会丢配对。
这是测试密钥不是发布密钥：正式分发请换成**发布者自己的 keystore** 重签
（重签后文件哈希会变，`Release/README.md` 的登记值要跟着更新）。

出包脚本按这个顺序找工具：环境变量（`ANDROID_HOME` / `ANDROID_NDK_HOME` / `GRADLE_BIN` / `BUILD_TOOLS` /
`LINKX_TEST_KEYSTORE`）→ `PATH` → 仓库自带的 Gradle wrapper。都没有时才退回维护者本机的 `Tools/`
布局，那个目录不入库。所以**装好标准的 JDK / Android SDK 与 NDK / WiX，设一个 `ANDROID_HOME`
就能出包**，不需要复刻维护者的目录结构。

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

七道门禁各管什么：

| 脚本 | 拦的是什么 |
|---|---|
| `check-protocol-sync.sh` | Proto 与两端实现漂移 |
| `check-crypto-audit.sh` | 依赖图混进 AES 系实现、加密用法越界 |
| `check-version-sync.sh` | 版本号在 Cargo / Android / README / 安装器模板各处不一致 |
| `check-compose-kotlin-pair.sh` | Compose 与 Kotlin 版本不配对（"编译过但出包才炸"） |
| `check-comment-hygiene.sh` | 源码里出现内部编号、注释占比与连续块超长 |
| `check-release-clean.sh` | 交付产物里残留调试控制面（依赖图闸门 + 字节扫描） |
| `check-install-single.sh` | 装完之后注册表里只有一份 LinkX，不分叉 |

`check-release-clean.sh` 的用法与它为什么还要**反向断言产品符号必须存在**，
写在 [`Debug-Plane.md`](Debug-Plane.md) 第 7 节。

## 真机验收做过什么

改完不是"编译过了"就算数。下面这些验收项跑在维护者手边的两台真设备上，用调试控制面驱动，
判据来自埋点与逐字节比对——**这套脚本依赖本机设备路径，不随本仓库发布**（列在这里是为了说明
每个结论是怎么来的，不是让你去找文件）：

| 验收项 | 判据 |
|---|---|
| 双端状态对照、配对、剪贴板往返 | 两端同一个入口读同一份状态，不一致就是 bug |
| 电脑→手机文件 | 小文件 / 中文名 / emoji 名 / 大文件，落盘逐字节比 sha256 |
| 手机→电脑文件 | 带"发送前必须不存在"前提，并记在途与丢包计数 |
| 断点续传 | 四个用例，含故障注入与不注错的反向对照 |
| 双向浸泡 | 多轮往返，只认 `file.meta` + `file.done` 这一对与落盘字节数 |
| 吞吐与内存红线 | 双向吞吐，外加电脑端工作集峰值 |
| 工作集构成 | 只读归因，不改被测代码 |
| 界面截图 | 活窗口出图，改完 UI 肉眼验收 |

> 依赖面已经很小：2026-09-30 删掉 vendored LocalSend 与未接线的互操作层之后，
> `Cargo.lock` 从 287 个唯一 crate 降到 160 个，tokio / hyper / reqwest / rustls 全部不再出现。
