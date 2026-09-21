use crate::error::AppError;
use std::path::Path;

const APP_NAME: &str = "CC Switch";

/// Quote a Windows HKCU Run command so paths with spaces actually launch.
///
/// auto-launch 0.5 writes `{path} {args}` with no quotes and a trailing space
/// when args are empty. Explorer then treats `...\Programs\CC` as the exe.
fn windows_run_command(exe_path: &Path) -> String {
    let mut s = exe_path.to_string_lossy().into_owned();
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        s = stripped.to_string();
    }
    format!("\"{}\"", s.trim_matches('"'))
}

#[cfg(windows)]
mod windows_impl {
    use super::{windows_run_command, AppError, APP_NAME};
    use winreg::enums::{RegType, HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE};
    use winreg::{RegKey, RegValue};

    const RUN_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Run";
    const APPROVED_KEY: &str =
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
    const APPROVED_ENABLED: [u8; 12] = [0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

    fn current_exe() -> Result<std::path::PathBuf, AppError> {
        std::env::current_exe().map_err(|e| AppError::Message(format!("无法获取应用路径: {e}")))
    }

    pub fn enable() -> Result<(), AppError> {
        let cmd = windows_run_command(&current_exe()?);
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        hkcu.create_subkey(RUN_KEY)
            .and_then(|(key, _)| key.set_value(APP_NAME, &cmd))
            .map_err(|e| AppError::Message(format!("写入开机自启注册表失败: {e}")))?;

        if let Ok((reg, _)) = hkcu.create_subkey(APPROVED_KEY) {
            let _ = reg.set_raw_value(
                APP_NAME,
                &RegValue {
                    vtype: RegType::REG_BINARY,
                    bytes: APPROVED_ENABLED.to_vec(),
                },
            );
        }

        log::info!("已启用开机自启: {cmd}");
        Ok(())
    }

    pub fn disable() -> Result<(), AppError> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        if let Ok(key) = hkcu.open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE) {
            match key.delete_value(APP_NAME) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(AppError::Message(format!("删除开机自启注册表失败: {e}")));
                }
            }
        }
        if let Ok(key) = hkcu.open_subkey_with_flags(APPROVED_KEY, KEY_SET_VALUE) {
            let _ = key.delete_value(APP_NAME);
        }
        log::info!("已禁用开机自启");
        Ok(())
    }

    pub fn is_enabled() -> Result<bool, AppError> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let run_value: Result<String, _> = hkcu
            .open_subkey_with_flags(RUN_KEY, KEY_READ)
            .and_then(|key| key.get_value(APP_NAME));
        let Some(cmd) = run_value.ok() else {
            return Ok(false);
        };
        if cmd.trim().is_empty() {
            return Ok(false);
        }

        if let Ok(raw) = hkcu
            .open_subkey_with_flags(APPROVED_KEY, KEY_READ)
            .and_then(|key| key.get_raw_value(APP_NAME))
        {
            if raw.bytes.len() >= 4 {
                let status = u32::from_le_bytes(
                    raw.bytes[0..4]
                        .try_into()
                        .expect("slice length checked above"),
                );
                // 6 / 7 = disabled in Task Manager / Settings > Startup
                if matches!(status, 6 | 7) {
                    return Ok(false);
                }
            }
        }

        Ok(true)
    }
}

#[cfg(not(windows))]
fn get_auto_launch() -> Result<auto_launch::AutoLaunch, AppError> {
    use auto_launch::AutoLaunchBuilder;

    let exe_path =
        std::env::current_exe().map_err(|e| AppError::Message(format!("无法获取应用路径: {e}")))?;

    // macOS 需要使用 .app bundle 路径，否则 AppleScript login item 会打开终端
    #[cfg(target_os = "macos")]
    let app_path = get_macos_app_bundle_path(&exe_path).unwrap_or(exe_path);

    #[cfg(not(target_os = "macos"))]
    let app_path = exe_path;

    AutoLaunchBuilder::new()
        .set_app_name(APP_NAME)
        .set_app_path(&app_path.to_string_lossy())
        .build()
        .map_err(|e| AppError::Message(format!("创建 AutoLaunch 失败: {e}")))
}

