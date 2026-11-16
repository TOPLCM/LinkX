//! 界面图标：一律取自 `svg/` 图标包（唯一真源），经 `Scripts/svg_assets.py` 生成 `icons_svg.rs` 的折线表；
//! 安卓拿同源 pathData 的 VectorDrawable，两端形状与尺寸必然一致。
//! 为什么自带覆盖度光栅器而不是 `PolyPolygon` 直填：GDI 的多边形填充**没有抗锯齿**，而图标包是细描边字形
//! （"连接"的笔画只有 0.9 网格单位 ≈ 1px），26–28px 下斜边阶梯化、笔画忽明忽暗。这里按非零环绕规则做扫描线
//! 覆盖度（4× 超采样 → 每像素 alpha），生成**预乘 ARGB** 位图用 `AlphaBlend` 合成。位图按 (图标, 边长, 颜色) 缓存。

use std::cmp::Ordering;
use std::ffi::c_void;
use std::ptr::null_mut;

use windows::Win32::Graphics::Gdi::{
    AlphaBlend, CreateCompatibleDC, CreateDIBSection, CreatePen, CreateSolidBrush, DeleteObject,
    GetStockObject, SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    BLENDFUNCTION, DIB_RGB_COLORS, HBITMAP, HBRUSH, HDC, HGDIOBJ, HPEN, NULL_BRUSH, PS_SOLID,
};

use crate::icons_svg::{
    ALBUM_PATHS, BATTERY_PATHS, BELL_PATHS, BLOCKS_PATHS, CLIPBOARD_PATHS, DOWNLOAD_PATHS,
    FOLDER_PATHS, INFO_PATHS, LINK_PATHS, MEDIA_PATHS, MUSIC_PATHS, NEXT_PATHS, PAUSE_PATHS,
    PHONE_PATHS, PLAY_PATHS, PREV_PATHS, SEND_PATHS, SETTINGS_PATHS, UPLOAD_PATHS, VOL_DOWN_PATHS,
    VOL_UP_PATHS,
};
use crate::render::colorref;

/// 逻辑网格边长（生成器与这里必须一致）
const GRID: f32 = 24.0;
// 安全活动区的上下界由生成器 `Scripts/svg_assets.py::SAFE_BOX` 决定，
// 这里不重复定义常量 —— 只有校验用的测试需要它，见 tests::SAFE_MIN/MAX。

/// 图标种类（导航/列表/按钮用；与 `svg/` 图标包一一对应）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Icon {
    /// 连接：手机 ⇄ 电脑
    Link,
    Bell,
    Clipboard,
    Folder,
    /// 媒体控制（导航项）
    Media,
    Blocks,
    Settings,
    Phone,
    /// 发送（导出到另一端）
    Send,
    /// 上行：本机 → 手机
    Upload,
    /// 下行：手机 → 本机
    Download,
    /// 相册（图片互传）
    Album,
    /// 关于（侧栏项，图标包「关于.svg」= ⓘ）
    Info,
    Battery,
    /// 音乐（正在播放占位）
    Music,
    Play,
    Pause,
    Prev,
    Next,
    VolUp,
    VolDown,
}

/// 全部变体（只在断言里遍历用，故 `cfg(test)`；生产绘制一律由具体调用点指名）
#[cfg(test)]
pub(crate) const ALL: [Icon; 21] = [
    Icon::Link,
    Icon::Bell,
    Icon::Clipboard,
    Icon::Folder,
    Icon::Media,
    Icon::Blocks,
    Icon::Settings,
    Icon::Phone,
    Icon::Send,
    Icon::Upload,
    Icon::Download,
    Icon::Album,
    Icon::Info,
    Icon::Battery,
    Icon::Music,
    Icon::Play,
    Icon::Pause,
    Icon::Prev,
    Icon::Next,
    Icon::VolUp,
    Icon::VolDown,
];

