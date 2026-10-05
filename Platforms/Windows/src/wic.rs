//! JPEG ⇄ 位图：只用系统自带的 **WIC**（`WindowsCodecs.dll`）。不用 crates.io 的图像解码库是因为体积与依赖
//! 审计红线，而本进程本来就加载了 `WindowsCodecs.dll`（实测 0.58 MB），解码缩略图不再额外增加镜像页。
//! 解码结果一律交给 GDI：`to_dib_section` 把 32bpp BGRA 写进一个 DIB section，渲染层用 `StretchBlt` 贴进格子 ——
//! 与图标那套 `AlphaBlend` 路径同源，不开第三条绘制路径。

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::ptr::null_mut;

use windows::Win32::Graphics::Gdi::{
    CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP,
    HGDIOBJ,
};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppBGRA, IWICImagingFactory,
    WICBitmapDitherTypeNone, WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand, WICRect,
};
// 编码侧 API 只服务走查预览态
#[cfg(debug_assertions)]
use windows::Win32::Graphics::Imaging::{
    GUID_ContainerFormatJpeg, IWICBitmapFrameEncode, WICBitmapEncoderCacheInMemory,
};
#[cfg(debug_assertions)]
use windows::Win32::System::Com::StructuredStorage::{CreateStreamOnHGlobal, GetHGlobalFromStream};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
#[cfg(debug_assertions)]
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::UI::Shell::SHCreateMemStream;

// 本线程 COM 是否已初始化（**只初始化、不卸载**：调用方是活到进程结束的 UI 线程）。绝不能照抄"返回 ok 就配对
// CoUninitialize"：已初始化时 `CoInitializeEx` 返回的 `S_FALSE` 也是 `is_ok()`，配对卸载会把本模块这次一起减掉，
// 之后每个 WIC 调用都以 `CO_E_NOTINITIALIZED` 失败 —— 表现是"缩略图一开始好好的，弹过一次文件夹对话框就全空"
thread_local! {
    static COM_INIT: Cell<bool> = const { Cell::new(false) };
    /// 工厂按线程缓存：COM 套间是线程的，跨线程复用句柄就是未定义行为
    static FACTORY: RefCell<Option<IWICImagingFactory>> = const { RefCell::new(None) };
}

pub(crate) fn ensure_com() -> Result<(), String> {
    if COM_INIT.with(|c| c.get()) {
        return Ok(());
    }
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if hr.is_ok() {
        COM_INIT.with(|c| c.set(true));
        Ok(())
    } else {
        Err(format!("COM 初始化失败: {hr}"))
    }
}

fn factory() -> Result<IWICImagingFactory, String> {
    ensure_com()?;
    if let Some(f) = FACTORY.with(|f| f.borrow().clone()) {
        return Ok(f);
    }
    let created: IWICImagingFactory = unsafe {
        CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| format!("创建 WIC 图像工厂失败: {e}"))?
    };
    FACTORY.with(|f| *f.borrow_mut() = Some(created.clone()));
    Ok(created)
}

/// 解码后的一次位图（32bpp BGRA，自上而下，行距 4 字节对齐 = `width * 4`）
pub(crate) struct Bitmap {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixels: Vec<u8>,
}

