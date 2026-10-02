#[cfg(windows)]
mod native {
    use std::{
        os::windows::ffi::OsStrExt,
        path::{Path, PathBuf},
        ptr,
    };
    use windows::{
        Win32::{
            Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
            System::Com::*,
            UI::{
                Input::KeyboardAndMouse::GetActiveWindow,
                Shell::{Common::ITEMIDLIST, *},
                WindowsAndMessaging::*,
            },
        },
        core::{Interface, PCSTR, PCWSTR},
    };

    struct Com;
    impl Com {
        fn new() -> windows::core::Result<Self> {
            unsafe {
                CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
            }
            Ok(Self)
        }
    }
    impl Drop for Com {
        fn drop(&mut self) {
            unsafe {
                CoUninitialize();
            }
        }
    }
    struct Item(*mut ITEMIDLIST);
    impl Drop for Item {
        fn drop(&mut self) {
            unsafe {
                CoTaskMemFree(Some(self.0.cast()));
            }
        }
    }
    struct Menu(HMENU);
    impl Drop for Menu {
        fn drop(&mut self) {
            unsafe {
                let _ = DestroyMenu(self.0);
            }
        }
    }

    fn shell_path(path: &Path) -> windows::core::Result<Vec<u16>> {
        // Preserve the selected link itself; resolving it could invoke Delete on its target.
        let path = std::path::absolute(path).map_err(|e| {
            windows::core::Error::new(
                windows::core::HRESULT::from_win32(e.raw_os_error().unwrap_or(1) as u32),
                e.to_string(),
            )
        })?;
        let wide: Vec<_> = path.as_os_str().encode_wide().collect();
        let mut plain = if wide.starts_with(&[92, 92, 63, 92, 85, 78, 67, 92]) {
            [vec![92, 92], wide[8..].to_vec()].concat()
        } else if wide.starts_with(&[92, 92, 63, 92]) {
            wide[4..].to_vec()
        } else {
            wide
        };
        plain.push(0);
        Ok(plain)
    }

