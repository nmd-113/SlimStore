use crate::{
    Artwork, MainWindow, http,
    icon_cache::{IconCache, Key, PublicMetadata},
    icon_discovery::PackageArtwork,
    platform::wide,
    scoop::{self, Scoop},
    scoop_catalog,
    storage::Storage,
};
use slint::{
    ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel,
};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    io::Cursor,
    path::Path,
    ptr::null_mut,
    rc::Rc,
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
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

impl IconSet {
    fn cache_key(&self) -> Option<Key> {
        let mut identity = format!("pinned:{}", self.id);
        for asset in &self.assets {
            identity.push(':');
            identity.push_str(asset.digest);
        }
        http::sha256(identity.as_bytes())
    }
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
    let side = if format == image::ImageFormat::Png {
        512
    } else {
        256
    };
    limits.max_image_width = Some(side);
    limits.max_image_height = Some(side);
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
    thumbnail(bytes, 128)
}

fn thumbnail(bytes: &[u8], side: u32) -> Option<image::RgbaImage> {
    let icon = if bytes.starts_with(PNG_SIGNATURE) {
        decode(bytes, image::ImageFormat::Png)
    } else if bytes.starts_with(ICO_SIGNATURE) {
        // Some official assets have a largest frame the standard ICO decoder rejects.
        decode(bytes, image::ImageFormat::Ico).or_else(|| alternate_ico_frame(bytes))
    } else {
        None
    }?;
    Some(icon.thumbnail(side, side).into_rgba8())
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
    pinned: Option<usize>,
    needs_description: bool,
    used: u64,
    ticket: u64,
}

#[derive(Default)]
struct Slots {
    epoch: u64,
    scope: Option<(i32, String)>,
    next_ticket: u64,
    pinned: HashMap<usize, crate::icon_discovery::Request>,
    dynamic: Vec<Option<Slot>>,
    pending: HashSet<usize>,
    failures: HashMap<usize, (Instant, bool)>,
    description_attempts: HashMap<usize, Instant>,
}

pub struct Loader {
    pub enabled: Arc<AtomicBool>,
    cache: Arc<IconCache>,
    slots: Rc<RefCell<Slots>>,
    online: Rc<VecModel<Image>>,
    local: Rc<VecModel<Image>>,
    descriptions: Rc<VecModel<SharedString>>,
    queue: Arc<WorkQueue>,
    tickets: Arc<Vec<AtomicU64>>,
}

impl Loader {
    pub fn set_disk_cache(&self, enabled: bool) {
        if self.cache.set_enabled(enabled) {
            let mut slots = self.slots.borrow_mut();
            slots.failures.clear();
            slots.description_attempts.clear();
        }
    }

    fn metadata_key(&self, package: &crate::packages::Package) -> Option<Key> {
        if !self.cache.enabled()
            || !(package.manageable()
                || package
                    .scoop_catalog
                    .as_ref()
                    .is_some_and(|row| scoop_catalog::official(&row.bucket).is_some()))
            || !crate::winget::can_pin_version(&package.version)
        {
            return None;
        }
        crate::icon_discovery::Request::from_package(package)?.cache_key()
    }

    pub fn cached_metadata(&self, package: &crate::packages::Package) -> Option<PublicMetadata> {
        self.cache.get_metadata(&self.metadata_key(package)?)
    }

    pub fn remember_metadata(
        &self,
        package: &crate::packages::Package,
        value: impl FnOnce() -> PublicMetadata,
    ) {
        if let Some(key) = self.metadata_key(package) {
            self.cache.record_metadata(key, value(), true);
        }
    }

    pub fn finish(&self) {
        self.enabled.store(false, Ordering::Release);
        self.queue.close();
        self.cache.finish();
    }

