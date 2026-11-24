use crate::{
    errors::AppError,
    http, metadata,
    packages::Package,
    scoop_catalog::{self, Row},
    winget::{REQUEST_SUPERSEDED, Request, Winget, can_pin_version, run_process},
};
use serde_json::Value;
use std::{
    ffi::OsString,
    fs::{self, File},
    io::Read,
    os::windows::ffi::OsStringExt,
    path::{Component, Path, PathBuf, Prefix},
    process::Command,
};
use windows_sys::Win32::{
    System::SystemInformation::GetSystemDirectoryW, UI::Shell::IsUserAnAdmin,
};

const MAPPINGS: &str = include_str!("../catalog/scoop.txt");
const MANIFEST_LIMIT: usize = 64 * 1024;
pub const SOURCE: &str = "scoop";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    id: &'static str,
    bucket: &'static str,
    name: &'static str,
    executable: &'static str,
}

impl Entry {
    #[cfg(test)]
    pub fn qualified(self) -> String {
        format!("{}/{}", self.bucket, self.name)
    }

    #[cfg(test)]
    fn official_path(self) -> String {
        scoop_catalog::manifest_path(self.bucket, self.name).unwrap()
    }
}

fn entries() -> impl Iterator<Item = Entry> {
    MAPPINGS.lines().filter_map(|line| {
        if line.starts_with('#') {
            return None;
        }
        let (id, rest) = line.split_once('|')?;
        let (bucket, scope) = rest.split_once('|')?;
        let (name, executable) = scope.split_once('|')?;
        (matches!(bucket, "main" | "extras")
            && !name.is_empty()
            && name.len() <= 64
            && name.as_bytes()[0].is_ascii_lowercase()
            && executable.ends_with(".exe")
            && Path::new(executable)
                .components()
                .all(|c| matches!(c, Component::Normal(_)))
            && name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'))
        .then_some(Entry {
            id,
            bucket,
            name,
            executable,
        })
    })
}

pub fn mapping(id: &str) -> Option<Entry> {
    entries().find(|entry| entry.id.eq_ignore_ascii_case(id))
}

pub fn canonical_id(bucket: &str, name: &str) -> Option<&'static str> {
    entries()
        .find(|entry| entry.bucket == bucket && entry.name.eq_ignore_ascii_case(name))
        .map(|entry| entry.id)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reviewed {
    pub bucket: String,
    pub name: String,
    pub version: String,
    pub url: String,
    manifest_hash: [u8; 32],
}

impl Reviewed {
    pub fn qualified(&self) -> String {
        format!("{}/{}", self.bucket, self.name)
    }
}

#[derive(Clone)]
pub struct Scoop {
    root: PathBuf,
    global: PathBuf,
    powershell: PathBuf,
}

fn local_absolute(path: &Path) -> bool {
    path.is_absolute()
        && matches!(path.components().next(), Some(Component::Prefix(p))
            if matches!(p.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)))
}

fn path_in_file(path: &Path, root: &Path) -> bool {
    fs::canonicalize(path)
        .ok()
        .zip(fs::canonicalize(root).ok())
        .is_some_and(|(path, root)| path.starts_with(&root) && path.is_file())
}

fn small_file(path: &Path, root: &Path) -> Option<Vec<u8>> {
    if !path_in_file(path, root) {
        return None;
    }
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take((MANIFEST_LIMIT + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= MANIFEST_LIMIT).then_some(bytes)
}

fn json(bytes: &[u8]) -> Option<Value> {
    serde_json::from_slice(bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes)).ok()
}

fn manifest_version(manifest: &Value) -> Option<&str> {
    let version = manifest.get("version")?.as_str()?;
    can_pin_version(version).then_some(version)
}

