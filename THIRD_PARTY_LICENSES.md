# 第三方组件许可登记（Third-Party Licenses）

本文件登记 LinkX 使用的**非原创代码与素材**，以及随产物分发的许可义务。
常规 crate 依赖由 `Cargo.lock` 管理，许可证以各 crate 自身声明为准，本文件末尾给出整体核查结论。

LinkX 本体以 **GPL-3.0-or-later** 授权（见 `LICENSE`）。下列条目各自保留其上游授权；
Apache-2.0 与 GPL-3 是兼容的 copyleft（与 GPL-2 才冲突），因此可以合法分发在 GPL-3 组合作品里
——**前提是履行各自的署名与保留义务**。

## 一、逐条登记

| # | 组件 | 在仓库里的位置 | 上游来源 | License | 义务 | 履行状态 |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | **LocalSend 文件名规则（移植版）** | 代码：`Crates/transfer/src/filename.rs`（**在生产路径里**）；许可：`Licenses/localsend/{LICENSE,NOTICE,PORTING.md}` | https://github.com/localsend/localsend ，路径 `packages/core/src/util/filename.rs`，commit `e768240d1ad95f0f162b852b5ff37bec71cde1ef`；Copyright 2022-2026 Tien Do Nam 及贡献者 | Apache-2.0 | §4(a) 随附许可全文；§4(b) 声明改动；§4(d) 保留署名 | ✅ 许可全文 + NOTICE + 改动/出处记录在 `Licenses/localsend/`，并由 MSI 随包分发 |
| 2 | **Gradle Wrapper** | `Platforms/Android/gradlew`、`gradlew.bat`、`gradle/wrapper/gradle-wrapper.jar` | https://github.com/gradle/gradle | Apache-2.0 | 文件自带 SPDX 与版权行，不得删除 | ✅ 上游自署名，无额外义务 |
| 3 | **Android 运行时依赖** | `Platforms/Android/app/build.gradle.kts` 声明 | AndroidX、Jetpack Compose、Material（Google 及贡献者） | Apache-2.0 | 随 APK 分发时给出许可清单 | ⚠️ **待补**：APK 目前不携带许可清单，见「二」 |
| 4 | **`svg/` 图标包（含产品 Logo 几何）** | `svg/*.svg` → 生成物 `Platforms/Windows/src/icons_svg.rs`、`Platforms/Android/app/src/main/res/drawable/ic_*.xml`、`Platforms/Windows/LinkX.ico` | 项目所有人自 **www.iconfont.cn** 下载（文件内保留 `p-id` / `t=` 导出指纹） | 所有人确认可公开免费使用（见「三」） | 出处如实登记；原作者逐件署名以站点记录为准 | ✅ 已登记；`Docs/Design/neumorphism.md` 另标注图标语言参照 Remix Icon（Apache-2.0） |

## 二、随产物分发（不是只写在仓库里）

Apache-2.0 §4(a) 与 GPL-3.0 §4 都要求"作品的任何分发形式"附带许可，
`linkx.exe` / `LinkX-*.msi` / `liblinkx_core.so` / `LinkX-*.apk` 都算分发形式：

- **MSI**：`Platforms/Windows/installers/LinkX-v4.wxs` 的 `MainExecutable` 组件把
  `LICENSE`、`THIRD_PARTY_LICENSES.md`、`Licenses/localsend/LICENSE`（装作 `LICENSE-localsend.txt`）、
  `Licenses/localsend/NOTICE`（装作 `NOTICE-localsend.txt`）四个文件放进安装目录。
  许可页第 2 屏的文案明确指向安装目录，缺文件即为虚假陈述（历史上真出现过）。
- **APK**：⚠️ 尚未做。落地方式是在 `Scripts/build-android-apk.sh` 打包前把同一组文本放进
  `assets/licenses/`，并在「关于」页给一个入口；列为发布后待办（F57）。
- **legacy wixl 链路**（`LinkX.wxs`）不带任何许可文件，其许可页文案已明写"不要用这条链路做公开分发"。

## 三、图标包口径（2026-09-30 由项目所有人确认）

事实（可复核）：`svg/` 下 24 个 SVG 全部带 iconfont.cn 的导出指纹 `p-id="…"` 与 `t="…"`；
`Scripts/svg_assets.py` 以该目录为唯一真源，把路径几何内联进 `Platforms/Windows/src/icons_svg.rs`、
生成安卓侧 24 个 vector drawable，并栅格化进 `LinkX.ico`（嵌入 exe 资源、MSI 的 `Icon` 表与快捷方式）。
**因此这些几何进入了产物，产品标识本身也在其中。**

授权口径：项目所有人（本仓库版权人）确认这些图标取自 **www.iconfont.cn**，按其下载时的站点条款
**可以公开免费使用**，故 `svg/` 随仓库公开分发。逐图标的原作者署名以 iconfont.cn 的条目页为准；
本文件记录来源与"由谁确认"，避免以后把它当成"我们自己画的"。

两点如实说明，不粉饰：

1. 早期文档的旧条目曾写"图标为双端共形的
   手绘矢量、不引入 iconfont"。那只在字面上排除了位图与图标字体，与实际来源不符，已更正。
2. iconfont.cn 的授权条款按站点当期版本执行；若日后发现某个图标不允许再分发，
   处置办法是替换该图标后重跑 `python Scripts/svg_assets.py`（生成物全部可重放），
   而不是去改历史文档。

## 四、依赖整体口径（GPL-3.0 兼容性核查结论）

- **2026-09-30 删除 vendored LocalSend 与未接线的互操作层之后**，`Cargo.lock` 从
  287 个唯一 crate 降到 **160 条 package 记录**，`tokio` / `hyper` / `reqwest` / `rustls`
  **全部为 0**（复核命令：`grep -c '^name = "tokio"$' Cargo.lock`）。
- 现存依赖全部来自 `registry+https://github.com/rust-lang/crates.io-index`，无 git 源、无 `[patch]`。
- **核查强度说明**：按名字初筛 copyleft/弱许可，命中仅 `libsqlite3-sys` + `rusqlite`（MIT）、
  `ring`、`rustls*` / `webpki*` 系（ISC / Apache-2.0 / MIT）。
  `Cargo.lock` 本身**不含 license 字段**，所以这是名字级初筛，不是机器核验。机器核验任选其一：

  ```bash
  cargo install cargo-license && cargo license -w
  cargo install cargo-deny && cargo deny init && cargo deny check bans licenses
  ```

- Y4 加密依赖门禁（`Scripts/check-crypto-audit.sh`）继续禁止 `aes-gcm`、`dtls`、`webrtc-srtp`、
  `tokio-tungstenite`、`zstd`、`mozjpeg`、`axum` 进入依赖图；
  `Scripts/check-release-clean.sh` 另在交付产物字节里禁止 `LocalSend` / `localsend` 串——
  删除之后这条断言更容易成立，也防止将来不小心把它带回来。
