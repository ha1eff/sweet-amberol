// SPDX-FileCopyrightText: 2022  Emmanuele Bassi
// SPDX-License-Identifier: GPL-3.0-or-later

use core::cmp::Ordering;
use std::{
    cell::Cell,
    path::{Path, PathBuf},
    rc::Rc,
};

use sha2::{Digest, Sha256};

use color_thief::{get_palette, ColorFormat};
use gtk::{gdk, gio, glib, prelude::*};
use log::{debug, warn};

use crate::{
    audio::{AudioPlayer, RepeatMode},
    config::APPLICATION_ID,
};

pub fn settings_manager() -> gio::Settings {
    // We ship a single schema for both default and development profiles
    let app_id = APPLICATION_ID.trim_end_matches(".Devel");
    gio::Settings::new(app_id)
}

pub fn format_remaining_time(t: i64) -> String {
    // We use an explicit LRM character so the MINUS SIGN character
    // stays in front of the remaining time even in RTL locales, instead
    // of getting flipped at the end of the text
    format!("\u{200e}\u{2212}{}:{:02}", (t - (t % 60)) / 60, t % 60)
}

pub fn format_time(t: i64) -> String {
    format!("{}:{:02}", (t - (t % 60)) / 60, t % 60)
}

/// Prefix used for the artist links embedded into artist labels.
pub const ARTIST_LINK_PREFIX: &str = "artist:";

/// Extracts the artist from a link activated in an artist label.
pub fn artist_from_link(uri: &str) -> Option<&str> {
    uri.strip_prefix(ARTIST_LINK_PREFIX)
        .filter(|artist| !artist.is_empty())
}

/// Builds Pango markup in which every known artist mentioned in `credit`
/// becomes its own link, so clicking a name opens exactly that artist's page.
/// Text that is not an artist is escaped, and plain text is returned when no
/// artists from the library appear in the credit.
pub fn artist_links_markup(credit: &str, artists: &[String]) -> String {
    let lower = credit.to_lowercase();
    let mut spans: Vec<(usize, usize, &String)> = Vec::new();

    for artist in artists {
        let needle = artist.to_lowercase();
        let mut start = 0;
        while let Some(position) = lower[start..].find(&needle) {
            let begin = start + position;
            let end = begin + needle.len();
            let boundary_before = begin == 0
                || !lower[..begin]
                    .chars()
                    .last()
                    .is_some_and(|c| c.is_alphanumeric());
            let boundary_after = end == lower.len()
                || !lower[end..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_alphanumeric());
            if boundary_before && boundary_after {
                spans.push((begin, end, artist));
                break;
            }
            start = begin + 1;
        }
    }

    // Names already come ordered, but sort defensively and drop overlaps so a
    // short name that is part of a longer one never becomes a second link.
    spans.sort_by_key(|(begin, _, _)| *begin);
    let mut markup = String::new();
    let mut cursor = 0;
    let mut used_until = 0;
    for (begin, end, artist) in spans {
        if begin < used_until {
            continue;
        }
        markup.push_str(&glib::markup_escape_text(&credit[cursor..begin]));
        let label = glib::markup_escape_text(artist);
        let href = glib::markup_escape_text(&format!("{ARTIST_LINK_PREFIX}{artist}"));
        markup.push_str(&format!("<a href=\"{href}\">{label}</a>"));
        cursor = end;
        used_until = end;
    }
    markup.push_str(&glib::markup_escape_text(&credit[cursor..]));
    markup
}

// The player cover can be expanded to 250px, and it still needs to stay crisp
// on HiDPI displays, so decode the artwork at twice that size. Artwork that is
// smaller than that is kept at its original resolution: upscaling it would only
// make the cover blurrier.
pub const COVER_SIZE: i32 = 250 * 2;

/// Album tiles are exactly 200 application pixels, so the library scan also
/// stores a dedicated thumbnail for them. Keeping it small makes the grid much
/// cheaper to fill than loading the same 500px image as the player.
pub const COVER_SMALL_SIZE: i32 = 200;

/// A mild unsharp mask. Downscaling artwork to 200px softens fine details, so
/// the small thumbnail is sharpened slightly to keep it from looking blurry.
const SMALL_COVER_SHARPEN: f32 = 0.16;

