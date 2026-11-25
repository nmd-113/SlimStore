use crate::{http, icon_cache::PublicMetadata, packages::Package, scoop_catalog};
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

const GITHUB_RAW: &str = "raw.githubusercontent.com";
const WINGET_REPO: &str = "/microsoft/winget-pkgs/master/manifests";
const UNIGET_DATABASE: &str = "/Devolutions/UniGetUI/ce8ef7b5ded0341f72c4d806fb4d0bd483dbc866/WebBasedData/screenshot-database-v2.json";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    id: String,
    version: String,
    bucket: Option<String>,
}

impl Request {
    pub fn from_package(package: &Package) -> Option<Self> {
        if let Some(row) = &package.scoop_catalog {
            Some(Self {
                id: row.name.clone(),
                version: row.version.clone(),
                bucket: Some(row.bucket.clone()),
            })
        } else if package.manageable() || package.source == crate::scoop::SOURCE {
            Some(Self {
                id: package.id.clone(),
                version: package.version.clone(),
                bucket: None,
            })
        } else {
            None
        }
    }

    pub fn for_screenshots(package: &Package) -> Option<Self> {
        if let Some(row) = &package.scoop_catalog {
            scoop_catalog::manifest_path(&row.bucket, &row.name)?;
        } else if package.source == crate::scoop::SOURCE {
            let (bucket, id) = crate::scoop::mapping(&package.id)?.catalog_key();
            return Some(Self {
                id: id.into(),
                version: package.version.clone(),
                bucket: Some(bucket.into()),
            });
        } else if !package.manageable() {
            return None;
        }
        Self::from_package(package)
    }

    fn short_name(&self) -> &str {
        if self.bucket.is_some() {
            &self.id
        } else {
            self.id.rsplit('.').next().unwrap_or(&self.id)
        }
    }

    pub fn cache_key(&self) -> Option<crate::icon_cache::Key> {
        if self.version.is_empty() {
            return None;
        }
        let id = self.id.to_ascii_lowercase();
        let identity = if let Some(bucket) = &self.bucket {
            format!("scoop:{bucket}/{id}:{}", self.version)
        } else {
            format!("winget:{id}:{}", self.version)
        };
        http::sha256(identity.as_bytes())
    }

