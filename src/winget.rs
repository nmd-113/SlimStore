//! Compatibility boundary for WinGet's human-readable CLI. Read the column positions
//! that WinGet prints for each table, and keep `show` metadata verbatim.
//! Reject ambiguous/truncated IDs. Keep `show` metadata verbatim rather than translating labels.
use crate::{
    errors::{AppError, describe_exit},
    metadata::clean_output,
    packages::{Package, deduplicate, valid_id},
};
use std::{
    collections::VecDeque,
    io::Read,
    os::windows::process::CommandExt,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use unicode_width::UnicodeWidthChar;

pub const SOURCE: &str = "winget";
const SOURCE_URL: &str = "https://cdn.winget.microsoft.com/cache";
pub(crate) const SOURCE_PACKAGE: &str = "Microsoft.Winget.Source_8wekyb3d8bbwe";
pub const NO_PACKAGES: u32 = 0x8a150014;
pub const NO_UPDATES: u32 = 0x8a15002b;
pub(crate) const REQUEST_SUPERSEDED: &str = "Request superseded.";
pub const EFFECTCRAFT_ID: &str = "ArtCraft.EffectCraft";
pub const EFFECTCRAFT_SCOPE_FIX_VERSION: &str = "0.25.1";
const OUTPUT_LIMIT: usize = 512 * 1024;

#[derive(Clone, Copy, Debug)]
pub enum Scope {
    Machine,
    User,
}

#[derive(Clone, Debug)]
pub enum Request {
    Version,
    Sources,
    Search(String),
    Tag(String),
    Details(String),
    CatalogExact(String),
    Installed,
    Updates,
    InstalledExact(String, Option<Scope>),
    UpdateExact(String),
    Change {
        verb: &'static str,
        id: String,
        version: Option<String>,
        accept_package: bool,
        effectcraft_machine_update: bool,
    },
}

pub fn can_pin_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 256
        && version.trim() == version
        && !version.eq_ignore_ascii_case("unknown")
        && !version.starts_with('-')
        && !version.contains(['<', '>', '…'])
        && !version.chars().any(char::is_control)
}

pub fn arguments(request: &Request, accept_source: bool) -> Result<Vec<String>, AppError> {
    let mut args: Vec<String> = match request {
        Request::Version => return Ok(vec!["--version".into()]),
        Request::Sources => {
            return Ok(vec![
                "source".into(),
                "export".into(),
                "--disable-interactivity".into(),
            ]);
        }
        Request::Search(query) => {
            let q = query.trim();
            if q.is_empty() || q.len() > 256 || q.chars().any(char::is_control) {
                return Err(AppError(
                    "Enter a search between 1 and 256 characters.".into(),
                ));
            }
            vec![
                "search".into(),
                "--query".into(),
                q.into(),
                "--count".into(),
                "250".into(),
            ]
        }
        Request::Tag(tag) => vec![
            "search".into(),
            "--tag".into(),
            tag.clone(),
            "--count".into(),
            "250".into(),
        ],
        Request::Details(id)
        | Request::InstalledExact(id, _)
        | Request::UpdateExact(id)
        | Request::CatalogExact(id) => {
            if !valid_id(id) {
                return Err(AppError(
                    "This package ID is incomplete or unsupported. No command was run.".into(),
                ));
            }
            let mut args = vec![
                match request {
                    Request::Details(_) => "show",
                    Request::CatalogExact(_) => "search",
                    _ => "list",
                }
                .into(),
                "--id".into(),
                id.clone(),
                "--exact".into(),
            ];
            if let Request::InstalledExact(_, Some(scope)) = request {
                args.extend([
                    "--scope".into(),
                    match scope {
                        Scope::Machine => "machine",
                        Scope::User => "user",
                    }
                    .into(),
                ]);
            }
            if matches!(request, Request::UpdateExact(_)) {
                args.push("--upgrade-available".into());
            }
            args
        }
        Request::Installed => vec!["list".into()],
        Request::Updates => vec!["list".into(), "--upgrade-available".into()],
        Request::Change {
            verb,
            id,
            version,
            accept_package,
            effectcraft_machine_update,
        } => {
            if !["install", "upgrade", "uninstall"].contains(verb) || !valid_id(id) {
                return Err(AppError(
                    "An exact, supported package ID and operation are required.".into(),
                ));
            }
            if *verb != "uninstall" && !accept_package {
                return Err(AppError(
                    "Review and accept the publisher's agreements before changing this app.".into(),
                ));
            }
            let mut a = vec![(*verb).into(), "--id".into(), id.clone(), "--exact".into()];
            if let Some(version) = version.as_ref().filter(|v| can_pin_version(v)) {
                a.extend(["--version".into(), version.clone()]);
            } else if *verb != "uninstall" || version.is_some() {
                return Err(AppError("A verified target version is required. Refresh the package and review it again.".into()));
            }
            if *verb != "uninstall" {
                // WinGet must choose a compatible installer, including x86-only apps on x64 Windows.
                // On upgrades this also preserves WinGet's installed-architecture matching.
                a.push("--accept-package-agreements".into());
            }
            if *verb == "install" {
                // An external installer can change inventory after our final check.
                a.push("--no-upgrade".into());
            }
            if *effectcraft_machine_update {
                if *verb != "upgrade"
                    || !id.eq_ignore_ascii_case(EFFECTCRAFT_ID)
                    || version.as_deref() != Some(EFFECTCRAFT_SCOPE_FIX_VERSION)
                {
                    return Err(AppError(
                        "The MSI scope fix applies only to EffectCraft upgrades.".into(),
                    ));
                }
                a.extend(
                    [
                        "--scope",
                        "machine",
                        "--installer-type",
                        "wix",
                        "--custom",
                        "ALLUSERS=1",
                    ]
                    .map(String::from),
                );
            }
            a
        }
    };
    args.extend([
        "--source".into(),
        SOURCE.into(),
        "--disable-interactivity".into(),
    ]);
    if accept_source {
        args.push("--accept-source-agreements".into());
    }
    Ok(args)
}

