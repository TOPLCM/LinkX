//! 主题与运行期环境：DPI / 明暗 / 系统字体。三件事在这里收口，其余模块只消费结果：
//! - **DPI**：从 `GetDpiForWindow` 取缩放比，版式尺寸按 `scale` 换算（整数逻辑像素 → 物理像素），高分屏不再糊；
//! - **明/暗**：`Theme::System` 时读注册表 `AppsUseLightTheme`（Win10 1809+ / Win11 的「应用模式」）跟随系统，也可强制；
//! - **字体**：从 `SPI_GETNONCLIENTMETRICS` 的 `lfMessageFont` 取**系统当前 UI 字体**（中文机 YaHei UI / 英文机 Segoe UI），不硬编码字体名。

use std::cell::Cell;
use std::sync::OnceLock;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR,
    DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWINDOWATTRIBUTE,
};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, SystemParametersInfoW, NONCLIENTMETRICSW, SPI_GETNONCLIENTMETRICS,
};

use crate::settings::Theme;

/// 一帧的绘制环境（每帧由 paint 入口现算，成本极低且随 DPI/主题变化自动跟进）
#[derive(Debug, Clone)]
pub(crate) struct Env {
    /// DPI 缩放比（96dpi = 1.0）
    pub scale: f32,
    /// 客户区宽（**物理**像素）；居中弹窗一类"按窗口定位"的版式要用
    pub win_w: i32,
    /// 客户区高（物理像素）
    pub win_h: i32,
    pub dark: bool,
    /// 系统 UI 字体名（NUL 结尾，直接喂 `CreateFontW`）
    pub face: Vec<u16>,
    pub pal: Palette,
}

impl Env {
    /// 逻辑像素 → 物理像素
    pub fn px(&self, logical: i32) -> i32 {
        (logical as f32 * self.scale).round() as i32
    }

    /// 客户区宽（**逻辑**像素）：`win_w` 是物理像素，版式一律按逻辑算。
    /// 这条换算只留这一份——各处手写一遍，迟早有一处漏掉 `round`
    pub fn logical_w(&self) -> i32 {
        (self.win_w as f32 / self.scale).round() as i32
    }

    /// 客户区高（逻辑像素），换算口径同 [`Env::logical_w`]
    pub fn logical_h(&self) -> i32 {
        (self.win_h as f32 / self.scale).round() as i32
    }
}

/// 调色板（明/暗两套；色值统一 0xRRGGBB，绘制时再转 COLORREF）
#[derive(Debug, Clone, Copy)]
pub(crate) struct Palette {
    pub nav_bg: u32,
    pub nav_sel_bg: u32,
    pub nav_hover_bg: u32,
    pub nav_text: u32,
    pub nav_text_sel: u32,
    pub nav_text_dim: u32,
    pub nav_brand_sub: u32,
    pub body_bg: u32,
    pub card_bg: u32,
    pub body_text: u32,
    pub sub_text: u32,
    pub divider: u32,
    pub foot_text: u32,
    pub accent: u32,
    pub accent_dim: u32,
    pub sel_bg: u32,
    pub row_hover_bg: u32,
    pub btn_bg: u32,
    pub btn_text: u32,
    pub ok_text: u32,
    pub err_text: u32,
}

/// 浅色：浅灰侧栏 + 白底内容区（真正的"明亮模式"，不是深色侧栏硬套浅色正文）
const LIGHT: Palette = Palette {
    nav_bg: 0xEFF1F5,
    nav_sel_bg: 0xE1EBFA,
    nav_hover_bg: 0xE4E8EE,
    nav_text: 0x232830,
    nav_text_sel: 0x1A73D9,
    nav_text_dim: 0x5F6774,
    nav_brand_sub: 0x8A909B,
    body_bg: 0xF6F7F9,
    card_bg: 0xFFFFFF,
    body_text: 0x1A1C1F,
    sub_text: 0x5B6270,
    divider: 0xE3E7EC,
    foot_text: 0x8A909B,
    accent: 0x3399F5,
    accent_dim: 0x1A73D9,
    sel_bg: 0xDCEBFF,
    row_hover_bg: 0xEDF1F6,
    btn_bg: 0xE2E5EA,
    btn_text: 0x1A1C1F,
    ok_text: 0x1E9E55,
    err_text: 0xD03030,
};

