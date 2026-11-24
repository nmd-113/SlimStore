use crate::{http, packages::Package, scoop, storage::Storage};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs::{self, File},
    io::{Cursor, Read},
    os::windows::fs::MetadataExt,
    path::Path,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

const BUCKETS: [&str; 2] = ["main", "extras"];
const ARCHIVE_LIMIT: usize = 8 * 1024 * 1024;
const MANIFEST_LIMIT: usize = 512 * 1024;
const EXPANDED_LIMIT: usize = 32 * 1024 * 1024;
const ENTRY_LIMIT: usize = 12_000;
const RESULT_LIMIT: usize = 250;
const FRESH: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    pub bucket: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub homepage: String,
    pub license: String,
    aliases: String,
}

pub fn valid_name(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(b))
        && !value.ends_with('.')
        && !value.split('.').next().is_some_and(|stem| {
            let stem = stem.to_ascii_uppercase();
            matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                || ["COM", "LPT"].into_iter().any(|prefix| {
                    stem.strip_prefix(prefix)
                        .is_some_and(|rest| rest.len() == 1 && rest.as_bytes()[0].is_ascii_digit())
                })
        })
}

pub fn official(bucket: &str) -> Option<&'static str> {
    match bucket {
        "main" => Some("Main"),
        "extras" => Some("Extras"),
        _ => None,
    }
}

pub fn manifest_path(bucket: &str, name: &str) -> Option<String> {
    valid_name(name).then_some(())?;
    Some(format!(
        "/ScoopInstaller/{}/master/bucket/{name}.json",
        official(bucket)?
    ))
}

impl Row {
    pub fn qualified(&self) -> String {
        format!("{}/{}", self.bucket, self.name)
    }

    pub fn manifest_url(&self) -> Option<String> {
        valid_name(&self.name).then_some(())?;
        Some(format!(
            "https://github.com/ScoopInstaller/{}/blob/master/bucket/{}.json",
            official(&self.bucket)?,
            self.name
        ))
    }

    pub fn package(self, scoop: Option<&scoop::Scoop>) -> Package {
        let installed = scoop.and_then(|s| s.catalog_installed(&self.bucket, &self.name));
        Package {
            id: self.qualified(),
            name: self.name.clone(),
            version: self.version.clone(),
            installed,
            source: scoop::SOURCE.into(),
            scoop_catalog: Some(Box::new(self)),
            ..Default::default()
        }
    }
}

