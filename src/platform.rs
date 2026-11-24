use std::{
    ffi::{OsStr, OsString},
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
};

pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(Some(0)).collect()
}

pub fn registered_winget() -> Option<PathBuf> {
    use windows_sys::Win32::{
        Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS},
        Storage::Packaging::Appx::{
            GetPackagePathByFullName, GetPackagesByPackageFamily, GetStagedPackageOrigin,
            PackageOrigin_Inbox, PackageOrigin_Store,
        },
    };

    let family = wide("Microsoft.DesktopAppInstaller_8wekyb3d8bbwe");
    let (mut count, mut size) = (0, 0);
    let status = unsafe {
        GetPackagesByPackageFamily(
            family.as_ptr(),
            &mut count,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
        )
    };
    if status != ERROR_INSUFFICIENT_BUFFER || !(1..=64).contains(&count) || size > 16_384 {
        return None;
    }
    let mut packages = vec![std::ptr::null_mut(); count as usize];
    let mut buffer = vec![0u16; size as usize];
    if unsafe {
        GetPackagesByPackageFamily(
            family.as_ptr(),
            &mut count,
            packages.as_mut_ptr(),
            &mut size,
            buffer.as_mut_ptr(),
        )
    } != ERROR_SUCCESS
    {
        return None;
    }

    // Only use package-managed code from the Microsoft family registered for this user.
    // Loose/developer and unsigned registrations must not substitute for App Installer.
    packages.into_iter().take(count as usize).find_map(|name| {
        let mut origin = 0;
        if name.is_null()
            || unsafe { GetStagedPackageOrigin(name, &mut origin) } != ERROR_SUCCESS
            || (origin != PackageOrigin_Store && origin != PackageOrigin_Inbox)
        {
            return None;
        }

        let mut size = 0;
        if unsafe { GetPackagePathByFullName(name, &mut size, std::ptr::null_mut()) }
            != ERROR_INSUFFICIENT_BUFFER
            || !(2..=32_768).contains(&size)
        {
            return None;
        }
        let mut path = vec![0u16; size as usize];
        if unsafe { GetPackagePathByFullName(name, &mut size, path.as_mut_ptr()) } != ERROR_SUCCESS
        {
            return None;
        }
        let path = PathBuf::from(OsString::from_wide(&path[..size as usize - 1]));
        let executable = path.join("winget.exe");
        (executable.is_absolute() && executable.is_file()).then_some(executable)
    })
}

pub fn message(text: &str) {
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::MessageBoxW(
            std::ptr::null_mut(),
            wide(text).as_ptr(),
            wide("SlimStore").as_ptr(),
            0x10,
        );
    }
}

pub fn open(target: &str) {
    unsafe {
        windows_sys::Win32::UI::Shell::ShellExecuteW(
            std::ptr::null_mut(),
            wide("open").as_ptr(),
            wide(target).as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
        );
    }
}

pub fn information(text: &str, cancel: bool) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        IDOK, MB_ICONINFORMATION, MB_OK, MB_OKCANCEL, MessageBoxW,
    };
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            wide(text).as_ptr(),
            wide("SlimStore").as_ptr(),
            MB_ICONINFORMATION | if cancel { MB_OKCANCEL } else { MB_OK },
        ) == IDOK
    }
}

fn registry_dword(path: &str, name: &str) -> Option<u32> {
    use windows_sys::Win32::System::Registry::*;
    let mut value = 0u32;
    let mut len = 4u32;
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            wide(path).as_ptr(),
            wide(name).as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&mut value as *mut u32).cast(),
            &mut len,
        )
    };
    (result == 0).then_some(value)
}
pub fn system_dark() -> bool {
    registry_dword(
        "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize",
        "AppsUseLightTheme",
    )
    .is_some_and(|v| v == 0)
}
pub fn accent() -> Option<slint::Color> {
    registry_dword("Software\\Microsoft\\Windows\\DWM", "AccentColor").map(|v| {
        slint::Color::from_rgb_u8(
            (v & 255) as u8,
            ((v >> 8) & 255) as u8,
            ((v >> 16) & 255) as u8,
        )
    })
}

pub fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32;
    }
    if unsafe { MoveFileExW(wide(from).as_ptr(), wide(to).as_ptr(), 0x1 | 0x8) } == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Diagnostic probe only. Never install a COM shim, package identity, or DLL.
pub fn probe_com() -> String {
    use windows_sys::Win32::System::Com::*;
    use windows_sys::core::GUID;
    unsafe {
        let initialized = CoInitializeEx(std::ptr::null(), COINIT_MULTITHREADED as u32);
        if initialized < 0 {
            return format!("COM initialization: 0x{:08X}", initialized as u32);
        }
        let clsid = GUID::from_u128(0xc53a4f16_787e_42a4_b304_29effb4bf597);
        let iid = GUID::from_u128(0x00000000_0000_0000_c000_000000000046);
        let mut object: *mut std::ffi::c_void = std::ptr::null_mut();
        let hr = CoCreateInstance(
            &clsid,
            std::ptr::null_mut(),
            CLSCTX_LOCAL_SERVER,
            &iid,
            &mut object,
        );
        if hr >= 0 && !object.is_null() {
            // IUnknown vtable: QueryInterface, AddRef, Release.
            let table = *(object as *mut *mut *mut std::ffi::c_void);
            let release: unsafe extern "system" fn(*mut std::ffi::c_void) -> u32 =
                std::mem::transmute(*table.add(2));
            release(object);
        }
        CoUninitialize();
        if hr >= 0 {
            "COM activation available; CLI selected for portable compatibility".into()
        } else {
            format!(
                "COM unavailable to unpackaged caller (0x{:08X}); using CLI",
                hr as u32
            )
        }
    }
}
