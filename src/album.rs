// SPDX-FileCopyrightText: 2026  Amberol contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

use async_channel::Sender;
use gtk::{gdk, glib, prelude::*, subclass::prelude::*};
use lofty::prelude::TaggedFileExt;
use lofty::probe::Probe;
use log::warn;
use once_cell::sync::Lazy;

use crate::{audio::CoverCache, i18n::i18n, library::AlbumRecord, utils};

type CoverKey = (String, String, PathBuf, i64, Option<CoverOverrideState>);
type CoverOverrideState = (bool, i64, u64);
// The grid releases covers when tiles are unbound. Keep only a small working
// set for scrolling back, rather than every album visited during the session.
const COVER_CACHE_LIMIT: usize = 64;
// Old libraries may only have the 500px cache, so the entry count alone would
// allow 64 MiB of textures. Keep a byte budget as well; the grid only needs
// the working set around the visible rows.
const COVER_CACHE_BUDGET_BYTES: usize = 16 * 1024 * 1024;
const COVER_WORKERS: u32 = 2;

static COVER_CACHE: Lazy<Mutex<VecDeque<(CoverKey, Option<gdk::Texture>, usize)>>> =
    Lazy::new(|| Mutex::new(VecDeque::new()));
// Requests waiting for the same album share one decode.
static COVER_REQUESTS: Lazy<Mutex<HashMap<CoverKey, Vec<Sender<Option<gdk::Texture>>>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static COVER_POOL: Lazy<glib::ThreadPool> = Lazy::new(|| {
    glib::ThreadPool::shared(Some(COVER_WORKERS)).expect("Unable to create album cover pool")
});

mod imp {
    use glib::{ParamSpec, ParamSpecBoolean, Value};
    use once_cell::sync::Lazy;

    use super::*;

    #[derive(Default)]
    pub struct Album {
        pub record: RefCell<Option<AlbumRecord>>,
        pub selected: Cell<bool>,
        pub favorite: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Album {
        const NAME: &'static str = "AmberolAlbum";
        type Type = super::Album;
    }

    impl ObjectImpl for Album {
        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: Lazy<Vec<glib::subclass::Signal>> = Lazy::new(|| {
                vec![glib::subclass::Signal::builder("cover-changed").build()]
            });
            SIGNALS.as_ref()
        }

        fn properties() -> &'static [ParamSpec] {
            static PROPERTIES: Lazy<Vec<ParamSpec>> = Lazy::new(|| {
                vec![
                    ParamSpecBoolean::builder("selected").build(),
                    ParamSpecBoolean::builder("favorite").build(),
                ]
            });
            PROPERTIES.as_ref()
        }

        fn set_property(&self, _id: usize, value: &Value, pspec: &ParamSpec) {
            match pspec.name() {
                "selected" => {
                    let selected = value.get().unwrap_or(false);
                    if self.selected.replace(selected) != selected {
                        self.obj().notify("selected");
                    }
                }
                "favorite" => {
                    let favorite = value.get().unwrap_or(false);
                    if self.favorite.replace(favorite) != favorite {
                        self.obj().notify("favorite");
                    }
                }
                _ => unimplemented!(),
            }
        }

        fn property(&self, _id: usize, pspec: &ParamSpec) -> Value {
            match pspec.name() {
                "selected" => self.selected.get().to_value(),
                "favorite" => self.favorite.get().to_value(),
                _ => unimplemented!(),
            }
        }
    }
}

glib::wrapper! {
    pub struct Album(ObjectSubclass<imp::Album>);
}

impl Album {
    pub fn new(record: AlbumRecord) -> Self {
        let album = glib::Object::new::<Self>();
        album.imp().record.replace(Some(record));
        album
    }

    pub fn record(&self) -> AlbumRecord {
        self.imp()
            .record
            .borrow()
            .clone()
            .expect("Album record must be set")
    }

    pub fn is_selected(&self) -> bool {
        self.imp().selected.get()
    }

