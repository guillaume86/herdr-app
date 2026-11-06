//! The app icon's badge: `setBadge` in mods. macOS draws it on the dock icon,
//! Windows as an overlay on the main window's taskbar button; Linux has no badge
//! here yet, so it does nothing there. Only the app's own process can badge its
//! icon: the Dock ignores a label set from outside.

use gpui_shell::HostValue;

/// What a mod's `setBadge(value)` shows: a string as given, a number as written
/// (0 shows nothing), null or nothing clears. An empty label clears.
pub fn label(value: Option<&HostValue>) -> Result<String, &'static str> {
    match value {
        None | Some(HostValue::Null) => Ok(String::new()),
        Some(HostValue::Str(s)) => Ok(s.clone()),
        Some(HostValue::Number(n)) if !n.is_finite() => Err("setBadge: the number must be finite"),
        Some(HostValue::Number(n)) if *n == 0.0 => Ok(String::new()),
        Some(HostValue::Number(n)) => Ok(n.to_string()),
        Some(_) => Err("setBadge takes a string, a number or null"),
    }
}

/// Shows `label` on the dock icon until the app quits or another call replaces it;
/// an empty label clears it. AppKit only lets the main thread touch the dock tile.
#[cfg(target_os = "macos")]
pub fn set(label: &str) -> Result<(), &'static str> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;
    use objc2_foundation::NSString;

    let mtm = MainThreadMarker::new().ok_or("setBadge: not on the main thread")?;
    let text = (!label.is_empty()).then(|| NSString::from_str(label));
    NSApplication::sharedApplication(mtm).dockTile().setBadgeLabel(text.as_deref());
    Ok(())
}

/// Windows: the label drawn as a small overlay icon on the main window's taskbar
/// button (what Teams and Outlook do: desktop apps have no system numeric badge).
/// Shows only with Settings > Personalization > Taskbar > "Show badges on taskbar apps".
#[cfg(windows)]
pub fn set(label: &str) -> Result<(), &'static str> {
    win::set(label)
}

#[cfg(not(any(target_os = "macos", windows)))]
pub fn set(_label: &str) -> Result<(), &'static str> {
    Ok(())
}

/// The main window, as it opens. Windows: the taskbar badges a window, not the
/// app, so `set` needs its handle.
#[cfg(windows)]
pub fn attach(window: &gpui::Window) {
    win::attach(window);
}

#[cfg(not(windows))]
pub fn attach(_: &gpui::Window) {}

#[cfg(windows)]
mod win {
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
    /// The label mods asked for last, and the one the taskbar shows now.
    static WANTED: Mutex<String> = Mutex::new(String::new());
    static SHOWN: Mutex<Option<String>> = Mutex::new(None);

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
            *SHOWN.lock().unwrap() = None;
            let wanted = WANTED.lock().unwrap().clone();
            if !wanted.is_empty() {
                let _ = show(&wanted);
            }
        }
        unsafe { DefSubclassProc(hwnd, msg, wp, lp) }
    }

    pub(super) fn set(label: &str) -> Result<(), &'static str> {
        *WANTED.lock().unwrap() = label.to_owned();
        show(label)
    }

    /// Mods call `setBadge` on every render: only a new label reaches the taskbar.
    fn show(label: &str) -> Result<(), &'static str> {
        let hwnd = HWND(HWND_.load(Ordering::SeqCst) as _);
        if hwnd.is_invalid() {
            return Ok(()); // no window yet; `attach` comes first, then the button
        }
        let mut shown = SHOWN.lock().unwrap();
        if shown.as_deref() == Some(label) {
            return Ok(());
        }
        overlay(hwnd, label).map_err(|e| {
            tracing::warn!("setBadge {label:?}: {e}");
            "setBadge: the taskbar refused the badge"
        })?;
        *shown = Some(label.to_owned());
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_show_as_given() {
        assert_eq!(label(Some(&HostValue::Str("3".into()))), Ok("3".into()));
        assert_eq!(label(Some(&HostValue::Str("!".into()))), Ok("!".into()));
    }

    #[test]
    fn numbers_show_as_written_and_zero_clears() {
        assert_eq!(label(Some(&HostValue::Number(7.0))), Ok("7".into()));
        assert_eq!(label(Some(&HostValue::Number(2.5))), Ok("2.5".into()));
        assert_eq!(label(Some(&HostValue::Number(0.0))), Ok(String::new()));
    }

    #[test]
    fn null_nothing_and_empty_clear() {
        assert_eq!(label(None), Ok(String::new()));
        assert_eq!(label(Some(&HostValue::Null)), Ok(String::new()));
        assert_eq!(label(Some(&HostValue::Str(String::new()))), Ok(String::new()));
    }

    #[test]
    fn other_values_are_refused() {
        assert!(label(Some(&HostValue::Bool(true))).is_err());
        assert!(label(Some(&HostValue::Number(f64::NAN))).is_err());
        assert!(label(Some(&HostValue::Array(vec![]))).is_err());
    }
}