/// JPEG 字节 → 32bpp BGRA。失败原因必须原样带到格子上：只写"显示不出来"用户就没有任何可行动信息。
pub(crate) fn decode_jpeg(jpeg: &[u8]) -> Result<Bitmap, String> {
    if jpeg.is_empty() {
        return Err("缩略图数据是空的".to_string());
    }
    let fac = factory()?;
    // SHCreateMemStream 把字节包成只读 IStream：全程不碰磁盘
    let stream = unsafe { SHCreateMemStream(Some(jpeg)) }.ok_or("建立内存流失败")?;
    unsafe {
        let decoder = fac
            .CreateDecoderFromStream(&stream, core::ptr::null(), WICDecodeMetadataCacheOnDemand)
            .map_err(|e| format!("JPEG 解码器创建失败: {e}"))?;
        let frame = decoder
            .GetFrame(0)
            .map_err(|e| format!("取第一帧失败: {e}"))?;
        let converter = fac
            .CreateFormatConverter()
            .map_err(|e| format!("创建像素格式转换器失败: {e}"))?;
        converter
            .Initialize(
                &frame,
                &GUID_WICPixelFormat32bppBGRA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
            .map_err(|e| format!("转 32bpp BGRA 失败: {e}"))?;
        let mut width = 0u32;
        let mut height = 0u32;
        converter
            .GetSize(&mut width, &mut height)
            .map_err(|e| format!("取图像尺寸失败: {e}"))?;
        if width == 0 || height == 0 {
            return Err("JPEG 声明的尺寸是 0".to_string());
        }
        // **先验尺寸，再分配**：宽高是 JPEG 自己声明的，声明 20000×20000 就要按 1.6 GB 开内存，
        // 而这次分配发生在渲染层的字节上限统计之前，那套 8 MB 封顶管不到它
        check_decode_size(width, height)?;
        let stride = width * 4;
        let mut pixels = vec![0u8; (stride * height) as usize];
        let rect = WICRect {
            X: 0,
            Y: 0,
            Width: width as i32,
            Height: height as i32,
        };
        converter
            .CopyPixels(&rect, stride, &mut pixels)
            .map_err(|e| format!("读出像素失败: {e}"))?;
        Ok(Bitmap {
            width,
            height,
            stride,
            pixels,
        })
    }
}

/// 解码前对 JPEG **自报尺寸**的闸门：相册缩略图长边最多 512（手机侧 `MAX_EDGE`），这里留 8 倍余量，超了
/// 就说明来的不是缩略图。乘法用 `u64`：`u32` 的 `宽×高×4` 在 release 下回绕成很小的数，会分配出一块装不下
/// 像素的缓冲 —— 那比 OOM 更糟。
fn check_decode_size(width: u32, height: u32) -> Result<(), String> {
    const MAX_DECODE_BYTES: u64 = 8 * 1024 * 1024;
    const MAX_DECODE_EDGE: u32 = 4096;
    if width > MAX_DECODE_EDGE || height > MAX_DECODE_EDGE {
        return Err(format!(
            "缩略图声明的尺寸过大（{width}×{height}），已拒绝解码"
        ));
    }
    let px = u64::from(width) * u64::from(height) * 4;
    if px > MAX_DECODE_BYTES {
        return Err(format!(
            "缩略图声明的尺寸过大（{width}×{height} ≈ {} MB），已拒绝解码",
            px / 1048576
        ));
    }
    Ok(())
}

/// 解码结果 → 32bpp DIB section。**调用方负责 `DeleteObject`**（渲染层给它配了有上限的缓存）。
pub(crate) fn to_dib_section(b: &Bitmap) -> Result<HBITMAP, String> {
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: b.width as i32,
            // 负高 = 自上而下，与 WIC 的行序一致，省掉翻行
            biHeight: -(b.height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = null_mut();
    let Ok(hbmp) = (unsafe { CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) })
    else {
        return Err("创建 DIB section 失败".to_string());
    };
    if bits.is_null() {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(hbmp.0));
        }
        return Err("DIB section 没有给出像素指针".to_string());
    }
    let total = (b.stride * b.height) as usize;
    unsafe {
        // 源与目标同为 BGRA 32bpp、行距同为 width*4 → 一次整体拷贝
        std::ptr::copy_nonoverlapping(b.pixels.as_ptr(), bits as *mut u8, total);
    }
    Ok(hbmp)
}