/// 图标 → 折线表（`Icon` 的每个变体都必须有，漏了就编译不过这里跑不到）
fn paths_of(icon: Icon) -> &'static [&'static [(f32, f32)]] {
    match icon {
        Icon::Link => LINK_PATHS,
        Icon::Bell => BELL_PATHS,
        Icon::Clipboard => CLIPBOARD_PATHS,
        Icon::Folder => FOLDER_PATHS,
        Icon::Media => MEDIA_PATHS,
        Icon::Blocks => BLOCKS_PATHS,
        Icon::Settings => SETTINGS_PATHS,
        Icon::Phone => PHONE_PATHS,
        Icon::Send => SEND_PATHS,
        Icon::Upload => UPLOAD_PATHS,
        Icon::Download => DOWNLOAD_PATHS,
        Icon::Album => ALBUM_PATHS,
        Icon::Info => INFO_PATHS,
        Icon::Battery => BATTERY_PATHS,
        Icon::Music => MUSIC_PATHS,
        Icon::Play => PLAY_PATHS,
        Icon::Pause => PAUSE_PATHS,
        Icon::Prev => PREV_PATHS,
        Icon::Next => NEXT_PATHS,
        Icon::VolUp => VOL_UP_PATHS,
        Icon::VolDown => VOL_DOWN_PATHS,
    }
}

/// 在 (x0, y0) 起、边长 `size` 像素的方框内绘制图标（颜色 `rgb`，带抗锯齿）
///
/// 尺寸一律由调用点给同一个常量（见 `render::NAV_ICON`），这里不再按图标种类微调。
pub(crate) fn draw(hdc: HDC, icon: Icon, x0: i32, y0: i32, size: i32, rgb: u32) {
    if size < 8 {
        return; // 小于 8px 填不出形状，宁可不画也不画成一团墨
    }
    let Some(glyph) = glyph_for(icon, size, rgb) else {
        return;
    };
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255, // 位图已是预乘 alpha，这里不再加常数透明度
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    unsafe {
        let old = SelectObject(glyph.dc, HGDIOBJ(glyph.bmp.0));
        let _ = AlphaBlend(hdc, x0, y0, size, size, glyph.dc, 0, 0, size, size, blend);
        SelectObject(glyph.dc, old);
    }
}

// ---------- 覆盖度光栅（抗锯齿核心）----------

/// 超采样倍数。4× 时每个目标像素由 16 个亚像素行贡献，斜边阶梯在 28px 下已看不出；
/// 再往上只是线性增加一次性开销（结果有缓存）。
const SS: i32 = 4;

/// 非零环绕扫描线 → 每像素覆盖率（0..=255），长度 size*size。横向按 span 与像素列的**重叠长度**精确积分、
/// 纵向按行等权（1/SS）：水平误差为 0、垂直 ≤ 1/16 像素，比"整格计数"细腻得多，而成本只是一次
/// O(行数 × 边数) 的循环 —— 28px 图标约 112 行、几百条边
fn coverage(paths: &[&[(f32, f32)]], size: i32) -> Vec<u8> {
    let n = size * SS;
    let scale = n as f32 / GRID;
    let polys: Vec<Vec<(f32, f32)>> = paths
        .iter()
        .map(|sub| sub.iter().map(|(x, y)| (x * scale, y * scale)).collect())
        .collect();

    let inv = 1.0 / (SS * SS) as f32;
    let mut acc = vec![0f32; (size * size) as usize];
    let mut hits: Vec<(f32, i32)> = Vec::new();
    let mut spans: Vec<(f32, f32)> = Vec::new();
    for sy in 0..n {
        let yc = sy as f32 + 0.5;
        hits.clear();
        for poly in &polys {
            let len = poly.len();
            if len < 3 {
                continue;
            }
            for i in 0..len {
                let (ax, ay) = poly[i];
                let (bx, by) = poly[(i + 1) % len];
                if ay == by {
                    continue; // 水平边不产生穿越
                }
                if (ay <= yc && by > yc) || (by <= yc && ay > yc) {
                    let t = (yc - ay) / (by - ay);
                    hits.push((ax + (bx - ax) * t, if by > ay { 1 } else { -1 }));
                }
            }
        }
        if hits.is_empty() {
            continue;
        }
        hits.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(Ordering::Equal));
        spans.clear();
        let mut wind = 0i32;
        let mut prev: Option<f32> = None;
        for &(xv, d) in &hits {
            if let Some(px) = prev {
                if wind != 0 && xv > px {
                    spans.push((px, xv));
                }
            }
            wind += d;
            prev = Some(xv);
        }
        let ty = (sy / SS) as usize;
        let row = ty * size as usize;
        for &(xa, xb) in &spans {
            let c0 = (xa / SS as f32).floor().max(0.0) as usize;
            let c1 = ((xb / SS as f32).ceil().max(0.0) as usize).min(size as usize);
            for c in c0..c1 {
                let px0 = c as f32 * SS as f32;
                let ov = xb.min(px0 + SS as f32) - xa.max(px0);
                if ov > 0.0 {
                    acc[row + c] += ov * inv;
                }
            }
        }
    }
    acc.iter()
        .map(|v| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)
        .collect()
}

