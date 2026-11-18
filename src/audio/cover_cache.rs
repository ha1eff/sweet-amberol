// SPDX-FileCopyrightText: 2022  Emmanuele Bassi
// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::Mutex,
};

use gtk::{gdk, gio, glib, prelude::*};
use log::debug;
use once_cell::sync::{Lazy, OnceCell};
use sha2::{Digest, Sha256};

use crate::utils;

/// Embedded artwork used as a fallback when the library has no cached cover.
/// This cache is deliberately small: queue rows and the player cover use the
/// compact thumbnails below instead of keeping a texture per song.
const COVER_CACHE_BUDGET_BYTES: usize = 16 * 1024 * 1024;

/// 200px album covers used by queue rows and the library grid. Keeping this
/// bounded means scrolling through a large playlist never accumulates one
/// texture per album for the whole session.
const THUMBNAIL_CACHE_BUDGET_BYTES: usize = 24 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct CoverArt {
    texture: gdk::Texture,
    palette: Vec<gdk::RGBA>,
    cache: Option<PathBuf>,
}

impl CoverArt {
    pub fn texture(&self) -> &gdk::Texture {
        self.texture.as_ref()
    }

    pub fn palette(&self) -> &Vec<gdk::RGBA> {
        self.palette.as_ref()
    }

    pub fn cache(&self) -> Option<&PathBuf> {
        self.cache.as_ref()
    }

    fn memory_bytes(&self) -> usize {
        estimate_texture_bytes(&self.texture)
            .saturating_add(self.palette.len() * std::mem::size_of::<gdk::RGBA>())
            .saturating_add(
                self.cache
                    .as_ref()
                    .map(|path| path.as_os_str().len())
                    .unwrap_or(0),
            )
    }
}

fn estimate_texture_bytes(texture: &gdk::Texture) -> usize {
    (texture.width().max(0) as usize)
        .saturating_mul(texture.height().max(0) as usize)
        .saturating_mul(4)
        .saturating_add(256)
}

#[derive(Debug)]
pub struct CoverCache {
    entries: HashMap<String, CoverArt>,
    order: VecDeque<String>,
    bytes: usize,
}

impl CoverCache {
    pub fn global() -> &'static Mutex<CoverCache> {
        static CACHE: OnceCell<Mutex<CoverCache>> = OnceCell::new();