    pub fn local_scoop(&self, scoop: &crate::scoop::Scoop) -> Option<std::path::PathBuf> {
        if let Some(bucket) = &self.bucket {
            scoop.catalog_icon(bucket, &self.id)
        } else {
            scoop.icon_executable(&self.id)
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Version {
    package_identifier: String,
    package_version: String,
    default_locale: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Locale {
    package_identifier: String,
    package_version: String,
    package_locale: Option<String>,
    manifest_type: Option<String>,
    description: Option<String>,
    short_description: Option<String>,
    publisher: Option<String>,
    license: Option<String>,
    license_url: Option<String>,
    package_url: Option<String>,
    publisher_url: Option<String>,
    publisher_support_url: Option<String>,
    release_notes_url: Option<String>,
    #[serde(default)]
    icons: Vec<WingetIcon>,
}

#[derive(Deserialize)]
struct WingetIcon {
    #[serde(rename = "IconSha256")]
    sha256: Option<String>,
}

fn segment(s: &str) -> bool {
    scoop_catalog::valid_name(s) && !s.contains("..")
}

fn read_winget_locale(request: &Request, read: impl Fn(&str) -> Option<Vec<u8>>) -> Option<Locale> {
    let id = &request.id;
    let version = &request.version;
    if !crate::packages::valid_id(id) || !segment(version) || !id.split('.').all(segment) {
        return None;
    }
    let hierarchy = id.replace('.', "/");
    let first = id.chars().next()?.to_ascii_lowercase();
    let base = format!("{WINGET_REPO}/{first}/{hierarchy}/{version}/{id}");
    let read = |suffix: &str| read(&format!("{base}{suffix}.yaml"));
    let exact = |id2: &str, ver: &str| id.eq_ignore_ascii_case(id2) && version == ver;
    let parse_locale = |yaml: &[u8]| -> Option<Locale> {
        let manifest: Locale = serde_saphyr::from_str(
            std::str::from_utf8(yaml)
                .ok()?
                .trim_start_matches('\u{feff}'),
        )
        .ok()?;
        exact(&manifest.package_identifier, &manifest.package_version).then_some(manifest)
    };
    // An exact en-US file marked as default already identifies the default locale.
    // An English translation or a missing file still needs the version manifest.
    let english = read(".locale.en-US");
    if let Some(locale) = english.as_deref().and_then(parse_locale)
        && locale.manifest_type.as_deref() == Some("defaultLocale")
        && locale.package_locale.as_deref() == Some("en-US")
    {
        return Some(locale);
    }
    let yaml = read("")?;
    let text = std::str::from_utf8(&yaml)
        .ok()?
        .trim_start_matches('\u{feff}');
    let identity: Version = serde_saphyr::from_str(text).ok()?;
    if !exact(&identity.package_identifier, &identity.package_version) {
        return None;
    }
    if let Some(locale) = identity.default_locale.filter(|locale| segment(locale)) {
        let bytes = if locale == "en-US" {
            english.or_else(|| read(".locale.en-US"))?
        } else {
            read(&format!(".locale.{locale}"))?
        };
        parse_locale(&bytes)
    } else {
        parse_locale(&yaml)
    }
}

pub struct PackageArtwork(ArtworkMetadata);

enum ArtworkMetadata {
    WinGet(Box<Locale>),
    Scoop {
        homepage: String,
        public: Option<Box<PublicMetadata>>,
    },
}

impl PackageArtwork {
    pub fn fetch(
        session: &http::Session,
        request: &Request,
        active: impl Fn() -> bool + Copy,
    ) -> Option<Self> {
        if let Some(bucket) = &request.bucket {
            let path = scoop_catalog::manifest_path(bucket, &request.id)?;
            let bytes = session.get(GITHUB_RAW, &path, 192 * 1024, active)?;
            let manifest: serde_json::Value =
                serde_json::from_slice(bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes))
                    .ok()?;
            let homepage = manifest
                .get("homepage")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let public = manifest
                .get("version")
                .and_then(serde_json::Value::as_str)
                .filter(|version| *version == request.version)
                .map(|_| {
                    let row = scoop_catalog::installed_row(bucket, &request.id, &manifest);
                    Box::new(PublicMetadata {
                        description: row.description,
                        license: row.license,
                        website: homepage.into(),
                        license_site: manifest["license"]["url"]
                            .as_str()
                            .unwrap_or_default()
                            .into(),
                        ..PublicMetadata::default()
                    })
                });
            Some(Self(ArtworkMetadata::Scoop {
                homepage: homepage.into(),
                public,
            }))
        } else {
            read_winget_locale(request, |path| {
                session.get(GITHUB_RAW, path, 96 * 1024, active)
            })
            .map(|locale| Self(ArtworkMetadata::WinGet(Box::new(locale))))
        }
    }

    pub fn description(&self) -> Option<&str> {
        match &self.0 {
            ArtworkMetadata::WinGet(locale) => [
                locale.description.as_deref(),
                locale.short_description.as_deref(),
            ]
            .into_iter()
            .flatten()
            .find(|text| !text.trim().is_empty()),
            ArtworkMetadata::Scoop { public, .. } => {
                public.as_ref().map(|data| data.description.as_str())
            }
        }
    }

    pub fn public_metadata(&self) -> Option<PublicMetadata> {
        match &self.0 {
            ArtworkMetadata::WinGet(locale) => Some(PublicMetadata {
                description: self.description().unwrap_or_default().into(),
                publisher: locale.publisher.clone().unwrap_or_default(),
                license: locale.license.clone().unwrap_or_default(),
                website: locale.package_url.clone().unwrap_or_default(),
                notes: locale.release_notes_url.clone().unwrap_or_default(),
                support: locale.publisher_support_url.clone().unwrap_or_default(),
                publisher_site: locale.publisher_url.clone().unwrap_or_default(),
                license_site: locale.license_url.clone().unwrap_or_default(),
            }),
            ArtworkMetadata::Scoop { public, .. } => public.as_deref().cloned(),
        }
    }

    fn project(&self, name: &str) -> Option<Project<'_>> {
        match &self.0 {
            ArtworkMetadata::WinGet(locale) => Project::from_locale(locale, name),
            ArtworkMetadata::Scoop { homepage, .. } => {
                Project::repository(homepage).or_else(|| Project::publisher(homepage, name))
            }
        }
    }
}

fn https_parts(url: &str) -> Option<(&str, &str)> {
    let (host, path) = url
        .strip_prefix("https://")?
        .split_once('/')
        .unwrap_or((url.strip_prefix("https://")?, ""));
    if host.len() > 253
        || !host.contains('.')
        || !host.split('.').all(|s| {
            !s.is_empty()
                && !s.starts_with('-')
                && !s.ends_with('-')
                && s.bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        })
        || path.len() > 1024
        || !path
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"/-_.+@".contains(&c))
        || path.split('/').any(|part| matches!(part, "." | ".."))
    {
        return None;
    }
    Some((host, path))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Project<'a> {
    host: &'a str,
    namespace: String,
    git: bool,
}

impl<'a> Project<'a> {
    fn repository(url: &'a str) -> Option<Self> {
        let (host, path) = https_parts(url)?;
        if !matches!(host, "github.com" | "gitlab.com" | "invent.kde.org") {
            return None;
        }
        let mut parts = path.trim_end_matches('/').split('/');
        let owner = parts.next()?;
        let repo = parts.next()?;
        let repo = repo.strip_suffix(".git").unwrap_or(repo);
        if !segment(owner) || !segment(repo) {
            return None;
        }
        // Recognize repository links used by package metadata. Other pages may
        // point to an organization or a different product on the same host.
        let tail: Vec<_> = parts.collect();
        let tail = tail.strip_prefix(&["-"]).unwrap_or(&tail);
        match tail {
            [] | ["issues"] | ["releases"] | ["releases", "latest"] => {}
            ["releases", "tag", tag] if segment(tag) => {}
            _ => return None,
        }
        let namespace = format!("{owner}/{repo}");
        Some(Self {
            host: if host == "github.com" {
                GITHUB_RAW
            } else {
                host
            },
            namespace: if host == "github.com" {
                namespace.to_ascii_lowercase()
            } else {
                namespace
            },
            git: true,
        })
    }

