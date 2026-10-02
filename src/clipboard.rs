//! Windows file clipboard: CF_HDROP plus Preferred DropEffect.
use std::path::PathBuf;

pub struct FileClipboard {
    pub paths: Vec<PathBuf>,
    pub moving: bool,
    pub sequence: u32,
}

#[cfg(windows)]
pub(crate) fn drop_files(paths: &[PathBuf]) -> Result<Vec<u8>, String> {
    use std::os::windows::ffi::OsStrExt;
    if paths.is_empty() {
        return Err("请先选中真实文件或目录。".into());
    }
    // DROPFILES is five DWORDs: offset, x, y, non-client flag, UTF-16 flag.
    let mut bytes: Vec<u8> = [20_u32, 0, 0, 0, 1]
        .into_iter()
        .flat_map(u32::to_le_bytes)
        .collect();
    for path in paths {
        if !path.is_absolute() {
            return Err("文件剪贴板只接受绝对路径。".into());
        }
        let wide: Vec<_> = path.as_os_str().encode_wide().collect();
        if wide.contains(&0) {
            return Err("路径包含无效的空字符。".into());
        }
        bytes.extend(wide.into_iter().chain(Some(0)).flat_map(u16::to_le_bytes));
    }
    bytes.extend([0, 0]);
    Ok(bytes)
}

fn move_effect(bytes: &[u8]) -> Result<bool, String> {
    let effect = u32::from_le_bytes(bytes.try_into().map_err(|_| "剪贴板移动标志无效。")?);
    match effect {
        1 | 3 => Ok(false),
        2 => Ok(true),
        _ => Err("剪贴板请求创建链接，当前粘贴操作仅支持复制或移动。".into()),
    }
}

#[cfg(windows)]
pub fn write(paths: &[PathBuf], moving: bool) -> Result<(), String> {
    use clipboard_win::{Clipboard, raw};
    let data = drop_files(paths)?;
    let effect =
        clipboard_win::register_format("Preferred DropEffect").ok_or("无法注册文件剪贴板格式。")?;
    let _guard = Clipboard::new_attempts(10).map_err(|e| format!("剪贴板正忙：{e}"))?;
    raw::set(clipboard_win::formats::CF_HDROP, &data).map_err(|e| e.to_string())?;
    raw::set_without_clear(
        effect.get(),
        &(if moving { 2_u32 } else { 1 }).to_le_bytes(),
    )
    .map_err(|e| e.to_string())
}

#[cfg(windows)]
pub fn read() -> Result<FileClipboard, String> {
    use clipboard_win::{Clipboard, formats, raw};
    let effect =
        clipboard_win::register_format("Preferred DropEffect").ok_or("无法注册文件剪贴板格式。")?;
    let _guard = Clipboard::new_attempts(10).map_err(|e| format!("剪贴板正忙：{e}"))?;
    if !clipboard_win::is_format_avail(formats::CF_HDROP) {
        return Err("剪贴板中没有文件；请先在本程序或资源管理器中复制文件。".into());
    }
    let paths: Vec<PathBuf> = clipboard_win::get(formats::FileList).map_err(|e| e.to_string())?;
    if paths.is_empty() || paths.iter().any(|p| !p.is_absolute()) {
        return Err("剪贴板文件路径无效。".into());
    }
    let moving = if clipboard_win::is_format_avail(effect.get()) {
        let mut bytes = [0_u8; 4];
        if raw::get(effect.get(), &mut bytes).map_err(|e| e.to_string())? != 4 {
            return Err("剪贴板移动标志无效。".into());
        }
        move_effect(&bytes)?
    } else {
        false
    };
    Ok(FileClipboard {
        paths,
        moving,
        sequence: clipboard_win::seq_num().map(|n| n.get()).unwrap_or(0),
    })
}

/// Never clear a clipboard the user changed while the file operation was running.
#[cfg(windows)]
pub fn finish_move(sequence: u32) -> Result<(), String> {
    let _guard = clipboard_win::Clipboard::new_attempts(10).map_err(|e| e.to_string())?;
    if sequence != 0 && clipboard_win::seq_num().map(|n| n.get()) == Some(sequence) {
        clipboard_win::empty().map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn write(_: &[PathBuf], _: bool) -> Result<(), String> {
    Err("文件剪贴板仅支持 Windows。".into())
}
#[cfg(not(windows))]
pub fn read() -> Result<FileClipboard, String> {
    Err("文件剪贴板仅支持 Windows。".into())
}
#[cfg(not(windows))]
pub fn finish_move(_: u32) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clipboard_move_flag_is_strict() {
        assert!(move_effect(&2_u32.to_le_bytes()).unwrap());
        assert!(!move_effect(&1_u32.to_le_bytes()).unwrap());
        assert!(!move_effect(&3_u32.to_le_bytes()).unwrap());
        assert!(move_effect(&4_u32.to_le_bytes()).is_err());
        assert!(move_effect(&[2]).is_err());
    }
    #[cfg(windows)]
    #[test]
    fn hdrop_preserves_utf16_paths_and_double_terminator() {
        use std::{
            ffi::OsString,
            os::windows::ffi::{OsStrExt, OsStringExt},
        };
        let second = OsString::from_wide(&[67, 58, 92, 0xd800, 46, 99, 115]);
        let paths = vec![PathBuf::from("C:\\项目\\配置.json"), PathBuf::from(second)];
        let bytes = drop_files(&paths).unwrap();
        assert_eq!(&bytes[..4], &20_u32.to_le_bytes());
        assert_eq!(&bytes[16..20], &1_u32.to_le_bytes());
        let wide: Vec<_> = bytes[20..]
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        let expected: Vec<_> = paths
            .iter()
            .flat_map(|p| p.as_os_str().encode_wide().chain(Some(0)))
            .chain(Some(0))
            .collect();
        assert_eq!(wide, expected);
        assert!(drop_files(&[]).is_err());
        assert!(drop_files(&[PathBuf::from("relative")]).is_err());
    }
}