/// Cover cache entries above this size are still the raw embedded artwork
/// written by older versions, so they are worth replacing with a thumbnail.
pub const COVER_CACHE_MAX_BYTES: u64 = 320 * 1024;

/// Images at or above this size are offered a smaller JPEG copy before they
/// are used as a custom cover. Full-size photos are several megabytes and are
/// far larger than anything the player can display.
pub const COVER_IMPORT_LARGE_DIMENSION: i32 = 2000;

/// User-selected images are decoded at this size at most. An 800px cover only
/// needs a little more source resolution for a clean downscale.
pub const COVER_IMPORT_DECODE_SIZE: i32 = 1600;

/// The size of the compressed custom cover offered for large images.
pub const COVER_IMPORT_OUTPUT_SIZE: i32 = 800;

/// Roughly the file size the compressed cover is tuned for.
pub const COVER_IMPORT_TARGET_BYTES: usize = 100 * 1024;

/// Playback memory limits. The player streams audio from disk, so the file
/// itself is never held in memory. The limit covers the song objects, cover
/// artwork, palettes and other UI state that grows with the queue.
///
/// Tracks up to 100 MiB may use at most 150 MiB; larger tracks get their own
/// size plus 50 MiB for covers and other data.
pub const PLAYBACK_LARGE_FILE_BYTES: u64 = 100 * 1024 * 1024;
pub const PLAYBACK_MEMORY_LIMIT_BYTES: u64 = 150 * 1024 * 1024;
pub const PLAYBACK_EXTRA_MEMORY_BYTES: u64 = 50 * 1024 * 1024;

/// The maximum amount of memory that a track of `file_size` bytes is allowed
/// to use.
pub fn playback_memory_limit(file_size: u64) -> u64 {
    if file_size <= PLAYBACK_LARGE_FILE_BYTES {
        PLAYBACK_MEMORY_LIMIT_BYTES
    } else {
        file_size.saturating_add(PLAYBACK_EXTRA_MEMORY_BYTES)
    }
}

/// Asks the allocator to return freed heap pages to the operating system.
/// Dropping large textures otherwise leaves them in the process RSS until the
/// allocator decides to reuse or release them.
pub fn release_unused_memory() {
    #[cfg(target_os = "linux")]
    unsafe {
        libc::malloc_trim(0);
    }
}

/// Decodes an image, scaling it down during decoding so that the longest side
/// never exceeds `max_size`. Unlike `Pixbuf::from_stream_at_scale`, this never
/// upscales small artwork, and it avoids allocating the full-size image.
pub fn load_cover_pixbuf(buffer: &glib::Bytes, max_size: i32) -> Option<gdk_pixbuf::Pixbuf> {
    let max_size = max_size.max(1);
    let loader = gdk_pixbuf::PixbufLoader::new();
    loader.connect_size_prepared(move |loader, width, height| {
        let longest_side = width.max(height);
        if longest_side > max_size {
            let factor = max_size as f64 / longest_side as f64;
            loader.set_size(
                ((width as f64) * factor).round().max(1.0) as i32,
                ((height as f64) * factor).round().max(1.0) as i32,
            );
        }
    });

    if let Err(err) = loader.write_bytes(buffer) {
        warn!("Unable to decode cover art: {err}");
        return None;
    }
    if let Err(err) = loader.close() {
        warn!("Unable to finish decoding cover art: {err}");
        return None;
    }

    loader.pixbuf()
}

pub fn load_cover_texture(buffer: &glib::Bytes) -> Option<gdk_pixbuf::Pixbuf> {
    load_cover_pixbuf(buffer, COVER_SIZE)
}

/// Loads artwork from disk, scaling it down to `max_size` while decoding.
pub fn load_cover_file_at(path: &Path, max_size: i32) -> Option<gdk_pixbuf::Pixbuf> {
    let data = std::fs::read(path).ok()?;
    load_cover_pixbuf(&glib::Bytes::from_owned(data), max_size)
}