    fn shell_object<T: Interface>(paths: &[PathBuf], owner: HWND) -> windows::core::Result<T> {
        let mut items = Vec::new();
        for path in paths {
            let name = shell_path(path)?;
            let mut item = ptr::null_mut();
            unsafe {
                SHParseDisplayName(PCWSTR(name.as_ptr()), None, &mut item, 0, None)?;
            }
            items.push(Item(item));
        }
        if items.is_empty() {
            return Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                0x80070057u32 as i32,
            )));
        }
        // GetUIObjectOf requires immediate children of the actual parent, not
        // absolute multi-component PIDLs handed to Desktop (Properties can target This PC).
        let parent: IShellFolder = unsafe { SHBindToParent(items[0].0, None)? };
        let parent_id = Item(unsafe { SHGetIDListFromObject(&parent)? });
        if items
            .iter()
            .all(|item| unsafe { ILIsParent(parent_id.0, item.0, true) }.as_bool())
        {
            let children: Vec<_> = items
                .iter()
                .map(|item| unsafe { ILFindLastID(item.0) } as *const ITEMIDLIST)
                .collect();
            return unsafe { parent.GetUIObjectOf(owner, &children, None) };
        }

        // Cross-folder selections need the default composite menu, rather than
        // the Desktop namespace's own context-menu handler.
        let pointers: Vec<_> = items
            .iter()
            .map(|item| item.0 as *const ITEMIDLIST)
            .collect();
        let desktop = unsafe { SHGetDesktopFolder()? };
        if T::IID == IContextMenu::IID {
            let mut selection: Vec<_> = items.iter().map(|item| item.0).collect();
            let menu = DEFCONTEXTMENU {
                hwnd: owner,
                psf: std::mem::ManuallyDrop::new(Some(desktop)),
                cidl: selection.len() as u32,
                apidl: selection.as_mut_ptr(),
                ..Default::default()
            };
            let result = unsafe { SHCreateDefaultContextMenu(&menu) };
            drop(std::mem::ManuallyDrop::into_inner(menu.psf));
            result
        } else {
            // Desktop's data object preserves absolute cross-folder selections (CF_HDROP).
            unsafe { desktop.GetUIObjectOf(owner, &pointers, None) }
        }
    }

    fn needs_open_with(path: &Path, code: windows::core::HRESULT) -> bool {
        use windows::Win32::Foundation::{
            CO_E_APPNOTFOUND, ERROR_FILE_NOT_FOUND, ERROR_NO_ASSOCIATION, ERROR_PATH_NOT_FOUND,
            REGDB_E_CLASSNOTREG,
        };
        // A missing document must not be mistaken for a missing associated application.
        path.is_file()
            && (code == CO_E_APPNOTFOUND
                || code == REGDB_E_CLASSNOTREG
                || [
                    ERROR_NO_ASSOCIATION,
                    ERROR_FILE_NOT_FOUND,
                    ERROR_PATH_NOT_FOUND,
                ]
                .iter()
                .any(|error| code == error.to_hresult()))
    }

    pub fn open_path(path: &Path) -> Result<(), String> {
        let result = (|| -> windows::core::Result<()> {
            let _com = Com::new()?;
            let file = shell_path(path)?;
            let mut info = SHELLEXECUTEINFOW {
                cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
                fMask: SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
                lpFile: PCWSTR(file.as_ptr()),
                nShow: SW_SHOWNORMAL.0,
                ..Default::default()
            };
            match unsafe { ShellExecuteExW(&mut info) } {
                Err(error) if needs_open_with(path, error.code()) => unsafe {
                    SHOpenWithDialog(
                        None,
                        &OPENASINFO {
                            pcszFile: PCWSTR(file.as_ptr()),
                            oaifInFlags: OAIF_EXEC,
                            ..Default::default()
                        },
                    )
                },
                result => result,
            }
        })();
        result.map_err(|e| format!("Windows 无法调用默认应用：{e}"))
    }

    pub(crate) fn data_object(
        paths: &[PathBuf],
        owner: HWND,
    ) -> windows::core::Result<IDataObject> {
        shell_object(paths, owner).or_else(|_| path_data_object(paths))
    }

    // A protected parent can prevent Shell PIDL parsing in the ordinary GUI process.
    // CF_HDROP carries the original paths without reading or staging their contents.
    fn path_data_object(paths: &[PathBuf]) -> windows::core::Result<IDataObject> {
        use std::os::windows::ffi::OsStringExt;
        use windows::Win32::System::{
            Memory::*,
            Ole::{CF_HDROP, ReleaseStgMedium},
        };
        let paths = paths
            .iter()
            .map(|p| {
                shell_path(p).map(|wide| {
                    PathBuf::from(std::ffi::OsString::from_wide(&wide[..wide.len() - 1]))
                })
            })
            .collect::<windows::core::Result<Vec<_>>>()?;
        let bytes = crate::clipboard::drop_files(&paths)
            .map_err(|e| windows::core::Error::new(windows::core::HRESULT::from_win32(87), e))?;
        unsafe {
            let data: IDataObject = SHCreateDataObject(None, None, None::<&IDataObject>)?;
            let memory = GlobalAlloc(GMEM_MOVEABLE, bytes.len())?;
            let pointer = GlobalLock(memory);
            if pointer.is_null() {
                let _ = windows::Win32::Foundation::GlobalFree(Some(memory));
                return Err(windows::core::Error::from_thread());
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.cast(), bytes.len());
            let _ = GlobalUnlock(memory);
            let format = FORMATETC {
                cfFormat: CF_HDROP.0,
                dwAspect: DVASPECT_CONTENT.0,
                lindex: -1,
                tymed: TYMED_HGLOBAL.0 as u32,
                ..Default::default()
            };
            let mut medium = STGMEDIUM {
                tymed: TYMED_HGLOBAL.0 as u32,
                u: STGMEDIUM_0 { hGlobal: memory },
                pUnkForRelease: std::mem::ManuallyDrop::new(None),
            };
            if let Err(error) = data.SetData(&format, &medium, true) {
                ReleaseStgMedium(&mut medium);
                return Err(error);
            }
            Ok(data)
        }
    }

    struct Handler {
        menu3: Option<IContextMenu3>,
        menu2: Option<IContextMenu2>,
    }
    unsafe extern "system" fn menu_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _: usize,
        data: usize,
    ) -> LRESULT {
        if matches!(
            message,
            WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR
        ) {
            // data points to a stack value kept alive through TrackPopupMenuEx; subclass is removed before it drops.
            let handler = unsafe { &*(data as *const Handler) };
            if let Some(menu) = &handler.menu3 {
                let mut result = LRESULT(0);
                if unsafe { menu.HandleMenuMsg2(message, wparam, lparam, Some(&mut result)) }
                    .is_ok()
                {
                    return result;
                }
            } else if let Some(menu) = &handler.menu2
                && unsafe { menu.HandleMenuMsg(message, wparam, lparam) }.is_ok()
            {
                return LRESULT(0);
            }
        }
        unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
    }

    pub fn show(paths: &[PathBuf]) -> Result<(), String> {
        let result = (|| -> windows::core::Result<()> {
            let _com = Com::new()?;
            let owner = unsafe { GetActiveWindow() };
            if owner.0.is_null() {
                return Err(windows::core::Error::from_thread());
            }
            let context: IContextMenu = shell_object(paths, owner)?;
            let menu = Menu(unsafe { CreatePopupMenu()? });
            unsafe {
                context
                    .QueryContextMenu(menu.0, 0, 1, 0x7fff, CMF_NORMAL)
                    .ok()?;
            }
            let handler = Handler {
                menu3: context.cast().ok(),
                menu2: context.cast().ok(),
            };
            let mut point = POINT::default();
            unsafe {
                GetCursorPos(&mut point)?;
            }
            if !unsafe {
                SetWindowSubclass(
                    owner,
                    Some(menu_proc),
                    0x4e4b47,
                    &handler as *const Handler as usize,
                )
            }
            .as_bool()
            {
                return Err(windows::core::Error::from_thread());
            }
            let command = unsafe {
                TrackPopupMenuEx(
                    menu.0,
                    TPM_RETURNCMD.0 | TPM_RIGHTBUTTON.0,
                    point.x,
                    point.y,
                    owner,
                    None,
                )
            }
            .0;
            unsafe {
                let _ = RemoveWindowSubclass(owner, Some(menu_proc), 0x4e4b47);
            }
            if command > 0 {
                let invoke = CMINVOKECOMMANDINFO {
                    cbSize: std::mem::size_of::<CMINVOKECOMMANDINFO>() as u32,
                    hwnd: owner,
                    lpVerb: PCSTR((command - 1) as usize as *const u8),
                    nShow: SW_SHOWNORMAL.0,
                    ..Default::default()
                };
                unsafe {
                    context.InvokeCommand(&invoke)?;
                }
            }
            Ok(())
        })();
        result.map_err(|e| format!("无法显示或执行系统右键菜单：{e}"))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn broken_association_offers_choice_only_for_existing_files() {
            use windows::Win32::Foundation::{
                CO_E_APPNOTFOUND, ERROR_ACCESS_DENIED, ERROR_NO_ASSOCIATION,
            };
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("日志.log");
            assert!(!needs_open_with(&path, CO_E_APPNOTFOUND));
            std::fs::write(&path, "test").unwrap();
            assert!(needs_open_with(&path, CO_E_APPNOTFOUND));
            assert!(needs_open_with(&path, ERROR_NO_ASSOCIATION.to_hresult()));
            assert!(!needs_open_with(&path, ERROR_ACCESS_DENIED.to_hresult()));
            assert!(!needs_open_with(dir.path(), CO_E_APPNOTFOUND));
        }

        #[test]
        fn shell_paths_are_literal_utf16_and_missing_files_report_errors() {
            for (path, expected) in [
                (
                    r"\\?\C:\项目\[test] $x;日志.log",
                    r"C:\项目\[test] $x;日志.log",
                ),
                (r"\\?\UNC\server\共享\日志.log", r"\\server\共享\日志.log"),
            ] {
                let wide = shell_path(Path::new(path)).unwrap();
                assert_eq!(
                    String::from_utf16(&wide[..wide.len() - 1]).unwrap(),
                    expected
                );
                assert_eq!(wide.last(), Some(&0));
            }
            let dir = tempfile::tempdir().unwrap();
            assert!(open_path(&dir.path().join("missing.log")).is_err());
        }

        #[test]
        fn shell_drag_data_exposes_multiple_unicode_files() {
            use windows::Win32::System::Ole::{CF_HDROP, ReleaseStgMedium};
            let _com = Com::new().unwrap();
            let dir = tempfile::tempdir().unwrap();
            let paths = [
                dir.path().join("中文 [a].txt"),
                dir.path().join("空 格.log"),
            ];
            for path in &paths {
                std::fs::write(path, "drag-test").unwrap();
            }
            let data = data_object(&paths, HWND::default()).unwrap();
            let format = FORMATETC {
                cfFormat: CF_HDROP.0,
                dwAspect: DVASPECT_CONTENT.0,
                lindex: -1,
                tymed: TYMED_HGLOBAL.0 as u32,
                ..Default::default()
            };
            let mut medium = unsafe { data.GetData(&format).unwrap() };
            let mut actual = Vec::new();
            unsafe {
                let drop = HDROP(medium.u.hGlobal.0);
                let count = DragQueryFileW(drop, u32::MAX, None);
                for i in 0..count {
                    let mut wide = vec![0; DragQueryFileW(drop, i, None) as usize + 1];
                    let n = DragQueryFileW(drop, i, Some(&mut wide));
                    actual.push(String::from_utf16(&wide[..n as usize]).unwrap());
                }
                ReleaseStgMedium(&mut medium);
            }
            assert_eq!(
                actual,
                paths
                    .iter()
                    .map(|p| p.to_string_lossy().to_string())
                    .collect::<Vec<_>>()
            );
        }

        #[test]
        fn windows_shell_builds_real_menu_for_selected_files() {
            let _com = Com::new().unwrap();
            let root = tempfile::tempdir().unwrap();
            let file = root.path().join("sample.txt");
            std::fs::write(&file, "sample").unwrap();
            let second = root.path().join("other");
            std::fs::create_dir(&second).unwrap();
            let second = second.join("sample.txt");
            std::fs::write(&second, "sample").unwrap();
            let sibling = root.path().join("sibling.txt");
            std::fs::write(&sibling, "sample").unwrap();
            for paths in [
                vec![file.clone()],
                vec![file.clone(), sibling],
                vec![file, second],
            ] {
                let context: IContextMenu = shell_object(&paths, HWND::default()).unwrap();
                let menu = Menu(unsafe { CreatePopupMenu().unwrap() });
                let result = unsafe { context.QueryContextMenu(menu.0, 0, 1, 0x7fff, CMF_NORMAL) };
                result.ok().unwrap();
                let mut verbs = Vec::new();
                for command in 0..(result.0 as u32 & 0xffff) {
                    let mut buffer = [0u16; 256];
                    if unsafe {
                        context.GetCommandString(
                            command as usize,
                            GCS_VERBW,
                            None,
                            windows::core::PSTR(buffer.as_mut_ptr().cast()),
                            buffer.len() as u32,
                        )
                    }
                    .is_ok()
                    {
                        let end = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
                        verbs.push(String::from_utf16_lossy(&buffer[..end]));
                    }
                }
                assert!(verbs.iter().any(|v| v == "properties"), "{verbs:?}");
                assert!(
                    verbs.iter().any(|v| v == "delete"),
                    "wrong selection menu: {verbs:?}"
                );
                assert!(
                    !verbs.iter().any(|v| v.eq_ignore_ascii_case("manage")),
                    "This PC menu: {verbs:?}"
                );
                assert!(unsafe { GetMenuItemCount(Some(menu.0)) } > 0);
            }
        }
    }
}

#[cfg(windows)]
pub(crate) use native::data_object;
#[cfg(windows)]
pub use native::{open_path, show};
#[cfg(not(windows))]
pub fn show(_: &[std::path::PathBuf]) -> Result<(), String> {
    Err("系统右键菜单仅支持 Windows。".into())
}

#[cfg(not(windows))]
pub fn open_path(_: &std::path::Path) -> Result<(), String> {
    Err("打开文件仅支持 Windows。".into())
}
