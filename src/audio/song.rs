// SPDX-FileCopyrightText: 2022  Emmanuele Bassi
// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    cell::{Cell, RefCell},
    fmt::{self, Display, Formatter},
    path::{Path, PathBuf},
    time::Instant,
};

use glib::{ParamSpec, ParamSpecBoolean, ParamSpecObject, ParamSpecString, ParamSpecUInt, Value};
use gtk::{gdk, gio, glib, prelude::*, subclass::prelude::*};
use itertools::Itertools;
use lofty::{
    config::ParseOptions,
    file::FileType,
    prelude::{Accessor, TaggedFileExt},
    probe::Probe,
    tag::ItemKey,
};
use log::{debug, warn};
use once_cell::sync::Lazy;
use sha2::{Digest, Sha256};

use crate::{
    audio::cover_cache::{self, CoverCache},
    i18n::{i18n, i18n_f},
    utils,
};

#[derive(Debug, Clone)]
pub struct SongData {
    artist: Option<String>,
    title: Option<String>,
    album: Option<String>,
    track_number: u32,
    disc_number: u32,
    /// Textures are intentionally not stored here. A queue can contain
    /// thousands of songs, and keeping one texture per song is what made the
    /// player consume hundreds of megabytes. Artwork is loaded on demand from
    /// the compact cache below and kept in a bounded global cache instead.
    cover_uuid: Option<String>,
    cover_cache: Option<PathBuf>,
    cover_small_path: Option<PathBuf>,
    cover_large_path: Option<PathBuf>,
    /// Set when the user removed the artwork instead of replacing it.
    cover_hidden: bool,
    /// Artist used by the album cover cache key. Kept separately because the
    /// track artist and album artist can differ on compilations.
    cover_artist: Option<String>,
    cover_palette: Option<Vec<gdk::RGBA>>,
    uuid: Option<String>,
    duration: u64,
    file_size: u64,
    codec: Option<String>,
    bitrate: Option<u32>,
    bit_depth: Option<u8>,
    sample_rate: Option<u32>,
    file: gio::File,
}

