//! linkx_core 公共面（导出为 `linkx_core.{dll,so}`）。
//!
//! 这里只留**版本常量**：它是跨端版本号的单一真源（Windows 页脚 / Android `versionName` /
//! 安装包名都取它）。C ABI 那套句柄 + TX 事件队列没有生产调用方，已撤除——Android 走 JNI
//! 导出（`feature = "jni"`），Windows 直接调 Rust 侧，两端都不经过 C ABI。

/// Core 版本（单一真源：Cargo.toml `version`），与 Android `versionName` / Windows 版本条同源。
pub const LINKX_FFI_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Android JNI 导出（feature="jni"，构建 linkx_core.so 时启用）
#[cfg(feature = "jni")]
mod jni_bridge;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_matches_cargo_package_version() {
        assert_eq!(LINKX_FFI_VERSION, env!("CARGO_PKG_VERSION"));
    }
}