    pub fn set_selected(&self, selected: bool) {
        if self.imp().selected.replace(selected) != selected {
            self.notify("selected");
        }
    }

    pub fn is_favorite(&self) -> bool {
        self.imp().favorite.get()
    }

    pub fn set_favorite(&self, favorite: bool) {
        if self.imp().favorite.replace(favorite) != favorite {
            self.notify("favorite");
        }
    }

    pub fn title(&self) -> String {
        let record = self.imp().record.borrow();
        let title = record
            .as_ref()
            .map(|record| record.title.as_str())
            .unwrap_or("");
        if title.is_empty() {
            i18n("Unknown album")
        } else {
            title.to_owned()
        }
    }

    pub fn artist(&self) -> String {
        let record = self.imp().record.borrow();
        let artist = record
            .as_ref()
            .map(|record| record.artist.as_str())
            .unwrap_or("");
        if artist.is_empty() {
            i18n("Unknown artist")
        } else {
            artist.to_owned()
        }
    }

    pub fn track_count(&self) -> u32 {
        self.imp()
            .record
            .borrow()
            .as_ref()
            .map(|record| record.track_count)
            .unwrap_or(0)
    }

    pub fn cover_texture(&self) -> Option<gdk::Texture> {
        let record = self.imp().record.borrow().clone()?;
        let key = cover_key(&record);

        if let Some(cached) = lookup_cover(&key) {
            return cached;
        }

        let cover = load_cover(&record);
        store_cover(key, cover.clone());
        cover
    }

    /// Loads the cover without blocking the UI thread.
    ///
    /// Album tiles use this instead of `cover_texture`; the callback runs on
    /// the main thread once the artwork has been decoded.
    pub fn cover_texture_async<F>(&self, callback: F)
    where
        F: FnOnce(Option<gdk::Texture>) + 'static,
    {
        let Some(record) = self.imp().record.borrow().clone() else {
            callback(None);
            return;
        };
        let key = cover_key(&record);

        if let Some(cached) = lookup_cover(&key) {
            callback(cached);
            return;
        }

        let (sender, receiver) = async_channel::bounded(1);
        let start_worker = {
            let Ok(mut requests) = COVER_REQUESTS.lock() else {
                callback(None);
                return;
            };
            match requests.get_mut(&key) {
                Some(waiters) => {
                    waiters.push(sender);
                    false
                }
                None => {
                    requests.insert(key.clone(), vec![sender]);
                    true
                }
            }
        };

        if start_worker {
            let worker_key = key.clone();
            let result = COVER_POOL.push(move || {
                let cover = load_cover(&record);
                store_cover(worker_key.clone(), cover.clone());

                let waiters = COVER_REQUESTS
                    .lock()
                    .ok()
                    .and_then(|mut requests| requests.remove(&worker_key));
                if let Some(waiters) = waiters {
                    for waiter in waiters {
                        let _ = waiter.send_blocking(cover.clone());
                    }
                }
            });

            if let Err(err) = result {
                warn!("Unable to queue album cover loading: {err}");
                let waiters = COVER_REQUESTS
                    .lock()
                    .ok()
                    .and_then(|mut requests| requests.remove(&key));
                if let Some(waiters) = waiters {
                    for waiter in waiters {
                        let _ = waiter.send_blocking(None);
                    }
                }
            }
        }

        glib::MainContext::default().spawn_local(async move {
            if let Ok(cover) = receiver.recv().await {
                callback(cover);
            }
        });
    }

    /// Drops the in-memory textures after the index has been updated.
    pub fn clear_cover_cache() {
        if let Ok(mut cache) = COVER_CACHE.lock() {
            cache.clear();
        }
    }

    /// Tells a visible album tile to pick up a custom cover or its removal.
    pub fn notify_cover_changed(&self) {
        self.emit_by_name::<()>("cover-changed", &[]);
    }
}

