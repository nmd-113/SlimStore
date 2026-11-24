use crate::{
    Artwork, MainWindow, http,
    platform::wide,
    scoop::{self, Scoop},
};
use slint::{ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    io::Cursor,
    path::Path,
    ptr::null_mut,
    rc::Rc,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Graphics::Gdi::*,
    UI::{
        Shell::ExtractIconExW,
        WindowsAndMessaging::{DI_NORMAL, DestroyIcon, DrawIconEx},
    },
};
const MAX_BYTES: usize = 512 * 1024;
const MAX_DIMENSION: u32 = 256;
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
const ICO_SIGNATURE: &[u8] = b"\0\0\x01\0";
const WINGET_ICONS: &str = include_str!("../catalog/online-icons.txt");
const PUBLISHER_ICONS: &str = include_str!("../catalog/publisher-icons.txt");
const UNIGET_ICONS: &str = include_str!("../catalog/uniget-icons.txt");
const RETRY_DELAY: Duration = Duration::from_secs(120);

struct Asset {
    host: &'static str,
    path: String,
    digest: &'static str,
}

#[derive(Default)]
struct IconIndex {
    slots: HashMap<String, usize>,
    sets: Vec<IconSet>,
}

struct IconSet {
    id: &'static str,
    assets: Vec<Asset>,
}

impl IconIndex {
    fn insert(&mut self, id: &'static str, asset: Asset) {
        let key = id.to_ascii_lowercase();
        if let Some(&slot) = self.slots.get(&key) {
            let assets = &mut self.sets[slot - 1].assets;
            if !assets.iter().any(|other| other.digest == asset.digest) {
                assets.push(asset);
            }
        } else {
            self.sets.push(IconSet {
                id,
                assets: vec![asset],
            });
            self.slots.insert(key, self.sets.len());
        }
    }
}

fn manifest_entries(data: &'static str) -> impl Iterator<Item = &'static str> {
    data.lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
}

fn remote_asset(rest: &'static str) -> Option<Asset> {
    let (url, digest) = rest.split_once('|')?;
    let (host, path) = url.strip_prefix("https://")?.split_once('/')?;
    if host.split('.').count() < 2
        || host
            .split('.')
            .any(|label| label.is_empty() || label.starts_with('-') || label.ends_with('-'))
        || !host
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'-'))
        || path.is_empty()
        || !path
            .bytes()
            .all(|c| c.is_ascii_graphic() && !matches!(c, b'?' | b'#' | b'\\' | b'|'))
    {
        return None;
    }
    Some(Asset {
        host,
        path: format!("/{path}"),
        digest,
    })
}

fn index() -> &'static IconIndex {
    static INDEX: OnceLock<IconIndex> = OnceLock::new();
    INDEX.get_or_init(|| {
        let mut index = IconIndex::default();
        for (id, digest) in manifest_entries(WINGET_ICONS).filter_map(|line| line.split_once('|')) {
            index.insert(
                id,
                Asset {
                    host: "cdn.winget.microsoft.com",
                    path: format!("/icons/{digest}.ico"),
                    digest,
                },
            );
        }
        for line in manifest_entries(PUBLISHER_ICONS) {
            if let Some((id, rest)) = line.split_once('|')
                && let Some(asset) = remote_asset(rest)
            {
                index.insert(id, asset);
            }
        }
        for line in manifest_entries(UNIGET_ICONS) {
            if let Some((id, rest)) = line.split_once('|')
                && let Some((_, rest)) = rest.split_once('|')
                && let Some(asset) = remote_asset(rest)
            {
                index.insert(id, asset);
            }
        }
        index
    })
}

pub fn slot_for(id: &str) -> i32 {
    index()
        .slots
        .get(&id.to_ascii_lowercase())
        .map_or(0, |slot| *slot as i32)
}

fn download(session: &http::Session, asset: &Asset, active: impl Fn() -> bool) -> Option<Vec<u8>> {
    let bytes = session.get(asset.host, &asset.path, MAX_BYTES, active)?;
    http::sha256_matches(&bytes, asset.digest).then_some(bytes)
}

fn decode(bytes: &[u8], format: image::ImageFormat) -> Option<image::DynamicImage> {
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(4 * 1024 * 1024);
    reader.limits(limits);
    reader.decode().ok()
}

