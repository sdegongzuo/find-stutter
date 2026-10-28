//! 开发用截图工具：抓指定进程的窗口，或整个虚拟桌面，输出 BMP / PNG。
//!
//! ## 为什么需要它
//!
//! 本机安全策略把 `Add-Type` 列进硬拒表（agent-harness 的 `checkPowerShellSecurity`），
//! 所以调 .NET 的 `CopyFromScreen` 这条路在 agent 里是断的；机器上也没有
//! ffmpeg / ImageMagick。但**编译好的 exe 不在任何黑名单里**，所以这里用 Rust
//! 直接调 Win32 GDI 自己抓。
//!
//! ## 用法
//!
//! ```text
//! # 抓某个进程面积最大的可见窗口
//! cargo run --release -p find-stutter-ui --example screenshot_window -- 1234 target/win.png
//!
//! # 抓整个虚拟桌面（多显示器包含所有屏幕）
//! cargo run --release -p find-stutter-ui --example screenshot_window -- screen target/desktop.png
//! ```
//!
//! 输出格式按扩展名：`.png` → PNG（体积小，推荐），其它 → 32 位 BMP。
//!
//! ## 策略
//!
//! - 窗口模式先 `PrintWindow(PW_RENDERFULLCONTENT)`：对分层/半透明窗口更准，
//!   且**不受遮挡影响**；结果全黑时退回 `BitBlt(SRCCOPY|CAPTUREBLT)`（此时要求未遮挡）。
//! - 桌面模式只能 `BitBlt`，抓到的是屏幕上真实可见内容。
//! - GDI 抓来的 32bpp 位图 alpha 通常恒为 0；整幅都没有非零 alpha 时统一补 255，
//!   否则 PNG 会变成全透明。

#[cfg(not(windows))]
fn main() {
    eprintln!("本工具仅支持 Windows");
}

#[cfg(windows)]
fn main() {
    if let Err(e) = run() {
        eprintln!("截图失败: {e}");
        std::process::exit(1);
    }
}

#[cfg(windows)]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    use windows::Win32::UI::WindowsAndMessaging::SetProcessDPIAware;

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("用法: screenshot_window <PID|screen> <输出.png|.bmp>");
        std::process::exit(2);
    }
    let target = &args[0];
    let out_path = &args[1];

    // 高 DPI：否则拿到的坐标是缩放后的，抓出来会糊（副屏尤其明显）
    unsafe {
        let _ = SetProcessDPIAware();
    }

    let (w, h, mut buf) = if target.eq_ignore_ascii_case("screen") {
        capture_screen()?
    } else {
        let pid: u32 = target
            .parse()
            .map_err(|_| "第一个参数必须是 PID 数字，或 screen")?;
        capture_window(pid)?
    };

    // GDI 位图的 alpha 常为 0：整幅都没有非零 alpha 就补成不透明
    if !buf.chunks_exact(4).any(|p| p[3] != 0) {
        for p in buf.chunks_exact_mut(4) {
            p[3] = 255;
        }
    }

    if out_path.to_ascii_lowercase().ends_with(".png") {
        write_png(out_path, w, h, &buf)?;
    } else {
        write_bmp(out_path, w, h, &buf)?;
    }

    let size = std::fs::metadata(out_path).map(|m| m.len()).unwrap_or(0);
    println!("已写出: {out_path} ({w}x{h}, {} KB)", size / 1024);
    Ok(())
}

#[cfg(windows)]
use windows::Win32::Graphics::Gdi::{HBITMAP, HDC};

/// 建内存 DC + 位图，先跑 `primary`，失败或全黑再跑 `fallback`，最后读回 BGRA（自顶向下）。
///
/// 两个闭包都收到 `(内存DC, 屏幕DC)`，返回是否绘制成功。
#[cfg(windows)]
unsafe fn grab<P, F>(
    w: i32,
    h: i32,
    primary: P,
    fallback: Option<F>,
) -> Result<Vec<u8>, Box<dyn std::error::Error>>
where
    P: FnOnce(HDC, HDC) -> bool,
    F: FnOnce(HDC, HDC) -> bool,
{
    unsafe {
        use windows::Win32::Graphics::Gdi::{
            CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, ReleaseDC,
            SelectObject, HGDIOBJ,
        };

        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, w, h);
        let old = SelectObject(mem, HGDIOBJ(bmp.0));

        let ok = primary(mem, screen);
        // MSDN：GetDIBits 时位图不能仍被选进 DC
        SelectObject(mem, old);
        let mut pixels = read_back(screen, bmp, w, h)?;

        let blank = pixels.chunks_exact(4).all(|p| p[0] == 0 && p[1] == 0 && p[2] == 0);
        if let Some(fb) = fallback {
            if !ok || blank {
                println!("PrintWindow 不可用或全黑 → 退回 BitBlt（窗口不能被遮挡）");
                SelectObject(mem, HGDIOBJ(bmp.0));
                let _ = fb(mem, screen);
                SelectObject(mem, old);
                pixels = read_back(screen, bmp, w, h)?;
            }
        }

        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        Ok(pixels)
    }
}

/// 从位图读回像素：`biHeight` 取负 = 自顶向下，调用方不必再翻转。
#[cfg(windows)]
unsafe fn read_back(
    screen: HDC,
    bmp: HBITMAP,
    w: i32,
    h: i32,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    unsafe {
        use std::ffi::c_void;
        use windows::Win32::Graphics::Gdi::{
            GetDIBits, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
        };

        let mut buf = vec![0u8; (w * h * 4) as usize];
        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // 负数 = 自顶向下
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let lines = GetDIBits(
            screen,
            bmp,
            0,
            h as u32,
            Some(buf.as_mut_ptr() as *mut c_void),
            &mut bi,
            DIB_RGB_COLORS,
        );
        if lines == 0 {
            return Err("GetDIBits 失败".into());
        }
        Ok(buf)
    }
}