// ---------- 位图缓存（GDI 句柄一律显式管理，绝不逐帧新建）----------

struct Glyph {
    bmp: HBITMAP,
    dc: HDC,
}

thread_local! {
    // 位图按 (图标, 边长, 颜色) 缓存；共享一个内存 DC，贴完即还原选中对象。
    static GLYPHS: std::cell::RefCell<Vec<(u8, i32, u32, Glyph)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static MEM_DC: std::cell::Cell<HDC> = const { std::cell::Cell::new(HDC(null_mut())) };
}

/// 上限：22 个图标 × 2 主题色 × 若干 DPI。超了说明调用点在按帧造新尺寸（比如动画缩放），
/// 那种用法本就不该走缓存 —— 清空重建，宁可掉帧也不能无界增长。
const GLYPH_CAP: usize = 192;

fn glyph_for(icon: Icon, size: i32, rgb: u32) -> Option<Glyph> {
    let key = icon as u8;
    let cached = GLYPHS.with(|g| {
        g.borrow()
            .iter()
            .find(|(k, s, c, _)| *k == key && *s == size && *c == rgb)
            .map(|(_, _, _, gl)| Glyph {
                bmp: gl.bmp,
                dc: gl.dc,
            })
    });
    if let Some(g) = cached {
        return Some(g);
    }
    let built = build_glyph(icon, size, rgb)?;
    GLYPHS.with(|g| {
        let mut cache = g.borrow_mut();
        cache.retain(|(k, s, c, _)| !(*k == key && *s == size && *c == rgb));
        if cache.len() >= GLYPH_CAP {
            for (_, _, _, old) in cache.drain(..) {
                unsafe {
                    let _ = DeleteObject(HGDIOBJ(old.bmp.0));
                }
            }
        }
        cache.push((
            key,
            size,
            rgb,
            Glyph {
                bmp: built.bmp,
                dc: built.dc,
            },
        ));
    });
    Some(built)
}

fn build_glyph(icon: Icon, size: i32, rgb: u32) -> Option<Glyph> {
    let alpha = coverage(paths_of(icon), size);
    let mem = mem_dc()?;
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size,
            // 负高 = 自上而下，省掉后面翻行
            biHeight: -size,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = null_mut();
    let bmp = unsafe { CreateDIBSection(mem, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) }.ok()?;
    if bits.is_null() {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(bmp.0));
        }
        return None;
    }
    // AlphaBlend(AC_SRC_ALPHA) 要求**预乘** alpha：BGRA = (b·a, g·a, r·a, a)
    let (r, g, b) = (rgb >> 16, (rgb >> 8) & 0xFF, rgb & 0xFF);
    let px = bits as *mut u8;
    let total = (size * size) as usize;
    unsafe {
        for i in 0..total {
            let a = *alpha.get_unchecked(i) as u32;
            *px.add(i * 4) = (b * a / 255) as u8;
            *px.add(i * 4 + 1) = (g * a / 255) as u8;
            *px.add(i * 4 + 2) = (r * a / 255) as u8;
            *px.add(i * 4 + 3) = a as u8;
        }
    }
    Some(Glyph { bmp, dc: mem })
}

fn mem_dc() -> Option<HDC> {
    let cur = MEM_DC.get();
    if !cur.is_invalid() {
        return Some(cur);
    }
    let dc = unsafe { CreateCompatibleDC(None) };
    if dc.is_invalid() {
        return None;
    }
    MEM_DC.set(dc);
    Some(dc)
}

// ---------- GDI 对象缓存（与 render 的调色板缓存同思路：绝不逐帧新建）----------

thread_local! {
    static SOLID_CACHE: std::cell::RefCell<Vec<(u32, HBRUSH)>> = const { std::cell::RefCell::new(Vec::new()) };
    static PEN_CACHE: std::cell::RefCell<Vec<(i32, u32, HPEN)>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn brush_for_icon(rgb: u32) -> HBRUSH {
    SOLID_CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        if let Some((_, b)) = cache.iter().find(|(k, _)| *k == rgb) {
            return *b;
        }
        let b = unsafe { CreateSolidBrush(colorref(rgb)) };
        cache.push((rgb, b));
        b
    })
}