#[derive(Clone)]
pub struct Winget {
    pub executable: Option<PathBuf>,
}

#[derive(Debug)]
pub struct Output {
    pub code: u32,
    pub text: String,
}
impl Output {
    pub fn success(&self) -> bool {
        [0, 3010, 0x8a150109].contains(&self.code)
    }
    pub fn checked(self) -> Result<Self, AppError> {
        if self.success() {
            Ok(self)
        } else {
            Err(AppError(format!(
                "{} (0x{:08X})\n\n{}",
                describe_exit(self.code),
                self.code,
                self.text
            )))
        }
    }
}

fn reader(
    mut pipe: impl Read + Send + 'static,
    buffer: Arc<Mutex<VecDeque<u8>>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut bytes = [0u8; 8192];
        while let Ok(n) = pipe.read(&mut bytes) {
            if n == 0 {
                break;
            }
            let mut b = buffer.lock().unwrap_or_else(|e| e.into_inner());
            let excess = (b.len() + n).saturating_sub(OUTPUT_LIMIT);
            b.drain(..excess);
            b.extend(&bytes[..n]);
        }
    })
}

impl Winget {
    pub fn detect() -> Self {
        Self {
            executable: std::env::var_os("SLIMSTORE_TEST_MISSING_WINGET")
                .is_none()
                .then(crate::platform::registered_winget)
                .flatten(),
        }
    }

    pub fn run(
        &self,
        request: &Request,
        accept_source: bool,
        obsolete: impl Fn() -> bool,
        progress: impl FnMut(String),
    ) -> Result<Output, AppError> {
        if !matches!(request, Request::Version | Request::Sources) {
            let sources = self
                .execute(&Request::Sources, false, &obsolete, |_| {})?
                .checked()?;
            if !verified_source(&configured_sources(&sources.text)) {
                return Err(AppError("The default Microsoft WinGet community source could not be verified. Review the configured sources in Settings.".into()));
            }
        }
        self.execute(request, accept_source, obsolete, progress)
    }

    fn execute(
        &self,
        request: &Request,
        accept_source: bool,
        obsolete: impl Fn() -> bool,
        progress: impl FnMut(String),
    ) -> Result<Output, AppError> {
        let executable = self.executable.as_ref().ok_or_else(|| AppError("WinGet is unavailable. Install or repair Microsoft App Installer using the official instructions in Settings. SlimStore does not install WinGet automatically.".into()))?;
        let args = arguments(request, accept_source)?;
        let change = matches!(request, Request::Change { .. });
        let mut command = Command::new(executable);
        command
            .args(args)
            .current_dir(executable.parent().expect("registered WinGet package path"));
        run_process(command, "WinGet", change, obsolete, progress)
    }