/// 纯色 JPEG 编码 —— **只有设计走查预览态用**：没有手机也要让格子显示真 JPEG，把「WIC 解码 → DIB → StretchBlt」
/// 这条真路径在截图里走一遍。刻意不走 `InitializeFromFilename`/临时文件：预览同样不允许留下缩略图的磁盘残留
#[cfg(debug_assertions)]
pub(crate) fn solid_jpeg(width: u32, height: u32, rgb: u32) -> Result<Vec<u8>, String> {
    let fac = factory()?;
    let (r, g, b) = ((rgb >> 16) & 0xFF, (rgb >> 8) & 0xFF, rgb & 0xFF);
    let stride = width * 4;
    let mut pixels = vec![0u8; (stride * height) as usize];
    for px in pixels.chunks_exact_mut(4) {
        px[0] = b as u8;
        px[1] = g as u8;
        px[2] = r as u8;
        px[3] = 255;
    }
    unsafe {
        let src = fac
            .CreateBitmapFromMemory(
                width,
                height,
                &GUID_WICPixelFormat32bppBGRA,
                stride,
                &pixels,
            )
            .map_err(|e| format!("建内存位图失败: {e}"))?;
        let stream = CreateStreamOnHGlobal(None, true).map_err(|e| format!("建内存流失败: {e}"))?;
        let encoder = fac
            // 第二个参数是"自定义编码器厂商 GUID"，用系统自带的 JPEG 编码器时给 null
            .CreateEncoder(&GUID_ContainerFormatJpeg, core::ptr::null())
            .map_err(|e| format!("建 JPEG 编码器失败: {e}"))?;
        encoder
            .Initialize(&stream, WICBitmapEncoderCacheInMemory)
            .map_err(|e| format!("编码器初始化失败: {e}"))?;
        let mut frame: Option<IWICBitmapFrameEncode> = None;
        encoder
            .CreateNewFrame(&mut frame, null_mut())
            .map_err(|e| format!("新建编码帧失败: {e}"))?;
        let frame = frame.ok_or("编码器没给出帧")?;
        frame
            .Initialize(None)
            .map_err(|e| format!("编码帧初始化失败: {e}"))?;
        frame
            .SetSize(width, height)
            .map_err(|e| format!("设置编码尺寸失败: {e}"))?;
        let mut fmt = GUID_WICPixelFormat32bppBGRA;
        frame
            .SetPixelFormat(&mut fmt)
            .map_err(|e| format!("设置像素格式失败: {e}"))?;
        frame
            .WriteSource(&src, core::ptr::null())
            .map_err(|e| format!("写入像素失败: {e}"))?;
        frame.Commit().map_err(|e| format!("提交编码帧失败: {e}"))?;
        encoder
            .Commit()
            .map_err(|e| format!("提交编码流失败: {e}"))?;
        let hglobal =
            GetHGlobalFromStream(&stream).map_err(|e| format!("取回编码内存失败: {e}"))?;
        let len = GlobalSize(hglobal);
        let p = GlobalLock(hglobal) as *const u8;
        if len == 0 || p.is_null() {
            // 不手动 GlobalFree：流是按 fDeleteOnRelease=TRUE 建的，这块内存归它所有，
            // 流 drop 时还要再放一次 —— 我们放手就是双重释放（堆破坏）。只把锁解开。
            if !p.is_null() {
                let _ = GlobalUnlock(hglobal);
            }
            return Err("编码结果取不回来（0 字节或内存上锁失败）".to_string());
        }
        let out = std::slice::from_raw_parts(p, len).to_vec();
        // 解锁失败无可为：数据已经拷进 out 了，这里不值得为 BOOL 分支
        let _ = GlobalUnlock(hglobal);
        if out.len() < 4 || out[..2] != [0xFF, 0xD8] {
            return Err("编码器给出的不是 JPEG（缺 SOI 标记）".to_string());
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::check_decode_size;

    /// 内存红线的真正闸门在**分配之前**：解码之后才记账的 8 MB 封顶挡不住自报天大尺寸的 JPEG。
    #[test]
    fn oversized_thumbnail_is_refused_before_allocating() {
        // 正常缩略图（手机侧长边上限 512）必须一路放行
        assert!(check_decode_size(512, 512).is_ok());
        assert!(check_decode_size(256, 341).is_ok());
        // 单边超限
        assert!(check_decode_size(20000, 8).is_err(), "单边超限必须拒绝");
        // 每边都不超 4096，但乘起来是 256 MB：这条也要挡住
        let err = check_decode_size(4096, 4096);
        assert!(err.is_err(), "总面积超限必须拒绝（u32 相乘会回绕成小数字）");
        assert!(err.unwrap_err().contains("MB"));
        // 0 尺寸由调用方先挡，这里不该把 0 当成"很大"
        assert!(check_decode_size(0, 0).is_ok());
    }
}
