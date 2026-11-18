// SPDX-FileCopyrightText: 2026 Amberol contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Writing cover artwork directly into audio file tags.
//!
//! `lofty` already reads tags everywhere in Amberol. Using its writer keeps
//! the same format support (ID3v2, Vorbis Comments, MP4 ilst, APE, ...),
//! touches only the metadata and never re-encodes the audio stream.

use std::{fs::OpenOptions, path::Path};

use lofty::{
    config::{ParseOptions, WriteOptions},
    file::{AudioFile, TaggedFileExt},
    picture::{MimeType, Picture, PictureType},
    probe::Probe,
    tag::{Tag, TagExt},
};

/// A cover change to apply to one audio file.
#[derive(Clone, Debug)]
pub enum CoverTagChange {
    Set {
        data: Vec<u8>,
        mime: MimeType,
    },
    Remove,
}

/// Detects the MIME type of an image that is about to be embedded. Both the
/// file picker and the compressor produce only these two formats.
pub fn mime_type_for_image(data: &[u8]) -> MimeType {
    if data.starts_with(&[0xff, 0xd8, 0xff]) {
        MimeType::Jpeg
    } else if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        MimeType::Png
    } else {
        // The picker only accepts PNG/JPEG, so this is only a safety net for
        // an image whose header could not be read.
        MimeType::Unknown("image/jpeg".to_owned())
    }
}

/// Extracts the front cover picture bytes from a file's metadata, preferring
/// the dedicated front-cover frame and falling back to the same generic
/// picture types the player treats as artwork.
pub fn read_cover_image(path: &Path) -> Option<(Vec<u8>, MimeType)> {
    let tagged = Probe::open(path).ok()?.read().ok()?;

    let picture = tagged
        .tags()
        .iter()
        .find_map(|tag| tag.get_picture_type(PictureType::CoverFront))
        .or_else(|| {
            tagged.tags().iter().find_map(|tag| {
                tag.pictures().iter().find(|picture| {
                    matches!(
                        picture.pic_type(),
                        PictureType::Other | PictureType::BandLogo
                    )
                })
            })
        })?;

    let mime = picture
        .mime_type()
        .cloned()
        .unwrap_or_else(|| mime_type_for_image(picture.data()));
    Some((picture.data().to_vec(), mime))
}

fn front_cover(data: Vec<u8>, mime: MimeType) -> Picture {
    Picture::unchecked(data)
        .pic_type(PictureType::CoverFront)
        .mime_type(mime)
        .description("Cover")
        .build()
}

/// Writes (or removes) the front cover picture in `path`'s tags.
///
/// Returns an error when the format cannot hold writable tags or the file is
/// not writable; in that case the caller keeps the old artwork.
pub fn write_cover_tag(path: &Path, change: &CoverTagChange) -> Result<(), String> {
    let mut tagged = Probe::open(path)
        .map_err(|err| err.to_string())?
        .options(ParseOptions::new().read_cover_art(true))
        .read()
        .map_err(|err| err.to_string())?;

    let file_type = tagged.file_type();
    let primary_type = file_type.primary_tag_type();

    let has_tag = tagged.tag(primary_type).is_some() || !tagged.tags().is_empty();

    if has_tag {
        let tag = if tagged.contains_tag_type(primary_type) {
            tagged.tag_mut(primary_type).expect("Primary tag exists")
        } else {
            tagged.first_tag_mut().expect("A tag was found above")
        };
        match change {
            CoverTagChange::Set { data, mime } => {
                let picture = front_cover(data.clone(), mime.clone());
                // Prefer replacing an existing front cover. If the file only
                // has an untyped picture (common in MP4 `covr`), replace that
                // one instead of piling a second copy onto the tag.
                let index = tag
                    .pictures()
                    .iter()
                    .position(|picture| picture.pic_type() == PictureType::CoverFront)
                    .or_else(|| (!tag.pictures().is_empty()).then_some(0));
                match index {
                    Some(index) => tag.set_picture(index, picture),
                    None => tag.push_picture(picture),
                }
            }
            CoverTagChange::Remove => {
                // These are exactly the types Amberol's reader falls back to
                // when no CoverFront picture is present.
                tag.remove_picture_type(PictureType::CoverFront);
                tag.remove_picture_type(PictureType::Other);
                tag.remove_picture_type(PictureType::BandLogo);
            }
        }

        tagged
            .save_to_path(path, WriteOptions::default())
            .map_err(|err| err.to_string())?;
    } else if let CoverTagChange::Set { data, mime } = change {
        if !file_type.tag_support(primary_type).is_writable() {
            return Err(format!("{primary_type:?} cannot be written"));
        }

        let mut tag = Tag::new(primary_type);
        tag.push_picture(front_cover(data.clone(), mime.clone()));

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|err| err.to_string())?;
        tag.save_to(&mut file, WriteOptions::default())
            .map_err(|err| err.to_string())?;
    }

    Ok(())
}