        CACHE.get_or_init(|| {
            let c = CoverCache::new();
            Mutex::new(c)
        })
    }

    fn new() -> Self {
        CoverCache {
            entries: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
        }
    }

    fn add_entry(&mut self, uuid: String, cover: CoverArt) {
        self.remove_entry(&uuid);
        self.bytes = self.bytes.saturating_add(cover.memory_bytes());
        self.order.push_back(uuid.clone());
        self.entries.insert(uuid, cover);
        self.trim_to_budget();
    }

    fn remove_entry(&mut self, uuid: &str) {
        if let Some(old) = self.entries.remove(uuid) {
            self.bytes = self.bytes.saturating_sub(old.memory_bytes());
        }
        if let Some(position) = self.order.iter().position(|key| key == uuid) {
            self.order.remove(position);
        }
    }

    pub(crate) fn lookup(&mut self, uuid: &str) -> Option<CoverArt> {
        let cover = self.entries.get(uuid)?.clone();
        if let Some(position) = self.order.iter().position(|key| key == uuid) {
            self.order.remove(position);
        }
        self.order.push_back(uuid.to_string());
        Some(cover)
    }

    /// Returns the cached texture without decoding the audio file again.
    pub fn lookup_texture(&mut self, uuid: &str) -> Option<gdk::Texture> {
        self.lookup(uuid).map(|cover| cover.texture)
    }

    fn trim_to_budget(&mut self) {
        while self.bytes > COVER_CACHE_BUDGET_BYTES && self.order.len() > 1 {
            let Some(oldest) = self.order.front().cloned() else {
                break;
            };
            // Do not evict the entry that was just inserted; it is the one
            // the caller is about to draw.
            if self
                .order
                .back()
                .is_some_and(|newest| newest == &oldest)
            {
                break;
            }
            self.remove_entry(&oldest);
        }
    }

    fn load_cover_art(&self, tag: &lofty::tag::Tag, path: Option<&Path>) -> Option<glib::Bytes> {
        if let Some(picture) = tag.get_picture_type(lofty::picture::PictureType::CoverFront) {
            debug!("Found CoverFront");
            return Some(glib::Bytes::from(picture.data()));
        } else {
            // If we don't have a CoverFront picture, we fall back to Other
            // and BandLogo types
            for picture in tag.pictures() {
                let cover_art = match picture.pic_type() {
                    lofty::picture::PictureType::Other => Some(glib::Bytes::from(picture.data())),
                    lofty::picture::PictureType::BandLogo => {
                        Some(glib::Bytes::from(picture.data()))
                    }
                    _ => None,
                };

                if cover_art.is_some() {
                    debug!("Found fallback");
                    return cover_art;
                }
            }
        }

        // We always favour the cover art in the song metadata because it's going
        // to be in a hot cache; looking for a separate file will blow a bunch of
        // caches out of the water, which will slow down loading the song into the
        // playlist model
        if let Some(p) = path {
            let ext_cover_basename = vec!["Cover", "cover", "Folder", "folder"];
            let ext_cover_ext = vec!["jpg", "png"];

            let ext_covers = ext_cover_basename
                .iter()
                .map(|&b| {
                    ext_cover_ext
                        .iter()
                        .map(move |&e| format!("{}.{}", &b, &e))
                        .collect::<Vec<_>>()
                })
                .fold(vec![], |mut v, mut dat| {
                    v.append(&mut dat);
                    v
                });
            for name in ext_covers {
                let mut cover_file = PathBuf::from(p);
                cover_file.push(name);
                debug!("Looking for external cover file: {:?}", &cover_file);

                let f = gio::File::for_path(&cover_file);
                if let Ok((res, _)) = f.load_bytes(None::<&gio::Cancellable>) {
                    debug!("Loading cover from external cover file");
                    return Some(res);
                }
            }
        }

        debug!("No cover art");

        None
    }

    pub fn cover_art(&mut self, path: &Path, tag: &lofty::tag::Tag) -> Option<(CoverArt, String)> {
        let mut album_artist = None;
        let mut track_artist = None;
        let mut album = None;

        fn get_text_value(value: &lofty::tag::ItemValue) -> Option<String> {
            match value {
                lofty::tag::ItemValue::Text(s) => Some(s.to_string()),
                _ => None,
            }
        }

        for item in tag.items() {
            match item.key() {
                lofty::prelude::ItemKey::AlbumTitle => album = get_text_value(item.value()),
                lofty::prelude::ItemKey::AlbumArtist => album_artist = get_text_value(item.value()),
                lofty::prelude::ItemKey::TrackArtist => track_artist = get_text_value(item.value()),
                _ => (),
            };
        }

        let uuid = cover_id_from_values(
            path,
            album.as_deref(),
            album_artist.as_deref(),
            track_artist.as_deref(),
        );

        match self.lookup(&uuid) {
            Some(c) => {
                debug!("Found cover for UUID '{}'", &uuid);
                Some((c.clone(), uuid))
            }
            None => {
                debug!("Loading cover art for UUID: {}", &uuid);

                // The library pre-extracts a compact cover for every album.
                // Prefer it over the embedded artwork: it avoids parsing and
                // decoding multi-megabyte pictures while the queue loads.
                let cached_cover = album.as_deref().and_then(|album| {
                    let artist = album_artist.as_deref().or(track_artist.as_deref())?;
                    let cache_path =
                        utils::album_cover_cache_path(album, artist, path);
                    let legacy = std::fs::metadata(&cache_path)
                        .map(|metadata| metadata.len() > utils::COVER_CACHE_MAX_BYTES)
                        .unwrap_or(false);
                    let data = std::fs::read(&cache_path).ok()?;
                    let pixbuf =
                        utils::load_cover_texture(&glib::Bytes::from_owned(data))?;
                    if legacy {
                        // Replacing the legacy full-size artwork with a
                        // thumbnail now makes every later playlist load decode
                        // in about a millisecond.
                        if let Some(thumbnail) =
                            utils::cover_thumbnail_from_pixbuf(&pixbuf)
                        {
                            utils::write_thumbnail_file(&cache_path, &thumbnail);
                        }
                    }
                    Some(pixbuf)
                });

                let cover_art = if cached_cover.is_none() {
                    self.load_cover_art(tag, path.parent())
                } else {
                    None
                };

                // The pixel buffer for the cover art
                let cover_pixbuf = cached_cover
                    .or_else(|| cover_art.as_ref().and_then(utils::load_cover_texture));

                // Cache the pixel buffer, so that the MPRIS controller can
                // reference it later
                let cache_path = if let Some(ref pixbuf) = cover_pixbuf {
                    utils::cache_cover_art(&uuid, pixbuf)
                } else {
                    None
                };

                // The texture we draw on screen
                let texture = cover_pixbuf.as_ref().map(gdk::Texture::for_pixbuf);

                // The color palette we use for styling the UI
                let palette = if let Some(ref pixbuf) = cover_pixbuf {
                    utils::load_palette(pixbuf)
                } else {
                    None
                };

                // We want both texture and palette
                if texture.is_some() && palette.is_some() {
                    let res = CoverArt {
                        texture: texture.unwrap(),
                        palette: palette.unwrap(),
                        cache: cache_path,
                    };

                    self.add_entry(uuid.clone(), res.clone());

                    Some((res, uuid))
                } else {
                    None
                }
            }
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.bytes = 0;
    }
}