    pub fn begin_view(&self, page: i32, heading: &str) {
        let mut slots = self.slots.borrow_mut();
        slots.epoch += 1;
        if slots
            .scope
            .as_ref()
            .is_none_or(|(old_page, old_heading)| *old_page != page || old_heading != heading)
        {
            slots.scope = Some((page, heading.to_owned()));
            self.queue.reset();
            slots.pending.clear();
        }
    }

    pub fn slot(&self, package: &crate::packages::Package, needs_description: bool) -> i32 {
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
        if pinned > 0 && !needs_description {
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
                    .filter(|slot| {
                        slot.request == request && slot.needs_description == needs_description
                    })
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
            pinned: (pinned > 0).then_some(pinned as usize),
            needs_description,
            used: epoch,
            ticket,
        });
        self.tickets[index].store(ticket, Ordering::Release);
        slots.pending.remove(&index);
        slots.failures.remove(&index);
        slots.description_attempts.remove(&index);
        drop(slots);
        self.online.set_row_data(index, Image::default());
        self.local.set_row_data(index, Image::default());
        self.descriptions
            .set_row_data(index, SharedString::default());
        index as i32
    }

    pub fn remember_description(&self, slot: i32, text: &str) {
        let Ok(row) = usize::try_from(slot) else {
            return;
        };
        if row <= index().sets.len() {
            return;
        }
        let text = crate::metadata::card_description(text);
        if !text.is_empty() {
            self.descriptions.set_row_data(row, text.into());
        }
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

#[derive(Clone, Copy)]
struct WorkId {
    index: usize,
    ticket: u64,
    view: u64,
}

struct Work {
    id: WorkId,
    pinned: Option<usize>,
    request: Option<crate::icon_discovery::Request>,
    icon: bool,
    description: bool,
    // A completed lookup can also tell the icon worker that no manifest was found.
    metadata: Option<Option<PackageArtwork>>,
}

impl WorkId {
    fn current(self, queue: &WorkQueue, tickets: &[AtomicU64]) -> bool {
        queue.view.load(Ordering::Acquire) == self.view
            && tickets[self.index].load(Ordering::Acquire) == self.ticket
    }

    fn post(
        self,
        ui: &slint::Weak<MainWindow>,
        queue: &Arc<WorkQueue>,
        tickets: &Arc<Vec<AtomicU64>>,
        apply: impl FnOnce(&MainWindow, i32) + Send + 'static,
    ) -> bool {
        let queue = queue.clone();
        let tickets = tickets.clone();
        ui.upgrade_in_event_loop(move |ui| {
            if self.current(&queue, &tickets) {
                apply(&ui, self.index as i32);
            }
        })
        .is_ok()
    }
}

const MAX_PENDING: usize = 64;

#[derive(Default)]
struct WorkQueue {
    pending: Mutex<Option<VecDeque<Work>>>,
    available: Condvar,
    view: AtomicU64,
}

impl WorkQueue {
    fn new() -> Self {
        Self {
            pending: Mutex::new(Some(VecDeque::new())),
            ..Self::default()
        }
    }

    fn enqueue(&self, work: Work) -> Option<Work> {
        let mut guard = self.pending.lock().unwrap();
        let pending = guard.as_mut()?;
        let dropped = if pending.len() == MAX_PENDING {
            pending.pop_front()
        } else {
            None
        };
        pending.push_back(work);
        self.available.notify_one();
        dropped
    }

    fn next(&self) -> Option<Work> {
        let guard = self.pending.lock().unwrap();
        let mut guard = self
            .available
            .wait_while(guard, |queue| {
                queue.as_ref().is_some_and(VecDeque::is_empty)
            })
            .unwrap();
        let pending = guard.as_mut()?;
        // Give the oldest pending description a turn before probing for more icons.
        if let Some(index) = pending.iter().position(|work| work.description) {
            pending.remove(index)
        } else {
            pending.pop_back()
        }
    }

    fn defer_icon(&self, work: Work, tickets: &[AtomicU64]) -> bool {
        let mut guard = self.pending.lock().unwrap();
        let Some(pending) = guard.as_mut() else {
            return false;
        };
        if !work.id.current(self, tickets) || pending.len() == MAX_PENDING {
            return false;
        }
        // New icon-only requests from the current viewport still get priority.
        pending.push_front(work);
        self.available.notify_one();
        true
    }

    fn reset(&self) {
        self.view.fetch_add(1, Ordering::Release);
        if let Some(pending) = self.pending.lock().unwrap().as_mut() {
            pending.clear();
        }
    }

    fn close(&self) {
        self.view.fetch_add(1, Ordering::Release);
        self.pending.lock().unwrap().take();
        self.available.notify_all();
    }
}

pub fn attach(
    ui: &MainWindow,
    initial: bool,
    scoop: Option<Scoop>,
    storage: Storage,
    cache_enabled: bool,
) -> Loader {
    let enabled = Arc::new(AtomicBool::new(initial));
    let cache = Arc::new(IconCache::new(storage, cache_enabled));
    let artwork = ui.global::<Artwork>();
    let pinned = index().sets.len();
    let count = pinned + DYNAMIC_SLOTS + 1;
    let online_model = Rc::new(VecModel::from(vec![Image::default(); count]));
    let local_model = Rc::new(VecModel::from(vec![Image::default(); count]));
    let descriptions = Rc::new(VecModel::from(vec![SharedString::default(); count]));
    artwork.set_online_icons(ModelRc::from(online_model.clone()));
    artwork.set_local_icons(ModelRc::from(local_model.clone()));
    artwork.set_descriptions(ModelRc::from(descriptions.clone()));
    let slots = Rc::new(RefCell::new(Slots {
        dynamic: vec![None; DYNAMIC_SLOTS],
        ..Default::default()
    }));
    let tickets = Arc::new((0..count).map(|_| AtomicU64::new(0)).collect::<Vec<_>>());
    let queue = Arc::new(WorkQueue::new());
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
        let queue = queue.clone();
        let tickets = tickets.clone();
        let have_scoop = have_scoop.clone();
        let cache = cache.clone();
        let mut scoop = scoop.clone();
        std::thread::spawn(move || {
            let mut http = None;
            loop {
                let Some(mut work) = queue.next() else {
                    return;
                };
                let id = work.id;
                let current = || id.current(&queue, &tickets);
                if !current() {
                    continue;
                }
                let online = enabled.load(Ordering::Relaxed);
                let active = || current() && enabled.load(Ordering::Relaxed);
                if active() && http.is_none() {
                    http = http::session();
                }
                let exact_key = cache
                    .enabled()
                    .then(|| work.request.as_ref()?.cache_key())
                    .flatten();
                let fetch_metadata = || {
                    let data =
                        PackageArtwork::fetch(http.as_ref()?, work.request.as_ref()?, active)?;
                    if active()
                        && let Some(key) = exact_key
                        && let Some(public) = data.public_metadata()
                    {
                        cache.record_metadata(key, public, false);
                    }
                    Some(data)
                };
                if work.description {
                    let cached = exact_key
                        .as_ref()
                        .and_then(|key| cache.get_metadata(key))
                        .filter(|data| !data.description.is_empty());
                    let tried = cached.is_none() && active();
                    let metadata = tried.then(fetch_metadata);
                    let text = cached
                        .as_ref()
                        .map(|data| data.description.as_str())
                        .or_else(|| {
                            metadata
                                .as_ref()
                                .and_then(Option::as_ref)
                                .and_then(PackageArtwork::description)
                        })
                        .map_or_else(String::new, crate::metadata::card_description);
                    let done = !work.icon;
                    if !id.post(&ui, &queue, &tickets, move |ui, slot| {
                        ui.global::<Artwork>().invoke_description_result(
                            slot,
                            text.into(),
                            tried,
                            done,
                        );
                    }) {
                        return;
                    }
                    if !done {
                        work.description = false;
                        work.metadata = metadata;
                        if !queue.defer_icon(work, &tickets)
                            && !id.post(&ui, &queue, &tickets, move |ui, slot| {
                                ui.global::<Artwork>().invoke_icon_result(
                                    slot,
                                    Image::default(),
                                    false,
                                    online,
                                    false,
                                );
                            })
                        {
                            return;
                        }
                    }
                    continue;
                }
                let set = work.pinned.and_then(|slot| index().sets.get(slot - 1));
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
                let icon = local.or_else(|| {
                    let pinned_key = cache.enabled().then(|| set?.cache_key()).flatten();
                    let cached = [pinned_key, exact_key]
                        .iter()
                        .flatten()
                        .find_map(|key| cache.get(key));
                    cached.or_else(|| {
                        if !active() {
                            return None;
                        }
                        let session = http.as_ref()?;
                        let pinned = set.and_then(|set| {
                            set.assets
                                .iter()
                                .find_map(|asset| pixels(&download(session, asset, active)?))
                        });
                        let (image, key) = if let Some(image) = pinned {
                            (image, pinned_key)
                        } else {
                            let metadata = work.metadata.unwrap_or_else(fetch_metadata)?;
                            let start = Instant::now();
                            let active = || active() && start.elapsed() < Duration::from_secs(6);
                            let image = crate::icon_discovery::load(
                                session,
                                work.request.as_ref()?,
                                &metadata,
                                MAX_BYTES,
                                active,
                                |bytes| thumbnail(bytes, DYNAMIC_PIXELS),
                            )?;
                            (image, exact_key)
                        };
                        if current()
                            && let Some(key) = key
                        {
                            cache.record(key, &image);
                        }
                        Some(image)
                    })
                });
                if !id.post(&ui, &queue, &tickets, move |ui, slot| {
                    let img = icon.map_or_else(Image::default, |icon| {
                        Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
                            icon.as_raw(),
                            icon.width(),
                            icon.height(),
                        ))
                    });
                    ui.global::<Artwork>()
                        .invoke_icon_result(slot, img, from_local, online, true);
                }) {
                    return;
                }
            }
        });
    }
    artwork.on_request_icon({
        let cache = cache.clone();
        let queue = queue.clone();
        let tickets = tickets.clone();
        let descriptions = descriptions.clone();
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
            let needs_art = if online || cache.enabled() {
                blank(&online_model)
            } else {
                have_scoop.load(Ordering::Relaxed)
            };
            if slots.pending.contains(&index) {
                return;
            }
            let icon = blank(&local_model)
                && needs_art
                && !slots
                    .failures
                    .get(&index)
                    .is_some_and(|(time, tried_online)| {
                        time.elapsed() < RETRY_DELAY && (!online || *tried_online)
                    });
            let (ticket, request, asset_slot, needs_description) = if index <= pinned {
                (0, slots.pinned.get(&index).cloned(), Some(index), false)
            } else {
                let Some(slot) = &slots.dynamic[index - pinned - 1] else {
                    return;
                };
                (
                    slot.ticket,
                    Some(slot.request.clone()),
                    slot.pinned,
                    slot.needs_description,
                )
            };
            let description = (online || cache.enabled())
                && needs_description
                && descriptions
                    .row_data(index)
                    .is_some_and(|text| text.is_empty())
                && slots
                    .description_attempts
                    .get(&index)
                    .is_none_or(|time| time.elapsed() >= RETRY_DELAY);
            if icon || description {
                let dropped = queue.enqueue(Work {
                    id: WorkId {
                        index,
                        ticket,
                        view: queue.view.load(Ordering::Acquire),
                    },
                    request,
                    icon,
                    description,
                    pinned: asset_slot,
                    metadata: None,
                });
                if let Some(dropped) = dropped
                    && dropped.id.current(&queue, &tickets)
                {
                    slots.pending.remove(&dropped.id.index);
                }
                slots.pending.insert(index);
            }
        }
    });
    artwork.on_description_result({
        let descriptions = descriptions.clone();
        let enabled = enabled.clone();
        let cache = cache.clone();
        let slots = slots.clone();
        move |slot, text, tried, done| {
            let row = slot as usize;
            let online = enabled.load(Ordering::Relaxed);
            let mut slots = slots.borrow_mut();
            if done {
                slots.pending.remove(&row);
            }
            if tried && online {
                slots.description_attempts.insert(row, Instant::now());
            }
            drop(slots);
            if (online || cache.enabled())
                && !text.is_empty()
                && descriptions
                    .row_data(row)
                    .is_some_and(|text| text.is_empty())
            {
                descriptions.set_row_data(row, text);
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
        move |slot, image, from_local, online, tried_icon| {
            let index = slot as usize;
            let mut slots = slots.borrow_mut();
            slots.pending.remove(&index);
            if tried_icon && image.size().width > 0 {
                slots.failures.remove(&index);
            } else if tried_icon {
                slots.failures.insert(
                    index,
                    (Instant::now(), online && enabled.load(Ordering::Relaxed)),
                );
            }
            drop(slots);
            if image.size().width > 0 {
                if from_local {
                    local_model.set_row_data(index, image);
                } else {
                    online_model.set_row_data(index, image);
                }
            }
        }
    });
    Loader {
        enabled,
        cache,
        slots,
        online: online_model,
        local: local_model,
        descriptions,
        queue,
        tickets,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work(index: usize, description: bool) -> Work {
        Work {
            id: WorkId {
                index,
                ticket: 0,
                view: 0,
            },
            pinned: None,
            request: None,
            icon: true,
            description,
            metadata: None,
        }
    }

    #[test]
    fn icon_queue_stays_bounded_and_discards_old_views() {
        let queue = WorkQueue::new();
        for index in 0..=MAX_PENDING {
            let discarded = queue.enqueue(work(index, false));
            assert_eq!(
                discarded.map(|work| work.id.index),
                (index == MAX_PENDING).then_some(0)
            );
        }
        assert_eq!(queue.next().unwrap().id.index, MAX_PENDING);
        queue.reset();
        assert_eq!(queue.view.load(Ordering::Acquire), 1);
        assert!(queue.pending.lock().unwrap().as_ref().unwrap().is_empty());
        queue.close();
        assert!(queue.next().is_none());
    }

    #[test]
    fn descriptions_run_in_order_ahead_of_icon_probes() {
        let queue = WorkQueue::new();
        let tickets = [const { AtomicU64::new(0) }; 4];
        queue.enqueue(work(0, false));
        queue.enqueue(work(1, true));
        queue.enqueue(work(2, true));
        let mut first = queue.next().unwrap();
        assert_eq!(first.id.index, 1);
        first.description = false;
        first.metadata = Some(None);
        assert!(queue.defer_icon(first, &tickets));
        queue.enqueue(work(3, false));
        assert_eq!(queue.next().unwrap().id.index, 2);
        assert_eq!(queue.next().unwrap().id.index, 3);
        assert_eq!(queue.next().unwrap().id.index, 0);
        assert!(matches!(queue.next().unwrap().metadata, Some(None)));
        queue.reset();
        assert!(!queue.defer_icon(work(1, false), &tickets));
    }

    #[test]
    fn artwork_is_keyed_by_exact_package_and_checksum() {
        for manifest in [WINGET_ICONS, PUBLISHER_ICONS, UNIGET_ICONS] {
            let mut names = HashSet::new();
            for line in manifest_entries(manifest) {
                let (id, rest) = line.split_once('|').unwrap();
                assert!(names.insert(id.to_ascii_lowercase()));
                assert!(crate::packages::valid_id(id));
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