/// Re-encodes a pixel buffer as a compact thumbnail. JPEG is used for opaque
/// artwork and PNG when transparency has to be preserved.
pub fn cover_thumbnail_from_pixbuf(pixbuf: &gdk_pixbuf::Pixbuf) -> Option<Vec<u8>> {
    let (format, options): (&str, &[(&str, &str)]) = if pixbuf.has_alpha() {
        ("png", &[])
    } else {
        ("jpeg", &[("quality", "88")])
    };

    match pixbuf.save_to_bufferv(format, options) {
        Ok(bytes) => Some(bytes),
        Err(err) => {
            warn!("Unable to encode cover thumbnail: {err}");
            None
        }
    }
}

/// Builds the small album-grid thumbnail, sharpened just enough to counteract
/// the softness introduced by scaling the artwork down to 200px.
pub fn small_cover_thumbnail_from_pixbuf(pixbuf: &gdk_pixbuf::Pixbuf) -> Option<Vec<u8>> {
    let longest_side = pixbuf.width().max(pixbuf.height());
    let scaled = if longest_side > COVER_SMALL_SIZE {
        let factor = COVER_SMALL_SIZE as f64 / longest_side as f64;
        pixbuf.scale_simple(
            ((pixbuf.width() as f64) * factor).round().max(1.0) as i32,
            ((pixbuf.height() as f64) * factor).round().max(1.0) as i32,
            gdk_pixbuf::InterpType::Hyper,
        )?
    } else {
        pixbuf.copy()?
    };

    let sharpened = sharpen_pixbuf(&scaled, SMALL_COVER_SHARPEN).unwrap_or(scaled);
    cover_thumbnail_from_pixbuf(&sharpened)
}

/// Produces both cached cover sizes from the same decoded image, so a library
/// scan never has to decode the embedded artwork twice.
pub fn cover_thumbnails_from_bytes(buffer: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let pixbuf = load_cover_pixbuf(&glib::Bytes::from_owned(buffer.to_vec()), COVER_SIZE)?;
    let large = cover_thumbnail_from_pixbuf(&pixbuf)?;
    let small = small_cover_thumbnail_from_pixbuf(&pixbuf)?;
    Some((large, small))
}

/// A user-selected image with both its original size and a safely bounded
/// decoded buffer. The original dimensions are reported before the loader
/// scales the image down, so a large photo is never fully decoded.
pub struct SelectedCoverImage {
    pub pixbuf: gdk_pixbuf::Pixbuf,
    pub width: i32,
    pub height: i32,
}

pub fn load_selected_cover_image(path: &Path) -> Option<SelectedCoverImage> {
    let data = std::fs::read(path).ok()?;
    let loader = gdk_pixbuf::PixbufLoader::new();
    let dimensions = Rc::new(Cell::new((0, 0)));

    let prepared_dimensions = dimensions.clone();
    loader.connect_size_prepared(move |loader, width, height| {
        prepared_dimensions.set((width, height));

        let longest_side = width.max(height);
        if longest_side > COVER_IMPORT_DECODE_SIZE {
            let factor = COVER_IMPORT_DECODE_SIZE as f64 / longest_side as f64;
            loader.set_size(
                ((width as f64) * factor).round().max(1.0) as i32,
                ((height as f64) * factor).round().max(1.0) as i32,
            );
        }
    });

    loader
        .write_bytes(&glib::Bytes::from_owned(data))
        .map_err(|err| warn!("Unable to decode selected cover: {err}"))
        .ok()?;
    loader
        .close()
        .map_err(|err| warn!("Unable to finish decoding selected cover: {err}"))
        .ok()?;

    let pixbuf = loader.pixbuf()?;
    let (width, height) = dimensions.get();
    Some(SelectedCoverImage {
        pixbuf,
        width,
        height,
    })
}