fn install_version(manifest: &Value) -> Option<&str> {
    // Scoop manifests can run PowerShell or launch external installers. Accept
    // the archive/shim subset for this fallback, including architecture overrides.
    let forbidden = [
        "depends",
        "installer",
        "uninstaller",
        "pre_install",
        "post_install",
        "pre_uninstall",
        "post_uninstall",
        "innosetup",
        "msi",
        "psmodule",
        "env_add_path",
        "env_set",
    ];
    let native = manifest
        .get("architecture")
        .map_or(Some(manifest), |arches| arches.get("64bit"))?;
    for section in [manifest, native] {
        if section.as_object()?.keys().any(|key| {
            forbidden
                .iter()
                .any(|field| key.eq_ignore_ascii_case(field))
                || key.eq_ignore_ascii_case("architecture") && key != "architecture"
        }) {
            return None;
        }
    }
    let url = native
        .get("url")
        .or_else(|| manifest.get("url"))?
        .as_str()?;
    let hash = native
        .get("hash")
        .or_else(|| manifest.get("hash"))?
        .as_str()?;
    let (digest, digest_length) = match hash.split_once(':') {
        Some(("sha512", digest)) => (digest, 128),
        Some(("sha256", digest)) => (digest, 64),
        None => (hash, 64),
        _ => return None,
    };
    let (authority, _) = url.strip_prefix("https://")?.split_once('/')?;
    let file = url.split_once("#/").map_or(url, |(_, name)| name);
    let file = file.to_ascii_lowercase();
    if authority.is_empty()
        || !authority
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-".contains(&byte))
        || url.contains(['?', '\\'])
        || url.chars().any(char::is_whitespace)
        || ![".zip", ".7z", ".exe"]
            .iter()
            .any(|extension| file.ends_with(extension))
        || digest.len() != digest_length
        || !digest.bytes().all(|digit| digit.is_ascii_hexdigit())
    {
        return None;
    }
    manifest_version(manifest).filter(|v| !v.eq_ignore_ascii_case("nightly"))
}

fn manifest_url(manifest: &Value) -> Option<&str> {
    manifest["architecture"]["64bit"]["url"]
        .as_str()
        .or_else(|| manifest["url"].as_str())
}

fn official_manifest(path: &str, active: impl Fn() -> bool) -> Option<Value> {
    let bytes = http::session()?.get("raw.githubusercontent.com", path, MANIFEST_LIMIT, active)?;
    json(&bytes)
}

impl Scoop {
    pub fn detect() -> Option<Self> {
        let profile = PathBuf::from(std::env::var_os("USERPROFILE")?);
        let config_dir = std::env::var_os("XDG_CONFIG_HOME")
            .map_or_else(|| profile.join(".config"), PathBuf::from);
        let config_file = config_dir.join("scoop/config.json");
        let config = match config_file.symlink_metadata() {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Value::Null,
            _ => json(&small_file(&config_file, &config_dir)?)?,
        };
        let configured = |environment, field| {
            std::env::var_os(environment)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .or_else(|| {
                    config
                        .get(field)
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                        .map(PathBuf::from)
                })
        };
        let root = configured("SCOOP", "root_path").unwrap_or_else(|| profile.join("scoop"));
        let global = configured("SCOOP_GLOBAL", "global_path").or_else(|| {
            std::env::var_os("ProgramData").map(|data| PathBuf::from(data).join("scoop"))
        })?;
        let mut buffer = [0u16; 260];
        let length =
            unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
        if length == 0
            || length >= buffer.len()
            || !local_absolute(&root)
            || !local_absolute(&global)
        {
            return None;
        }
        let powershell = PathBuf::from(OsString::from_wide(&buffer[..length]))
            .join("WindowsPowerShell/v1.0/powershell.exe");
        let scoop = Self {
            root,
            global,
            powershell,
        };
        scoop
            .launcher()
            .filter(|_| scoop.powershell.is_file())
            .map(|_| scoop)
    }

    fn launcher(&self) -> Option<PathBuf> {
        let core = self.root.join("apps/scoop");
        let script = core.join("current/bin/scoop.ps1");
        (path_in_file(&script, &self.root) && path_in_file(&script, &core)).then_some(script)
    }

    pub fn buckets_path(&self) -> Option<PathBuf> {
        let path = self.root.join("buckets");
        let canonical = fs::canonicalize(&path).ok()?;
        (canonical == fs::canonicalize(&self.root).ok()?.join("buckets")).then_some(path)
    }