fn alternate_ico_frame(bytes: &[u8]) -> Option<image::DynamicImage> {
    let count = usize::from(u16::from_le_bytes(bytes.get(4..6)?.try_into().ok()?));
    if count > 32 {
        return None;
    }
    let end = 6 + count * 16;
    let mut frames: Vec<_> = bytes.get(6..end)?.as_chunks::<16>().0.iter().collect();
    let dimension = |byte| if byte == 0 { 256 } else { u32::from(byte) };
    frames
        .sort_unstable_by_key(|entry| std::cmp::Reverse(dimension(entry[0]) * dimension(entry[1])));
    frames.into_iter().find_map(|entry| {
        let length = u32::from_le_bytes(entry[8..12].try_into().ok()?) as usize;
        let offset = u32::from_le_bytes(entry[12..].try_into().ok()?) as usize;
        if offset < end {
            return None;
        }
        let data = bytes.get(offset..offset.checked_add(length)?)?;
        let icon = if data.starts_with(PNG_SIGNATURE) {
            decode(data, image::ImageFormat::Png)?
        } else {
            // Let the standard decoder validate a single bounded frame.
            let mut single = Vec::with_capacity(22 + data.len());
            single.extend_from_slice(b"\0\0\x01\0\x01\0");
            single.extend_from_slice(&entry[..12]);
            single.extend_from_slice(&22u32.to_le_bytes());
            single.extend_from_slice(data);
            decode(&single, image::ImageFormat::Ico)?
        };
        (icon.width() == dimension(entry[0]) && icon.height() == dimension(entry[1]))
            .then_some(icon)
    })
}

pub(crate) fn pixels(bytes: &[u8]) -> Option<image::RgbaImage> {
    let icon = if bytes.starts_with(PNG_SIGNATURE) {
        decode(bytes, image::ImageFormat::Png)
    } else if bytes.starts_with(ICO_SIGNATURE) {
        // Some official assets have a largest frame the standard ICO decoder rejects.
        decode(bytes, image::ImageFormat::Ico).or_else(|| alternate_ico_frame(bytes))
    } else {
        None
    }?;
    Some(icon.thumbnail(128, 128).into_rgba8())
}

fn local_pixels(path: &Path) -> Option<image::RgbaImage> {
    const SIDE: i32 = 32;
    const BYTE_COUNT: usize = (SIDE * SIDE * 4) as usize;
    unsafe {
        let mut icon = null_mut();
        let extracted = ExtractIconExW(wide(path).as_ptr(), 0, &mut icon, null_mut(), 1);
        if icon.is_null() {
            return None;
        }
        let result = (|| {
            if extracted != 1 {
                return None;
            }
            let dc = CreateCompatibleDC(null_mut());
            if dc.is_null() {
                return None;
            }
            let mut data = null_mut();
            let mut info = BITMAPINFO::default();
            info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = SIDE;
            info.bmiHeader.biHeight = -SIDE;
            info.bmiHeader.biPlanes = 1;
            info.bmiHeader.biBitCount = 32;
            let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut data, null_mut(), 0);
            let result = if bitmap.is_null() || data.is_null() {
                None
            } else {
                let previous = SelectObject(dc, bitmap);
                if previous.is_null() {
                    None
                } else {
                    let draw = |background: u8| {
                        let bits = data.cast::<u8>();
                        bits.write_bytes(background, BYTE_COUNT);
                        (DrawIconEx(dc, 0, 0, icon, SIDE, SIDE, 0, null_mut(), DI_NORMAL) != 0
                            && GdiFlush() != 0)
                            .then(|| std::slice::from_raw_parts(bits, BYTE_COUNT).to_vec())
                    };
                    // Recover opacity from both composites, including legacy mask-based icons.
                    let rgba = draw(0).zip(draw(255)).and_then(|(dark, light)| {
                        let mut pixels = Vec::with_capacity(BYTE_COUNT);
                        for (dark, light) in
                            dark.as_chunks::<4>().0.iter().zip(light.as_chunks::<4>().0)
                        {
                            let alpha = 255 - light[1].saturating_sub(dark[1]);
                            let channel = |b: u8| {
                                (u16::from(b) * 255)
                                    .checked_div(u16::from(alpha))
                                    .unwrap_or(0)
                                    .min(255) as u8
                            };
                            pixels.extend_from_slice(&[
                                channel(dark[2]),
                                channel(dark[1]),
                                channel(dark[0]),
                                alpha,
                            ]);
                        }
                        image::RgbaImage::from_raw(SIDE as u32, SIDE as u32, pixels)
                    });
                    SelectObject(dc, previous);
                    rgba
                }
            };
            if !bitmap.is_null() {
                DeleteObject(bitmap);
            }
            DeleteDC(dc);
            result
        })();
        DestroyIcon(icon);
        result
    }
}