    pub fn read(
        &self,
        request: &Request,
        consent: bool,
        obsolete: impl Fn() -> bool,
    ) -> Result<Output, AppError> {
        self.run(request, consent, obsolete, |_| {})
    }

    pub fn review_package(
        &self,
        verb: &str,
        package: &Package,
        consent: bool,
        obsolete: impl Fn() -> bool + Copy,
    ) -> Result<Package, AppError> {
        if verb == "install" {
            return self
                .review_install(package, consent, obsolete)?
                .ok_or_else(|| AppError("WinGet could not verify a catalog version.".into()));
        }
        let lookup = |request, installed| {
            self.exact_package(&request, package, installed, consent, obsolete)
        };
        let installed = lookup(Request::InstalledExact(package.id.clone(), None), true)?;
        match verb {
            "uninstall" | "upgrade" => {
                let installed = installed.ok_or_else(|| AppError("No longer installed.".into()))?;
                if installed.installed != package.installed {
                    return Err(AppError(
                        "The installed version changed. Refresh and review the package again."
                            .into(),
                    ));
                }
                if verb == "uninstall" {
                    return Ok(installed);
                }
                lookup(Request::UpdateExact(package.id.clone()), true)?
                    .filter(|found| {
                        found.update_eligible()
                            && found.installed == installed.installed
                            && found.available == package.available
                            && found.available.as_deref().is_some_and(can_pin_version)
                    })
                    .ok_or_else(|| AppError("The reviewed update is no longer available. Refresh Updates and review the package again.".into()))
            }
            _ => Err(AppError("Unsupported package operation.".into())),
        }
    }

    /// A clean exact catalog miss is distinct from an unsuccessful WinGet check.
    pub fn review_install(
        &self,
        package: &Package,
        consent: bool,
        obsolete: impl Fn() -> bool + Copy,
    ) -> Result<Option<Package>, AppError> {
        let installed = self.exact_package(
            &Request::InstalledExact(package.id.clone(), None),
            package,
            true,
            consent,
            obsolete,
        )?;
        if installed.is_some() {
            return Err(AppError("Already installed.".into()));
        }
        let Some(catalog) = self.exact_package(
            &Request::CatalogExact(package.id.clone()),
            package,
            false,
            consent,
            obsolete,
        )?
        else {
            return Ok(None);
        };
        if !can_pin_version(&catalog.version) {
            return Err(AppError(
                "WinGet could not verify a catalog version.".into(),
            ));
        }
        let mut reviewed = package.clone();
        reviewed.version = catalog.version;
        reviewed.scoop = None;
        Ok(Some(reviewed))
    }

    pub fn exact_package(
        &self,
        request: &Request,
        package: &Package,
        installed: bool,
        consent: bool,
        obsolete: impl Fn() -> bool,
    ) -> Result<Option<Package>, AppError> {
        // Keep WinGet's structured "no matching packages" exit code intact so
        // an exact catalog miss can be reviewed by the Scoop fallback.
        let output = self.run(request, consent, obsolete, |_| {})?;
        let table = parse_table(&output, installed)?;
        if table.ambiguous || table.skipped != 0 {
            return Err(AppError(
                "WinGet returned incomplete or ambiguous inventory. Refresh and try again.".into(),
            ));
        }
        match table.packages.as_slice() {
            [] => Ok(None),
            [found] if found.key() == package.key() => Ok(Some(found.clone())),
            _ => Err(AppError(
                "WinGet could not verify a single exact package. Refresh and try again.".into(),
            )),
        }
    }
}

