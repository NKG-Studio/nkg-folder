#[cfg(windows)]
mod native {
    use eframe::egui;
    use std::path::PathBuf;
    use windows::Win32::{
        Foundation::{HWND, POINT, RECT},
        Graphics::Gdi::ScreenToClient,
        System::Ole::{DROPEFFECT_COPY, DROPEFFECT_MOVE, OleInitialize, OleUninitialize},
        UI::{
            Input::KeyboardAndMouse::{
                GetAsyncKeyState, ReleaseCapture, VK_CONTROL, VK_LBUTTON, VK_SHIFT,
            },
            Shell::SHDoDragDrop,
            WindowsAndMessaging::{
                GA_ROOT, GetAncestor, GetClientRect, GetCursorPos, WindowFromPoint,
            },
        },
    };

    pub fn cursor(window: isize, pixels_per_point: f32) -> Option<(egui::Pos2, bool)> {
        if window == 0 {
            return None;
        }
        let window = HWND(window as *mut _);
        let mut point = POINT::default();
        let mut rect = RECT::default();
        let over_window;
        unsafe {
            GetCursorPos(&mut point).ok()?;
            over_window = GetAncestor(WindowFromPoint(point), GA_ROOT) == window;
            if !ScreenToClient(window, &mut point).as_bool() {
                return None;
            }
            GetClientRect(window, &mut rect).ok()?;
        }
        let inside = over_window
            && point.x >= rect.left
            && point.x < rect.right
            && point.y >= rect.top
            && point.y < rect.bottom;
        Some((
            egui::pos2(
                point.x as f32 / pixels_per_point,
                point.y as f32 / pixels_per_point,
            ),
            inside,
        ))
    }

    pub fn buttons() -> (bool, bool, bool) {
        unsafe {
            (
                GetAsyncKeyState(VK_LBUTTON.0 as i32) < 0,
                GetAsyncKeyState(VK_SHIFT.0 as i32) < 0,
                GetAsyncKeyState(VK_CONTROL.0 as i32) < 0,
            )
        }
    }

    pub fn drag_out(window: isize, paths: &[PathBuf]) -> Result<String, String> {
        struct Ole;
        impl Drop for Ole {
            fn drop(&mut self) {
                unsafe {
                    OleUninitialize();
                }
            }
        }
        let result = (|| -> windows::core::Result<_> {
            unsafe {
                OleInitialize(None)?;
            }
            let _ole = Ole;
            let owner = HWND(window as *mut _);
            let data = crate::shell_menu::data_object(paths, owner)?;
            // The Shell owns the filesystem transfer. Do not delete sources again on MOVE.
            unsafe {
                let _ = ReleaseCapture();
            }
            let effect = unsafe {
                SHDoDragDrop(Some(owner), &data, None, DROPEFFECT_COPY | DROPEFFECT_MOVE)?
            };
            Ok(if effect == DROPEFFECT_MOVE {
                "已交由 Windows 移动文件"
            } else if effect == DROPEFFECT_COPY {
                "已交由 Windows 复制文件"
            } else {
                "已取消系统拖拽"
            }
            .to_string())
        })();
        result.map_err(|e| format!("系统拖拽失败：{e}"))
    }
}

#[cfg(windows)]
pub use native::*;
