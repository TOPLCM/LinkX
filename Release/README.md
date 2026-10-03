# Release 目录约定

这里是**交付物出口**，不是源码。仓库里只跟踪**校验值**，安装包二进制走 Releases 页面分发：
`.gitignore` 排除了 `Release/Windows/*`、`Release/Android/*`、`Release/Debug/*`
与 `Release/Archive/**/LinkX-*.{msi,apk,idsig}`。

```
Release/
  Windows/LinkX-<版本>-x64.msi              当前版本：电脑端安装包（+ .sha256）
  Android/LinkX-<版本>-release.apk          当前版本：手机端交付 APK（+ .idsig / .sha256）
  Debug/                                    历史调试包：本机产物，不入库、不归档
  Archive/v<版本>/                          已退役版本，每版一层
    Windows/LinkX-<版本>-x64.msi.sha256
    Android/LinkX-<版本>-release.apk.sha256
```

## 三条规则

1. **当前版本永远在 `Windows/` 与 `Android/`，且必须晚于最后一次代码改动**。
   产物比代码旧就不算交付。
2. **换版本时把上一版整体移入 `Archive/v<版本>/{Windows,Android}/`**，保留原始文件名与
   `.sha256`，不重命名、不重打包、不删；同时在 `Archive/00-README.md` 登记一行
   （版本 / 日期 / 大小 / SHA-256 / 归档原因）。所有版本同一套分层，不搞"这版分平台、
   那版平铺"。
3. **调试构建（`*-debug.apk`）不归档**，移到 `Release/Debug/` 留着自查；台账里只登记哈希。

## 当前版本 0.5.0 的校验值

| 平台 | 文件 | 大小 (bytes) | SHA-256 |
|---|---|---|---|
| Windows | `LinkX-0.5.0-x64.msi` | 626,688 | `cb9bebdc02f67719c4ce424f250d89e8749301de0b2252abb5237632979e8875` |
| Android | `LinkX-0.5.0-release.apk` | 2,407,710 | `16383f369fa61adfd2e3e508034047d1bf94e50bc742860176c64227acf1e56b` |
| Android | `LinkX-0.5.0-debug.apk`（调试用，不交付） | 10,013,284 | `7e769f06024f22a6651ab6f2df32f5b2ed18c7795f64cd93be257527c5e9ef92` |

> ⚠ **同一版本号 0.5.0 有过两套产物，早先那三行哈希一律作废**（2026-10-02 增补）：
> 0.5.0 第一次内部上线后，能力层那一轮（原编号 0.5.1）改完决定不另发版本号、整支并入主线按
> 0.5.0 发布，于是产物内容变了而版本号没变。历史上登记过的
> `e0e20fd9…`（MSI 638,976 B）与 `9cd7f58f…`（APK 2,428,190 B）**不是当前在表里的这两份**，
> 谁拿旧哈希去核对现在的包都会判"不一致"——那是应该的，它们本来就不是同一个东西。
> 教训单独记：**复用版本号就必须同时声明旧哈希作废**，否则"哈希对不上"会变成一次虚假告警。

> ⚠ **MSI 的哈希每次重打包都会变**（WiX 把打包时间写进产物），它只能核对"同一份文件"，
> 不能当"同一份代码"的指纹。代码层面的对应关系看提交与版本号；APK 在同样输入下可复现。

APK 用自签测试密钥签名（见 `Scripts/build-android-apk.sh`）；正式分发请用你自己的 keystore 重签，
重签后哈希会变，本表随之更新。

## 校验一个包

```powershell
Get-FileHash -Algorithm SHA256 Release\Windows\LinkX-0.5.0-x64.msi
# 与同目录的 .sha256、以及 Releases 页面登记的哈希一致，才算同一个产物
```