pub(crate) fn run_process(
    mut command: Command,
    name: &str,
    change: bool,
    obsolete: impl Fn() -> bool,
    mut progress: impl FnMut(String),
) -> Result<Output, AppError> {
    if obsolete() {
        return Err(AppError(REQUEST_SUPERSEDED.into()));
    }
    let mut child = command
        .creation_flags(0x08000000)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| AppError(format!("Could not start {name}: {error}.")))?;
    let stdout = Arc::new(Mutex::new(VecDeque::new()));
    let stderr = Arc::new(Mutex::new(VecDeque::new()));
    let a = reader(child.stdout.take().unwrap(), stdout.clone());
    let b = reader(child.stderr.take().unwrap(), stderr.clone());
    let started = Instant::now();
    let mut last_message = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if !change && (obsolete() || started.elapsed() > Duration::from_secs(90)) {
            let _ = child.kill();
            let _ = child.wait();
            // Readers normally terminate immediately; detached here to avoid blocking on an inherited pipe.
            return Err(AppError(if obsolete() {
                REQUEST_SUPERSEDED.into()
            } else {
                format!("{name} timed out. Check connectivity and try again.")
            }));
        }
        if change && last_message.elapsed() > Duration::from_millis(700) {
            let data: Vec<u8> = stdout
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .copied()
                .collect();
            let mut text = clean_output(&String::from_utf8_lossy(&data));
            if started.elapsed() > Duration::from_secs(45 * 60) {
                text.push_str("\nThis installer is taking longer than 45 minutes. Check any installer or UAC windows. SlimStore will keep waiting to avoid interrupting a system change.");
            }
            progress(text);
            last_message = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(80));
    };
    // Some installers inherit stdout handles. Bound the wait after a manager exits.
    let until = Instant::now() + Duration::from_secs(2);
    while (!a.is_finished() || !b.is_finished()) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
    }
    if a.is_finished() {
        let _ = a.join();
    }
    if b.is_finished() {
        let _ = b.join();
    }
    let out: Vec<u8> = stdout
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .copied()
        .collect();
    let err: Vec<u8> = stderr
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .copied()
        .collect();
    let text = clean_output(&format!(
        "{}\n{}",
        String::from_utf8_lossy(&out),
        String::from_utf8_lossy(&err)
    ));
    Ok(Output {
        code: status.code().unwrap_or(-1) as u32,
        text,
    })
}

#[derive(Debug)]
pub struct Table {
    pub packages: Vec<Package>,
    pub skipped: usize,
    pub ambiguous: bool,
}

fn column_starts(header: &str) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut column = 0;
    let mut boundary = true;
    for c in header.chars() {
        let whitespace = c.is_whitespace();
        if boundary && !whitespace {
            starts.push(column);
        }
        boundary = whitespace;
        column += c.width().unwrap_or(0);
    }
    starts
}

fn column_fields<'a>(line: &'a str, starts: &[usize]) -> Vec<&'a str> {
    let mut fields = Vec::with_capacity(starts.len());
    let (mut column, mut previous_byte) = (0, 0);
    let mut ends = starts.iter().skip(1).peekable();
    for (byte, c) in line.char_indices() {
        if ends.peek().is_some_and(|&&end| end == column) {
            fields.push(&line[previous_byte..byte]);
            previous_byte = byte;
            ends.next();
        }
        column += c.width().unwrap_or(0);
    }
    fields.push(&line[previous_byte..]);
    fields
}

