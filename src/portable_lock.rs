//! Read-only file sharing check for WinGet's registered portable packages.
use crate::{packages::valid_id, platform::wide, winget::SOURCE_PACKAGE};
use std::{
    fs::{self, OpenOptions},
    os::windows::fs::{MetadataExt, OpenOptionsExt},
    path::{Component, Path, PathBuf, Prefix},
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION, HANDLE, INVALID_HANDLE_VALUE,
    },
    Storage::FileSystem::{
        DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, GetDriveTypeW,
    },
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        },
        Registry::*,
        Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW},
    },
};

fn registry_string(root: HKEY, key: &[u16], view: u32, name: &str) -> Option<String> {
    let mut data = [0u16; 1024];
    let mut bytes = size_of_val(&data) as u32;
    let result = unsafe {
        RegGetValueW(
            root,
            key.as_ptr(),
            wide(name).as_ptr(),
            RRF_RT_REG_SZ | view,
            std::ptr::null_mut(),
            data.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    if result != 0 || bytes < 2 || bytes as usize > size_of_val(&data) {
        return None;
    }
    String::from_utf16(&data[..bytes as usize / 2 - 1]).ok()
}

fn local_root(path: &Path, product: &str) -> bool {
    let mut parts = path.components();
    let Some(Component::Prefix(prefix)) = parts.next() else {
        return false;
    };
    let Prefix::Disk(letter) = prefix.kind() else {
        return false;
    };
    if parts.next() != Some(Component::RootDir)
        || !parts.all(|part| matches!(part, Component::Normal(_)))
        || !path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case(product))
        || !matches!(
            unsafe { GetDriveTypeW(wide(format!("{}:\\", char::from(letter))).as_ptr()) },
            2 | 3
        )
    {
        return false;
    }
    let parents = path
        .ancestors()
        .filter(|p| p.has_root())
        .collect::<Vec<_>>();
    parents.into_iter().rev().all(|p| {
        fs::symlink_metadata(p)
            .is_ok_and(|m| m.is_dir() && m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0)
    })
}

fn collect_binaries(dir: &Path, depth: usize, remaining: &mut usize, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let Some(next) = remaining.checked_sub(1) else {
            return;
        };
        *remaining = next;
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            continue;
        }
        if depth > 0 && meta.is_dir() {
            collect_binaries(&path, depth - 1, remaining, files);
        } else if meta.is_file()
            && path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| {
                    ext.eq_ignore_ascii_case("exe") || ext.eq_ignore_ascii_case("dll")
                })
        {
            files.push(path);
        }
    }
}

fn sharing_lock(path: &Path) -> bool {
    // Request delete access without writing, deleting, or taking an exclusive handle.
    // ACL failures give no information about a lock.
    OpenOptions::new()
        .access_mode(DELETE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .is_err_and(|e| {
            matches!(
                e.raw_os_error().map(|code| code as u32),
                Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION)
            )
        })
}

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn running_binary(files: &[PathBuf]) -> Option<&Path> {
    if files.is_empty() {
        return None;
    }
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return None;
    }
    let snapshot = Handle(snapshot);
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    if unsafe { Process32FirstW(snapshot.0, &mut entry) } == 0 {
        return None;
    }
    loop {
        let name_end = entry
            .szExeFile
            .iter()
            .position(|&ch| ch == 0)
            .unwrap_or(entry.szExeFile.len());
        let name = String::from_utf16_lossy(&entry.szExeFile[..name_end]);
        if files.iter().any(|file| {
            file.file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.eq_ignore_ascii_case(&name))
        }) {
            let process =
                unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, entry.th32ProcessID) };
            if !process.is_null() {
                let process = Handle(process);
                let mut path = [0u16; 2_048];
                let mut size = path.len() as u32;
                if unsafe { QueryFullProcessImageNameW(process.0, 0, path.as_mut_ptr(), &mut size) }
                    != 0
                {
                    let executable = String::from_utf16_lossy(&path[..size as usize]);
                    if let Some(file) = files.iter().find(|file| {
                        file.to_str()
                            .is_some_and(|path| path.eq_ignore_ascii_case(&executable))
                    }) {
                        return Some(file);
                    }
                }
            }
        }
        if unsafe { Process32NextW(snapshot.0, &mut entry) } == 0 {
            return None;
        }
    }
}

pub enum InUse {
    Running(PathBuf),
    Locked(PathBuf),
}

pub fn find(id: &str, installed_version: Option<&str>) -> Option<InUse> {
    if !valid_id(id) {
        return None;
    }
    let version = installed_version?;
    let product = format!("{id}_{SOURCE_PACKAGE}");
    let key = wide(format!(
        "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{product}"
    ));
    for (root, view) in [
        (HKEY_CURRENT_USER, RRF_SUBKEY_WOW6464KEY),
        (HKEY_LOCAL_MACHINE, RRF_SUBKEY_WOW6464KEY),
        (HKEY_LOCAL_MACHINE, RRF_SUBKEY_WOW6432KEY),
    ] {
        let read = |name| registry_string(root, &key, view, name);
        if !read("WinGetPackageIdentifier").is_some_and(|value| value.eq_ignore_ascii_case(id))
            || read("WinGetSourceIdentifier").as_deref() != Some(SOURCE_PACKAGE)
            || read("WinGetInstallerType").as_deref() != Some("portable")
            || read("DisplayVersion").as_deref() != Some(version)
        {
            continue;
        }
        if let Some(path) = read("InstallLocation").map(PathBuf::from)
            && local_root(&path, &product)
        {
            let mut files = Vec::new();
            collect_binaries(&path, 4, &mut 256, &mut files);
            if let Some(file) = running_binary(&files) {
                return Some(InUse::Running(file.into()));
            }
            if let Some(file) = files.into_iter().find(|path| sharing_lock(path)) {
                return Some(InUse::Locked(file));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probes_a_held_executable_without_modifying_it() {
        let dir = std::env::temp_dir().join(format!("slimstore-lock-test-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let exe = dir.join("probe.exe");
        let content = b"Windows sharing test fixture";
        fs::write(&exe, content).unwrap();
        assert!(!sharing_lock(&exe));
        let handle = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&exe)
            .unwrap();
        assert!(sharing_lock(&exe));
        let mut files = Vec::new();
        collect_binaries(&dir, 1, &mut 0, &mut files);
        assert!(files.is_empty());
        drop(handle);
        assert!(!sharing_lock(&exe));
        assert_eq!(fs::read(&exe).unwrap(), content);
        fs::remove_file(exe).unwrap();
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn running_binary_checks_full_path_not_just_the_process_name() {
        let executable = std::env::current_exe().unwrap();
        assert_eq!(
            running_binary(std::slice::from_ref(&executable)),
            Some(executable.as_path())
        );
        let other = executable
            .parent()
            .unwrap()
            .join("another-directory")
            .join(executable.file_name().unwrap());
        assert!(running_binary(&[other]).is_none());
    }
}