    fn publisher(url: &'a str, name: &str) -> Option<Self> {
        let (host, path) = https_parts(url)?;
        let domain = host.strip_prefix("www.").unwrap_or(host);
        if path.is_empty()
            && domain
                .split('.')
                .next()
                .is_some_and(|token| normalized(token) == normalized(name))
            && crate::icons::known_publisher_host(host)
        {
            return Some(Self {
                host,
                namespace: String::new(),
                git: false,
            });
        }
        None
    }

    fn from_locale(locale: &'a Locale, name: &str) -> Option<Self> {
        let homepage = locale.package_url.as_deref();
        if let Some(project) = homepage.and_then(Self::repository) {
            return Some(project);
        }
        let release_repo = locale
            .release_notes_url
            .as_deref()
            .and_then(Self::repository);
        let mut repos = [
            locale.publisher_url.as_deref(),
            locale.publisher_support_url.as_deref(),
        ]
        .into_iter()
        .flatten()
        .filter_map(Self::repository)
        .chain(release_repo.iter().cloned());
        if let Some(repo) = repos.next()
            && (release_repo.is_some()
                || repo
                    .namespace
                    .rsplit_once('/')
                    .is_some_and(|(_, repo)| normalized(repo) == normalized(name)))
            && repos.all(|other| other == repo)
        {
            return Some(repo);
        }
        homepage.and_then(|url| Self::publisher(url, name))
    }

