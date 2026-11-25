use crate::{
    http,
    icon_discovery::{self, Request},
    packages::Package,
};
use image::{ImageFormat, ImageReader, Limits, RgbaImage};
use std::{
    io::Cursor,
    time::{Duration, Instant},
};

const INDEX: &str = include_str!("../catalog/screenshots.txt");
const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_SIDE: u32 = 2560;
const MAX_SHOTS: usize = 2;
const LOOKUP_BUDGET: Duration = Duration::from_secs(15);

struct Source {
    host: &'static str,
    path: &'static str,
    digest: &'static str,
    label: &'static str,
    credit: &'static str,
}

fn sources(id: &str) -> impl Iterator<Item = Source> {
    INDEX.lines().filter_map(move |line| {
        if line.starts_with('#') {
            return None;
        }
        let mut fields = line.split('|');
        if !fields.next()?.eq_ignore_ascii_case(id) {
            return None;
        }
        let host = fields.next()?;
        if !matches!(
            host,
            "raw.githubusercontent.com" | "media.githubusercontent.com"
        ) {
            return None;
        }
        Some(Source {
            host,
            path: fields.next()?,
            digest: fields.next()?,
            label: fields.next()?,
            credit: fields.next()?,
        })
    })
}

pub fn available(package: &Package) -> bool {
    Request::for_screenshots(package).is_some()
}

pub struct Screenshot {
    pub pixels: RgbaImage,
    pub label: String,
    pub credit: String,
}

fn decode(bytes: &[u8]) -> Option<RgbaImage> {
    let format = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        ImageFormat::Png
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        ImageFormat::Jpeg
    } else {
        return None;
    };
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(48 * 1024 * 1024);
    reader.limits(limits);
    Some(reader.decode().ok()?.thumbnail(1440, 1080).into_rgba8())
}

pub fn load(package: &Package, active: impl Fn() -> bool) -> Vec<Screenshot> {
    let start = Instant::now();
    let active = || active() && start.elapsed() < LOOKUP_BUDGET;
    let Some(session) = http::session() else {
        return Vec::new();
    };
    let reviewed: Vec<_> = sources(&package.id)
        .filter(|_| package.manageable())
        .take(MAX_SHOTS)
        .take_while(|_| active())
        .filter_map(|source| {
            let bytes = session.get(source.host, source.path, MAX_BYTES, active)?;
            if !http::sha256_matches(&bytes, source.digest) || !active() {
                return None;
            }
            Some(Screenshot {
                pixels: decode(&bytes)?,
                label: source.label.into(),
                credit: source.credit.into(),
            })
        })
        .collect();
    if !reviewed.is_empty() || !active() {
        return reviewed;
    }
    let Some(locations) = Request::for_screenshots(package)
        .and_then(|request| icon_discovery::screenshot_sources(&session, &request, active))
    else {
        return reviewed;
    };
    locations
        .paths
        .iter()
        .take_while(|_| active())
        .filter_map(|path| {
            let bytes = session.get(&locations.host, path, MAX_BYTES, active)?;
            active().then(|| decode(&bytes)).flatten()
        })
        .take(MAX_SHOTS)
        .enumerate()
        .map(|(index, pixels)| Screenshot {
            pixels,
            label: format!("Screenshot {}", index + 1),
            credit: locations.credit.clone(),
        })
        .collect()
}
