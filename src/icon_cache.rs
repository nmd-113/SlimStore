use crate::{
    http, metadata,
    storage::{CacheFile, Storage},
};
use image::RgbaImage;
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

const MAGIC: &[u8] = b"SLIMIC01";
const LIMIT: usize = 192;
const SIDE: u32 = 64;
const TTL: u64 = 14 * 24 * 60 * 60;
const METADATA_MAGIC: &[u8] = b"SLIMMD01";
const METADATA_LIMIT: usize = 128;
pub type Key = [u8; 32];

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct PublicMetadata {
    pub description: String,
    pub publisher: String,
    pub license: String,
    pub website: String,
    pub notes: String,
    pub support: String,
    pub publisher_site: String,
    pub license_site: String,
}

impl PublicMetadata {
    fn links(&mut self) -> [&mut String; 5] {
        [
            &mut self.website,
            &mut self.notes,
            &mut self.support,
            &mut self.publisher_site,
            &mut self.license_site,
        ]
    }

    fn sanitize(&mut self) {
        self.description = metadata::plain_text(&self.description, 3072);
        self.publisher = metadata::plain_text(&self.publisher, 128);
        self.license = metadata::plain_text(&self.license, 256);
        for url in self.links() {
            if url.len() > 1024 || crate::platform::browser_link(url).is_none() {
                url.clear();
            }
        }
    }

    pub fn fill_missing(&mut self, mut fallback: Self) {
        for (field, other) in [
            (&mut self.description, &mut fallback.description),
            (&mut self.publisher, &mut fallback.publisher),
            (&mut self.license, &mut fallback.license),
        ] {
            if field.is_empty() {
                *field = std::mem::take(other);
            }
        }
        for (field, other) in self.links().into_iter().zip(fallback.links()) {
            if field.is_empty() {
                *field = std::mem::take(other);
            }
        }
    }
}

#[derive(Deserialize, Serialize)]
struct MetadataEntry {
    key: Key,
    saved: u64,
    value: PublicMetadata,
}

#[derive(Default)]
struct MetadataSnapshot(VecDeque<MetadataEntry>);

impl MetadataSnapshot {
    fn decode(bytes: &[u8], now: u64) -> Option<Self> {
        let (body, checksum) = bytes.split_at_checked(bytes.len().checked_sub(32)?)?;
        if http::sha256(body)? != checksum {
            return None;
        }
        let entries: Vec<MetadataEntry> =
            serde_json::from_slice(body.strip_prefix(METADATA_MAGIC)?).ok()?;
        if entries.len() > METADATA_LIMIT {
            return None;
        }
        let mut result = Self::default();
        for mut entry in entries {
            if now.checked_sub(entry.saved).is_some_and(|age| age < TTL) {
                entry.value.sanitize();
                result.insert(entry);
            }
        }
        Some(result)
    }

    fn encode(&self) -> Option<Vec<u8>> {
        let mut bytes = METADATA_MAGIC.to_vec();
        serde_json::to_writer(&mut bytes, &self.0).ok()?;
        bytes.extend_from_slice(&http::sha256(&bytes)?);
        Some(bytes)
    }

    fn insert(&mut self, mut entry: MetadataEntry) {
        if let Some(index) = self.0.iter().position(|old| old.key == entry.key) {
            let mut old = self.0.remove(index).unwrap();
            if old.saved > entry.saved {
                std::mem::swap(&mut old, &mut entry);
            }
            entry.value.fill_missing(old.value);
        }
        let position = self.0.partition_point(|old| old.saved <= entry.saved);
        self.0.insert(position, entry);
        if self.0.len() > METADATA_LIMIT {
            self.0.pop_front();
        }
    }

    fn get(&self, key: &Key) -> Option<PublicMetadata> {
        let now = now();
        self.0
            .iter()
            .find(|e| &e.key == key && now.checked_sub(e.saved).is_some_and(|age| age < TTL))
            .map(|e| e.value.clone())
    }
}

#[derive(Default)]
struct MetadataCache {
    loaded: Option<MetadataSnapshot>,
    pending: MetadataSnapshot,
}