    pub fn bucket_path(&self, bucket: &str, official: bool) -> Option<PathBuf> {
        if !scoop_catalog::valid_name(bucket) {
            return None;
        }
        let buckets = self.buckets_path()?;
        let path = buckets.join(bucket);
        let canonical = fs::canonicalize(&path).ok()?;
        if canonical != fs::canonicalize(&buckets).ok()?.join(bucket) {
            return None;
        }
        if official {
            let repo = scoop_catalog::official(bucket)?;
            // A custom checkout called "extras" is not a catalog authority.
            // Official non-Git bootstrap installs can still use the online catalog.
            let config = small_file(&path.join(".git/config"), &path)?;
            let text = std::str::from_utf8(&config).ok()?;
            let mut lines = text.lines().map(str::trim);
            lines.find(|line| *line == "[remote \"origin\"]")?;
            let expected = format!("https://github.com/ScoopInstaller/{repo}");
            let url = lines
                .take_while(|line| !line.starts_with('['))
                .find_map(|line| {
                    let (key, value) = line.split_once('=')?;
                    (key.trim() == "url").then(|| value.trim().trim_end_matches(".git"))
                })?;
            if !url.eq_ignore_ascii_case(&expected) {
                return None;
            }
        }
        let nested = path.join("bucket");
        if nested.is_dir() {
            (fs::canonicalize(&nested).ok()? == canonical.join("bucket")).then_some(nested)
        } else if !official {
            Some(path)
        } else {
            None
        }
    }

    fn manifest(&self, bucket: &str, name: &str) -> Option<Vec<u8>> {
        scoop_catalog::manifest_path(bucket, name)?;
        let path = self
            .root
            .join("buckets")
            .join(bucket)
            .join("bucket")
            .join(format!("{name}.json"));
        let parent = path.parent()?;
        let expected_parent = fs::canonicalize(&self.root)
            .ok()?
            .join("buckets")
            .join(bucket)
            .join("bucket");
        if fs::canonicalize(parent).ok()? != expected_parent {
            return None;
        }
        let expected = path.file_name()?.to_str()?;
        let mut found = false;
        // Scoop resolves same-named manifests recursively. Official Main and Extras
        // are flat. Refuse a changed layout that could shadow this exact manifest.
        for (index, item) in fs::read_dir(parent).ok()?.enumerate() {
            let item = item.ok()?;
            let file_type = item.file_type().ok()?;
            if index >= 8_192 || !file_type.is_file() {
                return None;
            }
            if item.file_name().to_str()?.eq_ignore_ascii_case(expected) {
                if found {
                    return None;
                }
                found = true;
            }
        }
        found.then(|| small_file(&path, &self.root)).flatten()
    }

    fn installed_entry(root: &Path, bucket: &str, name: &str) -> Result<Option<String>, AppError> {
        let app = root.join("apps").join(name);
        match app.symlink_metadata() {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        // A directory without an identifiable current manifest can be a partial
        // install or a namesake from another bucket. Do not install over it.
        Self::installed_record(root, name)
            .and_then(|(found, manifest)| (found == bucket).then(|| manifest_version(&manifest).map(str::to_owned)).flatten())
            .map(Some)
            .ok_or_else(|| AppError(format!(
                "Scoop already has files for {bucket}/{name}. Check them in Scoop before continuing."
            )))
    }

    fn installed_record(root: &Path, name: &str) -> Option<(String, Value)> {
        let current = root.join("apps").join(name).join("current");
        if !fs::canonicalize(&current)
            .ok()?
            .starts_with(fs::canonicalize(root).ok()?.join("apps").join(name))
        {
            return None;
        }
        let read_record = |name: &str| {
            let modern = current.join(format!("scoop-{name}.json"));
            let path = match modern.symlink_metadata() {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    current.join(format!("{name}.json"))
                }
                _ => modern,
            };
            small_file(&path, root).and_then(|bytes| json(&bytes))
        };
        let installation = read_record("install")?;
        let bucket = installation["bucket"]
            .as_str()
            .filter(|bucket| scoop_catalog::valid_name(bucket))?;
        let manifest = read_record("manifest")?;
        manifest_version(&manifest)?;
        Some((bucket.to_owned(), manifest))
    }

