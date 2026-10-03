//! 「关于」页：文案的**单一来源**。
//!
//! 为什么单独一个文件：这一页写什么由产品定，不由渲染代码定；散进 `render.rs` 就得改
//! 绘制代码才能改一句署名。安卓侧的对应物是 `MainActivity.kt` 里的 `AboutLinks`，
//! **两处必须一起改**。
//!
//! 这一页只有"是什么 / 谁做的"三行，没有外链、也不需要打开浏览器的出口（版本与许可证在「功能」页与仓库里看得到）。
#![cfg(windows)]

pub(crate) const APP_NAME: &str = "LinkX";
/// 一句话定位（小字）。
pub(crate) const TAGLINE: &str = "开源的跨端协同效率革命工具";
/// 开发者署名。
pub(crate) const AUTHOR: &str = "开发者：Chaoming";