/// 深色（Win11 深色观感的近似：近黑蓝灰底 + 提亮强调色）
const DARK: Palette = Palette {
    nav_bg: 0x161A20,
    nav_sel_bg: 0x232A33,
    nav_hover_bg: 0x1D232B,
    nav_text: 0xEDF1F7,
    nav_text_sel: 0xFFFFFF,
    nav_text_dim: 0x9AA3B0,
    nav_brand_sub: 0x79828F,
    body_bg: 0x1B1F25,
    card_bg: 0x232830,
    body_text: 0xE9EDF3,
    sub_text: 0x9BA4B1,
    divider: 0x323844,
    foot_text: 0x7C8593,
    accent: 0x4AA8FF,
    accent_dim: 0x6CBCFF,
    sel_bg: 0x1E3A5C,
    row_hover_bg: 0x272D36,
    btn_bg: 0x2C333D,
    btn_text: 0xE9EDF3,
    ok_text: 0x4CD07E,
    err_text: 0xFF6B6B,
};

fn palette(dark: bool) -> Palette {
    if dark {
        DARK
    } else {
        LIGHT
    }
}

/// 版式断言专用的 `Env`：不碰窗口句柄、不读系统设置。`win_w/win_h` 与生产口径一致，填**物理**像素；缩放由 `scale` 显式给
#[cfg(test)]
pub(crate) fn test_env(win_w: i32, win_h: i32, scale: f32) -> Env {
    Env {
        scale,
        win_w,
        win_h,
        dark: false,
        face: vec![0],
        pal: palette(false),
    }
}

/// 像素走查专用的 `Env`：字体与调色板都取生产那一份。`test_env` 故意不读系统字体，
/// 用它导出的图不能代表真机观感（中文字形都会换脸）。
#[cfg(test)]
pub(crate) fn test_env_pixels(win_w: i32, win_h: i32, scale: f32, dark: bool) -> Env {
    Env {
        scale,
        win_w,
        win_h,
        dark,
        face: system_font_face(),
        pal: palette(dark),
    }
}

/// 这一份主题偏好当前落到明还是暗
pub(crate) fn is_dark(pref: Theme) -> bool {
    match pref {
        Theme::Light => false,
        Theme::Dark => true,
        Theme::System => system_is_dark(),
    }
}

pub(crate) fn detect(hwnd: HWND, pref: Theme) -> Env {
    let dark = is_dark(pref);
    let rc = client_rect(hwnd);
    Env {
        scale: dpi_scale(hwnd),
        win_w: rc.right - rc.left,
        win_h: rc.bottom - rc.top,
        dark,
        face: system_font_face(),
        pal: palette(dark),
    }
}

// ---------- 原生标题栏染色（沉浸式） ----------

/// 把**非客户区**（标题栏 / 边框 / 标题文字）染成当前调色板的颜色。客户区是自绘的，标题栏却归 DWM 画：
/// 不染色时深色模式下它就是一条刺眼的白条。这里走 DWM 属性而不是自己画非客户区 —— 快照、贴边最大化、
/// 键盘可达性全部保留，且一行像素都不多占。
/// `CAPTION_COLOR` / `BORDER_COLOR` 是 Win11 22000+ 才有的属性，Win10 上返回错误码、只剩明暗开关生效；
/// 属性设不上不报错 —— 标题栏颜色是观感问题，不该让它有机会影响主流程
pub(crate) fn apply_caption(hwnd: HWND, pref: Theme) {
    if hwnd.0.is_null() {
        return;
    }
    let dark = is_dark(pref);
    let p = palette(dark);
    unsafe {
        // Win10 1809 用的是 19，20H1 起改成 20；先试新值，失败再退旧值。
        let flag: i32 = if dark { 1 } else { 0 };
        if set_attr(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &flag).is_err() {
            let _ = set_attr(hwnd, DWMWINDOWATTRIBUTE(19), &flag);
        }
        let (caption, text, border) = (rgb(p.body_bg), rgb(p.body_text), rgb(p.divider));
        let _ = set_attr(hwnd, DWMWA_CAPTION_COLOR, &caption);
        let _ = set_attr(hwnd, DWMWA_TEXT_COLOR, &text);
        let _ = set_attr(hwnd, DWMWA_BORDER_COLOR, &border);
    }
}