    pub fn installed_package(&self, id: &str) -> Option<Package> {
        let entry = mapping(id)?;
        let version = self.catalog_installed(entry.bucket, entry.name)?;
        Some(Package {
            id: entry.id.into(),
            name: metadata::curated(entry.id)?.name.clone(),
            installed: Some(version.clone()),
            version,
            source: SOURCE.into(),
            ..Package::default()
        })
    }

    pub fn catalog_installed(&self, bucket: &str, name: &str) -> Option<String> {
        Self::installed_entry(&self.root, bucket, name)
            .ok()
            .flatten()
            .or_else(|| {
                Self::installed_entry(&self.global, bucket, name)
                    .ok()
                    .flatten()
            })
    }

    pub fn review_block(&self, row: &Row) -> Option<&'static str> {
        if scoop_catalog::official(&row.bucket).is_none() {
            return Some("Use Scoop directly for packages from additional buckets.");
        }
        if unsafe { IsUserAnAdmin() } != 0 {
            return Some("Run SlimStore in a standard-user session to review this install.");
        }
        let Some(bytes) = self.manifest(&row.bucket, &row.name) else {
            return Some(
                "Add or refresh this official bucket in Scoop to review the install here.",
            );
        };
        if !json(&bytes).is_some_and(|manifest| install_version(&manifest).is_some()) {
            return Some(
                "Use Scoop directly for this package. SlimStore can review only compatible, hashed archives and executables without install scripts or dependencies.",
            );
        }
        None
    }

    pub fn inventory(&self) -> Vec<Package> {
        let rows = [&self.root, &self.global].into_iter().flat_map(|root| {
            fs::read_dir(root.join("apps"))
                .into_iter()
                .flatten()
                .flatten()
                .take(2_000)
                .filter_map(move |app| {
                    let name = app.file_name().into_string().ok()?;
                    if !scoop_catalog::valid_name(&name) {
                        return None;
                    }
                    let (bucket, manifest) = Self::installed_record(root, &name)?;
                    if let Some(id) = canonical_id(&bucket, &name) {
                        return self.installed_package(id);
                    }
                    let mut package =
                        scoop_catalog::installed_row(&bucket, &name, &manifest).package(None);
                    package.installed = Some(package.version.clone());
                    Some(package)
                })
        });
        crate::packages::deduplicate(rows.collect())
    }

    pub fn icon_executable(&self, id: &str) -> Option<PathBuf> {
        let entry = mapping(id)?;
        [&self.root, &self.global].into_iter().find_map(|root| {
            Self::installed_entry(root, entry.bucket, entry.name)
                .ok()
                .flatten()?;
            let current = root.join("apps").join(entry.name).join("current");
            let executable = current.join(entry.executable);
            (path_in_file(&executable, root) && path_in_file(&executable, &current))
                .then_some(executable)
        })
    }

    pub fn catalog_icon(&self, bucket: &str, name: &str) -> Option<PathBuf> {
        if let Some(path) = canonical_id(bucket, name).and_then(|id| self.icon_executable(id)) {
            return Some(path);
        }
        [&self.root, &self.global].into_iter().find_map(|root| {
            let (actual, manifest) = Self::installed_record(root, name)?;
            if actual != bucket {
                return None;
            }
            let current = root.join("apps").join(name).join("current");
            let mut candidates = Vec::new();
            for section in [&manifest, &manifest["architecture"]["64bit"]] {
                for key in ["bin", "shortcuts"] {
                    if let Some(items) = section[key].as_array() {
                        candidates.extend(items.iter().take(16).filter_map(|item| {
                            item.as_str().or_else(|| item.as_array()?.first()?.as_str())
                        }));
                    } else if let Some(candidate) = section[key].as_str() {
                        candidates.push(candidate);
                    }
                }
            }
            let named = format!("{name}.exe");
            candidates.push(&named);
            candidates.into_iter().find_map(|candidate| {
                let path = Path::new(candidate);
                if !path
                    .components()
                    .all(|component| matches!(component, Component::Normal(_)))
                    || path
                        .extension()
                        .and_then(|s| s.to_str())
                        .is_none_or(|ext| !ext.eq_ignore_ascii_case("exe"))
                {
                    return None;
                }
                let executable = current.join(path);
                (path_in_file(&executable, root) && path_in_file(&executable, &current))
                    .then_some(executable)
            })
        })
    }

    pub fn check_absent(&self, id: &str) -> Result<(), AppError> {
        if let Some(entry) = mapping(id) {
            self.check_target_absent(entry.bucket, entry.name)?;
        }
        Ok(())
    }

    fn check_target_absent(&self, bucket: &str, name: &str) -> Result<(), AppError> {
        if Self::installed_entry(&self.root, bucket, name)?.is_some()
            || Self::installed_entry(&self.global, bucket, name)?.is_some()
        {
            return Err(AppError(format!(
                "Already installed through Scoop ({bucket}/{name})."
            )));
        }
        Ok(())
    }

    pub fn review(
        &self,
        winget: &Winget,
        package: &Package,
        consent: bool,
        obsolete: impl Fn() -> bool,
    ) -> Result<Package, AppError> {
        let (bucket, name) = if let Some(row) = package.scoop_catalog.as_deref() {
            (row.bucket.as_str(), row.name.as_str())
        } else {
            let entry = mapping(&package.id)
                .filter(|_| package.manageable())
                .ok_or_else(|| {
                    AppError("There is no reviewed Scoop mapping for this package.".into())
                })?;
            (entry.bucket, entry.name)
        };
        let upstream_path = scoop_catalog::manifest_path(bucket, name).ok_or_else(|| {
            AppError("Open the package in Scoop to install from this bucket.".into())
        })?;
        self.check_can_install(bucket, name, winget, package, consent, &obsolete)?;
        let bytes = self.manifest(bucket, name).ok_or_else(|| AppError(format!(
            "Scoop's {bucket}/{name} manifest is missing or unreadable. Check the installed bucket in Scoop."
        )))?;
        let local = json(&bytes)
            .ok_or_else(|| AppError("Scoop returned an unreadable local manifest.".into()))?;
        let version = install_version(&local).ok_or_else(|| AppError(
            "This Scoop manifest is outside SlimStore's verified archive and executable install subset.".into()
        ))?;
        let upstream = official_manifest(&upstream_path, || !obsolete());
        if obsolete() {
            return Err(AppError(REQUEST_SUPERSEDED.into()));
        }
        if upstream.as_ref() != Some(&local) {
            return Err(AppError(format!(
                "Could not match {bucket}/{name} to the current official Scoop manifest. Refresh the official bucket in Scoop and review again."
            )));
        }
        let mut reviewed = package.clone();
        reviewed.version = version.into();
        reviewed.scoop = Some(Box::new(Reviewed {
            bucket: bucket.into(),
            name: name.into(),
            version: version.into(),
            url: manifest_url(&local).unwrap_or_default().into(),
            manifest_hash: http::sha256(&bytes)
                .ok_or_else(|| AppError("Could not verify the Scoop manifest hash.".into()))?,
        }));
        Ok(reviewed)
    }

    fn check_can_install(
        &self,
        bucket: &str,
        name: &str,
        winget: &Winget,
        package: &Package,
        consent: bool,
        obsolete: impl Fn() -> bool,
    ) -> Result<(), AppError> {
        if obsolete() {
            return Err(AppError(REQUEST_SUPERSEDED.into()));
        }
        if unsafe { IsUserAnAdmin() } != 0 {
            return Err(AppError(
                "Run SlimStore as a standard user to install through Scoop.".into(),
            ));
        }
        self.check_target_absent(bucket, name)?;
        if package.installed.is_some() {
            return Err(AppError("Already installed.".into()));
        }
        let alias = canonical_id(bucket, name).map(|id| Package {
            id: id.into(),
            source: crate::winget::SOURCE.into(),
            ..Package::default()
        });
        let winget_package = if package.manageable() {
            Some(package)
        } else {
            alias.as_ref()
        };
        if winget.executable.is_some()
            && let Some(package) = winget_package
            && winget
                .exact_package(
                    &Request::InstalledExact(package.id.clone(), None),
                    package,
                    true,
                    consent,
                    obsolete,
                )?
                .is_some()
        {
            return Err(AppError("Already installed according to WinGet.".into()));
        }
        Ok(())
    }

    pub fn install(
        &self,
        winget: &Winget,
        package: &Package,
        reviewed: &Reviewed,
        consent: bool,
        obsolete: impl Fn() -> bool,
        progress: &mut dyn FnMut(String),
    ) -> Result<(bool, bool, String), AppError> {
        self.check_can_install(
            &reviewed.bucket,
            &reviewed.name,
            winget,
            package,
            consent,
            &obsolete,
        )?;
        let upstream = scoop_catalog::manifest_path(&reviewed.bucket, &reviewed.name)
            .and_then(|path| official_manifest(&path, || !obsolete()));
        if obsolete() {
            return Err(AppError(REQUEST_SUPERSEDED.into()));
        }
        let local = self
            .manifest(&reviewed.bucket, &reviewed.name)
            .filter(|bytes| http::sha256(bytes) == Some(reviewed.manifest_hash))
            .and_then(|bytes| json(&bytes));
        if upstream.is_none() || local != upstream {
            return Err(AppError(
                "The reviewed Scoop manifest changed or its official copy is unavailable. Review the install again.".into(),
            ));
        }
        let script = self
            .launcher()
            .ok_or_else(|| AppError("The existing Scoop launcher could not be verified.".into()))?;
        let command = self.install_command(&script, &reviewed.qualified());
        let output = run_process(command, "Scoop", true, obsolete, progress)?;
        let installed = Self::installed_entry(&self.root, &reviewed.bucket, &reviewed.name)
            .ok()
            .flatten();
        let verified = installed.as_deref() == Some(reviewed.version.as_str());
        Ok((
            output.code == 0,
            verified,
            format!("Scoop exit code 0x{:08X}\n\n{}", output.code, output.text),
        ))
    }

    fn install_command(&self, script: &Path, qualified: &str) -> Command {
        let mut command = Command::new(&self.powershell);
        command
            .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-File"])
            .arg(script)
            .args([
                "install",
                qualified,
                "--no-update-scoop",
                "--independent",
                "--arch",
                "64bit",
            ])
            .current_dir(self.powershell.parent().expect("system PowerShell path"))
            .env("SCOOP", &self.root)
            .env("SCOOP_GLOBAL", &self.global);
        command
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scoop_uses_exact_curated_app_and_fixed_noninteractive_arguments() {
        let mut ids = std::collections::HashSet::new();
        for entry in entries() {
            assert!(ids.insert(entry.id.to_ascii_lowercase()));
            assert!(metadata::curated(entry.id).is_some());
            assert!(crate::icons::slot_for(entry.id) > 0);
        }
        let entry = mapping("kde.dolphin").unwrap();
        assert!(mapping("DolphinEmulator.Dolphin").is_none());
        let scoop = Scoop {
            root: "C:/test/scoop".into(),
            global: "C:/test/global".into(),
            powershell: "C:/Windows/System32/WindowsPowerShell/v1.0/powershell.exe".into(),
        };
        let script = Path::new("C:/test/scoop/apps/scoop/current/bin/scoop.ps1");
        let command = scoop.install_command(script, &entry.qualified());
        let arguments: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect();
        assert_eq!(
            arguments,
            [
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-File",
                script.to_str().unwrap(),
                "install",
                "extras/dolphin",
                "--no-update-scoop",
                "--independent",
                "--arch",
                "64bit",
            ]
        );
    }

    #[test]
    fn rejects_unhashed_non_https_scripted_and_incompatible_manifests() {
        let good = json!({"version": "2.0", "architecture": {"64bit": {
            "url": "https://example.com/app.zip", "hash": "a".repeat(64)
        }}});
        assert_eq!(install_version(&good), Some("2.0"));
        let mut sha512 = good.clone();
        sha512["architecture"]["64bit"]["hash"] = json!(format!("sha512:{}", "b".repeat(128)));
        assert_eq!(install_version(&sha512), Some("2.0"));
        for (key, data) in [
            ("url", json!("http://example.com/app.zip")),
            ("url", json!("https://user@example.com/app.zip")),
            ("url", json!("https://example.com/setup.msi")),
            ("hash", json!("a".repeat(32))),
            ("hash", json!(format!("sha1:{}", "a".repeat(40)))),
            ("pre_install", json!("Write-Host ignored")),
            ("POST_INSTALL", json!("Write-Host ignored")),
            ("depends", json!("git")),
        ] {
            let mut edited = good.clone();
            edited["architecture"]["64bit"][key] = data;
            assert_eq!(install_version(&edited), None, "{key}");
        }
        assert_eq!(
            install_version(&json!({"version":"2.0","architecture":{"arm64":{}}})),
            None
        );
    }

    #[test]
    fn recognizes_both_scoop_record_formats_and_rejects_the_wrong_bucket() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("slimstore-scoop-{}-{unique}", std::process::id()));
        let entry = mapping("PuTTY.PuTTY").unwrap();
        fs::create_dir(&root).unwrap();
        let apps = root.join("apps");
        let app = apps.join("putty");
        let current = app.join("current");
        let folders = [&apps, &app, &current];
        let files = [
            "scoop-manifest.json",
            "scoop-install.json",
            "manifest.json",
            "install.json",
        ];
        let results = (|| -> Result<[bool; 3], AppError> {
            for dir in folders {
                fs::create_dir(dir)?;
            }
            let partial_rejected = Scoop::installed_entry(&root, entry.bucket, entry.name).is_err();
            fs::write(current.join(files[0]), r#"{"version":"0.85"}"#)?;
            fs::write(current.join(files[1]), r#"{"bucket":"extras"}"#)?;
            let modern_verified =
                Scoop::installed_entry(&root, entry.bucket, entry.name)? == Some("0.85".into());
            fs::remove_file(current.join(files[0]))?;
            fs::remove_file(current.join(files[1]))?;
            fs::write(current.join(files[2]), r#"{"version":"0.85"}"#)?;
            fs::write(current.join(files[3]), r#"{"bucket":"main"}"#)?;
            let wrong_rejected = Scoop::installed_entry(&root, entry.bucket, entry.name).is_err();
            fs::write(current.join(files[3]), r#"{"bucket":"extras"}"#)?;
            let legacy_verified =
                Scoop::installed_entry(&root, entry.bucket, entry.name)? == Some("0.85".into());
            Ok([
                partial_rejected,
                modern_verified,
                wrong_rejected && legacy_verified,
            ])
        })();
        for file in files {
            let _ = fs::remove_file(current.join(file));
        }
        for dir in folders.into_iter().rev() {
            let _ = fs::remove_dir(dir);
        }
        fs::remove_dir(root).unwrap();
        assert_eq!(results.unwrap(), [true; 3]);
    }

    #[test]
    #[ignore = "downloads the official Scoop manifests for read-only verification"]
    fn live_scoop_fallback_manifests() {
        let session = http::session().unwrap();
        for entry in entries() {
            let bytes = session
                .get(
                    "raw.githubusercontent.com",
                    &entry.official_path(),
                    MANIFEST_LIMIT,
                    || true,
                )
                .unwrap_or_else(|| panic!("{} manifest is unavailable", entry.qualified()));
            let manifest = super::json(&bytes).unwrap();
            assert!(
                install_version(&manifest).is_some(),
                "Unsupported manifest for {}",
                entry.qualified()
            );
        }
    }
}