pub fn parse_table(output: &Output, installed: bool) -> Result<Table, AppError> {
    if [NO_PACKAGES, NO_UPDATES].contains(&output.code) {
        return Ok(Table {
            packages: vec![],
            skipped: 0,
            ambiguous: false,
        });
    }
    if !output.success() {
        return Err(AppError(format!(
            "{} (0x{:08X})\n{}",
            describe_exit(output.code),
            output.code,
            output.text
        )));
    }
    let clean = clean_output(&output.text);
    let mut header = "";
    let mut columns = Vec::new();
    let mut packages = Vec::new();
    let mut skipped = 0;
    for line in clean.lines() {
        if line.len() >= 8 && line.bytes().all(|c| c == b'-') {
            columns = column_starts(header);
            continue;
        }
        if columns.is_empty() {
            header = line;
            continue;
        }
        if columns.len() < 3 || line.is_empty() {
            continue;
        }
        let fields = column_fields(line, &columns);
        let [name, id, version, rest @ ..] = fields.as_slice() else {
            continue;
        };
        // Real rows are padded at the ID boundary. Summaries and truncation
        // notices after the table do not have package columns.
        let id_token = id.trim_end();
        if !name.ends_with(' ')
            || !id.ends_with(' ')
            || id_token.is_empty()
            || version.trim().is_empty()
        {
            continue;
        }
        if !valid_id(id_token) {
            if id_token.contains(['.', '…']) && !id_token.chars().any(char::is_whitespace) {
                skipped += 1;
            }
            continue;
        }
        let version = version.trim().to_string();
        let available = installed
            .then(|| rest.first().map(|s| s.trim()))
            .flatten()
            .filter(|v| !v.is_empty() && *v != SOURCE)
            .map(str::to_string);
        packages.push(Package {
            id: id_token.into(),
            name: name.trim_end().into(),
            version: version.clone(),
            installed: installed.then_some(version),
            available,
            source: SOURCE.into(),
            scoop: None,
            scoop_catalog: None,
        });
    }
    if packages.is_empty() {
        return Err(AppError(format!(
            "This WinGet output format is not supported. No package actions were enabled.\n\n{}",
            clean
        )));
    }
    let row_count = packages.len();
    let packages = deduplicate(packages);
    let ambiguous = packages.len() < row_count;
    Ok(Table {
        packages,
        skipped,
        ambiguous,
    })
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Source {
    name: String,
    arg: String,
    #[serde(default)]
    identifier: String,
    #[serde(rename = "Type", default)]
    kind: String,
    #[serde(default)]
    trust_level: Vec<String>,
}

fn configured_sources(text: &str) -> Vec<Source> {
    text.lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn verified_source(sources: &[Source]) -> bool {
    let mut matches = sources
        .iter()
        .filter(|s| s.name.eq_ignore_ascii_case(SOURCE));
    matches.next().is_some_and(|s| {
        s.name == SOURCE
            && s.arg.trim_end_matches('/') == SOURCE_URL
            && s.identifier == SOURCE_PACKAGE
            && s.kind == "Microsoft.PreIndexed.Package"
            && s.trust_level.iter().any(|level| level == "Trusted")
            && s.trust_level.iter().any(|level| level == "StoreOrigin")
    }) && matches.next().is_none()
}

pub fn source_summary(text: &str) -> String {
    let sources = configured_sources(text);
    let lines = sources
        .iter()
        .map(|s| format!("{} · {}", s.name, s.arg))
        .collect::<Vec<_>>()
        .join("\n");
    let status = if verified_source(&sources) {
        "Microsoft WinGet community source verified."
    } else {
        "The default Microsoft WinGet community source could not be verified. Package actions are unavailable."
    };
    format!(
        "{}\n\n{status}",
        if sources.is_empty() { text } else { &lines }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    const TRUSTED_SOURCE: &str = r#"{"Name":"winget","Arg":"https://cdn.winget.microsoft.com/cache","Identifier":"Microsoft.Winget.Source_8wekyb3d8bbwe","Type":"Microsoft.PreIndexed.Package","TrustLevel":["Trusted","StoreOrigin"]}"#;

    fn output(text: &str) -> Output {
        Output {
            code: 0,
            text: text.into(),
        }
    }
    #[test]
    fn detects_repeated_exact_ids_without_losing_inventory_matches() {
        let output = output(
            "Name Id    Version\n--------------------------\nOne  A.One 1.0\nOne  A.One 2.0\n",
        );
        let table = parse_table(&output, true).unwrap();
        assert!(table.ambiguous);
        assert_eq!(table.skipped, 0);
        assert_eq!(table.packages.len(), 1);
    }
    #[test]
    fn exact_catalog_no_packages_exit_is_a_clean_miss() {
        let table = parse_table(
            &Output {
                code: NO_PACKAGES,
                text: "No package found matching input criteria.".into(),
            },
            false,
        )
        .unwrap();
        assert!(table.packages.is_empty());
        assert_eq!(table.skipped, 0);
        assert!(!table.ambiguous);
    }
    #[test]
    #[ignore = "read-only live WinGet source integration"]
    fn live_search_inventory_details_sources() {
        let w = Winget::detect();
        assert_eq!(w.executable, crate::platform::registered_winget());
        let version = w
            .read(&Request::Version, false, || false)
            .unwrap()
            .checked()
            .unwrap();
        println!("WinGet: {}", version.text);
        let search = w
            .read(&Request::Search("Mozilla.Firefox".into()), false, || false)
            .unwrap();
        let results = parse_table(&search, false).unwrap();
        assert!(results.packages.iter().any(|p| p.id == "Mozilla.Firefox"));
        assert_eq!(results.skipped, 0);
        let installed =
            parse_table(&w.read(&Request::Installed, false, || false).unwrap(), true).unwrap();
        println!(
            "Recognized installed: {} ({} skipped)",
            installed.packages.len(),
            installed.skipped
        );
        assert_eq!(installed.skipped, 0);
        let updates =
            parse_table(&w.read(&Request::Updates, false, || false).unwrap(), true).unwrap();
        println!("Updates: {}", updates.packages.len());
        assert!(updates.packages.iter().all(|p| p.available.is_some()));
        assert_eq!(updates.skipped, 0);
        for entry in updates
            .packages
            .iter()
            .filter(|p| p.update_eligible())
            .take(2)
        {
            assert_eq!(
                w.review_package("upgrade", entry, false, || false)
                    .unwrap()
                    .available,
                entry.available
            );
        }
        let details = w
            .read(&Request::Details("Mozilla.Firefox".into()), false, || false)
            .unwrap()
            .checked()
            .unwrap();
        assert!(details.text.contains("Mozilla.Firefox"));
        let absent = w
            .read(
                &Request::InstalledExact("SlimStore.NonexistentTestPackage".into(), None),
                false,
                || false,
            )
            .unwrap();
        assert!(parse_table(&absent, true).unwrap().packages.is_empty());
        let sources = w
            .read(&Request::Sources, false, || false)
            .unwrap()
            .checked()
            .unwrap();
        assert!(verified_source(&configured_sources(&sources.text)));
        println!("{}", crate::platform::probe_com());
    }

    #[test]
    #[ignore = "read-only live WinGet manifest tag integration"]
    fn live_category_tag() {
        let w = Winget::detect();
        let result = parse_table(
            &w.read(&Request::Tag("browser".into()), false, || false)
                .unwrap(),
            false,
        )
        .unwrap();
        assert!(!result.packages.is_empty());
        println!("Browser-tagged packages: {}", result.packages.len());
    }
    #[test]
    fn exact_separate_arguments_and_consent() {
        let a = arguments(
            &Request::Change {
                verb: "install",
                id: "Mozilla.Firefox".into(),
                version: Some("160.0.1".into()),
                accept_package: true,
                effectcraft_machine_update: false,
            },
            false,
        )
        .unwrap();
        assert!(a.windows(2).any(|p| p == ["--id", "Mozilla.Firefox"]));
        assert!(a.contains(&"--exact".into()));
        assert!(a.contains(&"--no-upgrade".into()));
        assert!(!a.contains(&"--allow-reboot".into()));
        assert!(a.windows(2).any(|p| p == ["--version", "160.0.1"]));
        assert!(!a.contains(&"--accept-source-agreements".into()));
        assert!(
            arguments(
                &Request::Change {
                    verb: "install",
                    id: "Foo.Bar".into(),
                    version: Some("1.0".into()),
                    accept_package: false,
                    effectcraft_machine_update: false,
                },
                false
            )
            .is_err()
        );
        assert!(
            arguments(
                &Request::Change {
                    verb: "uninstall",
                    id: "--all".into(),
                    version: None,
                    accept_package: false,
                    effectcraft_machine_update: false,
                },
                true
            )
            .is_err()
        );
    }
    #[test]
    fn effectcraft_machine_update_limits_the_scope_and_installer() {
        let request = Request::Change {
            verb: "upgrade",
            id: EFFECTCRAFT_ID.into(),
            version: Some(EFFECTCRAFT_SCOPE_FIX_VERSION.into()),
            accept_package: true,
            effectcraft_machine_update: true,
        };
        let args = arguments(&request, false).unwrap();
        assert!(args.windows(2).any(|p| p == ["--scope", "machine"]));
        assert!(
            args.windows(2)
                .any(|p| p == ["--version", EFFECTCRAFT_SCOPE_FIX_VERSION])
        );
        assert!(args.windows(2).any(|p| p == ["--installer-type", "wix"]));
        assert!(args.windows(2).any(|p| p == ["--custom", "ALLUSERS=1"]));
        assert!(!args.iter().any(|arg| matches!(
            arg.as_str(),
            "--force" | "--uninstall-previous" | "--override"
        )));
        assert!(
            arguments(
                &Request::Change {
                    verb: "upgrade",
                    id: "Other.Package".into(),
                    version: Some(EFFECTCRAFT_SCOPE_FIX_VERSION.into()),
                    accept_package: true,
                    effectcraft_machine_update: true,
                },
                false
            )
            .is_err()
        );
    }
    #[test]
    fn search_does_not_invoke_a_shell() {
        let a = arguments(&Request::Search("a; $(whoami)".into()), false).unwrap();
        assert_eq!(a[2], "a; $(whoami)");
    }
    #[test]
    fn localized_unicode_and_display_columns() {
        let o = output(
            "Nume          Identificator  Versiune\n------------------------------------\n日本語 Editor Vendor.Editor  1.2.3\nȘtiință       Vendor.Science 2.0\n",
        );
        let p = parse_table(&o, false).unwrap().packages;
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].name, "日本語 Editor");
        assert_eq!(p[1].id, "Vendor.Science");
    }
    #[test]
    fn detects_installed_and_available_versions() {
        let o = output(
            "Nom                    ID                 Version Disponible\n--------------------------------------------------------\nAdobe Acrobat (64-bit) Adobe.Acrobat.Pro  24.2    26.3\nWindows Runtime        Microsoft.DotNet.8 < 8.0.0\n",
        );
        let p = parse_table(&o, true).unwrap().packages;
        assert_eq!(p[0].installed.as_deref(), Some("24.2"));
        assert_eq!(p[0].available.as_deref(), Some("26.3"));
        assert_eq!(p[1].installed.as_deref(), Some("< 8.0.0"));
    }
    #[test]
    fn rejects_truncated_rows_and_unknown_formats() {
        let o = output(
            "Name   Id           Version\n---------------------------\nAn app Vendor.Long… 1.0\n",
        );
        assert!(parse_table(&o, false).is_err());
        assert!(
            parse_table(
                &Output {
                    code: 0,
                    text: "unrecognized output".into()
                },
                false
            )
            .is_err()
        );
    }
    #[test]
    fn preserves_name_tokens_with_dots() {
        let o = output(
            "Name                    Id            Version\n--------------------------------------------\nNotepad++.Notepad++ 1.2 Vendor.Editor 3.4\n",
        );
        let p = parse_table(&o, false).unwrap().packages;
        assert_eq!(p[0].id, "Vendor.Editor");
    }
    #[test]
    fn missing_winget_is_an_actionable_error() {
        let w = Winget { executable: None };
        assert!(
            w.read(&Request::Version, false, || false)
                .unwrap_err()
                .0
                .contains("official instructions")
        );
    }
    #[test]
    fn structured_sources() {
        assert!(
            source_summary("{\"Name\":\"winget\",\"Arg\":\"https://example.com\"}")
                .contains("winget · https://example.com")
        );
        assert!(verified_source(&configured_sources(TRUSTED_SOURCE)));
        assert!(!verified_source(&configured_sources(
            &TRUSTED_SOURCE.replace(SOURCE_URL, "https://example.com/cache")
        )));
        assert!(!verified_source(&configured_sources(
            &TRUSTED_SOURCE.replace("StoreOrigin", "Other")
        )));
        assert!(!verified_source(&configured_sources(&format!(
            "{TRUSTED_SOURCE}\n{TRUSTED_SOURCE}"
        ))));
    }

    #[test]
    fn only_the_id_column_can_identify_a_package() {
        let search = output(
            "Name   Id             Version Match\n--------------------------------------------\nViewer Example.Viewer v1.2.3  Tag: media.viewer\n<additional entries truncated due to result limit>\n",
        );
        let row = parse_table(&search, false).unwrap().packages.remove(0);
        assert_eq!(
            (row.id.as_str(), row.version.as_str()),
            ("Example.Viewer", "v1.2.3")
        );
        let list = output(
            "Name   Id             Version Available\n--------------------------------------------\nViewer Example.Viewer v1.2.3  v1.2.4\n1 upgrades available.\n1 package(s) have version numbers that cannot be determined.\n",
        );
        let table = parse_table(&list, true).unwrap();
        assert_eq!(table.skipped, 0);
        assert_eq!(table.packages[0].available.as_deref(), Some("v1.2.4"));
    }
}