const DYNAMIC_SLOTS: usize = 512;
const DYNAMIC_PIXELS: u32 = 64;

#[derive(Clone)]
struct Slot {
    request: crate::icon_discovery::Request,
    used: u64,
    ticket: u64,
}

#[derive(Default)]
struct Slots {
    epoch: u64,
    next_ticket: u64,
    pinned: HashMap<usize, crate::icon_discovery::Request>,
    dynamic: Vec<Option<Slot>>,
    pending: HashSet<usize>,
    failures: HashMap<usize, (Instant, bool)>,
}

pub struct Loader {
    pub enabled: Arc<AtomicBool>,
    slots: Rc<RefCell<Slots>>,
    online: Rc<VecModel<Image>>,
    local: Rc<VecModel<Image>>,
    tickets: Arc<Vec<AtomicU64>>,
}

impl Loader {
    pub fn begin_view(&self) {
        self.slots.borrow_mut().epoch += 1;
    }

    pub fn slot(&self, package: &crate::packages::Package) -> i32 {
        let Some(request) = crate::icon_discovery::Request::from_package(package) else {
            return 0;
        };
        let canonical = package
            .scoop_catalog
            .as_deref()
            .and_then(|row| scoop::canonical_id(&row.bucket, &row.name))
            .unwrap_or(&package.id);
        let pinned = slot_for(canonical);
        let mut slots = self.slots.borrow_mut();
        if pinned > 0 {
            // Prefer WinGet metadata for the pinned icon's dynamic fallback.
            let known = slots
                .pinned
                .entry(pinned as usize)
                .or_insert_with(|| request.clone());
            if package.manageable() {
                *known = request;
            }
            return pinned;
        }
        let epoch = slots.epoch;
        if let Some((position, slot)) =
            slots.dynamic.iter_mut().enumerate().find_map(|(i, slot)| {
                slot.as_mut()
                    .filter(|slot| slot.request == request)
                    .map(|slot| (i, slot))
            })
        {
            slot.used = epoch;
            return (index().sets.len() + 1 + position) as i32;
        }
        let Some(victim) = slots.dynamic.iter().position(Option::is_none).or_else(|| {
            slots
                .dynamic
                .iter()
                .enumerate()
                .filter_map(|(i, slot)| {
                    let slot = slot.as_ref()?;
                    (slot.used < epoch).then_some((slot.used, i))
                })
                .min()
                .map(|(_, i)| i)
        }) else {
            return 0;
        };
        let index = index().sets.len() + 1 + victim;
        slots.next_ticket += 1;
        let ticket = slots.next_ticket;
        slots.dynamic[victim] = Some(Slot {
            request,
            used: epoch,
            ticket,
        });
        self.tickets[index].store(ticket, Ordering::Release);
        slots.pending.remove(&index);
        slots.failures.remove(&index);
        drop(slots);
        self.online.set_row_data(index, Image::default());
        self.local.set_row_data(index, Image::default());
        index as i32
    }
}

pub fn known_publisher_host(host: &str) -> bool {
    manifest_entries(PUBLISHER_ICONS)
        .filter_map(|line| line.split_once('|'))
        .any(|(_, rest)| {
            rest.strip_prefix("https://")
                .and_then(|url| url.split_once('/'))
                .is_some_and(|(registered, _)| registered == host)
        })
}

struct Work {
    index: usize,
    ticket: u64,
    request: Option<crate::icon_discovery::Request>,
}