impl MetadataCache {
    fn get(&mut self, storage: &Storage, key: &Key) -> Option<PublicMetadata> {
        self.pending.get(key).or_else(|| {
            self.loaded
                .get_or_insert_with(|| {
                    storage
                        .read_cache(CacheFile::Metadata)
                        .ok()
                        .and_then(|bytes| MetadataSnapshot::decode(&bytes, now()))
                        .unwrap_or_default()
                })
                .get(key)
        })
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

struct Entry {
    key: Key,
    saved: u64,
    image: RgbaImage,
}

#[derive(Default)]
struct Snapshot(VecDeque<Entry>);

impl Snapshot {
    fn decode(bytes: &[u8], now: u64) -> Option<Self> {
        let (body, checksum) = bytes.split_at_checked(bytes.len().checked_sub(32)?)?;
        if http::sha256(body)? != checksum {
            return None;
        }
        let mut bytes = body.strip_prefix(MAGIC)?;
        let mut result = Self::default();
        let mut count = 0;
        while !bytes.is_empty() {
            count += 1;
            if count > LIMIT {
                return None;
            }
            let (header, rest) = bytes.split_at_checked(42)?;
            let key = header[..32].try_into().ok()?;
            let saved = u64::from_le_bytes(header[32..40].try_into().ok()?);
            let (w, h) = (u32::from(header[40]), u32::from(header[41]));
            if !(1..=SIDE).contains(&w) || !(1..=SIDE).contains(&h) {
                return None;
            }
            let (pixels, rest) = rest.split_at_checked((w * h * 4) as usize)?;
            bytes = rest;
            if now.checked_sub(saved).is_some_and(|age| age < TTL) {
                let image = RgbaImage::from_raw(w, h, pixels.to_vec())?;
                result.insert(Entry { key, saved, image });
            }
        }
        Some(result)
    }

    fn encode(&self) -> Option<Vec<u8>> {
        let mut bytes = MAGIC.to_vec();
        for entry in &self.0 {
            bytes.extend_from_slice(&entry.key);
            bytes.extend_from_slice(&entry.saved.to_le_bytes());
            bytes.extend([entry.image.width() as u8, entry.image.height() as u8]);
            bytes.extend_from_slice(entry.image.as_raw());
        }
        bytes.extend_from_slice(&http::sha256(&bytes)?);
        Some(bytes)
    }

    fn insert(&mut self, entry: Entry) {
        if let Some(index) = self.0.iter().position(|old| old.key == entry.key) {
            if self.0[index].saved > entry.saved {
                return;
            }
            self.0.remove(index);
        }
        let position = self.0.partition_point(|old| old.saved <= entry.saved);
        self.0.insert(position, entry);
        if self.0.len() > LIMIT {
            self.0.pop_front();
        }
    }

    fn get(&self, key: &Key) -> Option<RgbaImage> {
        let now = now();
        self.0
            .iter()
            .find(|e| &e.key == key && now.checked_sub(e.saved).is_some_and(|age| age < TTL))
            .map(|entry| entry.image.clone())
    }
}

pub struct IconCache {
    storage: Storage,
    enabled: AtomicBool,
    loaded: Mutex<Option<Snapshot>>,
    pending: Mutex<Snapshot>,
    metadata: Mutex<MetadataCache>,
}

impl IconCache {
    pub fn new(storage: Storage, enabled: bool) -> Self {
        Self {
            storage,
            enabled: AtomicBool::new(enabled),
            loaded: Mutex::default(),
            pending: Mutex::default(),
            metadata: Mutex::default(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    pub fn set_enabled(&self, enabled: bool) -> bool {
        if self.enabled.swap(enabled, Ordering::AcqRel) == enabled {
            return false;
        }
        if !enabled {
            self.pending.lock().unwrap().0.clear();
            self.loaded.lock().unwrap().take();
            *self.metadata.lock().unwrap() = MetadataCache::default();
        }
        true
    }

    pub fn get(&self, key: &Key) -> Option<RgbaImage> {
        if !self.enabled() {
            return None;
        }
        if let Some(image) = self.pending.lock().unwrap().get(key) {
            return Some(image);
        }
        self.loaded
            .lock()
            .unwrap()
            .get_or_insert_with(|| {
                self.storage
                    .read_cache(CacheFile::Icons)
                    .ok()
                    .and_then(|bytes| Snapshot::decode(&bytes, now()))
                    .unwrap_or_default()
            })
            .get(key)
    }

    pub fn get_metadata(&self, key: &Key) -> Option<PublicMetadata> {
        if !self.enabled() {
            return None;
        }
        self.metadata.lock().unwrap().get(&self.storage, key)
    }

    pub fn record_metadata(&self, key: Key, mut value: PublicMetadata, prefer_live: bool) {
        if !self.enabled() {
            return;
        }
        value.sanitize();
        if value == PublicMetadata::default() {
            return;
        }
        let mut cache = self.metadata.lock().unwrap();
        if !self.enabled() {
            return;
        }
        if let Some(previous) = cache.get(&self.storage, &key) {
            let mut existing = previous.clone();
            if prefer_live {
                value.fill_missing(existing);
            } else {
                existing.fill_missing(value);
                value = existing;
            }
            if value == previous {
                return;
            }
        }
        cache.pending.insert(MetadataEntry {
            key,
            saved: now(),
            value,
        });
    }

    pub fn record(&self, key: Key, image: &RgbaImage) {
        if !self.enabled() {
            return;
        }
        let mut pending = self.pending.lock().unwrap();
        if !self.enabled() {
            return;
        }
        let image = if image.width() <= SIDE && image.height() <= SIDE {
            image.clone()
        } else {
            image::imageops::thumbnail(image, SIDE, SIDE)
        };
        pending.insert(Entry {
            key,
            saved: now(),
            image,
        });
    }

    pub fn finish(&self) {
        if !self.enabled.swap(false, Ordering::AcqRel) {
            return;
        }
        let pending = std::mem::take(&mut *self.pending.lock().unwrap());
        let metadata = std::mem::take(&mut self.metadata.lock().unwrap().pending);
        if !metadata.0.is_empty() {
            let _ = self.storage.update_cache(CacheFile::Metadata, |bytes| {
                let mut merged = bytes
                    .and_then(|bytes| MetadataSnapshot::decode(&bytes, now()))
                    .unwrap_or_default();
                for entry in metadata.0 {
                    merged.insert(entry);
                }
                merged.encode()
            });
        }
        if pending.0.is_empty() {
            return;
        }
        // The cache is optional. Read-only or unavailable storage falls back to memory.
        let _ = self.storage.update_cache(CacheFile::Icons, |bytes| {
            let mut merged = bytes
                .and_then(|bytes| Snapshot::decode(&bytes, now()))
                .unwrap_or_default();
            for entry in pending.0 {
                merged.insert(entry);
            }
            merged.encode()
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Settings;
    use std::fs;

    #[test]
    fn cache_roundtrips_only_with_saved_opt_in_and_can_be_removed() {
        let dir = std::env::temp_dir().join(format!(
            "slimstore-icon-test-{}-{}",
            std::process::id(),
            now()
        ));
        let mut storage = Storage::beside(&dir.join("SlimStore.exe"));
        let icon = RgbaImage::from_pixel(64, 32, image::Rgba([12, 34, 56, 255]));
        let key = [42; 32];
        let data = PublicMetadata {
            description: "An app\nfor making maps".into(),
            website: "https://example.org/maps".into(),
            publisher: "Example Studio".into(),
            support: "http://example.org/insecure".into(),
            ..PublicMetadata::default()
        };
        let transient = IconCache::new(storage.clone(), true);
        transient.record(key, &icon);
        transient.record_metadata(key, data.clone(), true);
        transient.finish();
        assert!(!dir.exists());

        storage
            .save(
                &Settings {
                    icon_cache: true,
                    ..Default::default()
                },
                true,
            )
            .unwrap();
        let first = IconCache::new(storage.clone(), true);
        first.record(key, &icon);
        first.record_metadata(key, data, true);
        first.record_metadata(
            key,
            PublicMetadata {
                description: "Background manifest description".into(),
                license: "GPL-3.0".into(),
                ..PublicMetadata::default()
            },
            false,
        );
        first.finish();
        let bytes = storage.read_cache(CacheFile::Icons).unwrap();
        let metadata_bytes = storage.read_cache(CacheFile::Metadata).unwrap();
        let second = IconCache::new(storage.clone(), true);
        assert_eq!(second.get(&key), Some(icon));
        assert!(second.get(&[43; 32]).is_none());
        let cached = second.get_metadata(&key).unwrap();
        assert_eq!(cached.description, "An app for making maps");
        assert_eq!(cached.publisher, "Example Studio");
        assert_eq!(cached.website, "https://example.org/maps");
        assert_eq!(cached.license, "GPL-3.0");
        assert!(cached.support.is_empty());
        assert!(second.get_metadata(&[43; 32]).is_none());
        assert!(Snapshot::decode(&bytes[..bytes.len() - 1], now()).is_none());
        assert!(Snapshot::decode(&bytes, now() + TTL).unwrap().0.is_empty());
        assert!(
            MetadataSnapshot::decode(&metadata_bytes[..metadata_bytes.len() - 1], now()).is_none()
        );
        assert!(
            MetadataSnapshot::decode(&metadata_bytes, now() + TTL)
                .unwrap()
                .0
                .is_empty()
        );

        second.record_metadata([43; 32], cached, true);
        storage.save(&Settings::default(), false).unwrap();
        storage.clear_cache().unwrap();
        second.finish();
        assert!(storage.read_cache(CacheFile::Icons).is_err());
        assert!(storage.read_cache(CacheFile::Metadata).is_err());
        fs::remove_dir(dir.join("SlimStore-data")).unwrap();
        fs::remove_dir(dir).unwrap();
    }
}
