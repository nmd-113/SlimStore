use std::collections::HashSet;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Package {
    pub id: String,
    pub name: String,
    pub version: String,
    pub installed: Option<String>,
    pub available: Option<String>,
    pub source: String,
    pub scoop: Option<Box<crate::scoop::Reviewed>>,
    pub scoop_catalog: Option<Box<crate::scoop_catalog::Row>>,
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && !id.starts_with('-')
        && !id.contains('…')
        && !id.ends_with("...")
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._+-".contains(&c))
        && id.contains('.')
}

impl Package {
    pub fn reviewed_source(&self) -> String {
        self.scoop.as_ref().map_or_else(
            || self.source.clone(),
            |scoop| format!("Scoop ({})", scoop.qualified()),
        )
    }

    pub fn key(&self) -> String {
        format!("{}:{}", self.source.to_lowercase(), self.id.to_lowercase())
    }
    pub fn manageable(&self) -> bool {
        self.source == "winget" && valid_id(&self.id)
    }
    pub fn update_eligible(&self) -> bool {
        self.manageable()
            && self
                .installed
                .as_deref()
                .is_some_and(|v| v.chars().any(|c| c.is_ascii_digit()) && !v.contains(['<', '>']))
            && self.available.is_some()
    }
}

pub fn deduplicate(packages: Vec<Package>) -> Vec<Package> {
    let mut seen = HashSet::new();
    packages
        .into_iter()
        .filter(|p| seen.insert(p.key()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn safe_identifiers() {
        for id in [
            "Mozilla.Firefox",
            "7zip.7zip",
            "Microsoft.VCRedist.2015+.x64",
            "Fastfetch-cli.Fastfetch",
        ] {
            assert!(valid_id(id));
        }
        for id in [
            "",
            "--all",
            "Foo; rm",
            "Foo.Bar…",
            "Foo.Bar...",
            "Foo.\nBar",
            "$(bad)",
        ] {
            assert!(!valid_id(id));
        }
    }
    #[test]
    fn matching_is_exact_case_insensitive_and_source_aware() {
        let a = Package {
            id: "Mozilla.Firefox".into(),
            source: "winget".into(),
            ..Default::default()
        };
        let mut b = a.clone();
        b.id = "mozilla.firefox".into();
        let mut c = a.clone();
        c.id.push_str(".MSIX");
        assert_eq!(deduplicate(vec![a, b, c]).len(), 2);
    }
    #[test]
    fn only_known_installed_versions_are_bulk_upgrade_eligible() {
        let mut p = Package {
            id: "Mozilla.Firefox".into(),
            source: "winget".into(),
            installed: Some("Unknown".into()),
            available: Some("123".into()),
            ..Default::default()
        };
        assert!(!p.update_eligible());
        p.installed = Some("122".into());
        assert!(p.update_eligible());
        p.source.clear();
        assert!(!p.update_eligible());
    }
}
