use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::windows::{
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT},
    Storage::FileSystem::*,
    System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject},
};

const MAX_SETTINGS: u64 = 16_384;
const MAX_CATALOG: u64 = 6 * 1024 * 1024;
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
pub enum CacheFile {
    Icons,
    Metadata,
}

impl CacheFile {
    fn name(self) -> &'static str {
        match self {
            Self::Icons => "icons-v1.bin",
            Self::Metadata => "app-data-v1.bin",
        }
    }

    fn limit(self) -> u64 {
        match self {
            Self::Icons => 4 * 1024 * 1024,
            Self::Metadata => 2 * 1024 * 1024,
        }
    }
}

fn check_storage_entry(metadata: &fs::Metadata, directory: bool) -> io::Result<()> {
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || if directory {
            !metadata.is_dir()
        } else {
            !metadata.is_file()
        }
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "SlimStore settings require a regular directory and file",
        ));
    }
    Ok(())
}

fn open_directory(path: &Path) -> io::Result<File> {
    let dir = OpenOptions::new()
        .access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    check_storage_entry(&dir.metadata()?, true)?;
    Ok(dir)
}

struct SettingsLock(HANDLE);

impl SettingsLock {
    fn acquire(directory: &File) -> io::Result<Self> {
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        if unsafe { GetFileInformationByHandle(directory.as_raw_handle(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let name = crate::platform::wide(format!(
            "Global\\SlimStore-settings-{:08x}-{:08x}{:08x}",
            info.dwVolumeSerialNumber, info.nFileIndexHigh, info.nFileIndexLow
        ));
        let mutex = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if mutex.is_null() {
            return Err(io::Error::last_os_error());
        }
        let error = match unsafe { WaitForSingleObject(mutex, 150) } {
            WAIT_OBJECT_0 | WAIT_ABANDONED => return Ok(Self(mutex)),
            WAIT_TIMEOUT => io::Error::new(
                io::ErrorKind::TimedOut,
                "Another SlimStore window is saving settings",
            ),
            _ => io::Error::last_os_error(),
        };
        unsafe { CloseHandle(mutex) };
        Err(error)
    }
}

impl Drop for SettingsLock {
    fn drop(&mut self) {
        unsafe {
            ReleaseMutex(self.0);
            CloseHandle(self.0);
        }
    }
}

fn check_settings_path(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => check_storage_entry(&metadata, false),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: u8,
    pub accent: bool,
    pub source_consent: bool,
    pub offline_icons_only: bool,
    pub icon_cache: bool,
    pub scoop_catalog_cache: bool,
    pub scoop_additional_buckets: bool,
}

#[derive(Clone)]
pub struct Storage {
    dir: PathBuf,
    pub enabled: bool,
}
impl Storage {
    pub fn beside(executable: &Path) -> Self {
        let parent = executable.parent().unwrap_or(Path::new("."));
        let preferred = parent.join("SlimStore-data");
        Self {
            dir: preferred,
            enabled: false,
        }
    }
    fn read_bounded(&self, name: &str, limit: u64) -> io::Result<Vec<u8>> {
        let _dir = open_directory(&self.dir)?;
        let file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(self.dir.join(name))?;
        check_storage_entry(&file.metadata()?, false)?;
        let mut bytes = Vec::new();
        file.take(limit + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "SlimStore data file is too large",
            ));
        }
        Ok(bytes)
    }

    fn write_atomic(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let dir = open_directory(&self.dir)?;
        let _lock = SettingsLock::acquire(&dir)?;
        self.replace_atomic(name, bytes)
    }

    fn replace_atomic(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        let target = self.dir.join(name);
        check_settings_path(&target)?;
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let counter = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let temp = self.dir.join(format!(
            "slimstore-{}-{time}-{counter}.tmp",
            std::process::id()
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        let written = file.write_all(bytes).and_then(|()| file.sync_all());
        drop(file);
        let result = written.and_then(|()| crate::platform::replace_file(&temp, &target));
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
        result
    }

    pub fn read_cache(&self, file: CacheFile) -> io::Result<Vec<u8>> {
        self.read_bounded(file.name(), file.limit())
    }

    pub fn update_cache(
        &self,
        file: CacheFile,
        merge: impl FnOnce(Option<Vec<u8>>) -> Option<Vec<u8>>,
    ) -> io::Result<()> {
        let dir = open_directory(&self.dir)?;
        let _lock = SettingsLock::acquire(&dir)?;
        // A different window may have turned off persistence since this one opened.
        if !self
            .saved_settings()
            .is_some_and(|settings| settings.icon_cache)
        {
            return Ok(());
        }
        let Some(bytes) = merge(self.read_cache(file).ok()) else {
            return Ok(());
        };
        if bytes.len() as u64 > file.limit() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Package cache is too large",
            ));
        }
        self.replace_atomic(file.name(), &bytes)
    }

