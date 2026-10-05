# 第三方组件许可登记（Third-Party Licenses）

本文件登记 LinkX 使用的**非原创代码与素材**，以及随产物分发的许可义务。
常规 crate 依赖由 `Cargo.lock` 管理，许可证以各 crate 自身声明为准，本文件末尾给出整体核查结论。

LinkX 本体以 **GPL-3.0-or-later** 授权（见 `LICENSE`）。下列条目各自保留其上游授权；
Apache-2.0 与 GPL-3 是兼容的 copyleft（与 GPL-2 才冲突），因此可以合法分发在 GPL-3 组合作品里，
前提是履行各自的署名与保留义务。

## 一、逐条登记

| 组件 | 出处 | 许可 | 我方义务 |
|---|---|---|---|
| LocalSend 文件名规则（移植版） | `localsend/localsend` 的 `packages/core/src/util/filename.rs`，commit `e768240d1ad95f0f162b852b5ff37bec71cde1ef` | Apache-2.0 | 随附许可全文、声明改动、保留署名 |
| Gradle Wrapper | `gradle/gradle` | Apache-2.0 | 保留文件自带的 SPDX 与版权行 |
| Android 运行时依赖 | AndroidX、Jetpack Compose、Material（Google 及贡献者） | Apache-2.0 | 随 APK 分发时给出许可清单 |
| `svg/` 图标包（含产品 Logo 几何） | 取自 www.iconfont.cn，经 `Scripts/svg_assets.py` 生成两端资源 | 可公开免费使用，见「三」 | 如实登记出处 |

各项在仓库里的位置与履行情况如下。

**LocalSend 移植件**：代码是 `Crates/transfer/src/filename.rs`，在生产路径里；上游版权人是
Tien Do Nam 及贡献者（Copyright 2022-2026）。许可全文（`LICENSE`）、上游署名（`NOTICE`）
与移植范围和改动声明（`PORTING.md`）都在 `Licenses/localsend/`，对应 Apache-2.0 §4(a)、§4(b)、
§4(d) 三项义务，并随 MSI 分发到安装目录。vendored 代码层的移除过程写在 `PORTING.md`，
仓库里已不含 vendored 代码。

**Gradle Wrapper**：`Platforms/Android/gradlew`、`gradlew.bat` 与 `gradle/wrapper/`。
上游文件自带 SPDX 与版权行，不得删除，无额外义务。

**Android 运行时依赖**：由 `Platforms/Android/app/build.gradle.kts` 声明。
APK 目前还没有随附许可清单，这一项义务尚未履行，落地方式见「二」。

**图标包**：来源与授权见「三」。

## 二、随产物分发（不是只写在仓库里）

Apache-2.0 §4(a) 与 GPL-3.0 §4 都要求"作品的任何分发形式"附带许可，
`linkx.exe`、`LinkX-*.msi`、`liblinkx_core.so`、`LinkX-*.apk` 都算分发形式。

- **MSI**：`Platforms/Windows/installers/LinkX-v4.wxs` 的 `MainExecutable` 组件把 `LICENSE`、
  `THIRD_PARTY_LICENSES.md`、`Licenses/localsend/LICENSE`（安装为 `LICENSE-localsend.txt`）、
  `Licenses/localsend/NOTICE`（安装为 `NOTICE-localsend.txt`）四个文件放进安装目录。
- **APK**：尚未携带许可清单。做法是在 `Scripts/build-android-apk.sh` 打包前把同一组文本放进
  `assets/licenses/`，并在「关于」页给一个入口。
- **早期 wixl 链路**（`Platforms/Windows/installers/LinkX.wxs`）不带任何许可文件，
  不要用它做公开分发。

## 三、图标包来源与授权

`svg/` 下 25 个 SVG 里有 24 个带 iconfont.cn 的导出指纹 `p-id="…"` 与 `t="…"`，
`关于.svg` 没有这组指纹。`Scripts/svg_assets.py` 以该目录为唯一真源，把路径几何内联进
`Platforms/Windows/src/icons_svg.rs`，生成安卓侧 24 个 vector drawable，并栅格化进 `LinkX.ico`
（嵌入 exe 资源、MSI 的 `Icon` 表与快捷方式）。这些几何因此进入了产物，产品标识本身也在其中。

授权口径：本仓库版权人确认这些图标取自 www.iconfont.cn，按其下载时的站点条款可以公开免费使用，
所以 `svg/` 随仓库公开分发。逐图标的原作者署名以 iconfont.cn 的条目页为准；这里记录来源与
确认人，是为了避免日后把它当成我们自己画的。

这些图标的线性描边风格参照 Remix Icon（Apache-2.0），只是画法上的参照：仓库里没有它的任何文件，
也没从它那儿取过路径。进了产物的几何全部出自 `svg/` 这 25 个文件。

如果日后发现某个图标不允许再分发，处置办法是替换该图标后重跑 `python Scripts/svg_assets.py`，
生成物全部可重放。

## 四、依赖整体口径（GPL-3.0 兼容性核查结论）

- `Cargo.lock` 当前是 **160 条 package 记录**，`tokio`、`hyper`、`reqwest`、`rustls` 全部为 0
  （复核命令：`grep -c '^name = "tokio"$' Cargo.lock`）。
- 现存依赖全部来自 `registry+https://github.com/rust-lang/crates.io-index`，无 git 源、无 `[patch]`。
- 上面的核查是按名字初筛 copyleft 与弱许可，命中的只有 `libsqlite3-sys` 与 `rusqlite`（MIT）、
  `ring`、`rustls*` 与 `webpki*` 系（ISC、Apache-2.0、MIT）。`Cargo.lock` 本身不含 license 字段，
  所以这是名字级初筛，不是机器核验。机器核验任选其一：

  ```bash
  cargo install cargo-license && cargo license -w
  cargo install cargo-deny && cargo deny init && cargo deny check bans licenses
  ```

- 与依赖有关的门禁有两道。**加密选型门禁** `Scripts/check-crypto-audit.sh` 只有一条硬规则：
  依赖图里不许出现 AES 系实现（本项目唯一的 AEAD 是 ChaCha20-Poly1305），同时必须能看到
  `chacha20poly1305`、`snow`、`x25519-dalek`、`sha2`、`hkdf` 这五件。**交付洁净门禁**
  `Scripts/check-release-clean.sh` 另外在交付产物的字节里禁止 `LocalSend` 与 `localsend` 串，
  防的是将来不小心把它带回来。