/// Centre-crops an image to a square, scales it to `size` and encodes it as a
/// JPEG close to `target_bytes`. The quality is chosen as high as possible
/// without exceeding the byte budget, so 800×800 covers stay around 100 KiB
/// instead of several megabytes.
pub fn square_jpeg_cover(
    pixbuf: &gdk_pixbuf::Pixbuf,
    size: i32,
    target_bytes: usize,
) -> Option<Vec<u8>> {
    let (width, height) = (pixbuf.width(), pixbuf.height());
    let side = width.min(height);
    if side <= 0 || size <= 0 {
        return None;
    }

    let x = (width - side) / 2;
    let y = (height - side) / 2;
    let square = pixbuf.new_subpixbuf(x, y, side, side);
    let scaled = square.scale_simple(size, size, gdk_pixbuf::InterpType::Bilinear)?;

    let flattened = if scaled.has_alpha() {
        // JPEG has no alpha channel. Composite transparent images onto a
        // neutral background instead of failing to save them.
        let background =
            gdk_pixbuf::Pixbuf::new(gdk_pixbuf::Colorspace::Rgb, false, 8, size, size)?;
        background.fill(0xffffffff);
        scaled.composite(
            &background,
            0,
            0,
            size,
            size,
            0.0,
            0.0,
            1.0,
            1.0,
            gdk_pixbuf::InterpType::Bilinear,
            255,
        );
        background
    } else {
        scaled
    };

    let encode = |quality: i32| {
        let quality = quality.to_string();
        flattened
            .save_to_bufferv("jpeg", &[("quality", quality.as_str())])
            .ok()
    };

    let mut low = 10;
    let mut high = 95;
    let mut best = None;
    while low <= high {
        let quality = (low + high) / 2;
        let Some(bytes) = encode(quality) else {
            return None;
        };
        if bytes.len() <= target_bytes {
            best = Some(bytes);
            low = quality + 1;
        } else {
            high = quality - 1;
        }
    }

    best.or_else(|| encode(10))
}

/// Repairs an existing cache entry: optionally re-encodes the large image in
/// the compact format and makes sure the 200px thumbnail is present.
pub fn ensure_cover_thumbnails(path: &Path, reencode_large: bool) -> bool {
    let Ok(data) = std::fs::read(path) else {
        return false;
    };
    let Some(pixbuf) = load_cover_pixbuf(&glib::Bytes::from_owned(data), COVER_SIZE) else {
        return false;
    };

    if reencode_large {
        if let Some(large) = cover_thumbnail_from_pixbuf(&pixbuf) {
            write_thumbnail_file(path, &large);
        }
    }

    let Some(small) = small_cover_thumbnail_from_pixbuf(&pixbuf) else {
        return false;
    };
    write_thumbnail_file(&small_cover_cache_path(path), &small)
}

/// A 3x3 unsharp mask that leaves the alpha channel untouched.
fn sharpen_pixbuf(
    pixbuf: &gdk_pixbuf::Pixbuf,
    amount: f32,
) -> Option<gdk_pixbuf::Pixbuf> {
    let width = pixbuf.width() as usize;
    let height = pixbuf.height() as usize;
    let channels = pixbuf.n_channels() as usize;
    if width < 3 || height < 3 || channels < 3 || pixbuf.bits_per_sample() != 8 {
        return None;
    }

    let src = pixbuf.read_pixel_bytes();
    let src = src.as_ref();
    let src_stride = pixbuf.rowstride() as usize;

    let out = gdk_pixbuf::Pixbuf::new(
        pixbuf.colorspace(),
        pixbuf.has_alpha(),
        8,
        pixbuf.width(),
        pixbuf.height(),
    )?;
    let dst_stride = out.rowstride() as usize;

    unsafe {
        let dst = out.pixels();
        for y in 0..height {
            let src_row = &src[y * src_stride..y * src_stride + width * channels];
            let dst_row = &mut dst[y * dst_stride..y * dst_stride + width * channels];
            dst_row.copy_from_slice(src_row);
        }

        for y in 1..height - 1 {
            for x in 1..width - 1 {
                let index = y * dst_stride + x * channels;
                let up = (y - 1) * src_stride + x * channels;
                let down = (y + 1) * src_stride + x * channels;
                let left = y * src_stride + (x - 1) * channels;
                let right = y * src_stride + (x + 1) * channels;

                for channel in 0..3 {
                    let center = src[index + channel] as f32;
                    let neighbours = src[up + channel] as f32
                        + src[down + channel] as f32
                        + src[left + channel] as f32
                        + src[right + channel] as f32;
                    let value = center + amount * (4.0 * center - neighbours);
                    dst[index + channel] = value.clamp(0.0, 255.0) as u8;
                }
            }
        }
    }

    Some(out)
}