    pub fn clear_cache(&self) -> io::Result<()> {
        let result = (|| {
            let dir = open_directory(&self.dir)?;
            let _lock = SettingsLock::acquire(&dir)?;
            if self
                .saved_settings()
                .is_some_and(|settings| settings.icon_cache)
            {
                return Ok(());
            }
            let remove = |file: CacheFile| {
                let path = self.dir.join(file.name());
                check_settings_path(&path)?;
                match fs::remove_file(path) {
                    Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                    result => result,
                }
            };
            let icons = remove(CacheFile::Icons);
            let metadata = remove(CacheFile::Metadata);
            icons.and(metadata)
        })();
        match result {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }

    pub fn read_catalog(&self, name: &str) -> io::Result<Vec<u8>> {
        self.read_bounded(name, MAX_CATALOG)
    }

    pub fn write_catalog(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        if bytes.len() as u64 > MAX_CATALOG {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Scoop catalog index is too large",
            ));
        }
        self.write_atomic(name, bytes)
    }

    fn saved_settings(&self) -> Option<Settings> {
        self.read_bounded("settings.json", MAX_SETTINGS)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    }

    pub fn load(&mut self) -> Settings {
        let result = self.saved_settings();
        self.enabled = result.is_some();
        result.unwrap_or_default()
    }
    pub fn save(&mut self, settings: &Settings, enable: bool) -> Result<String, String> {
        if !enable {
            if self.enabled {
                let file = self.dir.join("settings.json");
                let result = (|| -> io::Result<()> {
                    let dir = open_directory(&self.dir)?;
                    let _lock = SettingsLock::acquire(&dir)?;
                    check_settings_path(&file)?;
                    fs::remove_file(&file)
                })();
                if let Err(e) = result
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    return Err(format!(
                        "Using memory for this session. Could not remove {}: {e}. Remove it manually to disable saved settings next time.",
                        file.display()
                    ));
                }
            }
            self.enabled = false;
            return Ok("Memory only · no SlimStore settings will be saved".into());
        }
        let result = (|| -> Result<(), Box<dyn std::error::Error>> {
            let bytes = serde_json::to_vec_pretty(settings)?;
            Ok(self.write_atomic("settings.json", &bytes)?)
        })();
        match result {
            Ok(()) => {
                self.enabled = true;
                Ok(format!("Settings saved in {}", self.dir.display()))
            }
            Err(e) => {
                self.enabled = false;
                Err(format!("Memory only · settings could not be saved: {e}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("slimstore-test-{name}-{}", std::process::id()))
    }
    #[test]
    fn default_load_creates_nothing() {
        let p = path("memory").join("SlimStore.exe");
        let mut s = Storage::beside(&p);
        assert_eq!(s.load().theme, 0);
        assert!(!s.enabled);
        assert!(!p.parent().unwrap().exists());
    }
    #[test]
    fn opt_in_roundtrip_and_disable() {
        let p = path("persist");
        let mut s = Storage::beside(&p.join("SlimStore.exe"));
        s.save(
            &Settings {
                theme: 2,
                ..Default::default()
            },
            true,
        )
        .unwrap();
        assert_eq!(s.load().theme, 2);
        s.save(&Settings::default(), false).unwrap();
        assert!(!s.enabled);
        std::fs::remove_dir(&s.dir).unwrap();
        std::fs::remove_dir(p).unwrap();
    }
    #[test]
    fn unwritable_location_falls_back() {
        let p = path("blocked");
        std::fs::write(&p, b"file").unwrap();
        let mut s = Storage::beside(&p.join("SlimStore.exe"));
        assert!(s.save(&Settings::default(), true).is_err());
        assert!(!s.enabled);
        std::fs::remove_file(p).unwrap();
    }
    #[test]
    fn corrupt_settings_are_optional() {
        let p = path("corrupt");
        let mut s = Storage::beside(&p.join("SlimStore.exe"));
        std::fs::create_dir_all(&s.dir).unwrap();
        std::fs::write(s.dir.join("settings.json"), b"broken").unwrap();
        assert_eq!(s.load().theme, 0);
        assert!(!s.enabled);
        std::fs::remove_file(s.dir.join("settings.json")).unwrap();
        std::fs::remove_dir(&s.dir).unwrap();
        std::fs::remove_dir(p).unwrap();
    }

    #[test]
    fn concurrent_settings_saves_leave_one_complete_file() {
        let p = path("concurrent");
        let exe = p.join("SlimStore.exe");
        let start = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            for theme in [1, 2] {
                let exe = &exe;
                let start = &start;
                scope.spawn(move || {
                    let mut storage = Storage::beside(exe);
                    start.wait();
                    storage
                        .save(
                            &Settings {
                                theme,
                                accent: theme == 2,
                                ..Default::default()
                            },
                            true,
                        )
                        .unwrap();
                });
            }
        });
        let mut storage = Storage::beside(&exe);
        let loaded = storage.load();
        assert!(matches!(
            (loaded.theme, loaded.accent),
            (1, false) | (2, true)
        ));
        assert_eq!(fs::read_dir(&storage.dir).unwrap().count(), 1);
        storage.save(&Settings::default(), false).unwrap();
        fs::remove_dir(&storage.dir).unwrap();
        fs::remove_dir(p).unwrap();
    }

    #[test]
    fn oversized_settings_ignore_an_otherwise_valid_prefix() {
        let p = path("oversized");
        let mut storage = Storage::beside(&p.join("SlimStore.exe"));
        fs::create_dir_all(&storage.dir).unwrap();
        let file = storage.dir.join("settings.json");
        fs::write(
            &file,
            format!("{{\"theme\":2}}{}", " ".repeat(MAX_SETTINGS as usize)),
        )
        .unwrap();
        assert_eq!(storage.load().theme, 0);
        assert!(!storage.enabled);
        fs::remove_file(file).unwrap();
        fs::remove_dir(&storage.dir).unwrap();
        fs::remove_dir(p).unwrap();
    }

    #[test]
    fn settings_directory_reparse_point_is_rejected() {
        let p = path("linked");
        let outside = p.join("sentinel");
        let file = outside.join("settings.json");
        let original = b"{\"theme\":1,\"accent\":true}";
        fs::create_dir_all(&outside).unwrap();
        fs::write(&file, original).unwrap();
        let mut storage = Storage::beside(&p.join("SlimStore.exe"));
        match std::os::windows::fs::symlink_dir(&outside, &storage.dir) {
            Ok(()) => {
                assert_eq!(storage.load().theme, 0);
                assert!(storage.save(&Settings::default(), true).is_err());
                assert_eq!(fs::read(&file).unwrap(), original);
                assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
                fs::remove_dir(&storage.dir).unwrap();
                println!("Verified Windows directory reparse point rejection");
            }
            Err(e) if e.raw_os_error() == Some(1314) => {
                println!("Directory reparse point fixture requires Windows symlink privileges");
            }
            Err(e) => panic!("Could not create the test directory link: {e}"),
        }
        fs::remove_file(file).unwrap();
        fs::remove_dir(outside).unwrap();
        fs::remove_dir(p).unwrap();
    }
}
