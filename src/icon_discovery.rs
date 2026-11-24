use crate::{http, packages::Package, scoop_catalog};
use serde::Deserialize;
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
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
                version: String::new(),
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
    package_url: Option<String>,
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

fn fetch_winget(
    session: &http::Session,
    request: &Request,
    active: impl Fn() -> bool + Copy,
) -> Option<Locale> {
    let id = &request.id;
    let version = &request.version;
    if !crate::packages::valid_id(id) || !segment(version) || !id.split('.').all(segment) {
        return None;
    }
    let hierarchy = id.replace('.', "/");
    let first = id.chars().next()?.to_ascii_lowercase();
    let base = format!("{WINGET_REPO}/{first}/{hierarchy}/{version}/{id}");
    let read = |suffix: &str| {
        session.get(
            GITHUB_RAW,
            &format!("{base}{suffix}.yaml"),
            96 * 1024,
            active,
        )
    };
    let yaml = read("")?;
    let text = std::str::from_utf8(&yaml)
        .ok()?
        .trim_start_matches('\u{feff}');
    let identity: Version = serde_saphyr::from_str(text).ok()?;
    let exact = |id2: &str, ver: &str| id.eq_ignore_ascii_case(id2) && version == ver;
    if !exact(&identity.package_identifier, &identity.package_version) {
        return None;
    }
    let locale = if let Some(locale) = identity.default_locale.filter(|locale| segment(locale)) {
        let bytes = read(&format!(".locale.{locale}"))?;
        serde_saphyr::from_str(
            std::str::from_utf8(&bytes)
                .ok()?
                .trim_start_matches('\u{feff}'),
        )
        .ok()?
    } else {
        serde_saphyr::from_str(text).ok()?
    };
    let locale: Locale = locale;
    exact(&locale.package_identifier, &locale.package_version).then_some(locale)
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

#[derive(Clone, Debug)]
struct Project<'a> {
    host: &'a str,
    namespace: String,
    git: bool,
}

impl<'a> Project<'a> {
    fn identify(url: &'a str, name: &str) -> Option<Self> {
        let (host, path) = https_parts(url)?;
        if matches!(host, "github.com" | "gitlab.com" | "invent.kde.org") {
            let parts: Vec<_> = path.trim_end_matches('/').split('/').collect();
            if parts.len() != 2 || !parts.iter().all(|part| segment(part)) {
                return None;
            }
            return Some(Self {
                host: if host == "github.com" {
                    GITHUB_RAW
                } else {
                    host
                },
                namespace: format!("{}/{}", parts[0], parts[1].trim_end_matches(".git")),
                git: true,
            });
        }
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

    fn candidate(&self, url: &str) -> Option<String> {
        let (host, path) = https_parts(url)?;
        if host != self.host || (!path.ends_with(".png") && !path.ends_with(".ico")) {
            return None;
        }
        if self.git {
            let rest = path.strip_prefix(&self.namespace)?.strip_prefix('/')?;
            if self.host != GITHUB_RAW && !rest.starts_with("-/raw/") {
                return None;
            }
            if self.host == GITHUB_RAW && rest.split_once('/').is_none() {
                return None;
            }
        }
        Some(format!("/{path}"))
    }

    fn conventional(&self, name: &str) -> Vec<String> {
        if self.git {
            let prefix = if self.host == GITHUB_RAW {
                format!("/{}/HEAD", self.namespace)
            } else {
                format!("/{}/-/raw/HEAD", self.namespace)
            };
            [
                format!("{prefix}/icon.png"),
                format!("{prefix}/assets/{name}.png"),
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
    icons_and_screenshots: HashMap<String, Icon>,
}

#[derive(Deserialize)]
struct Icon {
    icon: String,
}

#[derive(Default)]
struct DatabaseCache {
    tried: Option<Instant>,
    data: Option<Database>,
}

fn uniget_candidate(
    session: &http::Session,
    keys: &[String],
    project: &Project<'_>,
    active: impl Fn() -> bool,
) -> Option<String> {
    static CACHE: OnceLock<Mutex<DatabaseCache>> = OnceLock::new();
    let mut cache = CACHE.get_or_init(Default::default).try_lock().ok()?;
    if cache.data.is_none()
        && cache
            .tried
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(120))
        && active()
    {
        cache.data = session
            .get(GITHUB_RAW, UNIGET_DATABASE, 2 * 1024 * 1024, &active)
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        if active() {
            cache.tried = Some(Instant::now());
        }
    }
    keys.iter().find_map(|key| {
        project.candidate(&cache.data.as_ref()?.icons_and_screenshots.get(key)?.icon)
    })
}

pub fn load(
    session: &http::Session,
    request: &Request,
    limit: usize,
    active: impl Fn() -> bool + Copy,
    decode: impl Fn(&[u8]) -> Option<image::RgbaImage>,
) -> Option<image::RgbaImage> {
    let short = request.id.rsplit('.').next().unwrap_or(&request.id);
    let homepage = if let Some(bucket) = &request.bucket {
        let path = scoop_catalog::manifest_path(bucket, &request.id)?;
        let bytes = session.get(GITHUB_RAW, &path, 192 * 1024, active)?;
        let doc: serde_json::Value =
            serde_json::from_slice(bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes)).ok()?;
        doc["homepage"].as_str()?.to_owned()
    } else {
        let locale = fetch_winget(session, request, active)?;
        if let Some(image) = locale
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
        locale.package_url?
    };
    let project = Project::identify(&homepage, short)?;
    let keys = [
        request.id.to_ascii_lowercase(),
        short.to_ascii_lowercase(),
        normalized(short),
    ];
    let suggested = uniget_candidate(session, &keys, &project, active);
    suggested
        .into_iter()
        .chain(project.conventional(short))
        .find_map(|path| {
            let bytes = session.get(project.host, &path, limit, active)?;
            decode(&bytes)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_artwork_within_the_identified_project() {
        let project = Project::identify("https://github.com/owner/project", "project").unwrap();
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
    #[ignore = "read-only lookups against official package and artwork sources"]
    fn live_exact_package_metadata_and_dynamic_icon() {
        let session = http::session().unwrap();
        let winget = Request {
            id: "Mozilla.Firefox".into(),
            version: "160.0.1".into(),
            bucket: None,
        };
        let locale = fetch_winget(&session, &winget, || true).unwrap();
        assert_eq!(locale.package_identifier, winget.id);
        let scoop = Request {
            id: "eartrumpet".into(),
            version: String::new(),
            bucket: Some("extras".into()),
        };
        let image = load(&session, &scoop, 512 * 1024, || true, crate::icons::pixels);
        assert!(
            image.is_some(),
            "EarTrumpet should load from its own project repository"
        );
    }
}