/// 获取 macOS 上的 .app bundle 路径
/// 将 `/path/to/CC Switch.app/Contents/MacOS/CC Switch` 转换为 `/path/to/CC Switch.app`
#[cfg(target_os = "macos")]
fn get_macos_app_bundle_path(exe_path: &std::path::Path) -> Option<std::path::PathBuf> {
    let path_str = exe_path.to_string_lossy();
    // 查找 .app/Contents/MacOS/ 模式
    if let Some(app_pos) = path_str.find(".app/Contents/MacOS/") {
        let app_bundle_end = app_pos + 4; // ".app" 的结束位置
        Some(std::path::PathBuf::from(&path_str[..app_bundle_end]))
    } else {
        None
    }
}

/// 启用开机自启
pub fn enable_auto_launch() -> Result<(), AppError> {
    #[cfg(windows)]
    {
        return windows_impl::enable();
    }

    #[cfg(not(windows))]
    {
        get_auto_launch()?
            .enable()
            .map_err(|e| AppError::Message(format!("启用开机自启失败: {e}")))?;
        log::info!("已启用开机自启");
        Ok(())
    }
}

/// 禁用开机自启
pub fn disable_auto_launch() -> Result<(), AppError> {
    #[cfg(windows)]
    {
        return windows_impl::disable();
    }

    #[cfg(not(windows))]
    {
        get_auto_launch()?
            .disable()
            .map_err(|e| AppError::Message(format!("禁用开机自启失败: {e}")))?;
        log::info!("已禁用开机自启");
        Ok(())
    }
}

/// 检查是否已启用开机自启
pub fn is_auto_launch_enabled() -> Result<bool, AppError> {
    #[cfg(windows)]
    {
        return windows_impl::is_enabled();
    }

    #[cfg(not(windows))]
    {
        get_auto_launch()?
            .is_enabled()
            .map_err(|e| AppError::Message(format!("检查开机自启状态失败: {e}")))
    }
}

/// Rewrite the OS auto-start entry when settings say it should be on.
/// Repairs a broken Run key (unquoted path / trailing space) on every launch.
pub fn sync_auto_launch_from_settings() {
    if crate::settings::get_settings().launch_on_startup {
        if let Err(e) = enable_auto_launch() {
            log::warn!("同步开机自启失败: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn test_get_macos_app_bundle_path_valid() {
        let exe_path = std::path::Path::new("/Applications/CC Switch.app/Contents/MacOS/CC Switch");
        let result = get_macos_app_bundle_path(exe_path);
        assert_eq!(
            result,
            Some(std::path::PathBuf::from("/Applications/CC Switch.app"))
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_get_macos_app_bundle_path_with_spaces() {
        let exe_path =
            std::path::Path::new("/Users/test/My Apps/CC Switch.app/Contents/MacOS/CC Switch");
        let result = get_macos_app_bundle_path(exe_path);
        assert_eq!(
            result,
            Some(std::path::PathBuf::from(
                "/Users/test/My Apps/CC Switch.app"
            ))
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_get_macos_app_bundle_path_not_in_bundle() {
        let exe_path = std::path::Path::new("/usr/local/bin/cc-switch");
        let result = get_macos_app_bundle_path(exe_path);
        assert_eq!(result, None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_get_macos_app_bundle_path_dev_build() {
        // 开发环境下的路径通常不在 .app bundle 内
        let exe_path = std::path::Path::new("/Users/dev/project/target/debug/cc-switch");
        let result = get_macos_app_bundle_path(exe_path);
        assert_eq!(result, None);
    }

    #[test]
    fn windows_run_command_quotes_paths_with_spaces() {
        let path = Path::new(r"C:\Users\13493\AppData\Local\Programs\CC Switch\cc-switch.exe");
        assert_eq!(
            windows_run_command(path),
            r#""C:\Users\13493\AppData\Local\Programs\CC Switch\cc-switch.exe""#
        );
    }

    #[test]
    fn windows_run_command_strips_extended_path_prefix() {
        let path = Path::new(r"\\?\C:\Program Files\CC Switch\cc-switch.exe");
        assert_eq!(
            windows_run_command(path),
            r#""C:\Program Files\CC Switch\cc-switch.exe""#
        );
    }
}