/// 实心画刷（渲染层填充复用）
pub(crate) fn brush_solid(rgb: u32) -> HBRUSH {
    brush_for_icon(rgb)
}

/// 实心画笔（渲染层描边复用）
pub(crate) fn pen_solid(width: i32, rgb: u32) -> HPEN {
    PEN_CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        if let Some((_, _, p)) = cache.iter().find(|(w, cc, _)| *w == width && *cc == rgb) {
            return *p;
        }
        let p = unsafe { CreatePen(PS_SOLID, width, colorref(rgb)) };
        cache.push((width, rgb, p));
        p
    })
}

/// 空画刷句柄（只描边不填充）
pub(crate) fn null_brush() -> HGDIOBJ {
    unsafe { GetStockObject(NULL_BRUSH) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 与生成器 `SAFE_BOX` 一致：图标包归一化后的活动区
    const SAFE_MIN: f32 = 3.0;
    const SAFE_MAX: f32 = 21.0;

    /// **所有图标视觉尺寸一致**（用户明确要求）：每个图标都必须撑满安全活动区，且不得越界。
    /// 遍历生成器导出的 `ALL_ICON_PATHS`，所以加图标不需要改这里 —— 漏登记才会失败。
    #[test]
    fn every_icon_fills_the_same_safe_box() {
        for (name, paths) in crate::icons_svg::ALL_ICON_PATHS {
            assert!(!paths.is_empty(), "{name} 没有几何（图标包没生成？）");
            let xs: Vec<f32> = paths.iter().flat_map(|s| s.iter().map(|p| p.0)).collect();
            let ys: Vec<f32> = paths.iter().flat_map(|s| s.iter().map(|p| p.1)).collect();
            let (min_x, max_x) = (
                xs.iter().cloned().fold(1e9f32, f32::min),
                xs.iter().cloned().fold(-1e9f32, f32::max),
            );
            let (min_y, max_y) = (
                ys.iter().cloned().fold(1e9f32, f32::min),
                ys.iter().cloned().fold(-1e9f32, f32::max),
            );
            let span = (max_x - min_x).max(max_y - min_y);
            assert!(
                (span - (SAFE_MAX - SAFE_MIN)).abs() < 0.6,
                "{name} 视觉尺寸与其他图标不一致：跨距 {span}，期望 {}",
                SAFE_MAX - SAFE_MIN
            );
            assert!(
                min_x >= SAFE_MIN - 0.6
                    && max_x <= SAFE_MAX + 0.6
                    && min_y >= SAFE_MIN - 0.6
                    && max_y <= SAFE_MAX + 0.6,
                "{name} 越出安全活动区：x {min_x}..{max_x} y {min_y}..{max_y}"
            );
        }
    }

    /// `Icon` 的每个变体都要能取到几何（漏一条 = 画出来是空的）
    #[test]
    fn every_enum_variant_has_geometry() {
        for icon in ALL {
            assert!(
                !paths_of(icon).is_empty(),
                "{icon:?} 在 paths_of 里没接到折线表"
            );
        }
    }

    /// **真渲染校验**：把每个图标画进内存 DIB，导出 BMP 供肉眼比对，
    /// 并断言每个图标都真的"上了墨"。
    ///
    /// 为什么值得为它写一段 unsafe GDI：图标这件事的正确性只在像素上成立。
    /// 只看折线表通过测试、或只看安卓 vector，都不能证明 Windows 自绘壳画得对
    /// （填充规则选错时图形会整块糊掉或完全空白，两种都长得"像没画"）。
    #[test]
    fn render_icons_into_bitmap() {
        use std::ffi::c_void;
        use std::ptr::null_mut;
        use windows::Win32::Foundation::{COLORREF, RECT};
        use windows::Win32::Graphics::Gdi::{
            CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, FillRect, GdiFlush,
            SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HGDIOBJ,
        };

        /// COLORREF 是 0x00BBGGRR（与本项目内部 0xRRGGBB 相反），这里直接按位拼，
        /// 免得再走一次 render::colorref 让人误读。
        fn cref(r: u32, g: u32, b: u32) -> COLORREF {
            COLORREF((b << 16) | (g << 8) | r)
        }

        const CELL: i32 = 42;
        const ICON: i32 = 28; // 与 render::NAV_ICON 同尺寸
        let icons: Vec<(String, Icon)> = ALL.iter().map(|i| (format!("{i:?}"), *i)).collect();
        let w = CELL * icons.len() as i32;
        let h = CELL;
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // 负数 = 自上而下，省掉后面翻行
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        unsafe {
            let hdc = CreateCompatibleDC(None);
            let mut bits: *mut c_void = null_mut();
            let hbmp = CreateDIBSection(hdc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
                .expect("CreateDIBSection");
            let old = SelectObject(hdc, HGDIOBJ(hbmp.0));
            // 浅灰底（与浅色主题列表底接近），图标用深墨色
            let bg = CreateSolidBrush(cref(240, 242, 245));
            let full = RECT {
                left: 0,
                top: 0,
                right: w,
                bottom: h,
            };
            FillRect(hdc, &full, bg);
            let _ = DeleteObject(HGDIOBJ(bg.0));
            for (i, (_, icon)) in icons.iter().enumerate() {
                draw(hdc, *icon, i as i32 * CELL + 7, 7, ICON, 0x1C_1E_22);
            }
            let _ = GdiFlush();
            let px = bits as *mut u8;
            // 统计每格的"墨"像素占比（BGRX，非底色即算墨），并单独数出**半透明过渡像素**
            // —— 后者是抗锯齿是否真的生效的直接证据：GDI 直填时这一项恒为 0。
            let mut report = Vec::new();
            for (i, (name, _)) in icons.iter().enumerate() {
                let mut ink = 0usize;
                let mut soft = 0usize;
                for y in 0..h {
                    for x in 0..ICON {
                        let off = ((y * w + i as i32 * CELL + 7 + x) * 4) as usize;
                        let b = *px.add(off) as i32;
                        let g = *px.add(off + 1) as i32;
                        let r = *px.add(off + 2) as i32;
                        if (r - 240).abs() > 24 || (g - 242).abs() > 24 || (b - 245).abs() > 24 {
                            ink += 1;
                            // 由红通道反解覆盖率：bg=240 → ink=28
                            let a = (240 - r) as f64 / 212.0;
                            if (0.12..0.88).contains(&a) {
                                soft += 1;
                            }
                        }
                    }
                }
                let pct = ink as f64 * 100.0 / (ICON * ICON) as f64;
                let soft_pct = soft as f64 * 100.0 / (ICON * ICON) as f64;
                report.push((name.to_string(), pct, soft_pct));
                assert!(
                    pct > 6.0,
                    "图标 {name} 几乎没有被画出来（覆盖率 {pct:.1}%）"
                );
                assert!(
                    pct < 92.0,
                    "图标 {name} 整块被糊满（覆盖率 {pct:.1}%）—— 填充规则不对"
                );
                assert!(
                    soft_pct > 1.0,
                    "图标 {name} 没有任何半透明过渡像素（{soft_pct:.2}%）—— 抗锯齿没生效，\
                     斜边会阶梯化、细笔画会忽明忽暗"
                );
            }
            // 导出 PPM（P6，无压缩、头极简）供人眼复核。
            // 不用 BMP：BMP 的 DIB 头字段顺序（planes 在 bitCount 之前）写错一次，
            // 看图软件就只会报"unsupported depth"，白耗一轮。
            let mut out: Vec<u8> = Vec::with_capacity(16 + (w * h * 3) as usize);
            out.extend_from_slice(
                format!(
                    "P6
{w} {h}
255
"
                )
                .as_bytes(),
            );
            for j in 0..h {
                for i in 0..w as usize {
                    let off = ((j * w + i as i32) * 4) as usize;
                    out.push(*px.add(off + 2));
                    out.push(*px.add(off + 1));
                    out.push(*px.add(off));
                }
            }
            let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../Temp/svg-preview");
            std::fs::create_dir_all(dir).ok();

            std::fs::write(format!("{dir}/windows-icons-rendered.ppm"), &out).ok();
            SelectObject(hdc, old);
            let _ = DeleteObject(HGDIOBJ(hbmp.0));
            let _ = DeleteDC(hdc);
            eprintln!("墨覆盖率：{report:?}");
        }
    }

    /// 抽稀后仍要够密：单个子路径少于 3 点就填不出面
    #[test]
    fn no_degenerate_subpaths() {
        for icon in ALL {
            for (i, sub) in paths_of(icon).iter().enumerate() {
                assert!(
                    sub.len() >= 3,
                    "{icon:?} 第 {i} 条子路径只有 {} 点",
                    sub.len()
                );
            }
        }
    }
}