    fn candidate(&self, url: &str) -> Option<String> {
        self.asset_path(url, &["png", "ico"])
    }

    fn asset_path(&self, url: &str, extensions: &[&str]) -> Option<String> {
        let (host, path) = https_parts(url)?;
        let (_, extension) = path.rsplit_once('.')?;
        let github_page = host == "github.com" && self.host == GITHUB_RAW;
        if (!github_page && host != self.host)
            || !extensions
                .iter()
                .any(|ext| extension.eq_ignore_ascii_case(ext))
        {
            return None;
        }
        if self.git {
            let (owner, rest) = path.split_once('/')?;
            let (repo, rest) = rest.split_once('/')?;
            let namespace = format!("{owner}/{repo}");
            if namespace != self.namespace
                && !(self.host == GITHUB_RAW && namespace.eq_ignore_ascii_case(&self.namespace))
            {
                return None;
            }
            if self.host == GITHUB_RAW {
                let file = if github_page {
                    rest.strip_prefix("blob/")
                        .or_else(|| rest.strip_prefix("raw/"))?
                } else {
                    rest
                };
                let (reference, file_path) = file.split_once('/')?;
                if reference.is_empty() || file_path.is_empty() {
                    return None;
                }
                return Some(format!("/{namespace}/{file}"));
            }
            if !rest.starts_with("-/raw/") {
                return None;
            }
        }
        Some(format!("/{path}"))
    }

    fn repository_root(&self) -> Option<String> {
        if !self.git {
            return None;
        }
        Some(if self.host == GITHUB_RAW {
            format!("/{}/HEAD", self.namespace)
        } else {
            format!("/{}/-/raw/HEAD", self.namespace)
        })
    }

    fn conventional(&self, name: &str) -> Vec<String> {
        if let Some(prefix) = self.repository_root() {
            let lowercase = name.to_ascii_lowercase();
            [
                format!("{prefix}/icon.png"),
                format!("{prefix}/icon.ico"),
                format!("{prefix}/fastlane/metadata/android/en-US/images/icon.png"),
                format!("{prefix}/docs/images/icon.png"),
                format!("{prefix}/assets/{name}.png"),
                format!("{prefix}/snap/gui/{lowercase}.png"),
                format!("{prefix}/data/pixmaps/hicolor/256x256/apps/{lowercase}-icon.png"),
                format!("{prefix}/assets/icon.png"),
                format!("{prefix}/res/icon.png"),
            ]
            .into()
        } else {
            ["/favicon.png".into(), "/favicon.ico".into()].into()
        }
    }
}

fn normalized(s: &str) -> String {
    s.bytes()
        .filter(u8::is_ascii_alphanumeric)
        .map(|b| b.to_ascii_lowercase() as char)
        .collect()
}

#[derive(Deserialize)]
struct Database {
    icons_and_screenshots: HashMap<String, DatabaseEntry>,
}

#[derive(Deserialize)]
struct DatabaseEntry {
    icon: String,
    #[serde(default)]
    images: Vec<String>,
}

#[derive(Default)]
struct DatabaseCache {
    tried: Option<Instant>,
    data: Option<Arc<Database>>,
}

fn uniget_database(
    session: &http::Session,
    active: impl Fn() -> bool,
    wait: bool,
) -> Option<Arc<Database>> {
    static CACHE: OnceLock<Mutex<DatabaseCache>> = OnceLock::new();
    let mutex = CACHE.get_or_init(Default::default);
    let mut cache = if wait {
        mutex.lock().ok()?
    } else {
        mutex.try_lock().ok()?
    };
    if cache.data.is_none()
        && cache
            .tried
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(120))
        && active()
    {
        cache.data = session
            .get(GITHUB_RAW, UNIGET_DATABASE, 2 * 1024 * 1024, &active)
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .map(Arc::new);
        if active() {
            cache.tried = Some(Instant::now());
        }
    }
    cache.data.clone()
}