/// Writes a cover cache file atomically so that readers never see a partial
/// image while a legacy entry is being replaced.
pub fn write_thumbnail_file(path: &Path, thumbnail: &[u8]) -> bool {
    let tmp_path = path.with_extension("img.tmp");
    if let Err(err) = std::fs::write(&tmp_path, thumbnail) {
        warn!("Unable to write cover cache {:?}: {err}", tmp_path);
        return false;
    }
    if let Err(err) = std::fs::rename(&tmp_path, path) {
        warn!("Unable to replace cover cache {:?}: {err}", path);
        let _ = std::fs::remove_file(&tmp_path);
        return false;
    }
    true
}

#[cfg(test)]
mod cover_tests {
    use super::*;

    #[test]
    fn cover_decode_preserves_aspect_ratio_and_limits_size() {
        for (width, height, expected) in [(1200, 600, (500, 250)), (300, 900, (167, 500))] {
            let image =
                gdk_pixbuf::Pixbuf::new(gdk_pixbuf::Colorspace::Rgb, false, 8, width, height)
                    .unwrap();
            image.fill(0xff0000ff);
            let bytes = glib::Bytes::from_owned(image.save_to_bufferv("png", &[]).unwrap());
            let cover = load_cover_texture(&bytes).unwrap();
            assert_eq!((cover.width(), cover.height()), expected);
        }
        assert!(load_cover_texture(&glib::Bytes::from_static(b"invalid image")).is_none());
    }
}

/// Location of the two cover caches used by the album grid and the player.
///
/// The key is stable for an album, so the covers only have to be extracted
/// from the audio file during a library scan, not every time the grid is
/// shown.
pub fn album_cover_cache_path(album: &str, artist: &str, track_path: &Path) -> PathBuf {
    let mut cache_dir = glib::user_cache_dir();
    cache_dir.push("amberol");
    cache_dir.push("album-covers");
    cache_dir.push(format!(
        "{}.img",
        album_cover_key(album, artist, track_path)
    ));
    cache_dir
}