impl SongData {
    pub fn artist(&self) -> Option<&str> {
        self.artist.as_deref()
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn album(&self) -> Option<&str> {
        self.album.as_deref()
    }

    pub fn track_number(&self) -> u32 {
        self.track_number
    }

    pub fn disc_number(&self) -> u32 {
        self.disc_number
    }

    pub fn uuid(&self) -> Option<&str> {
        self.uuid.as_deref()
    }

    pub fn cover_uuid(&self) -> Option<&str> {
        self.cover_uuid.as_deref()
    }

    pub fn duration(&self) -> u64 {
        self.duration
    }

    /// A short, localized description of the audio stream.
    pub fn track_info(&self) -> String {
        let mut parts = Vec::new();

        if let Some(codec) = &self.codec {
            parts.push(codec.clone());
        }
        if let Some(bitrate) = self.bitrate {
            parts.push(i18n_f("{} kbps", &[&bitrate.to_string()]));
        }
        if let Some(bit_depth) = self.bit_depth {
            parts.push(i18n_f("{} bit", &[&bit_depth.to_string()]));
        }
        if let Some(sample_rate) = self.sample_rate {
            let value = if sample_rate % 1000 == 0 {
                (sample_rate / 1000).to_string()
            } else {
                format!("{:.1}", sample_rate as f64 / 1000.0)
            };
            parts.push(i18n_f("{} kHz", &[&value]));
        }

        parts.join(" · ")
    }

    pub fn cover_texture(&mut self) -> Option<gdk::Texture> {
        self.cover_texture_at(utils::COVER_SMALL_SIZE)
    }

    /// The player cover needs the 500px cache; queue rows use the 200px one.
    pub fn cover_texture_large(&mut self) -> Option<gdk::Texture> {
        self.cover_texture_at(utils::COVER_SIZE)
    }

    /// Custom cover targets for the current track and, when the file has
    /// album metadata, for the whole album.
    pub fn cover_override_paths(&self) -> Option<(PathBuf, PathBuf)> {
        let path = self.file.path()?;
        Some((
            utils::track_cover_override_path(&path),
            utils::track_cover_hidden_marker_path(&path),
        ))
    }

    pub fn album_cover_override_paths(&self) -> Option<(PathBuf, PathBuf)> {
        let album = self.album.as_deref()?;
        let artist = self.cover_artist.as_deref().unwrap_or("");
        Some((
            utils::album_cover_override_path(album, artist),
            utils::album_cover_hidden_marker_path(album, artist),
        ))
    }

    pub fn cover_artist(&self) -> Option<&str> {
        self.cover_artist.as_deref()
    }

    /// Re-reads the custom cover state from disk. Called after the user
    /// changes or removes artwork, without parsing the audio file again.
    pub fn refresh_cover(&mut self) {
        let Some(path) = self.file.path() else {
            return;
        };

        let cover_paths = utils::resolve_song_cover_paths(
            self.album.as_deref(),
            self.cover_artist.as_deref(),
            &path,
        );

        self.cover_large_path = cover_paths.large;
        self.cover_small_path = cover_paths.small;
        self.cover_cache = cover_paths.cache;
        self.cover_hidden = cover_paths.hidden;
        self.cover_palette = None;
    }

    fn cover_texture_at(&mut self, max_size: i32) -> Option<gdk::Texture> {
        if self.cover_hidden {
            return None;
        }

        if max_size <= utils::COVER_SMALL_SIZE {
            if let Some(path) = self.cover_small_path.as_ref() {
                if let Some(texture) = cover_cache::cached_thumbnail(path, max_size) {
                    return Some(texture);
                }
            }
        }

        if let Some(path) = self.cover_large_path.as_ref() {
            if let Some(texture) = cover_cache::cached_thumbnail(path, max_size) {
                return Some(texture);
            }
        }

        if let Some(path) = self.cover_small_path.as_ref() {
            if let Some(texture) = cover_cache::cached_thumbnail(path, max_size) {
                return Some(texture);
            }
        }

        self.embedded_cover_texture()
    }

    /// Fallback for files that are not part of the indexed library, or for
    /// libraries created before the album thumbnail cache existed.
    fn embedded_cover_texture(&mut self) -> Option<gdk::Texture> {
        if let Some(uuid) = self.cover_uuid.as_deref() {
            if let Ok(mut cache) = CoverCache::global().lock() {
                if let Some(texture) = cache.lookup_texture(uuid) {
                    return Some(texture);
                }
            }
        }

        let path = self.file.path()?;
        let tagged_file = read_tagged_file(&path, true)?;
        let tag = tagged_file
            .primary_tag()
            .or_else(|| tagged_file.first_tag())?;
        let mut cache = CoverCache::global().lock().ok()?;
        let (cover, uuid) = cache.cover_art(&path, tag)?;
        self.cover_uuid = Some(uuid);
        if self.cover_cache.is_none() {
            self.cover_cache = cover.cache().cloned();
        }
        Some(cover.texture().clone())
    }

    pub fn cover_palette(&mut self) -> Option<Vec<gdk::RGBA>> {
        if self.cover_palette.is_none() {
            self.cover_palette = self.load_palette();
        }
        self.cover_palette.clone()
    }

    fn load_palette(&mut self) -> Option<Vec<gdk::RGBA>> {
        if self.cover_hidden {
            return None;
        }

        if let Some(path) = self.cover_small_path.as_ref().or(self.cover_large_path.as_ref()) {
            if let Some(pixbuf) = utils::load_cover_file_at(path, utils::COVER_SMALL_SIZE) {
                if let Some(palette) = utils::load_palette(&pixbuf) {
                    return Some(palette);
                }
            }
        }

        if let Some(uuid) = self.cover_uuid.as_deref() {
            if let Ok(mut cache) = CoverCache::global().lock() {
                if let Some(cover) = cache.lookup(uuid) {
                    return Some(cover.palette().clone());
                }
            }
        }

        let path = self.file.path()?;
        let tagged_file = read_tagged_file(&path, true)?;
        let tag = tagged_file
            .primary_tag()
            .or_else(|| tagged_file.first_tag())?;
        let mut cache = CoverCache::global().lock().ok()?;
        let (cover, uuid) = cache.cover_art(&path, tag)?;
        self.cover_uuid = Some(uuid);
        if self.cover_cache.is_none() {
            self.cover_cache = cover.cache().cloned();
        }
        Some(cover.palette().clone())
    }

    pub fn cover_cache(&mut self) -> Option<PathBuf> {
        if self.cover_hidden {
            return None;
        }

        if self.cover_cache.is_none() {
            // Only the current song is affected; this writes the PNG used by
            // MPRIS and does not keep the texture alive.
            let _ = self.embedded_cover_texture();
        }
        self.cover_cache.clone()
    }

    pub fn from_uri(uri: &str) -> Self {
        let now = Instant::now();

        let file = gio::File::for_uri(uri);
        let path = file.path().expect("Unable to find file");

        let (mime_type, _) = gio::content_type_guess(Some(&path), None);
        if !mime_type.starts_with("audio") || mime_type.ends_with("x-mpegurl") {
            return SongData::default();
        }

        // Parse tags without reading embedded pictures. Covers are loaded
        // lazily, only when a visible row or the player actually needs them.
        let tagged_file = match read_tagged_file(&path, false) {
            Some(tagged_file) => tagged_file,
            None => {
                warn!("Unable to parse file {:?}", path);
                return SongData::default();
            }
        };

        let mut artist = None;
        let mut album_artist = None;
        let mut title = None;
        let mut album = None;
        let mut track_number = 0;
        let mut disc_number = 0;
        let mut cover_uuid = None;
        {
            if let Some(tag) = tagged_file
                .primary_tag()
                .or_else(|| tagged_file.first_tag())
            {
                debug!("Found primary tag");
                artist = Some(tag.get_strings(ItemKey::TrackArtist).join(", "));
                album_artist = tag
                    .get_string(ItemKey::AlbumArtist)
                    .map(|s| s.to_string());
                title = tag.title().map(|s| s.to_string());
                album = tag.album().map(|s| s.to_string());
                track_number = tag.track().unwrap_or(0);
                disc_number = tag.disk().unwrap_or(0);
                cover_uuid = Some(cover_cache::cover_id(&path, tag));
            } else {
                warn!("Unable to load primary tag for: {}", uri);
            }
        }

        // The library scan writes both thumbnail sizes next to the database
        // cache. When they exist, the artwork never has to be decoded while
        // the queue is being built. User-selected artwork is checked first.
        let cover_artist = album_artist.or_else(|| artist.clone());
        let cover_paths = utils::resolve_song_cover_paths(
            album.as_deref(),
            cover_artist.as_deref(),
            &path,
        );
        let file_size = std::fs::metadata(&path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);

        let uuid = match file.query_info(
            "standard::display-name",
            gio::FileQueryInfoFlags::NONE,
            gio::Cancellable::NONE,
        ) {
            Ok(info) => {
                let mut hasher = Sha256::new();

                hasher.update(info.display_name().as_str());

                if let Some(ref artist) = artist {
                    hasher.update(artist);
                }
                if let Some(ref title) = title {
                    hasher.update(title);
                }
                if let Some(ref album) = album {
                    hasher.update(album);
                }

                Some(format!("{:x}", hasher.finalize()))
            }
            _ => None,
        };

        let properties = lofty::prelude::AudioFile::properties(&tagged_file);
        let duration = properties.duration().as_secs();
        let codec = codec_name(tagged_file.file_type(), &path);
        let bitrate = properties
            .audio_bitrate()
            .or_else(|| properties.overall_bitrate());
        let bit_depth = properties.bit_depth();
        let sample_rate = properties.sample_rate();

        debug!(
            "Song {:?} ('{:?}') loading time: {} ms",
            &uuid,
            &title,
            now.elapsed().as_millis()
        );

        SongData {
            artist,
            title,
            album,
            track_number,
            disc_number,
            cover_uuid,
            cover_cache: cover_paths.cache,
            cover_small_path: cover_paths.small,
            cover_large_path: cover_paths.large,
            cover_hidden: cover_paths.hidden,
            cover_artist,
            cover_palette: None,
            uuid,
            duration,
            file_size,
            codec,
            bitrate,
            bit_depth,
            sample_rate,
            file,
        }
    }

    pub fn uri(&self) -> String {
        self.file.uri().to_string()
    }

    pub fn file(&self) -> gio::File {
        self.file.clone()
    }

    pub fn file_size(&self) -> u64 {
        self.file_size
    }
}

fn read_tagged_file(path: &Path, read_cover_art: bool) -> Option<lofty::file::TaggedFile> {
    let options = ParseOptions::new().read_cover_art(read_cover_art);
    Probe::open(path)
        .ok()?
        .guess_file_type()
        .ok()?
        .options(options)
        .read()
        .ok()
}

impl Default for SongData {
    fn default() -> Self {
        SongData {
            artist: Some("Invalid Artist".to_string()),
            title: Some("Invalid Title".to_string()),
            album: Some("Invalid Album".to_string()),
            track_number: 0,
            disc_number: 0,
            cover_uuid: None,
            cover_cache: None,
            cover_small_path: None,
            cover_large_path: None,
            cover_hidden: false,
            cover_artist: None,
            cover_palette: None,
            uuid: None,
            duration: 0,
            file_size: 0,
            codec: None,
            bitrate: None,
            bit_depth: None,
            sample_rate: None,
            file: gio::File::for_path("/does-not-exist"),
        }
    }
}

fn codec_name(file_type: FileType, path: &std::path::Path) -> Option<String> {
    let name = match file_type {
        FileType::Aac => "AAC".to_string(),
        FileType::Aiff => "AIFF".to_string(),
        FileType::Ape => "APE".to_string(),
        FileType::Flac => "FLAC".to_string(),
        FileType::Mpeg => "MP3".to_string(),
        FileType::Mp4 => path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| extension.to_uppercase())
            .unwrap_or_else(|| "M4A".to_string()),
        FileType::Mpc => "Musepack".to_string(),
        FileType::Opus => "Opus".to_string(),
        FileType::Vorbis => "Ogg Vorbis".to_string(),
        FileType::Speex => "Speex".to_string(),
        FileType::Wav => "WAV".to_string(),
        FileType::WavPack => "WavPack".to_string(),
        FileType::Custom(name) => name.to_string(),
        _ => return None,
    };