pub fn attach(ui: &MainWindow, initial: bool, scoop: Option<Scoop>) -> Loader {
    let enabled = Arc::new(AtomicBool::new(initial));
    let artwork = ui.global::<Artwork>();
    let pinned = index().sets.len();
    let count = pinned + DYNAMIC_SLOTS + 1;
    let online_model = Rc::new(VecModel::from(vec![Image::default(); count]));
    let local_model = Rc::new(VecModel::from(vec![Image::default(); count]));
    artwork.set_online_icons(ModelRc::from(online_model.clone()));
    artwork.set_local_icons(ModelRc::from(local_model.clone()));
    let slots = Rc::new(RefCell::new(Slots {
        dynamic: vec![None; DYNAMIC_SLOTS],
        ..Default::default()
    }));
    let tickets = Arc::new((0..count).map(|_| AtomicU64::new(0)).collect::<Vec<_>>());
    let (send, receive) = mpsc::sync_channel::<Work>(64);
    let receive = Arc::new(Mutex::new(receive));
    let have_scoop = Arc::new(AtomicBool::new(scoop.is_some()));
    artwork.set_local_enabled(scoop.is_some());
    artwork.on_scoop_changed({
        let have_scoop = have_scoop.clone();
        let slots = slots.clone();
        move |available| {
            if have_scoop.swap(available, Ordering::Relaxed) != available {
                slots.borrow_mut().failures.clear();
            }
        }
    });
    for _ in 0..2 {
        let ui = ui.as_weak();
        let enabled = enabled.clone();
        let receive = receive.clone();
        let tickets = tickets.clone();
        let have_scoop = have_scoop.clone();
        let mut scoop = scoop.clone();
        std::thread::spawn(move || {
            let mut http = None;
            loop {
                let Ok(work) = receive.lock().unwrap_or_else(|e| e.into_inner()).recv() else {
                    return;
                };
                let current = || tickets[work.index].load(Ordering::Acquire) == work.ticket;
                if !current() {
                    continue;
                }
                let set = index().sets.get(work.index - 1);
                if have_scoop.load(Ordering::Relaxed) != scoop.is_some() {
                    scoop = Scoop::detect();
                }
                let local = scoop
                    .as_ref()
                    .and_then(|scoop| {
                        set.and_then(|set| scoop.icon_executable(set.id))
                            .or_else(|| {
                                work.request
                                    .as_ref()
                                    .and_then(|request| request.local_scoop(scoop))
                            })
                    })
                    .and_then(|path| local_pixels(&path));
                let from_local = local.is_some();
                let online = enabled.load(Ordering::Relaxed);
                let active = || current() && enabled.load(Ordering::Relaxed);
                let icon = if from_local {
                    local
                } else if active() {
                    if http.is_none() {
                        http = http::session();
                    }
                    http.as_ref().and_then(|session| {
                        set.and_then(|set| {
                            set.assets
                                .iter()
                                .find_map(|asset| pixels(&download(session, asset, active)?))
                        })
                        .or_else(|| {
                            crate::icon_discovery::load(
                                session,
                                work.request.as_ref()?,
                                MAX_BYTES,
                                active,
                                |bytes| {
                                    let icon = pixels(bytes)?;
                                    Some(image::imageops::thumbnail(
                                        &icon,
                                        DYNAMIC_PIXELS,
                                        DYNAMIC_PIXELS,
                                    ))
                                },
                            )
                        })
                    })
                } else {
                    None
                };
                let tickets = tickets.clone();
                if ui
                    .upgrade_in_event_loop(move |ui| {
                        if tickets[work.index].load(Ordering::Acquire) != work.ticket {
                            return;
                        }
                        let img = icon.map_or_else(Image::default, |icon| {
                            Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
                                icon.as_raw(),
                                icon.width(),
                                icon.height(),
                            ))
                        });
                        ui.global::<Artwork>().invoke_icon_result(
                            work.index as i32,
                            img,
                            from_local,
                            online,
                        );
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
    }
    artwork.on_request_icon({
        let (enabled, slots, online_model, local_model) = (
            enabled.clone(),
            slots.clone(),
            online_model.clone(),
            local_model.clone(),
        );
        move |slot| {
            let Ok(index) = usize::try_from(slot) else {
                return;
            };
            if index == 0 || index >= count {
                return;
            }
            let online = enabled.load(Ordering::Relaxed);
            let mut slots = slots.borrow_mut();
            let blank = |model: &VecModel<Image>| {
                model
                    .row_data(index)
                    .is_some_and(|image| image.size().width == 0)
            };
            if !(blank(&local_model)
                && (online && blank(&online_model)
                    || !online && have_scoop.load(Ordering::Relaxed)))
            {
                return;
            }
            if slots
                .failures
                .get(&index)
                .is_some_and(|(time, tried_online)| {
                    time.elapsed() < RETRY_DELAY && (!online || *tried_online)
                })
                || slots.pending.contains(&index)
            {
                return;
            }
            let (ticket, request) = if index <= pinned {
                (0, slots.pinned.get(&index).cloned())
            } else {
                let Some(slot) = &slots.dynamic[index - pinned - 1] else {
                    return;
                };
                (slot.ticket, Some(slot.request.clone()))
            };
            if send
                .try_send(Work {
                    index,
                    ticket,
                    request,
                })
                .is_ok()
            {
                slots.pending.insert(index);
            }
        }
    });
    artwork.on_icon_result({
        let (slots, online_model, local_model, enabled) = (
            slots.clone(),
            online_model.clone(),
            local_model.clone(),
            enabled.clone(),
        );
        move |slot, image, from_local, online| {
            let index = slot as usize;
            let mut slots = slots.borrow_mut();
            slots.pending.remove(&index);
            if image.size().width > 0 {
                slots.failures.remove(&index);
                drop(slots);
                if from_local {
                    local_model.set_row_data(index, image);
                } else {
                    online_model.set_row_data(index, image);
                }
            } else {
                slots.failures.insert(
                    index,
                    (Instant::now(), online && enabled.load(Ordering::Relaxed)),
                );
            }
        }
    });
    Loader {
        enabled,
        slots,
        online: online_model,
        local: local_model,
        tickets,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artwork_is_keyed_by_exact_package_and_checksum() {
        for manifest in [WINGET_ICONS, PUBLISHER_ICONS, UNIGET_ICONS] {
            let mut names = HashSet::new();
            for line in manifest_entries(manifest) {
                let (id, rest) = line.split_once('|').unwrap();
                assert!(names.insert(id.to_ascii_lowercase()));
                assert!(crate::metadata::catalog().iter().any(|app| app.id == id));
                assert_ne!(slot_for(id), 0);
                if manifest == PUBLISHER_ICONS {
                    assert!(remote_asset(rest).is_some());
                }
                if manifest == UNIGET_ICONS {
                    assert!(remote_asset(rest.split_once('|').unwrap().1).is_some());
                }
            }
        }
        for Asset { digest: hash, .. } in index().sets.iter().flat_map(|set| &set.assets) {
            assert_eq!(hash.len(), 64);
            assert!(hash.bytes().all(|c| c.is_ascii_hexdigit()));
        }
        assert_eq!(slot_for("Mozilla.Firefox"), slot_for("mozilla.firefox"));
        assert_eq!(slot_for("Mozilla.Firefox.Unlisted"), 0);
        assert!(http::sha256_matches(
            b"abc",
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        ));
        assert!(!http::sha256_matches(
            b"abcd",
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        ));
        assert!(pixels(b"invalid ICO").is_none());
        for url in [
            "http://example.com/logo.ico|hash",
            "https://example.com:443/logo.ico|hash",
            "https://user@example.com/logo.ico|hash",
            "https://example.com/logo.ico?key=value|hash",
        ] {
            assert!(remote_asset(url).is_none());
        }
    }

    #[test]
    fn windows_icon_resource_preserves_transparency() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("SlimStore-icon.ico");
        let icon = local_pixels(&path).unwrap();
        assert_eq!(icon.dimensions(), (32, 32));
        assert!(icon.pixels().any(|p| p.0[3] < 255));
        assert!(icon.pixels().any(|p| p.0[3] > 0));
    }

    #[test]
    #[ignore = "downloads read-only hash-pinned assets from WinGet and publisher hosts"]
    fn live_artwork_decodes() {
        let worker_count = 6;
        let failed = std::thread::scope(|scope| {
            let workers = (0..worker_count)
                .map(|offset| {
                    scope.spawn(move || {
                        let session = http::session().unwrap();
                        index()
                            .sets
                            .iter()
                            .flat_map(|set| &set.assets)
                            .skip(offset)
                            .step_by(worker_count)
                            .filter(|asset| {
                                let Some(bytes) = download(&session, asset, || true) else {
                                    return true;
                                };
                                pixels(&bytes).is_none_or(|icon| {
                                    !(16..=128).contains(&icon.width())
                                        || !(16..=128).contains(&icon.height())
                                })
                            })
                            .map(|asset| format!("https://{}{}", asset.host, asset.path))
                            .collect::<Vec<_>>()
                    })
                })
                .collect::<Vec<_>>();
            workers
                .into_iter()
                .flat_map(|worker| worker.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert!(failed.is_empty(), "Unusable artwork: {failed:#?}");
    }
}