fn clean(value: Option<&str>, limit: usize) -> String {
    value
        .unwrap_or_default()
        .chars()
        .filter(|c| {
            !c.is_control() && !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .take(limit)
        .collect()
}

fn row(bucket: &str, name: &str, bytes: &[u8]) -> Row {
    // Keep the package searchable by slug even if metadata cannot be decoded.
    let manifest: serde_json::Value =
        serde_json::from_slice(bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes))
            .unwrap_or_default();
    installed_row(bucket, name, &manifest)
}

pub fn installed_row(bucket: &str, name: &str, manifest: &serde_json::Value) -> Row {
    let mut aliases = Vec::new();
    for section in [manifest, &manifest["architecture"]["64bit"]] {
        for field in ["bin", "shortcuts"] {
            match &section[field] {
                serde_json::Value::String(name) => aliases.push(name.as_str()),
                serde_json::Value::Array(items) => {
                    for item in items.iter().take(16) {
                        if let Some(name) = item.as_str() {
                            aliases.push(name);
                        }
                        if let Some(parts) = item.as_array() {
                            aliases.extend(parts.iter().take(2).filter_map(|part| part.as_str()));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Row {
        bucket: bucket.to_owned(),
        name: name.to_owned(),
        version: clean(manifest["version"].as_str(), 80),
        description: clean(manifest["description"].as_str(), 384),
        homepage: clean(manifest["homepage"].as_str(), 512),
        license: clean(
            manifest["license"]
                .as_str()
                .or_else(|| manifest["license"]["identifier"].as_str()),
            100,
        ),
        aliases: clean(Some(&aliases.join(" ")), 384),
    }
}

fn rows_from_zip(bucket: &str, bytes: &[u8], active: impl Fn() -> bool) -> Option<Vec<Row>> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).ok()?;
    if archive.len() > ENTRY_LIMIT {
        return None;
    }
    let mut expanded = 0usize;
    let mut rows = Vec::new();
    let mut seen = HashSet::new();
    for n in 0..archive.len() {
        if !active() {
            return None;
        }
        let mut file = archive.by_index(n).ok()?;
        let path = file.name().ok()?;
        let mut parts = path.split('/');
        let (Some(_), Some("bucket"), Some(file_name), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let Some(name) = file_name
            .strip_suffix(".json")
            .filter(|name| valid_name(name))
        else {
            continue;
        };
        if !file.is_file() || !seen.insert(name.to_ascii_lowercase()) {
            return None;
        }
        let name = name.to_owned();
        let mut bytes = Vec::new();
        if file.size() <= MANIFEST_LIMIT as u64 {
            file.by_ref()
                .take((MANIFEST_LIMIT + 1) as u64)
                .read_to_end(&mut bytes)
                .ok()?;
            expanded = expanded.checked_add(bytes.len())?;
            if expanded > EXPANDED_LIMIT || bytes.len() > MANIFEST_LIMIT {
                return None;
            }
        }
        rows.push(row(bucket, &name, &bytes));
    }
    (!rows.is_empty()).then_some(rows)
}

fn regular(path: &Path, directory: bool) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| {
        m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
            && if directory { m.is_dir() } else { m.is_file() }
    })
}

fn local_rows(bucket: &str, root: &Path, active: impl Fn() -> bool) -> Option<Vec<Row>> {
    if !regular(root, true) {
        return None;
    }
    let mut rows = Vec::new();
    let mut names = HashSet::new();
    let mut pending = vec![(root.to_owned(), 0)];
    let mut count = 0;
    let mut bytes = Vec::new();
    let mut expanded = 0;
    while let Some((dir, depth)) = pending.pop() {
        for file in fs::read_dir(dir).ok()? {
            count += 1;
            if count > ENTRY_LIMIT || !active() {
                return None;
            }
            let Ok(file) = file else { continue };
            let path = file.path();
            if depth < 8 && regular(&path, true) && file.file_name() != ".git" {
                pending.push((path, depth + 1));
                continue;
            }
            let Some(name) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .filter(|s| valid_name(s))
            else {
                continue;
            };
            if path.extension().and_then(|s| s.to_str()) != Some("json") || !regular(&path, false) {
                continue;
            }
            if !names.insert(name.to_ascii_lowercase()) {
                continue;
            }
            bytes.clear();
            if let Ok(file) = File::open(&path) {
                let _ = file
                    .take((MANIFEST_LIMIT + 1) as u64)
                    .read_to_end(&mut bytes);
            }
            expanded += bytes.len();
            if expanded > EXPANDED_LIMIT {
                return None;
            }
            rows.push(row(
                bucket,
                name,
                if bytes.len() <= MANIFEST_LIMIT {
                    &bytes
                } else {
                    &[]
                },
            ));
        }
    }
    Some(rows)
}

#[derive(Serialize, Deserialize)]
struct Snapshot {
    bucket: String,
    saved: u64,
    rows: Vec<Row>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(super) fn cache_name(bucket: &str) -> &'static str {
    if bucket == "main" {
        "scoop-main-v1.json"
    } else {
        "scoop-extras-v1.json"
    }
}

fn read_snapshot(storage: &Storage, bucket: &str) -> Option<Snapshot> {
    let data = storage.read_catalog(cache_name(bucket)).ok()?;
    let snapshot: Snapshot = serde_json::from_slice(&data).ok()?;
    let mut names = HashSet::new();
    (snapshot.bucket == bucket
        && !snapshot.rows.is_empty()
        && snapshot.rows.len() <= ENTRY_LIMIT
        && snapshot.rows.iter().all(|r| {
            r.bucket == bucket && valid_name(&r.name) && names.insert(r.name.to_ascii_lowercase())
        }))
    .then_some(snapshot)
}

#[derive(Default)]
pub struct Catalog {
    official: [Option<Snapshot>; 2],
    attempted: [Option<Instant>; 2],
    additional: Vec<Row>,
    local_root: Option<std::path::PathBuf>,
    local_attempt: Option<Instant>,
}

pub struct Search {
    pub packages: Vec<Package>,
    pub complete: bool,
    pub cached: bool,
    pub total: usize,
}

impl Catalog {
    pub fn search(
        &mut self,
        query: &str,
        scoop: Option<&scoop::Scoop>,
        storage: Option<&Storage>,
        additional: bool,
        force: bool,
        active: impl Fn() -> bool + Copy,
    ) -> Search {
        let mut cached = false;
        let mut complete = true;
        let session = std::cell::OnceCell::new();
        for (index, bucket) in BUCKETS.into_iter().enumerate() {
            let current = &mut self.official[index];
            if current.is_none()
                && let Some(storage) = storage
            {
                *current = read_snapshot(storage, bucket);
            }
            let fresh = |s: &Snapshot| {
                now()
                    .checked_sub(s.saved)
                    .is_some_and(|age| age < FRESH.as_secs())
            };
            if !force && current.as_ref().is_some_and(fresh) {
                continue;
            }
            let local = scoop.and_then(|s| s.bucket_path(bucket, true));
            if current.is_none()
                && !force
                && active()
                && let Some(root) = &local
                && let Some(modified) = root
                    .parent()
                    .and_then(|parent| fs::metadata(parent.join(".git/FETCH_HEAD")).ok())
                    .and_then(|info| info.modified().ok())
                    .filter(|time| time.elapsed().is_ok_and(|age| age < FRESH))
                && let Some(rows) = local_rows(bucket, root, active).filter(|rows| !rows.is_empty())
            {
                *current = Some(Snapshot {
                    bucket: bucket.into(),
                    saved: modified
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    rows,
                });
            }
            if !force && current.as_ref().is_some_and(fresh) {
                continue;
            }
            if active()
                && (force
                    || self.attempted[index].is_none_or(|t| t.elapsed() >= Duration::from_secs(60)))
            {
                let network = session
                    .get_or_init(http::session)
                    .as_ref()
                    .and_then(|session| {
                        let path = format!(
                            "/ScoopInstaller/{}/zip/refs/heads/master",
                            official(bucket)?
                        );
                        let bytes =
                            session.get("codeload.github.com", &path, ARCHIVE_LIMIT, active)?;
                        rows_from_zip(bucket, &bytes, active)
                    });
                if active() {
                    self.attempted[index] = Some(Instant::now());
                }
                if let Some(rows) = network {
                    let snapshot = Snapshot {
                        bucket: bucket.into(),
                        saved: now(),
                        rows,
                    };
                    if active()
                        && let Some(storage) = storage
                        && let Ok(bytes) = serde_json::to_vec(&snapshot)
                    {
                        let _ = storage.write_catalog(cache_name(bucket), &bytes);
                    }
                    *current = Some(snapshot);
                    continue;
                }
            }
            if current.is_none()
                && active()
                && let Some(root) = local
                && let Some(rows) =
                    local_rows(bucket, &root, active).filter(|rows| !rows.is_empty())
            {
                *current = Some(Snapshot {
                    bucket: bucket.into(),
                    saved: 0,
                    rows,
                });
            }
            cached |= current.is_some();
            complete &= current.is_some();
        }
        let root = scoop
            .filter(|_| additional)
            .and_then(scoop::Scoop::buckets_path);
        if self.local_root != root {
            self.local_root = root.clone();
            self.additional.clear();
            self.local_attempt = None;
        }
        if let Some(root) = root
            && active()
            && (force
                || self
                    .local_attempt
                    .is_none_or(|t| t.elapsed() >= Duration::from_secs(300)))
        {
            self.additional.clear();
            if let Ok(items) = fs::read_dir(root) {
                for item in items.flatten().take(32) {
                    if !active() || self.additional.len() >= ENTRY_LIMIT {
                        break;
                    }
                    if let Some(bucket) = item
                        .file_name()
                        .to_str()
                        .filter(|b| valid_name(b) && official(&b.to_ascii_lowercase()).is_none())
                        && let Some(path) = scoop.and_then(|s| s.bucket_path(bucket, false))
                        && let Some(rows) = local_rows(bucket, &path, active)
                    {
                        let remaining = ENTRY_LIMIT - self.additional.len();
                        self.additional.extend(rows.into_iter().take(remaining));
                    }
                }
            }
            if active() {
                self.local_attempt = Some(Instant::now());
            }
        }
        let rows = self
            .official
            .iter()
            .flatten()
            .flat_map(|b| b.rows.iter())
            .chain(self.additional.iter());
        let (rows, total) = matches(query, rows);
        Search {
            packages: rows
                .into_iter()
                .take_while(|_| active())
                .map(|row| row.clone().package(scoop))
                .collect(),
            complete,
            cached,
            total,
        }
    }
}

fn matches<'a>(query: &str, rows: impl Iterator<Item = &'a Row>) -> (Vec<&'a Row>, usize) {
    let query = query.to_lowercase();
    let compact = |s: &str| {
        s.chars()
            .filter(char::is_ascii_alphanumeric)
            .collect::<String>()
    };
    let needle = compact(&query);
    let terms: Vec<_> = query.split_whitespace().take(16).collect();
    let mut results: Vec<_> = rows
        .filter_map(|row| {
            let name = row.name.to_ascii_lowercase();
            let compact_name = compact(&name);
            let rank = if name == query
                || row.qualified().eq_ignore_ascii_case(&query)
                || (!needle.is_empty() && compact_name == needle)
            {
                0
            } else if name.starts_with(&query) {
                1
            } else if name.contains(&query)
                || (!needle.is_empty() && compact_name.contains(&needle))
            {
                2
            } else {
                let aliases = row.aliases.to_ascii_lowercase();
                let description = row.description.to_lowercase();
                if terms.is_empty()
                    || !terms
                        .iter()
                        .all(|t| name.contains(t) || aliases.contains(t) || description.contains(t))
                {
                    return None;
                }
                3
            };
            Some((rank, row))
        })
        .collect();
    let total = results.len();
    results
        .sort_unstable_by(|(a, x), (b, y)| (a, &x.name, &x.bucket).cmp(&(b, &y.name, &y.bucket)));
    results.truncate(RESULT_LIMIT);
    (results.into_iter().map(|(_, row)| row).collect(), total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_exact_and_keeps_bucket_identity() {
        let a = row(
            "main",
            "fastfetch",
            br#"{"version":"1","description":"System information"}"#,
        );
        let b = row("extras", "fastfetch", br#"{"version":"2"}"#);
        let c = row(
            "extras",
            "something",
            br#"{"description":"Like fastfetch"}"#,
        );
        let (results, total) = matches("fastfetch", [&c, &a, &b].into_iter());
        assert_eq!(total, 3);
        assert_eq!(results[0].qualified(), "extras/fastfetch");
        assert_eq!(results[1].qualified(), "main/fastfetch");
        assert_eq!(matches("main/fastfetch", [&a, &b].into_iter()).1, 1);
        assert!(!valid_name("../fastfetch"));
        assert!(!valid_name("--force"));
        assert!(!valid_name("NUL.json"));
    }

    #[test]
    fn stale_portable_index_survives_unavailable_refresh() {
        let root = std::env::temp_dir().join(format!(
            "slimstore-catalog-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let storage = Storage::beside(&root.join("SlimStore.exe"));
        for bucket in BUCKETS {
            let snapshot = Snapshot {
                bucket: bucket.into(),
                saved: 1,
                rows: vec![row(bucket, "fixture", br#"{"version":"1.0"}"#)],
            };
            storage
                .write_catalog(cache_name(bucket), &serde_json::to_vec(&snapshot).unwrap())
                .unwrap();
        }
        let mut catalog = Catalog {
            attempted: std::array::from_fn(|_| Some(Instant::now())),
            ..Default::default()
        };
        let result = catalog.search("fixture", None, Some(&storage), false, false, || true);
        let folder = root.join("SlimStore-data");
        for bucket in BUCKETS {
            fs::remove_file(folder.join(cache_name(bucket))).unwrap();
        }
        fs::remove_dir(folder).unwrap();
        fs::remove_dir(root).unwrap();
        assert!(result.complete && result.cached);
        assert_eq!(result.packages.len(), 2);
    }

    #[test]
    #[ignore = "read-only downloads of the official Scoop catalog snapshots"]
    fn live_catalog_covers_both_official_buckets() {
        let mut catalog = Catalog::default();
        let result = catalog.search("nirlauncher", None, None, false, false, || true);
        assert!(result.complete);
        assert!(
            result
                .packages
                .iter()
                .any(|p| p.id == "extras/nirlauncher" && !p.version.is_empty())
        );
        let entries: Vec<_> = catalog
            .official
            .iter()
            .flatten()
            .map(|s| (s.bucket.as_str(), s.rows.len()))
            .collect();
        eprintln!("Official Scoop index: {entries:?}");
        assert!(entries.iter().map(|(_, count)| count).sum::<usize>() > 4000);
        let started = Instant::now();
        let result = catalog.search("fastfetch", None, None, false, false, || true);
        eprintln!("Warm Scoop search: {:?}", started.elapsed());
        assert_eq!(result.packages[0].id, "main/fastfetch");
    }
}
