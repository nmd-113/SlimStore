use crate::packages::Package;

pub const CATEGORIES: &[&str] = &[
    "All categories",
    "Browsers",
    "Development",
    "Gaming",
    "Multimedia",
    "Productivity",
    "Utilities",
    "Security",
    "Communication",
    "Graphics & Design",
    "System Tools",
    "Education",
    "Networking",
    "File Management",
    "Compression",
    "Other",
];

#[derive(serde::Deserialize)]
pub struct Curated {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub version: String,
    pub publisher: String,
    pub description: String,
    pub summary: String,
    pub category: String,
    pub shelf: String,
    pub license: String,
    pub website: String,
    pub notes: String,
}

pub fn catalog() -> &'static [Curated] {
    static CATALOG: std::sync::OnceLock<Vec<Curated>> = std::sync::OnceLock::new();
    CATALOG.get_or_init(|| {
        // The snapshot is generated and checked in with the source.
        serde_json::from_str(include_str!("../catalog/verified.json")).unwrap_or_default()
    })
}

pub fn curated(id: &str) -> Option<&'static Curated> {
    catalog().iter().find(|p| p.id.eq_ignore_ascii_case(id))
}

pub fn category(id: &str) -> &'static str {
    curated(id).map_or("Other", |p| p.category.as_str())
}

pub const COLLECTIONS: &[&str] = &[
    "All collections",
    "Featured",
    "Essential",
    "Developer Tools",
    "Productivity",
    "Multimedia",
    "Gaming",
    "Utilities",
];

pub fn in_collection(id: &str, index: usize) -> bool {
    let Some(entry) = curated(id) else {
        return index == 0;
    };
    match index {
        0 => true,
        1 | 2 => entry.shelf == COLLECTIONS[index],
        3 => entry.category == "Development",
        4..=6 => entry.category == COLLECTIONS[index],
        7 => matches!(
            entry.category.as_str(),
            "Utilities" | "System Tools" | "Compression"
        ),
        _ => false,
    }
}

pub fn category_tag(category: &str) -> Option<&'static str> {
    match category {
        "Browsers" => Some("browser"),
        "Development" => Some("development"),
        "Gaming" => Some("gaming"),
        "Multimedia" => Some("multimedia"),
        "Productivity" => Some("productivity"),
        "Utilities" => Some("utility"),
        "Security" => Some("security"),
        "Communication" => Some("communication"),
        "Graphics & Design" => Some("graphics"),
        "System Tools" => Some("system"),
        "Education" => Some("education"),
        "Networking" => Some("network"),
        "File Management" => Some("file-manager"),
        "Compression" => Some("compression"),
        _ => None,
    }
}
pub fn initial_packages(home: bool) -> Vec<Package> {
    if home {
        // Editorial picks span browsers, communication, media, gaming, and daily tools.
        return [
            "Mozilla.Firefox",
            "VideoLAN.VLC",
            "Google.Chrome",
            "Discord.Discord",
            "Valve.Steam",
            "Microsoft.VisualStudioCode",
            "Microsoft.PowerToys",
            "Microsoft.Edge",
            "Spotify.Spotify",
            "7zip.7zip",
            "OBSProject.OBSStudio",
            "Git.Git",
            "TheDocumentFoundation.LibreOffice",
            "ShareX.ShareX",
            "Notepad++.Notepad++",
            "Microsoft.WindowsTerminal",
            "Bitwarden.Bitwarden",
            "Brave.Brave",
            "EpicGames.EpicGamesLauncher",
            "Mojang.MinecraftLauncher",
            "Telegram.TelegramDesktop",
            "Microsoft.Teams",
            "Zoom.Zoom",
            "SlackTechnologies.Slack",
            "OpenWhisperSystems.Signal",
            "Audacity.Audacity",
            "HandBrake.HandBrake",
            "GIMP.GIMP.3",
            "BlenderFoundation.Blender",
            "Inkscape.Inkscape",
            "Obsidian.Obsidian",
            "SumatraPDF.SumatraPDF",
            "RARLab.WinRAR",
            "Rufus.Rufus",
            "voidtools.Everything",
            "TeamViewer.TeamViewer",
        ]
        .into_iter()
        .filter_map(curated)
        .map(offline_package)
        .collect();
    }
    let mut packages: Vec<_> = catalog().iter().map(offline_package).collect();
    packages.sort_by_cached_key(|package| package.name.to_lowercase());
    packages
}

fn offline_package(p: &Curated) -> Package {
    Package {
        id: p.id.clone(),
        name: p.name.clone(),
        version: p.version.clone(),
        source: "winget".into(),
        ..Default::default()
    }
}

pub fn initials(name: &str) -> String {
    let parts: Vec<_> = name
        .split_whitespace()
        .filter_map(|s| s.chars().find(|c| c.is_alphanumeric()))
        .take(2)
        .collect();
    let value: String = parts.into_iter().flat_map(char::to_uppercase).collect();
    if value.is_empty() { "A".into() } else { value }
}

/// Remove terminal controls without interpreting or executing them. Preserve Unicode and line breaks.
pub fn clean_output(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for x in chars.by_ref() {
                    if ('@'..='~').contains(&x) {
                        break;
                    }
                }
            } else if chars.peek() == Some(&']') {
                chars.next();
                while let Some(x) = chars.next() {
                    if x == '\u{7}' {
                        break;
                    }
                    if x == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
        } else if c == '\n' || c == '\t' || !c.is_control() {
            result.push(c);
        }
    }
    result.trim_start_matches('\u{feff}').trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unicode_and_control_normalization() {
        assert_eq!(
            clean_output("\u{feff}\x1b[32mȘtiință 日本語\x1b[0m\r\n"),
            "Știință 日本語"
        );
    }
    #[test]
    fn categories_are_conservative() {
        assert_eq!(category("mozilla.firefox"), "Browsers");
        assert_eq!(category("Unknown.Security"), "Other");
    }
    #[test]
    fn unicode_initials() {
        assert_eq!(initials("Știință 日本語"), "Ș日");
        assert_eq!(initials(""), "A");
    }
    #[test]
    fn curated_ids_are_valid_and_unique() {
        let mut ids = std::collections::HashSet::new();
        assert!(catalog().len() >= 60);
        for p in catalog() {
            assert!(crate::packages::valid_id(&p.id));
            assert!(ids.insert(p.id.to_ascii_lowercase()));
            assert!(CATEGORIES.contains(&p.category.as_str()));
        }
    }

    #[test]
    fn home_picks_are_distinct_catalog_packages() {
        let packages = initial_packages(true);
        let ids = packages
            .iter()
            .map(|p| p.id.as_str())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(ids.len(), packages.len());
        assert_eq!(ids.len(), 36);
        for id in [
            "Google.Chrome",
            "Discord.Discord",
            "Valve.Steam",
            "Microsoft.Teams",
        ] {
            assert!(ids.contains(id));
            assert!(crate::icons::slot_for(id) > 0);
        }
    }
}
