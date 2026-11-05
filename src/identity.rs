//! How a Herdr run beside the main one (its own `HERDR_APP_HOME` and
//! `HC_SESSION`) tells itself apart: `HERDR_APP_NAME` titles its windows
//! (Alt+Tab, the taskbar, the dock), `HERDR_APP_ID` gives it its own taskbar
//! button (Windows AppUserModelID, Linux app_id) and `HERDR_APP_ICON` (a
//! Windows .ico) its own icon. Unset, it's plain "Herdr", as before.

use gpui::{App, SharedString, Window};

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// The windows' title.
pub fn name() -> SharedString {
    env("HERDR_APP_NAME").unwrap_or_else(|| "Herdr".into()).into()
}

/// Linux: the windows' app_id, which picks their .desktop file and icon.
pub fn app_id() -> String {
    env("HERDR_APP_ID").unwrap_or_else(|| "herdr-app".into())
}

/// Before the first window opens. Windows: an explicit AppUserModelID only
/// with `HERDR_APP_ID`: pinned Herdr.lnk shortcuts carry none, so giving every
/// Herdr one would split them from their windows.
#[cfg(windows)]
pub fn init(cx: &App) {
    if let Some(id) = env("HERDR_APP_ID") {
        cx.set_app_identity(&id, &name());
    }
}

#[cfg(not(windows))]
pub fn init(_: &App) {}

/// Each window, as it opens. Windows: `HERDR_APP_ICON` replaces the icon
/// built into the program (title bar, taskbar, Alt+Tab).
#[cfg(windows)]
pub fn dress(window: &Window) {
    let Some(icon) = env("HERDR_APP_ICON") else { return };
    let path = icon_path(&icon);
    if let Err(e) = win::set_icon(window, &path) {
        tracing::warn!("HERDR_APP_ICON {}: {e}", path.display());
    }
}

#[cfg(not(windows))]
pub fn dress(_: &Window) {}

/// Absolute, or relative to the app's home (as sound files are).
#[cfg_attr(not(windows), allow(dead_code))]
fn icon_path(icon: &str) -> std::path::PathBuf {
    let path = std::path::Path::new(icon);
    if path.is_absolute() { path.to_path_buf() } else { crate::mods::app_dir().join(path) }
}

#[cfg(windows)]
mod win {
    use gpui::Window;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_LOADFROMFILE, LoadImageW, SM_CXICON, SM_CXSMICON, SendMessageW, WM_SETICON,
    };

    pub(super) fn set_icon(window: &Window, path: &Path) -> std::io::Result<()> {
        match HasWindowHandle::window_handle(window).map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => set_hwnd_icon(h.hwnd.get() as HWND, path),
            _ => Err(std::io::Error::other("no window handle")),
        }
    }

    /// Both sizes from the file, each at the size Windows draws it. The window
    /// keeps them for the life of the app, so they're never freed.
    pub(super) fn set_hwnd_icon(hwnd: HWND, path: &Path) -> std::io::Result<()> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        for (which, metric) in [(ICON_BIG, SM_CXICON), (ICON_SMALL, SM_CXSMICON)] {
            let size = unsafe { GetSystemMetrics(metric) };
            // SAFETY: `wide` is a null-terminated path that outlives the call.
            let icon = unsafe { LoadImageW(std::ptr::null_mut(), wide.as_ptr(), IMAGE_ICON, size, size, LR_LOADFROMFILE) };
            if icon.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            unsafe { SendMessageW(hwnd, WM_SETICON, which as usize, icon as isize) };
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_icon_is_in_the_app_home() {
        assert_eq!(icon_path("icon.ico"), crate::mods::app_dir().join("icon.ico"));
        let absolute = if cfg!(windows) { r"C:\icons\gui.ico" } else { "/icons/gui.ico" };
        assert_eq!(icon_path(absolute), std::path::PathBuf::from(absolute));
    }

    /// A hidden window takes both icons from the file; a missing file says so
    /// and leaves the window's icons alone.
    #[cfg(windows)]
    #[test]
    fn the_icon_file_becomes_the_windows_icons() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, ICON_BIG, ICON_SMALL, SendMessageW, WM_GETICON, WS_OVERLAPPEDWINDOW};
        let icons = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/icons");
        let class: Vec<u16> = "STATIC".encode_utf16().chain([0]).collect();
        let hwnd = unsafe {
            CreateWindowExW(0, class.as_ptr(), std::ptr::null(), WS_OVERLAPPEDWINDOW, 0, 0, 100, 100, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null())
        };
        assert!(!hwnd.is_null());
        let icon = |which| unsafe { SendMessageW(hwnd, WM_GETICON, which as usize, 0) };
        assert!(win::set_hwnd_icon(hwnd, &icons.join("missing.ico")).is_err());
        assert_eq!((icon(ICON_BIG), icon(ICON_SMALL)), (0, 0));
        win::set_hwnd_icon(hwnd, &icons.join("herdr-app.ico")).unwrap();
        assert!(icon(ICON_BIG) != 0 && icon(ICON_SMALL) != 0);
        unsafe { DestroyWindow(hwnd) };
    }
}