fn database_keys(request: &Request, project: &Project<'_>) -> [String; 5] {
    let short = request.short_name();
    [
        request.id.clone(),
        request.id.to_ascii_lowercase(),
        project.namespace.to_ascii_lowercase(),
        short.to_ascii_lowercase(),
        normalized(short),
    ]
}

const SCREENSHOT_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg"];
const SCREENSHOT_CANDIDATES: usize = 4;

pub struct GallerySources {
    pub host: String,
    pub paths: Vec<String>,
    pub credit: String,
}

fn readme_images<'a>(
    readme: &'a str,
    project: &'a Project<'_>,
    root: &'a str,
) -> impl Iterator<Item = String> + 'a {
    readme.split("![").skip(1).filter_map(move |image| {
        let (label, link) = image.split_once("](")?;
        let url = link.split_once(')')?.0.trim();
        if !label.to_ascii_lowercase().contains("screenshot")
            && !url.to_ascii_lowercase().contains("screenshot")
        {
            return None;
        }
        let absolute;
        let url = if url.starts_with("https://") {
            url
        } else {
            // Resolve only simple paths from the repository's root README.
            let relative = url.strip_prefix("./").unwrap_or(url);
            if relative.starts_with("//") {
                return None;
            }
            absolute = format!(
                "https://{}{root}/{}",
                project.host,
                relative.trim_start_matches('/')
            );
            &absolute
        };
        project.asset_path(url, SCREENSHOT_EXTENSIONS)
    })
}

pub fn screenshot_sources(
    session: &http::Session,
    request: &Request,
    active: impl Fn() -> bool + Copy,
) -> Option<GallerySources> {
    let metadata = PackageArtwork::fetch(session, request, active)?;
    let project = metadata.project(request.short_name())?;
    let keys = database_keys(request, &project);
    let mut seen = HashSet::new();
    let mut paths = Vec::new();
    if let Some(db) = uniget_database(session, active, true) {
        let images = keys
            .iter()
            .filter_map(|key| db.icons_and_screenshots.get(key))
            .flat_map(|entry| &entry.images);
        for url in images {
            if let Some(path) = project.asset_path(url, SCREENSHOT_EXTENSIONS)
                && !path.rsplit('/').next().is_some_and(|file| {
                    let file = file.to_ascii_lowercase();
                    ["banner", "logo", "badge", "avatar"]
                        .iter()
                        .any(|label| file.contains(label))
                })
                && seen.insert(path.clone())
            {
                paths.push(path);
                if paths.len() == SCREENSHOT_CANDIDATES {
                    break;
                }
            }
        }
        // Prefer paths that explicitly identify a screenshot over promotional images.
        paths.sort_by_key(|path| !path.to_ascii_lowercase().contains("screenshot"));
    }
    if paths.len() < 2
        && let Some(root) = project.repository_root()
    {
        if let Some(bytes) = session.get(
            project.host,
            &format!("{root}/README.md"),
            128 * 1024,
            active,
        ) && let Ok(readme) = std::str::from_utf8(&bytes)
        {
            paths.extend(
                readme_images(readme, &project, &root)
                    .filter(|path| seen.insert(path.clone()))
                    .take(SCREENSHOT_CANDIDATES - paths.len()),
            );
        }
        for file in [
            "screenshot.png",
            "docs/screenshot.png",
            "assets/screenshot.png",
        ] {
            if paths.len() == SCREENSHOT_CANDIDATES {
                break;
            }
            let path = format!("{root}/{file}");
            if seen.insert(path.clone()) {
                paths.push(path);
            }
        }
    }
    let source = if project.git {
        &project.namespace
    } else {
        project.host
    };
    Some(GallerySources {
        host: project.host.to_owned(),
        paths,
        credit: format!("Project artwork · {source}"),
    })
}

