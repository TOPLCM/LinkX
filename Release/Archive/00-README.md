# Release 归档台账

本目录存放**已退役版本**的交付登记。布局对每个版本都一样：

```text
Archive/
  v<版本>/
    Windows/LinkX-<版本>-x64.msi          安装包本体
    Windows/LinkX-<版本>-x64.msi.sha256   出包时算出的校验值
    Android/LinkX-<版本>-release.apk
    Android/LinkX-<版本>-release.apk.sha256
```

**安装包本体就在这个目录里，跟着仓库一起发布**（`Release/Archive/**/LinkX-*.idsig` 除外：那是
apksigner 的中间产物，用户用不上）。想拿某一版的包，`git clone` 后直接进对应文件夹取，或者到
Releases 页面下载 —— **一个版本一条**（v0.1.0 到 v0.4.5 各一条，v0.5.0、v0.5.1 也各在自己的条目）。

这些历史标签指向的是"把这一版的包放进入库"的那次提交，不是当年的源码提交：公开仓库的提交历史是
整理过的，当年的逐版提交没有一并搬过来。要复核某版到底装了什么，以这条台账的大小与 SHA-256 为准。
比对方法见 [`../README.md`](../README.md) 里「校验一个包」那节。

规则（与 [`../README.md`](../README.md) 一致）：

- 每次出新版，把上一版整体移入 `Archive/v<版本>/`，**保留原始文件名**，不重命名、不重打包；
- 每个版本在本文件按平台各登记一行：版本 / 文件 / 大小 / SHA-256；
- 调试构建（`*-debug.apk`）不进归档，只留在本机 `Release/Debug/`。

## 逐版台账

| 版本 | 平台 | 文件 | 大小 (bytes) | SHA-256 |
|---|---|---|---|---|
| 0.1.0 | Windows | `LinkX-0.1.0-x64.msi` | 457,216 | `bef55560ac250f89c9c9dc491378cdda015384fee34a28b4c9a098c425d2f41f` |
| 0.1.0 | Android | `LinkX-0.1.0-m1-release.apk` | 1,837,246 | `bb067c863fd5f488773b5e2db00ef10dc9ee27db538b252bfc62e807774d061a` |
| 0.2.0 | Windows | `LinkX-0.2.0-x64.msi` | 539,648 | `ce957cd08ba39adbf82917187ea593c505f0077b826af6beaa3e254ef608b262` |
| 0.2.0 | Android | `LinkX-0.2.0-release.apk` | 2,029,814 | `c060183ab00d55469312967fc56acd8a3db2e5d755baf7638d23386cc6459e49` |
| 0.3.0 | Windows | `LinkX-0.3.0-x64.msi` | 598,016 | `b7ecd5b9efd645f9824b5bfd2a9e5a011ae5a7048cf7e7b3bbc7a0f2fa75e05e` |
| 0.3.0 | Android | `LinkX-0.3.0-release.apk` | 2,300,150 | `a3194be837141be3aa5088dc39c2b163d9f7fa3fcdc1850ed0c4f5447a3406bc` |
| 0.4.0 | Windows | `LinkX-0.4.0-x64.msi` | 585,728 | `ddbaed0935b90ec26cf934cf5f64d4475e66a49db293442f1b200ef0d8b47a5b` |
| 0.4.0 | Android | `LinkX-0.4.0-release.apk` | 2,378,926 | `b328e63f0daf2c2e00f7877e80cc5dc4caea52875b950d0057ddc635ad1fb932` |
| 0.4.1 | Windows | `LinkX-0.4.1-x64.msi` | 593,920 | `b05e0c39e20e0b62bef13fd7084a4063cdb3419bc451614c4e6ec0130a739fb8` |
| 0.4.1 | Android | `LinkX-0.4.1-release.apk` | 2,391,214 | `7adb83afe49e5cf271c95d3b111e23c5b2e22dd7e52b5388d3c9250208c9c842` |
| 0.4.2 | Windows | `LinkX-0.4.2-x64.msi` | 618,496 | `c970b9e0310d845d2923698ae48e9ca478af02289daa00da3143ed48116336b4` |
| 0.4.2 | Android | `LinkX-0.4.2-release.apk` | 2,424,038 | `530f8ab114805da54666152a5dc45f405989989b19fc932ed4b2d4d9dc4ade2b` |
| 0.4.3 | Windows | `LinkX-0.4.3-x64.msi` | 618,496 | `af1167b5eec968d74dd8ab8c1c2a7bdd5a447412b5f55e4cfce73b7083b94c1f` |
| 0.4.3 | Android | `LinkX-0.4.3-release.apk` | 2,424,038 | `022cf3ddad78c2841e9824794d20c91b07ec2b68df25a5a869c3193fc092c04a` |
| 0.4.4 | Windows | `LinkX-0.4.4-x64.msi` | 626,688 | `96db5f96979e9a5a7c6e559dcb57f0c80f90f9da6f5ee134d5216e87f7dc4194` |
| 0.4.4 | Android | `LinkX-0.4.4-release.apk` | 2,424,038 | `d99561dfbdf726e9808346cf9a393b6bf77d3117d3bd3e6c33858bd626ed5ab2` |
| 0.4.5 | Windows | `LinkX-0.4.5-x64.msi` | 630,784 | `567ac6775dcdf4caef1e1e9222fda2dd7a6b5ebad9118bc359d5ac12307226af` |
| 0.4.5 | Android | `LinkX-0.4.5-release.apk` | 2,428,134 | `26462226f016ab9ff73acf9c1e81b20417e58338a4bc4e0aa747c0d0da056275` |
| 0.5.0 | Windows | `LinkX-0.5.0-x64.msi` | 626,688 | `cb9bebdc02f67719c4ce424f250d89e8749301de0b2252abb5237632979e8875` |
| 0.5.0 | Android | `LinkX-0.5.0-release.apk` | 2,407,710 | `16383f369fa61adfd2e3e508034047d1bf94e50bc742860176c64227acf1e56b` |

## 当前版本

0.5.1 的产物与校验值见 [`../README.md`](../README.md)；逐版改了什么见
[`../../CHANGELOG.md`](../../CHANGELOG.md)。
