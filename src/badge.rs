//! The badge on the app's icon, which mods set with `setBadge`: the Dock icon
//! on macOS, an overlay on the main window's taskbar button on Windows. Only
//! the app's own process can draw it. Linux keeps no badge for now; the call
//! is accepted and does nothing there.

use std::cell::RefCell;

thread_local! {
    static SHOWN: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Shows `text` on the app icon; `None` or blank clears it. Runs on the UI thread.
pub fn set(text: Option<&str>) {
    let text = text.map(str::trim).filter(|t| !t.is_empty()).map(str::to_owned);
    let changed = SHOWN.with_borrow_mut(|shown| {
        if *shown == text {
            return false;
        }
        shown.clone_from(&text);
        true
    });
    if changed {
        platform::set(text.as_deref());
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;
    use objc2_foundation::NSString;

    pub fn set(text: Option<&str>) {
        let Some(mtm) = MainThreadMarker::new() else {
            tracing::warn!("setBadge off the main thread");
            return;
        };
        let tile = NSApplication::sharedApplication(mtm).dockTile();
        tile.setBadgeLabel(text.map(NSString::from_str).as_deref());
    }
}

/// The main window, as it opens. Windows: the taskbar badges a window, not the
/// app, so the badge needs its handle.
#[cfg(windows)]
pub fn attach(window: &gpui::Window) {
    platform::attach(window);
}

#[cfg(not(windows))]
pub fn attach(_: &gpui::Window) {}

/// Windows: the text drawn as a small overlay icon on the main window's taskbar
/// button (what Teams and Outlook do: desktop apps have no system numeric badge).
/// Shows only with Settings > Personalization > Taskbar > "Show badges on taskbar apps".
#[cfg(windows)]
mod platform {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Gdi::*;
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx};
    use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
    use windows::Win32::UI::Shell::{DefSubclassProc, ITaskbarList3, SetWindowSubclass, TaskbarList};
    use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, DestroyIcon, HICON, ICONINFO, RegisterWindowMessageW, SM_CXSMICON};
    use windows::core::{HSTRING, w};

    static HWND_: AtomicIsize = AtomicIsize::new(0);
    static BUTTON_CREATED: AtomicU32 = AtomicU32::new(0);
    /// The text mods asked for last ("" for none): the taskbar gets it again
    /// whenever it makes the window's button anew.
    static WANTED: Mutex<String> = Mutex::new(String::new());

    pub(super) fn attach(window: &gpui::Window) {
        let Ok(RawWindowHandle::Win32(h)) = HasWindowHandle::window_handle(window).map(|h| h.as_raw()) else { return };
        let hwnd = h.hwnd.get();
        HWND_.store(hwnd, Ordering::SeqCst);
        // The taskbar drops an overlay set before its button exists, and every
        // overlay when Explorer restarts: it says so with this message, so the
        // badge goes back on then.
        BUTTON_CREATED.store(unsafe { RegisterWindowMessageW(w!("TaskbarButtonCreated")) }, Ordering::SeqCst);
        unsafe {
            let _ = SetWindowSubclass(HWND(hwnd as _), Some(watch), 0x4844_4247, 0);
        }
    }

    unsafe extern "system" fn watch(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _id: usize, _data: usize) -> LRESULT {
        if msg != 0 && msg == BUTTON_CREATED.load(Ordering::SeqCst) {
            let wanted = WANTED.lock().unwrap().clone();
            if !wanted.is_empty() {
                show(&wanted);
            }
        }
        unsafe { DefSubclassProc(hwnd, msg, wp, lp) }
    }

    /// Only a changed text gets here (`super::set`), so a mod can call
    /// `setBadge` as often as it likes.
    pub fn set(text: Option<&str>) {
        let label = text.unwrap_or_default();
        label.clone_into(&mut WANTED.lock().unwrap());
        show(label);
    }

    fn show(label: &str) {
        let hwnd = HWND(HWND_.load(Ordering::SeqCst) as _);
        if hwnd.is_invalid() {
            return; // no window yet; `attach` comes first, then the button
        }
        if let Err(e) = overlay(hwnd, label) {
            tracing::warn!("setBadge {label:?}: {e}");
        }
    }

    fn overlay(hwnd: HWND, label: &str) -> windows::core::Result<()> {
        unsafe {
            // Whatever thread a mod runs on: S_FALSE or RPC_E_CHANGED_MODE just mean COM is up.
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let taskbar: ITaskbarList3 = CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER)?;
            taskbar.HrInit()?;
            if label.is_empty() {
                return taskbar.SetOverlayIcon(hwnd, HICON::default(), None);
            }
            let size = GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForWindow(hwnd)).max(16);
            let icon = draw(label, size)?;
            // The taskbar keeps its own copy; the description is what screen readers say.
            let result = taskbar.SetOverlayIcon(hwnd, icon, &HSTRING::from(label));
            let _ = DestroyIcon(icon);
            result
        }
    }

    const RED: (f32, f32, f32) = (220.0, 38.0, 38.0);

    /// A red disc with the label in white, optically centred on its ink and
    /// shrunk until it fits (two characters at most read at 16 px).
    fn draw(label: &str, size: i32) -> windows::core::Result<HICON> {
        let s = size as usize;
        let wide = label.chars().count() > 1;
        let (max_w, max_h) = (size as f32 * if wide { 0.8 } else { 0.62 }, size as f32 * 0.6);
        let mut em = size;
        let (cov, cw, (x0, y0, x1, y1)) = loop {
            let (cov, cw, bounds) = text_mask(label, em, size)?;
            let Some((x0, y0, x1, y1)) = bounds else { break (cov, cw, (0, 0, 0, 0)) };
            if ((x1 - x0) as f32 <= max_w && (y1 - y0) as f32 <= max_h) || em <= 7 {
                break (cov, cw, (x0, y0, x1, y1));
            }
            em -= 1;
        };
        let r = size as f32 / 2.0;
        let dx = (r - (x0 + x1) as f32 / 2.0).round() as i32;
        let dy = (r - (y0 + y1) as f32 / 2.0).round() as i32;
        let ch = cov.len() / cw;
        let mut px = vec![0u32; s * s];
        for y in 0..s {
            for x in 0..s {
                let d = ((x as f32 + 0.5 - r).powi(2) + (y as f32 + 0.5 - r).powi(2)).sqrt();
                let a = (r - d + 0.5).clamp(0.0, 1.0);
                if a == 0.0 {
                    continue;
                }
                let (tx, ty) = (x as i32 - dx, y as i32 - dy);
                let t = if tx >= 0 && ty >= 0 && (tx as usize) < cw && (ty as usize) < ch { cov[ty as usize * cw + tx as usize] } else { 0.0 };
                let mix = |c: f32| (c + (255.0 - c) * t).round() as u32;
                px[y * s + x] = ((a * 255.0).round() as u32) << 24 | mix(RED.0) << 16 | mix(RED.1) << 8 | mix(RED.2);
            }
        }
        unsafe {
            let (color, bits) = dib(size, size)?;
            std::ptr::copy_nonoverlapping(px.as_ptr(), bits, px.len());
            let mask_bits = vec![0u8; s.div_ceil(16) * 2 * s];
            let mask = CreateBitmap(size, size, 1, 1, Some(mask_bits.as_ptr().cast()));
            let info = ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
            let icon = CreateIconIndirect(&info);
            let _ = DeleteObject(color.into());
            let _ = DeleteObject(mask.into());
            icon
        }
    }

    /// The label in white on black at `em` px (grayscale anti-aliasing, so a
    /// pixel's level is its ink coverage), with the ink's bounds.
    #[allow(clippy::type_complexity)]
    fn text_mask(label: &str, em: i32, size: i32) -> windows::core::Result<(Vec<f32>, usize, Option<(i32, i32, i32, i32)>)> {
        let (w, h) = (size * 3, size * 2);
        unsafe {
            let dc = CreateCompatibleDC(None);
            let (bmp, bits) = dib(w, h)?;
            let old_bmp = SelectObject(dc, bmp.into());
            let font = CreateFontW(
                -em, 0, 0, 0, FW_BOLD.0 as i32, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS,
                ANTIALIASED_QUALITY, (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32, w!("Segoe UI"),
            );
            let old_font = SelectObject(dc, font.into());
            SetTextColor(dc, COLORREF(0x00FF_FFFF));
            SetBkMode(dc, TRANSPARENT);
            let text: Vec<u16> = label.encode_utf16().collect();
            let _ = TextOutW(dc, size / 2, size / 4, &text);
            let _ = GdiFlush();
            let raw = std::slice::from_raw_parts(bits, (w * h) as usize);
            let cov: Vec<f32> = raw.iter().map(|p| ((p >> 16) & 0xFF).max((p >> 8) & 0xFF).max(p & 0xFF) as f32 / 255.0).collect();
            SelectObject(dc, old_font);
            SelectObject(dc, old_bmp);
            let _ = DeleteObject(font.into());
            let _ = DeleteObject(bmp.into());
            let _ = DeleteDC(dc);
            let mut bounds: Option<(i32, i32, i32, i32)> = None;
            for (i, c) in cov.iter().enumerate() {
                if *c > 0.08 {
                    let (x, y) = ((i as i32) % w, (i as i32) / w);
                    bounds = Some(match bounds {
                        None => (x, y, x + 1, y + 1),
                        Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x + 1), y1.max(y + 1)),
                    });
                }
            }
            Ok((cov, w as usize, bounds))
        }
    }

    /// A top-down 32-bit DIB section and its pixels (0xAARRGGBB).
    unsafe fn dib(w: i32, h: i32) -> windows::core::Result<(HBITMAP, *mut u32)> {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let bmp = unsafe { CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0)? };
        Ok((bmp, bits.cast()))
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    pub fn set(_text: Option<&str>) {}
}