    Some(name)
}

mod imp {
    use super::*;

    #[derive(Debug, Default)]
    pub struct Song {
        pub data: RefCell<SongData>,
        pub playing: Cell<bool>,
        pub selected: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Song {
        const NAME: &'static str = "AmberolSong";
        type Type = super::Song;
    }

    impl ObjectImpl for Song {
        fn properties() -> &'static [ParamSpec] {
            static PROPERTIES: Lazy<Vec<ParamSpec>> = Lazy::new(|| {
                vec![
                    ParamSpecString::builder("uri").construct_only().build(),
                    ParamSpecString::builder("artist").read_only().build(),
                    ParamSpecString::builder("title").read_only().build(),
                    ParamSpecString::builder("album").read_only().build(),
                    ParamSpecUInt::builder("duration").read_only().build(),
                    ParamSpecObject::builder::<gdk::Texture>("cover")
                        .read_only()
                        .build(),
                    ParamSpecBoolean::builder("playing").build(),
                    ParamSpecBoolean::builder("selected").build(),
                ]
            });
            PROPERTIES.as_ref()
        }

        fn set_property(&self, _id: usize, value: &Value, pspec: &ParamSpec) {
            match pspec.name() {
                "uri" => {
                    let obj = self.obj();
                    if let Ok(p) = value.get::<&str>() {
                        self.data.replace(SongData::from_uri(p));
                        obj.notify("artist");
                        obj.notify("title");
                        obj.notify("album");
                        obj.notify("duration");
                        obj.notify("cover");
                    }
                }
                "playing" => {
                    let p = value.get::<bool>().expect("Value must be a boolean");
                    self.playing.set(p);
                }
                "selected" => {
                    let p = value.get::<bool>().expect("Value must be a boolean");
                    self.selected.set(p);
                }
                _ => unimplemented!(),
            }
        }