/// Returns the stable identifier used by [`CoverCache`] for an album. This
/// only reads the tag values, so callers can decide later whether the artwork
/// actually needs to be decoded.
pub fn cover_id(path: &Path, tag: &lofty::tag::Tag) -> String {
    let mut album = None;
    let mut album_artist = None;
    let mut track_artist = None;

    fn get_text_value(value: &lofty::tag::ItemValue) -> Option<String> {
        match value {
            lofty::tag::ItemValue::Text(s) => Some(s.to_string()),
            _ => None,
        }
    }

    for item in tag.items() {
        match item.key() {
            lofty::prelude::ItemKey::AlbumTitle => album = get_text_value(item.value()),
            lofty::prelude::ItemKey::AlbumArtist => {
                album_artist = get_text_value(item.value())
            }
            lofty::prelude::ItemKey::TrackArtist => {
                track_artist = get_text_value(item.value())
            }
            _ => (),
        };
    }

    cover_id_from_values(
        path,
        album.as_deref(),
        album_artist.as_deref(),
        track_artist.as_deref(),
    )
}

fn cover_id_from_values(
    path: &Path,
    album: Option<&str>,
    album_artist: Option<&str>,
    track_artist: Option<&str>,
) -> String {
    // We use the album and artist to ensure we share the same cover data for
    // every track in the album; if we don't have an album, we use the file
    // name.
    let mut hasher = Sha256::new();
    if let Some(album) = album {
        hasher.update(album.as_bytes());

        if let Some(artist) = album_artist {
            hasher.update(artist.as_bytes());
        } else if let Some(artist) = track_artist {
            hasher.update(artist.as_bytes());
        }

        if let Some(parent) = path.parent() {
            hasher.update(parent.to_string_lossy().as_bytes());
        }
    } else {
        hasher.update(path.to_string_lossy().as_bytes());
    }

    format!("{:x}", hasher.finalize())
}

struct ThumbnailEntry {
    texture: gdk::Texture,
    bytes: usize,
}

#[derive(Default)]
struct ThumbnailCache {
    entries: HashMap<(PathBuf, i32), ThumbnailEntry>,
    order: VecDeque<(PathBuf, i32)>,
    bytes: usize,
}

impl ThumbnailCache {
    fn store(&mut self, key: (PathBuf, i32), texture: gdk::Texture) {
        if let Some(old) = self.entries.remove(&key) {
            self.bytes = self.bytes.saturating_sub(old.bytes);
            if let Some(position) = self.order.iter().position(|cached| cached == &key) {
                self.order.remove(position);
            }
        }

        let bytes = estimate_texture_bytes(&texture);
        self.bytes = self.bytes.saturating_add(bytes);
        self.order.push_back(key.clone());
        self.entries.insert(key, ThumbnailEntry { texture, bytes });

        while self.bytes > THUMBNAIL_CACHE_BUDGET_BYTES && self.order.len() > 1 {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(old) = self.entries.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(old.bytes);
            }
        }
    }

    fn lookup(&mut self, key: &(PathBuf, i32)) -> Option<gdk::Texture> {
        let texture = self.entries.get(key)?.texture.clone();
        if let Some(position) = self.order.iter().position(|cached| cached == key) {
            self.order.remove(position);
        }
        self.order.push_back(key.clone());
        Some(texture)
    }
}

static THUMBNAILS: Lazy<Mutex<ThumbnailCache>> =
    Lazy::new(|| Mutex::new(ThumbnailCache::default()));

/// Loads a cached album thumbnail and keeps at most a fixed amount of them in
/// memory. Visible rows can decode 200px artwork cheaply; the rest is dropped
/// instead of being retained for every song in the queue.
pub fn cached_thumbnail(path: &Path, max_size: i32) -> Option<gdk::Texture> {
    let key = (path.to_path_buf(), max_size);
    if let Ok(mut cache) = THUMBNAILS.lock() {
        if let Some(texture) = cache.lookup(&key) {
            return Some(texture);
        }
    }

    let pixbuf = utils::load_cover_file_at(path, max_size)?;
    let texture = gdk::Texture::for_pixbuf(&pixbuf);
    if let Ok(mut cache) = THUMBNAILS.lock() {
        cache.store(key, texture.clone());
    }
    Some(texture)
}

/// Drops cached textures for a file whose contents changed on disk, so a
/// replaced custom cover is decoded again instead of showing the old image.
pub fn forget_thumbnail(path: &Path) {
    if let Ok(mut cache) = THUMBNAILS.lock() {
        cache.entries.retain(|(cached_path, _), _| cached_path != path);
        cache.order.retain(|(cached_path, _)| cached_path != path);
        cache.bytes = cache
            .entries
            .values()
            .map(|entry| entry.bytes)
            .fold(0usize, usize::saturating_add);
    }
}

pub fn clear_thumbnail_cache() {
    if let Ok(mut cache) = THUMBNAILS.lock() {
        cache.entries.clear();
        cache.order.clear();
        cache.bytes = 0;
    }
}