pub fn load(
    session: &http::Session,
    request: &Request,
    metadata: &PackageArtwork,
    limit: usize,
    active: impl Fn() -> bool + Copy,
    decode: impl Fn(&[u8]) -> Option<image::RgbaImage>,
) -> Option<image::RgbaImage> {
    let short = request.short_name();
    if let ArtworkMetadata::WinGet(locale) = &metadata.0
        && let Some(image) = locale
            .icons
            .iter()
            .take(3)
            .filter_map(|icon| {
                let digest = icon.sha256.as_deref()?.to_ascii_lowercase();
                (digest.len() == 64 && digest.bytes().all(|c| c.is_ascii_hexdigit()))
                    .then_some(digest)
            })
            .find_map(|digest| {
                let bytes = session.get(
                    "cdn.winget.microsoft.com",
                    &format!("/icons/{digest}.ico"),
                    limit,
                    active,
                )?;
                http::sha256_matches(&bytes, &digest)
                    .then(|| decode(&bytes))
                    .flatten()
            })
    {
        return Some(image);
    }
    let project = metadata.project(short)?;
    let suggested = uniget_database(session, active, false).and_then(|db| {
        database_keys(request, &project)
            .iter()
            .find_map(|key| project.candidate(&db.icons_and_screenshots.get(key)?.icon))
    });
    suggested
        .into_iter()
        .chain(project.conventional(short))
        .take_while(|_| active())
        .find_map(|path| {
            let bytes = session.get(project.host, &path, limit, active)?;
            decode(&bytes)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_default_locale_avoids_a_version_lookup() {
        use std::cell::RefCell;

        let request = Request {
            id: "Example.App".into(),
            version: "1.0".into(),
            bucket: None,
        };
        let identity = "PackageIdentifier: Example.App\nPackageVersion: '1.0'\n";
        let english = format!(
            "{identity}PackageLocale: en-US\nManifestType: defaultLocale\nShortDescription: English\n"
        );
        let japanese = format!(
            "{identity}PackageLocale: ja-JP\nManifestType: defaultLocale\nShortDescription: Japanese\n"
        );
        let version = format!("{identity}DefaultLocale: ja-JP\n");
        for (english, expected, calls) in [
            (english.clone(), "English", 1),
            (english.replace("defaultLocale", "locale"), "Japanese", 3),
            (
                english.replace("Example.App", "Example.Other"),
                "Japanese",
                3,
            ),
            (english.replace("'1.0'", "'2.0'"), "Japanese", 3),
            (english.replace("en-US", "en-GB"), "Japanese", 3),
        ] {
            let paths = RefCell::new(Vec::new());
            let locale = read_winget_locale(&request, |path| {
                paths.borrow_mut().push(path.to_owned());
                let name = path.rsplit('/').next()?;
                let manifest = match name {
                    "Example.App.locale.en-US.yaml" => &english,
                    "Example.App.locale.ja-JP.yaml" => &japanese,
                    "Example.App.yaml" => &version,
                    _ => return None,
                };
                Some(manifest.as_bytes().to_vec())
            })
            .unwrap();
            assert_eq!(locale.short_description.as_deref(), Some(expected));
            assert_eq!(paths.into_inner().len(), calls);
        }
    }

    #[test]
    fn accepts_only_artwork_within_the_identified_project() {
        let project = Project::repository("https://github.com/owner/project").unwrap();
        assert!(
            project
                .candidate("https://raw.githubusercontent.com/owner/project/HEAD/icon.png")
                .is_some()
        );
        for link in [
            "https://raw.githubusercontent.com/owner/project-other/HEAD/icon.png",
            "https://raw.githubusercontent.com/owner/project/../other/icon.png",
            "https://raw.githubusercontent.com/owner/project/%2e%2e/icon.png",
            "https://raw.githubusercontent.com/owner/project/HEAD/icon.png?x=y",
            "https://images.invalid/project.png",
            "https://raw.githubusercontent.com@127.0.0.1/owner/project/HEAD/icon.png",
        ] {
            assert!(project.candidate(link).is_none(), "{link}");
        }
    }

    #[test]
    fn screenshot_links_stay_inside_the_project_repository() {
        let project = Project::repository("https://github.com/owner/project").unwrap();
        let root = project.repository_root().unwrap();
        let readme = "![Screenshot](./docs/screenshot.png)\n\
                      ![Screenshot](https://github.com/Owner/Project/blob/main/screens/other.jpg)\n\
                      ![Screenshot](../outside.png)\n\
                      ![Screenshot](https://raw.githubusercontent.com/owner/other/main/screenshot.png)\n\
                      ![Screenshot](//example.com/screenshot.png)\n\
                      ![Screenshot](./docs/screenshot.svg)";
        assert_eq!(
            readme_images(readme, &project, &root).collect::<Vec<_>>(),
            [
                "/owner/project/HEAD/docs/screenshot.png",
                "/Owner/Project/main/screens/other.jpg"
            ]
        );
    }

    #[test]
    fn manifest_repository_fallback_requires_package_specific_links() {
        let mut locale: Locale = serde_saphyr::from_str(
            "PackageIdentifier: Tiled.Tiled\nPackageVersion: 1.12.2\n\
             PackageUrl: https://www.mapeditor.org\n\
             PublisherUrl: https://github.com/mapeditor/tiled\n\
             PublisherSupportUrl: https://github.com/MapEditor/Tiled/issues\n\
             ReleaseNotesUrl: https://github.com/mapeditor/tiled/releases/tag/v1.12.2\n",
        )
        .unwrap();
        let project = Project::from_locale(&locale, "Tiled").unwrap();
        assert_eq!(project.namespace, "mapeditor/tiled");
        assert!(
            project
                .conventional("Tiled")
                .iter()
                .any(|path| { path == "/mapeditor/tiled/HEAD/snap/gui/tiled.png" })
        );
        locale.publisher_support_url = Some("https://github.com/mapeditor/other/issues".into());
        assert!(Project::from_locale(&locale, "Tiled").is_none());
        assert!(Project::repository("https://github.com/mapeditor/tiled/wiki").is_none());
        assert!(Project::repository("https://github.com/mapeditor").is_none());
    }

    #[test]
    #[ignore = "read-only lookups against official package and artwork sources"]
    fn live_exact_package_metadata_and_dynamic_icon() {
        let session = http::session().unwrap();
        let known = Package {
            id: "HandBrake.HandBrake".into(),
            source: "winget".into(),
            ..Package::default()
        };
        let current = crate::winget::Winget::detect()
            .exact_package(
                &crate::winget::Request::CatalogExact(known.id.clone()),
                &known,
                false,
                false,
                || false,
            )
            .expect("read exact WinGet package")
            .expect("find HandBrake in the WinGet catalog");
        let winget = Request::from_package(&current).unwrap();
        let reads = std::cell::Cell::new(0);
        let locale = read_winget_locale(&winget, |path| {
            reads.set(reads.get() + 1);
            session.get(GITHUB_RAW, path, 96 * 1024, || true)
        })
        .expect("fetch HandBrake manifest for the current exact WinGet version");
        assert_eq!(locale.package_identifier, current.id);
        assert_eq!(locale.package_version, current.version);
        assert_eq!(reads.get(), 1);
        let scoop = Request {
            id: "eartrumpet".into(),
            version: String::new(),
            bucket: Some("extras".into()),
        };
        let metadata = PackageArtwork::fetch(&session, &scoop, || true).unwrap();
        let image = load(
            &session,
            &scoop,
            &metadata,
            512 * 1024,
            || true,
            crate::icons::pixels,
        );
        assert!(
            image.is_some(),
            "EarTrumpet should load from its own project repository"
        );
    }
}
