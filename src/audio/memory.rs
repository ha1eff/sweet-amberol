// SPDX-FileCopyrightText: 2026  Amberol contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Keeps playback memory within the limits requested for Amberol:
//!
//! * tracks up to 100 MiB may use at most 150 MiB;
//! * larger tracks get their own size plus 50 MiB.
//!
//! Audio is streamed from disk, so the played file is not held in memory.
//! What does grow is artwork and per-song UI state, especially when a queue
//! contains thousands of tracks. Cover textures are loaded lazily and their
//! caches are bounded, while this module provides a final safety net: if the
//! process still exceeds the playback budget, it drops the caches and asks the
//! allocator to return the freed pages to the system.

use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

use gtk::glib::{self, ControlFlow};
use log::{debug, warn};
use once_cell::sync::{Lazy, OnceCell};

use crate::{
    album::Album,
    audio::{cover_cache, CoverCache},
    utils,
};

/// Size of the track that is currently playing, or zero when nothing plays.
static CURRENT_FILE_SIZE: AtomicU64 = AtomicU64::new(0);

/// Avoid clearing caches every few seconds while the process is above the
/// limit because of memory that is not ours to release (GTK, GStreamer, the
/// driver, font caches). One attempt every 20 seconds is enough.
static LAST_RECLAIM: Lazy<Mutex<Option<Instant>>> = Lazy::new(|| Mutex::new(None));
static WATCHDOG_STARTED: OnceCell<()> = OnceCell::new();

pub fn set_current_playback_size(file_size: u64) {
    CURRENT_FILE_SIZE.store(file_size, Ordering::Relaxed);

    if file_size > 0 {
        // A new song is a good moment to check whether the previous queue
        // left more artwork in memory than the new budget allows.
        enforce_if_needed();
    }
}

/// Returns the current limit in bytes, or `None` when nothing is playing.
pub fn playback_budget() -> Option<u64> {
    match CURRENT_FILE_SIZE.load(Ordering::Relaxed) {
        0 => None,
        size => Some(utils::playback_memory_limit(size)),
    }
}

fn process_rss_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let resident_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        return None;
    }
    Some(resident_pages.saturating_mul(page_size as u64))
}

/// Drops every artwork cache and returns the freed pages to the OS.
///
/// The periodic watchdog calls this only when the process is above its
/// playback budget. A playlist edit is an explicit moment at which the user
/// expects the removed songs (and, above all, their textures and palettes) to
/// stop occupying memory, so the window calls this directly after removals.
pub fn reclaim() {
    cover_cache::clear_thumbnail_cache();
    if let Ok(mut covers) = CoverCache::global().lock() {
        covers.clear();
    }
    Album::clear_cover_cache();
    utils::release_unused_memory();
}

/// Checks the current playback budget and releases cached artwork if the
/// process is above it.
pub fn enforce_if_needed() {
    let Some(budget) = playback_budget() else {
        return;
    };
    let Some(rss) = process_rss_bytes() else {
        return;
    };
    if rss <= budget {
        return;
    }

    let now = Instant::now();
    if let Ok(mut last) = LAST_RECLAIM.lock() {
        if last
            .map(|last| now.duration_since(last) < Duration::from_secs(20))
            .unwrap_or(false)
        {
            return;
        }
        *last = Some(now);
    } else {
        warn!("Unable to lock the playback memory guard");
        return;
    }

    debug!(
        "Playback memory is {} MiB, above the {} MiB budget; releasing cover caches",
        rss / (1024 * 1024),
        budget / (1024 * 1024)
    );
    reclaim();

    if let Some(after) = process_rss_bytes() {
        debug!(
            "Playback memory after reclaim: {} MiB",
            after / (1024 * 1024)
        );
    }
}

/// Starts the periodic memory check. Safe to call more than once.
pub fn start_watchdog() {
    if WATCHDOG_STARTED.set(()).is_err() {
        return;
    }

    glib::timeout_add_seconds_local(10, || {
        enforce_if_needed();
        ControlFlow::Continue
    });
}