fn cover_key(record: &AlbumRecord) -> CoverKey {
    (
        record.title.clone(),
        record.artist.clone(),
        record.cover_path.clone(),
        record.modified_at,
        cover_override_state(record),
    )
}

fn cover_override_state(record: &AlbumRecord) -> Option<CoverOverrideState> {
    let image = utils::album_cover_override_path(&record.title, &record.artist);
    if let Ok(metadata) = fs::metadata(&image) {
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos() as i64)
            .unwrap_or(0);
        return Some((false, modified, metadata.len()));
    }

    let hidden = utils::album_cover_hidden_marker_path(&record.title, &record.artist);
    hidden.is_file().then_some((true, 0, 0))
}

fn texture_bytes(texture: Option<&gdk::Texture>) -> usize {
    let Some(texture) = texture else {
        return 0;
    };
    (texture.width().max(0) as usize)
        .saturating_mul(texture.height().max(0) as usize)
        .saturating_mul(4)
        .saturating_add(256)
}

fn lookup_cover(key: &CoverKey) -> Option<Option<gdk::Texture>> {
    let mut cache = COVER_CACHE.lock().ok()?;
    let index = cache
        .iter()
        .position(|(cached_key, _, _)| cached_key == key)?;
    let entry = cache.remove(index)?;
    let cover = entry.1.clone();
    cache.push_back(entry);
    Some(cover)
}

fn store_cover(key: CoverKey, cover: Option<gdk::Texture>) {
    let Ok(mut cache) = COVER_CACHE.lock() else {
        return;
    };
    if let Some(index) = cache
        .iter()
        .position(|(cached_key, _, _)| cached_key == &key)
    {
        cache.remove(index);
    }

    let bytes = texture_bytes(cover.as_ref());
    cache.push_back((key, cover, bytes));

    while cache.len() > COVER_CACHE_LIMIT {
        cache.pop_front();
    }

    let mut total: usize = cache.iter().map(|(_, _, bytes)| *bytes).sum();
    while total > COVER_CACHE_BUDGET_BYTES && cache.len() > 1 {
        if let Some((_, _, bytes)) = cache.pop_front() {
            total = total.saturating_sub(bytes);
        } else {
            break;
        }
    }
}

fn texture_from_file(path: &Path) -> Option<gdk::Texture> {
    // Album tiles are small; decoding external artwork at 500px would keep
    // several megabytes per visible tile alive for no visible benefit.
    let pixbuf = utils::load_cover_file_at(path, utils::COVER_SMALL_SIZE)?;
    Some(gdk::Texture::for_pixbuf(&pixbuf))
}

/// Loads an album thumbnail. Legacy entries produced by older versions hold
/// the original multi-megabyte artwork, so they are replaced with a compact
/// thumbnail while the decoded pixels are still available.
fn texture_from_cache_file(
    path: &Path,
    max_size: i32,
) -> Option<gdk::Texture> {
    let legacy = fs::metadata(path)
        .map(|metadata| metadata.len() > utils::COVER_CACHE_MAX_BYTES)
        .unwrap_or(false);
    let data = fs::read(path).ok()?;
    let pixbuf = utils::load_cover_pixbuf(&glib::Bytes::from_owned(data), max_size)?;
    let texture = gdk::Texture::for_pixbuf(&pixbuf);

    if legacy {
        write_compact_cover(path, &pixbuf);
    }

    Some(texture)
}

fn write_compact_cover(path: &Path, pixbuf: &gdk_pixbuf::Pixbuf) {
    if let Some(thumbnail) = utils::cover_thumbnail_from_pixbuf(pixbuf) {
        utils::write_thumbnail_file(path, &thumbnail);
    }
}

fn external_cover(track_path: &Path) -> Option<PathBuf> {
    let parent = track_path.parent()?;
    const NAMES: &[&str] = &[
        "Cover.jpg",
        "Cover.jpeg",
        "Cover.png",
        "cover.jpg",
        "cover.jpeg",
        "cover.png",
        "Folder.jpg",
        "Folder.jpeg",
        "Folder.png",
        "folder.jpg",
        "folder.jpeg",
        "folder.png",
    ];

    NAMES
        .iter()
        .map(|name| parent.join(name))
        .find(|path| path.is_file())
}