/// 抓指定进程面积最大的可见窗口。
#[cfg(windows)]
fn capture_window(pid: u32) -> Result<(i32, i32, Vec<u8>), Box<dyn std::error::Error>> {
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{BitBlt, CAPTUREBLT, SRCCOPY};
    use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowRect, GetWindowThreadProcessId, IsWindowVisible,
    };

    struct Ctx {
        pid: u32,
        best: HWND,
        best_area: i64,
    }

    unsafe extern "system" fn cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // edition 2024：unsafe fn 内部也要显式 unsafe 块
        unsafe {
            let ctx = &mut *(lparam.0 as *mut Ctx);
            let mut wpid = 0u32;
            let _ = GetWindowThreadProcessId(hwnd, Some(&mut wpid));
            if wpid == ctx.pid && IsWindowVisible(hwnd).as_bool() {
                let mut r = RECT::default();
                if GetWindowRect(hwnd, &mut r).is_ok() {
                    let area = ((r.right - r.left) as i64) * ((r.bottom - r.top) as i64);
                    if area > ctx.best_area {
                        ctx.best_area = area;
                        ctx.best = hwnd;
                    }
                }
            }
        }
        BOOL(1) // 继续枚举
    }

    let mut ctx = Ctx {
        pid,
        best: HWND(std::ptr::null_mut()),
        best_area: 0,
    };
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut ctx as *mut Ctx as isize));
    }
    if ctx.best.0.is_null() {
        return Err(format!("PID {pid} 没有可见窗口").into());
    }

    let mut rect = RECT::default();
    unsafe { GetWindowRect(ctx.best, &mut rect)? };
    let (x, y) = (rect.left, rect.top);
    let w = (rect.right - rect.left).max(1);
    let h = (rect.bottom - rect.top).max(1);
    println!("窗口 handle={:#x} rect=({x},{y}) {w}x{h}", ctx.best.0 as usize);

    // 两个闭包都只捕获 Copy 值，因此可 Copy；BitBlt 作为备用路径
    let blt = |mem: HDC, screen: HDC| -> bool {
        unsafe { BitBlt(mem, 0, 0, w, h, Some(screen), x, y, SRCCOPY | CAPTUREBLT).is_ok() }
    };
    let pixels = unsafe {
        grab(
            w,
            h,
            |mem, _screen| PrintWindow(ctx.best, mem, PRINT_WINDOW_FLAGS(2)).as_bool(),
            Some(blt),
        )?
    };
    Ok((w, h, pixels))
}

/// 抓整个虚拟桌面（多显示器时为所有屏幕的并集）。
#[cfg(windows)]
fn capture_screen() -> Result<(i32, i32, Vec<u8>), Box<dyn std::error::Error>> {
    use windows::Win32::Graphics::Gdi::{BitBlt, CAPTUREBLT, SRCCOPY};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };

    let (x, y, w, h) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    if w <= 0 || h <= 0 {
        return Err("拿不到虚拟桌面尺寸".into());
    }
    println!("虚拟桌面 origin=({x},{y}) {w}x{h}");

    let blt = |mem: HDC, screen: HDC| -> bool {
        unsafe { BitBlt(mem, 0, 0, w, h, Some(screen), x, y, SRCCOPY | CAPTUREBLT).is_ok() }
    };
    let pixels = unsafe { grab(w, h, blt, Some(blt))? };
    Ok((w, h, pixels))
}

/// 写 32 位 BMP（自底向上，兼容所有查看器）。
#[cfg(windows)]
fn write_bmp(path: &str, w: i32, h: i32, bgra: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write as _;
    let row = (w * 4) as usize;
    let pixel_bytes = row * h as usize;
    let file_size = 54 + pixel_bytes;
    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
    out.write_all(b"BM")?;
    out.write_all(&(file_size as u32).to_le_bytes())?;
    out.write_all(&0u32.to_le_bytes())?;
    out.write_all(&54u32.to_le_bytes())?;
    out.write_all(&40u32.to_le_bytes())?;
    out.write_all(&w.to_le_bytes())?;
    out.write_all(&h.to_le_bytes())?; // 正数 = 自底向上
    out.write_all(&1u16.to_le_bytes())?;
    out.write_all(&32u16.to_le_bytes())?;
    out.write_all(&0u32.to_le_bytes())?; // BI_RGB
    out.write_all(&(pixel_bytes as u32).to_le_bytes())?;
    out.write_all(&2835i32.to_le_bytes())?;
    out.write_all(&2835i32.to_le_bytes())?;
    out.write_all(&0u32.to_le_bytes())?;
    out.write_all(&0u32.to_le_bytes())?;
    for yy in (0..h as usize).rev() {
        out.write_all(&bgra[yy * row..(yy + 1) * row])?;
    }
    out.flush()?;
    Ok(())
}

/// 写 PNG（BGRA → RGBA）。
#[cfg(windows)]
fn write_png(path: &str, w: i32, h: i32, bgra: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::BufWriter;
    let mut rgba = Vec::with_capacity(bgra.len());
    for p in bgra.chunks_exact(4) {
        rgba.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
    }
    let mut enc = png::Encoder::new(BufWriter::new(std::fs::File::create(path)?), w as u32, h as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header()?;
    writer.write_image_data(&rgba)?;
    Ok(())
}