/// 调色板的 `0xRRGGBB` → `COLORREF`（`0x00BBGGRR`，字节序反过来）
fn rgb(v: u32) -> COLORREF {
    COLORREF(((v & 0xFF) << 16) | (v & 0xFF00) | ((v >> 16) & 0xFF))
}

unsafe fn set_attr<T>(
    hwnd: HWND,
    attr: DWMWINDOWATTRIBUTE,
    value: &T,
) -> windows::core::Result<()> {
    DwmSetWindowAttribute(
        hwnd,
        attr,
        value as *const T as *const core::ffi::c_void,
        std::mem::size_of::<T>() as u32,
    )
}

/// 客户区尺寸（句柄失效/最小化时可能为 0，调用方按 0 兜底即可）
fn client_rect(hwnd: HWND) -> RECT {
    let mut rc = RECT::default();
    if !hwnd.0.is_null() {
        unsafe {
            let _ = GetClientRect(hwnd, &mut rc);
        }
    }
    rc
}

/// 窗口所在显示器的 DPI 缩放比（取不到时按 96dpi）
fn dpi_scale(hwnd: HWND) -> f32 {
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    let dpi = if dpi == 0 { 96 } else { dpi };
    dpi as f32 / 96.0
}

// ---------- 系统深浅色 ----------

thread_local! {
    /// 缓存「系统是否深色」（仅 UI 线程访问）。系统主题变化时由 `invalidate_theme_cache` 失效。
    static DARK_CACHE: Cell<Option<bool>> = const { Cell::new(None) };
}

/// 系统主题缓存失效（收到 `WM_SETTINGCHANGE` / `WM_THEMECHANGED` 时调用）
pub(crate) fn invalidate_theme_cache() {
    DARK_CACHE.with(|c| c.set(None));
}

/// 系统当前是否为深色（读 `AppsUseLightTheme`，0 = 深色；读不到按浅色）
pub(crate) fn system_is_dark() -> bool {
    DARK_CACHE.with(|c| {
        if let Some(v) = c.get() {
            return v;
        }
        let v = read_apps_use_light_theme()
            .map(|light| !light)
            .unwrap_or(false);
        c.set(Some(v));
        v
    })
}

/// 读 `HKCU\...\Themes\Personalize\AppsUseLightTheme`（REG_DWORD；非 0 = 浅色）
fn read_apps_use_light_theme() -> Option<bool> {
    const SUBKEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
    const VALUE: PCWSTR = w!("AppsUseLightTheme");
    let mut data: u32 = 1;
    let mut size = std::mem::size_of::<u32>() as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            SUBKEY,
            VALUE,
            RRF_RT_REG_DWORD,
            None,
            Some(&mut data as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    if rc.is_ok() {
        Some(data != 0)
    } else {
        None
    }
}

// ---------- 系统 UI 字体 ----------

/// 系统 UI 字体候选链（仅在 `SPI_GETNONCLIENTMETRICS` 失败时兜底）
const FACE_FALLBACK: [&str; 3] = ["Microsoft YaHei UI", "Segoe UI", "SimSun"];

/// 系统 UI 字体名（NUL 结尾）。进程内只取一次。
pub(crate) fn system_font_face() -> Vec<u16> {
    static FACE: OnceLock<Vec<u16>> = OnceLock::new();
    FACE.get_or_init(|| query_system_font_face().unwrap_or_else(fallback_face))
        .clone()
}

fn fallback_face() -> Vec<u16> {
    let mut v: Vec<u16> = FACE_FALLBACK[0].encode_utf16().collect();
    v.push(0);
    v
}

/// 取「消息框」字体（`lfMessageFont`）——这正是资源管理器/设置界面的 UI 字体
fn query_system_font_face() -> Option<Vec<u16>> {
    let mut ncm = NONCLIENTMETRICSW {
        cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    unsafe {
        SystemParametersInfoW(
            SPI_GETNONCLIENTMETRICS,
            ncm.cbSize,
            Some(&mut ncm as *mut NONCLIENTMETRICSW as *mut _),
            Default::default(),
        )
        .ok()?;
    }
    let raw = ncm.lfMessageFont.lfFaceName;
    let len = raw.iter().position(|c| *c == 0).unwrap_or(raw.len());
    if len == 0 {
        return None;
    }
    let mut face = raw[..len].to_vec();
    face.push(0);
    Some(face)
}