fn load_cover(record: &AlbumRecord) -> Option<gdk::Texture> {
    // A cover chosen by the user takes precedence over anything extracted
    // from the audio files. Never rewrite this file as a legacy thumbnail.
    let override_path =
        utils::album_cover_override_path(&record.title, &record.artist);
    if override_path.is_file() {
        if let Some(pixbuf) =
            utils::load_cover_file_at(&override_path, utils::COVER_SMALL_SIZE)
        {
            return Some(gdk::Texture::for_pixbuf(&pixbuf));
        }
    }

    if utils::album_cover_hidden_marker_path(&record.title, &record.artist).is_file() {
        return None;
    }

    // Covers extracted during the scan are already scaled and can be loaded
    // without touching the audio file again. The grid uses the dedicated
    // 200px thumbnail; the 500px image is only a fallback for libraries
    // indexed before the small cache existed.
    let small_cache_path =
        utils::album_cover_small_cache_path(&record.title, &record.artist, &record.cover_path);
    if small_cache_path.is_file() {
        if let Some(texture) =
            texture_from_cache_file(&small_cache_path, utils::COVER_SMALL_SIZE)
        {
            return Some(texture);
        }
    }

    let cache_path =
        utils::album_cover_cache_path(&record.title, &record.artist, &record.cover_path);
    if cache_path.is_file() {
        // The 500px cache is only a fallback for libraries indexed before the
        // 200px thumbnails existed. Decode it at grid size without rewriting
        // the large image, so the player still gets its full-resolution cover.
        if let Some(pixbuf) = utils::load_cover_file_at(&cache_path, utils::COVER_SMALL_SIZE) {
            let texture = gdk::Texture::for_pixbuf(&pixbuf);
            if !small_cache_path.is_file() {
                utils::ensure_cover_thumbnails(&cache_path, false);
            }
            return Some(texture);
        }
    }

    if let Some(cover_path) = external_cover(&record.cover_path) {
        if let Some(texture) = texture_from_file(&cover_path) {
            return Some(texture);
        }
    }

    // Fallback for libraries indexed by older versions, before the album
    // thumbnail cache existed.
    let tagged_file = Probe::open(&record.cover_path)
        .ok()?
        .guess_file_type()
        .ok()?
        .read()
        .ok()?;
    let tag = tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag())?;
    let mut cache = CoverCache::global().lock().ok()?;
    cache.cover_art(&record.cover_path, tag).map(|(cover, _)| {
        if let Some(path) = cover.cache() {
            if let Some(pixbuf) = utils::load_cover_file_at(path, utils::COVER_SMALL_SIZE) {
                return gdk::Texture::for_pixbuf(&pixbuf);
            }
        }
        cover.texture().clone()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cover_cache_is_bounded_and_refreshes_recent_entries() {
        Album::clear_cover_cache();
        let album = |index| {
            Album::new(AlbumRecord {
                title: format!("album {index}"),
                artist: String::new(),
                cover_path: PathBuf::from(format!("/nonexistent-amberol-test/{index}")),
                track_count: 1,
                modified_at: 0,
            })
        };
        for index in 0..COVER_CACHE_LIMIT {
            assert!(album(index).cover_texture().is_none());
        }
        // A negative cache hit must also refresh its recency.
        assert!(album(0).cover_texture().is_none());
        assert!(album(COVER_CACHE_LIMIT).cover_texture().is_none());
        let cache = COVER_CACHE.lock().unwrap();
        assert_eq!(cache.len(), COVER_CACHE_LIMIT);
        assert!(cache.iter().any(|(key, _, _)| key.0 == "album 0"));
        assert!(!cache.iter().any(|(key, _, _)| key.0 == "album 1"));
        drop(cache);
        Album::clear_cover_cache();
    }
}