        fn property(&self, _id: usize, pspec: &ParamSpec) -> Value {
            let obj = self.obj();
            match pspec.name() {
                "artist" => obj.artist().to_value(),
                "title" => obj.title().to_value(),
                "album" => obj.album().to_value(),
                "duration" => obj.duration().to_value(),
                "uri" => obj.uri().to_value(),
                "cover" => obj.cover_texture().to_value(),
                "playing" => self.playing.get().to_value(),
                "selected" => self.selected.get().to_value(),
                _ => unimplemented!(),
            }
        }
    }
}

glib::wrapper! {
    pub struct Song(ObjectSubclass<imp::Song>);
}

impl Song {
    pub fn new(uri: &str) -> Self {
        glib::Object::builder::<Self>().property("uri", uri).build()
    }

    pub fn from_uri(uri: &str) -> Result<Song, &'static str> {
        let res = Song::new(uri);
        if res.equals(&Song::default()) {
            Err("Invalid song")
        } else {
            Ok(res)
        }
    }

    pub fn empty() -> Self {
        glib::Object::new()
    }

    pub fn equals(&self, other: &Self) -> bool {
        if self.uuid().is_some() && other.uuid().is_some() {
            self.uuid() == other.uuid()
        } else {
            self.uri() == other.uri()
        }
    }

    pub fn uri(&self) -> String {
        self.imp().data.borrow().uri()
    }

    pub fn artist(&self) -> String {
        match self.imp().data.borrow().artist() {
            Some(artist) => artist.to_string(),
            None => i18n("Unknown artist"),
        }
    }

    pub fn title(&self) -> String {
        match self.imp().data.borrow().title() {
            Some(title) => title.to_string(),
            None => i18n("Unknown title"),
        }
    }

    pub fn album(&self) -> String {
        match self.imp().data.borrow().album() {
            Some(album) => album.to_string(),
            None => i18n("Unknown album"),
        }
    }

    pub fn track_number(&self) -> u32 {
        self.imp().data.borrow().track_number()
    }

    pub fn disc_number(&self) -> u32 {
        self.imp().data.borrow().disc_number()
    }

    /// The 200px texture used by queue rows.
    pub fn cover_texture(&self) -> Option<gdk::Texture> {
        self.imp().data.borrow_mut().cover_texture()
    }

    /// The 500px texture used by the player cover.
    pub fn cover_texture_large(&self) -> Option<gdk::Texture> {
        self.imp().data.borrow_mut().cover_texture_large()
    }

    /// Custom cover paths for this track only.
    pub fn cover_override_paths(&self) -> Option<(PathBuf, PathBuf)> {
        self.imp().data.borrow().cover_override_paths()
    }

    /// Custom cover paths for the whole album.
    pub fn album_cover_override_paths(&self) -> Option<(PathBuf, PathBuf)> {
        self.imp().data.borrow().album_cover_override_paths()
    }

    /// Raw album title and album artist used as the album cover key.
    pub fn cover_album_info(&self) -> Option<(String, String)> {
        let data = self.imp().data.borrow();
        let album = data.album()?.to_owned();
        Some((album, data.cover_artist().unwrap_or("").to_owned()))
    }

    /// Picks up a custom cover written by the window without re-reading tags.
    pub fn refresh_cover(&self) {
        self.imp().data.borrow_mut().refresh_cover();
        self.notify("cover");
    }

    pub fn cover_color(&self) -> Option<gdk::RGBA> {
        self.imp()
            .data
            .borrow_mut()
            .cover_palette()
            .and_then(|p| p.first().copied())
    }

    pub fn cover_palette(&self) -> Option<Vec<gdk::RGBA>> {
        self.imp().data.borrow_mut().cover_palette()
    }

    pub fn cover_uuid(&self) -> Option<String> {
        self.imp().data.borrow().cover_uuid().map(|s| s.to_string())
    }

    pub fn cover_cache(&self) -> Option<PathBuf> {
        self.imp().data.borrow_mut().cover_cache()
    }

    /// Size of the audio file on disk. The memory guard uses it to choose the
    /// playback budget.
    pub fn file_size(&self) -> u64 {
        self.imp().data.borrow().file_size()
    }

    pub fn duration(&self) -> u64 {
        self.imp().data.borrow().duration()
    }

    pub fn track_info(&self) -> String {
        self.imp().data.borrow().track_info()
    }

    pub fn playing(&self) -> bool {
        self.imp().playing.get()
    }

    pub fn set_playing(&self, playing: bool) {
        let was_playing = self.imp().playing.replace(playing);
        if was_playing != playing {
            self.notify("playing");
        }
    }

    pub fn selected(&self) -> bool {
        self.imp().selected.get()
    }

    pub fn set_selected(&self, selected: bool) {
        let was_selected = self.imp().selected.replace(selected);
        if was_selected != selected {
            self.notify("selected");
        }
    }

    pub fn uuid(&self) -> Option<String> {
        self.imp().data.borrow().uuid().map(|s| s.to_string())
    }

    pub fn search_key(&self) -> String {
        format!("{} {} {}", self.artist(), self.album(), self.title())
    }

    pub fn file(&self) -> gio::File {
        self.imp().data.borrow().file()
    }
}

impl Default for Song {
    fn default() -> Self {
        Self::empty()
    }
}

impl Display for Song {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(
            f,
            "Song {{ uuid: {}, uri: {}, artist: '{}', title: '{}' }}",
            self.uuid().unwrap(),
            self.uri(),
            self.artist(),
            self.title()
        )
    }
}
