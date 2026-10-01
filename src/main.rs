#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod clipboard;
mod drag_drop;
mod files;
mod model;
mod search;
mod shell_menu;
mod theme;

fn config_directory() -> Result<std::path::PathBuf, String> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        use windows::Win32::{
            System::Com::CoTaskMemFree,
            UI::Shell::{FOLDERID_LocalAppData, KF_FLAG_DEFAULT, SHGetKnownFolderPath},
        };
        // Ask Windows for this user's directory; launch-time environment variables
        // must not select a different workspace.
        let directory = unsafe {
            let raw = SHGetKnownFolderPath(&FOLDERID_LocalAppData, KF_FLAG_DEFAULT, None)
                .map_err(|e| format!("无法获取当前用户的本地应用数据目录：{e}"))?;
            let directory = std::ffi::OsString::from_wide(raw.as_wide());
            CoTaskMemFree(Some(raw.0.cast()));
            std::path::PathBuf::from(directory)
        };
        if !directory.is_absolute() {
            return Err("Windows 返回的配置目录不是绝对路径".into());
        }
        Ok(directory.join("NkgFolder"))
    }
    #[cfg(not(windows))]
    Err("此程序仅支持 Windows 配置目录".into())
}

fn main() -> eframe::Result {
    let config_directory = match config_directory() {
        Ok(path) => path,
        Err(error) => {
            rfd::MessageDialog::new()
                .set_title("NKG Folder — 配置目录错误")
                .set_description(error)
                .set_level(rfd::MessageLevel::Error)
                .show();
            return Ok(());
        }
    };
    eframe::run_native(
        "NKG Folder",
        eframe::NativeOptions {
            viewport: eframe::egui::ViewportBuilder::default()
                .with_icon(
                    eframe::icon_data::from_png_bytes(include_bytes!("../assets/app-icon.png"))
                        .expect("embedded application icon"),
                )
                .with_inner_size([1440.0, 920.0])
                .with_min_inner_size([850.0, 540.0])
                .with_decorations(false)
                .with_drag_and_drop(true),
            ..Default::default()
        },
        Box::new(move |cc| Ok(Box::new(app::FolderApp::new(cc, config_directory)))),
    )
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn windows_config_directory_is_absolute_and_stable() {
        let directory = super::config_directory().unwrap();
        assert!(directory.is_absolute());
        assert!(directory.ends_with("NkgFolder"));
        assert_eq!(directory, super::config_directory().unwrap());
    }
}