fn album_cover_key(album: &str, artist: &str, track_path: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(album.as_bytes());
    hasher.update([0]);
    hasher.update(artist.as_bytes());
    hasher.update([0]);
    if let Some(parent) = track_path.parent() {
        hasher.update(parent.to_string_lossy().as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

fn track_cover_key(track_path: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(track_path.to_string_lossy().as_bytes());
    format!("{:x}", hasher.finalize())
}

fn cover_override_dir(kind: &str) -> PathBuf {
    let mut dir = glib::user_data_dir();
    dir.push("amberol");
    dir.push("cover-overrides");
    dir.push(kind);
    dir
}

/// Custom artwork selected for a single track.
pub fn track_cover_override_path(track_path: &Path) -> PathBuf {
    cover_override_dir("tracks").join(format!("{}.img", track_cover_key(track_path)))
}

/// Marker that hides the displayed artwork for a single track.
pub fn track_cover_hidden_marker_path(track_path: &Path) -> PathBuf {
    cover_override_dir("tracks").join(format!("{}.hidden", track_cover_key(track_path)))
}

/// Album overrides are keyed by the album identity rather than by a folder,
/// so an album split across disc directories still shares one cover.
fn album_override_key(album: &str, artist: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(album.as_bytes());
    hasher.update([0]);
    hasher.update(artist.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Custom artwork selected for an entire album.
pub fn album_cover_override_path(album: &str, artist: &str) -> PathBuf {
    cover_override_dir("albums").join(format!("{}.img", album_override_key(album, artist)))
}

/// Marker that hides the displayed artwork for an entire album.
pub fn album_cover_hidden_marker_path(album: &str, artist: &str) -> PathBuf {
    cover_override_dir("albums").join(format!("{}.hidden", album_override_key(album, artist)))
}

/// The artwork paths a song should use, taking custom covers into account.
#[derive(Clone, Debug, Default)]
pub struct ResolvedCoverPaths {
    pub large: Option<PathBuf>,
    pub small: Option<PathBuf>,
    pub cache: Option<PathBuf>,
    pub hidden: bool,
}

/// Resolves the custom cover for a track. A track-specific override wins over
/// an album override, which in turn wins over the artwork extracted from the
/// file.
pub fn resolve_song_cover_paths(
    album: Option<&str>,
    artist: Option<&str>,
    track_path: &Path,
) -> ResolvedCoverPaths {
    let track_override = track_cover_override_path(track_path);
    if track_override.is_file() {
        return ResolvedCoverPaths {
            large: Some(track_override.clone()),
            small: None,
            cache: Some(track_override),
            hidden: false,
        };
    }

    if track_cover_hidden_marker_path(track_path).is_file() {
        return ResolvedCoverPaths {
            hidden: true,
            ..Default::default()
        };
    }

    if let Some(album) = album {
        let artist = artist.unwrap_or("");
        let album_override = album_cover_override_path(album, artist);
        if album_override.is_file() {
            return ResolvedCoverPaths {
                large: Some(album_override.clone()),
                small: None,
                cache: Some(album_override),
                hidden: false,
            };
        }

        if album_cover_hidden_marker_path(album, artist).is_file() {
            return ResolvedCoverPaths {
                hidden: true,
                ..Default::default()
            };
        }
    }

    let Some(album) = album else {
        return ResolvedCoverPaths::default();
    };

    let large = album_cover_cache_path(album, artist.unwrap_or(""), track_path);
    let small = small_cover_cache_path(&large);
    let large = large.is_file().then_some(large);
    let small = small.is_file().then_some(small);
    let cache = large.clone().or_else(|| small.clone());

    ResolvedCoverPaths {
        large,
        small,
        cache,
        hidden: false,
    }
}

/// The 200px companion of an album cover cache entry.
pub fn small_cover_cache_path(path: &Path) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!("{stem}-200.img"))
}

pub fn album_cover_small_cache_path(
    album: &str,
    artist: &str,
    track_path: &Path,
) -> PathBuf {
    small_cover_cache_path(&album_cover_cache_path(album, artist, track_path))
}

pub fn cache_cover_art(uuid: &str, pixbuf: &gdk_pixbuf::Pixbuf) -> Option<PathBuf> {
    let mut cache_path = glib::user_cache_dir();
    cache_path.push("amberol");
    cache_path.push("covers");
    glib::mkdir_with_parents(&cache_path, 0o755);

    cache_path.push(format!("{}.png", &uuid));
    if cache_path.is_file() {
        return Some(cache_path);
    }

    // This is called from cover loading workers as well as the UI thread.
    // `save_to_streamv_async` requires the main context, so encode in memory
    // and write the file synchronously instead of panicking off-thread.
    let bytes = pixbuf
        .save_to_bufferv("png", &[("tEXt::Software", "amberol")])
        .map_err(|err| {
            warn!("Unable to encode cover data: {err}");
        })
        .ok()?;

    debug!("Creating cover data cache at {:?}", &cache_path);
    if !write_thumbnail_file(&cache_path, &bytes) {
        return None;
    }

    Some(cache_path)
}

fn color_format(has_alpha: bool) -> ColorFormat {
    if has_alpha {
        ColorFormat::Rgba
    } else {
        ColorFormat::Rgb
    }
}

pub fn load_palette(pixbuf: &gdk_pixbuf::Pixbuf) -> Option<Vec<gdk::RGBA>> {
    // Palette extraction only needs the dominant colors. Sampling a small
    // copy keeps this fast even for full-resolution artwork.
    const PALETTE_SAMPLE_SIZE: i32 = 64;
    let longest_side = pixbuf.width().max(pixbuf.height());
    let scaled = if longest_side > PALETTE_SAMPLE_SIZE {
        let factor = PALETTE_SAMPLE_SIZE as f64 / longest_side as f64;
        pixbuf.scale_simple(
            ((pixbuf.width() as f64) * factor).round().max(1.0) as i32,
            ((pixbuf.height() as f64) * factor).round().max(1.0) as i32,
            gdk_pixbuf::InterpType::Bilinear,
        )
    } else {
        None
    };
    let pixbuf = scaled.as_ref().unwrap_or(pixbuf);

    if let Ok(palette) = get_palette(
        pixbuf.pixel_bytes().unwrap().as_ref(),
        color_format(pixbuf.has_alpha()),
        5,
        4,
    ) {
        let colors: Vec<gdk::RGBA> = palette
            .iter()
            .map(|c| {
                gdk::RGBA::new(
                    c.r as f32 / 255.0,
                    c.g as f32 / 255.0,
                    c.b as f32 / 255.0,
                    1.0,
                )
            })
            .collect();

        return Some(colors);
    }

    None
}

fn load_files_from_folder_internal(
    base: &gio::File,
    folder: &gio::File,
    recursive: bool,
) -> Vec<gio::File> {
    let mut enumerator = if let Ok(enumerator) = folder.enumerate_children(
        "standard::name,standard::type",
        gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
        None::<&gio::Cancellable>,
    ) {
        enumerator
    } else {
        warn!("Unable to enumerate {}", folder.uri());
        return Vec::new();
    };

    let mut files = Vec::new();
    while let Some(info) = enumerator.next().and_then(|s| s.ok()) {
        let child = enumerator.child(&info);
        if recursive && info.file_type() == gio::FileType::Directory {
            let mut res = load_files_from_folder_internal(base, &child, recursive);
            files.append(&mut res);
        } else if info.file_type() == gio::FileType::Regular {
            files.push(child.clone());
        }
    }

    // gio::FileEnumerator has no guaranteed order, so we should
    // rely on the basename being formatted in a way that gives us an
    // implicit order; if anything, this will queue songs in the same
    // order in which they appear in the directory when browsing its
    // contents
    files.sort_by(|a, b| cmp_two_files(Some(base), a, b));

    files
}

pub fn cmp_two_files(base: Option<&gio::File>, a: &gio::File, b: &gio::File) -> Ordering {
    let parent_a = a.parent().unwrap();
    let parent_b = b.parent().unwrap();
    let parent_basename_a = if let Some(base) = base {
        if let Some(path) = base.relative_path(&parent_a) {
            path
        } else {
            parent_a.basename().unwrap()
        }
    } else {
        parent_a.basename().unwrap()
    };
    let parent_basename_b = if let Some(base) = base {
        if let Some(path) = base.relative_path(&parent_b) {
            path
        } else {
            parent_b.basename().unwrap()
        }
    } else {
        parent_b.basename().unwrap()
    };
    let basename_a = a.basename().unwrap();
    let basename_b = b.basename().unwrap();

    let mut order = cmp_like_nautilus(
        &parent_basename_a.to_string_lossy(),
        &parent_basename_b.to_string_lossy(),
    );

    if order.is_eq() {
        order = cmp_like_nautilus(&basename_a.to_string_lossy(), &basename_b.to_string_lossy());
    }

    order
}

fn cmp_like_nautilus(filename_a: &str, filename_b: &str) -> Ordering {
    let order;

    let sort_last_a = filename_a.as_bytes()[0] == b'.' || filename_a.as_bytes()[0] == b'#';
    let sort_last_b = filename_b.as_bytes()[0] == b'.' || filename_b.as_bytes()[0] == b'#';

    if !sort_last_a && sort_last_b {
        order = Ordering::Less;
    } else if sort_last_a && !sort_last_b {
        order = Ordering::Greater;
    } else {
        let key_a = glib::FilenameCollationKey::from(filename_a);
        let key_b = glib::FilenameCollationKey::from(filename_b);
        order = key_a.partial_cmp(&key_b).unwrap();
    }

    order
}

pub fn load_files_from_folder(folder: &gio::File, recursive: bool) -> Vec<gio::File> {
    use std::time::Instant;

    let now = Instant::now();
    let res = load_files_from_folder_internal(folder, folder, recursive);
    debug!(
        "Folder enumeration: {} us (recursive: {}), total files: {}",
        now.elapsed().as_micros(),
        recursive,
        res.len()
    );

    res
}

/// Playback state that is stored together with the cached playlist, so that
/// restoring the session brings back the same song, position and player
/// settings.
#[derive(Debug, Clone)]
pub struct SessionState {
    pub current_uri: Option<String>,
    pub position: u64,
    pub playing: bool,
    pub volume: f64,
    pub shuffled: bool,
    pub repeat_mode: RepeatMode,
}

pub fn store_playlist(player: &AudioPlayer) {
    let queue = player.queue();
    let state = player.state();

    let pls = glib::KeyFile::new();
    pls.set_string("playlist", "X-GNOME-Title", "Amberol's current playlist");

    pls.set_int64("playlist", "NumberOfEntries", queue.n_songs() as i64);

    for i in 0..queue.n_songs() {
        let song = queue.song_at(i).expect("Unknown song");
        let path = song.file().path().expect("Unknown file");
        let path_str = path.to_string_lossy();
        pls.set_string("playlist", &format!("File{i}"), &path_str);
    }

    if let Some(song) = state.current_song() {
        pls.set_string("playlist", "CurrentUri", &song.uri());
    }
    pls.set_int64("playlist", "Position", state.position() as i64);
    pls.set_boolean("playlist", "Playing", state.playing());
    pls.set_double("playlist", "Volume", state.volume());
    pls.set_boolean("playlist", "Shuffled", queue.is_shuffled());
    pls.set_string("playlist", "RepeatMode", &queue.repeat_mode().to_string());

    let mut pls_cache = glib::user_cache_dir();
    pls_cache.push("amberol");
    pls_cache.push("playlists");
    glib::mkdir_with_parents(&pls_cache, 0o755);

    pls_cache.push("current.pls");
    match pls.save_to_file(&pls_cache) {
        Ok(_) => debug!("Current playlist updated to: {:?}", &pls_cache),
        Err(e) => debug!("Unable to save current playlist: {e}"),
    }
}

pub fn load_cached_session() -> Option<(Vec<gio::File>, SessionState)> {
    let mut pls_cache = glib::user_cache_dir();
    pls_cache.push("amberol");
    pls_cache.push("playlists");
    pls_cache.push("current.pls");

    let pls = glib::KeyFile::new();
    if let Err(e) = pls.load_from_file(&pls_cache, glib::KeyFileFlags::NONE) {
        debug!("Unable to load current playlist: {e}");
        return None;
    }

    let n_entries: usize = match pls.int64("playlist", "NumberOfEntries") {
        Ok(n) => n as usize,
        Err(_) => 0,
    };

    let mut songs = Vec::with_capacity(n_entries);

    for i in 0..n_entries {
        match pls.value("playlist", &format!("File{i}")) {
            Ok(p) => songs.push(gio::File::for_path(p)),
            Err(e) => debug!("Skipping File{i} from playlist: {e}"),
        }
    }

    if songs.is_empty() {
        return None;
    }

    let state = SessionState {
        current_uri: pls.string("playlist", "CurrentUri").ok().map(|s| s.to_string()),
        position: pls.int64("playlist", "Position").unwrap_or(0).max(0) as u64,
        playing: pls.boolean("playlist", "Playing").unwrap_or(false),
        volume: pls.double("playlist", "Volume").unwrap_or(1.0).clamp(0.0, 1.0),
        shuffled: pls.boolean("playlist", "Shuffled").unwrap_or(false),
        repeat_mode: match pls
            .string("playlist", "RepeatMode")
            .map(|s| s.to_string())
            .unwrap_or_else(|_| "consecutive".to_string())
            .as_str()
        {
            "repeat-all" => RepeatMode::RepeatAll,
            "repeat-one" => RepeatMode::RepeatOne,
            _ => RepeatMode::Consecutive,
        },
    };

    Some((songs, state))
}

pub fn has_cached_playlist() -> bool {
    // Checking the session at startup must not allocate a `gio::File` for
    // every entry; only the count matters here. The actual files are loaded
    // once, by `load_cached_session`.
    let mut pls_cache = glib::user_cache_dir();
    pls_cache.push("amberol");
    pls_cache.push("playlists");
    pls_cache.push("current.pls");

    let pls = glib::KeyFile::new();
    if pls.load_from_file(&pls_cache, glib::KeyFileFlags::NONE).is_err() {
        return false;
    }

    pls.int64("playlist", "NumberOfEntries")
        .map(|entries| entries > 0)
        .unwrap_or(false)
}

pub fn clear_cached_playlist() {
    let mut pls_cache = glib::user_cache_dir();
    pls_cache.push("amberol");
    pls_cache.push("playlists");
    pls_cache.push("current.pls");

    match std::fs::remove_file(&pls_cache) {
        Ok(()) => debug!("Removed cached playlist: {:?}", &pls_cache),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => debug!("Unable to remove cached playlist: {e}"),
    }
}
