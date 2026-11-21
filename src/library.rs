// SPDX-FileCopyrightText: 2026  Amberol contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Persistent, read-only music library index.
//!
//! Amberol never writes to the audio files. The database only stores the
//! metadata that has been read from them, plus the file modification time and
//! size so that subsequent scans can skip files that have not changed.

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::UNIX_EPOCH,
};

use gtk::glib;
use lofty::{
    prelude::{Accessor, AudioFile, ItemKey, TaggedFileExt},
    probe::Probe,
};
use log::{debug, warn};
use once_cell::sync::Lazy;
use rand::seq::IndexedRandom;
use regex::Regex;
use rusqlite::{params, Connection, OptionalExtension, Transaction};

use crate::utils;

pub type LibraryResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AlbumRecord {
    pub title: String,
    pub artist: String,
    pub cover_path: PathBuf,
    pub track_count: u32,
    pub modified_at: i64,
}

#[derive(Clone, Debug, Default)]
pub struct ScanStats {
    pub total: u32,
    pub added: u32,
    pub updated: u32,
    pub removed: u32,
    pub failed: u32,
}

#[derive(Clone, Debug, Default)]
pub struct DeleteAlbumStats {
    pub deleted: Vec<PathBuf>,
    pub failed: u32,
}

#[derive(Clone, Debug)]
pub enum ScanEvent {
    Progress { done: u32, total: u32 },
    Finished(ScanStats),
    Failed(String),
}

/// Error returned by a scan that was stopped because the database was reset.
pub const SCAN_CANCELLED: &str = "library scan was cancelled";

/// Upper bound for the memory a first scan is allowed to use for its working
/// set. Track rows are streamed straight into SQLite, so only the list of
/// audio paths and a small per-album cover set stay in memory. The scan logs a
/// warning if those structures ever grow beyond this budget.
pub const SCAN_MEMORY_LIMIT_BYTES: usize = 300 * 1024 * 1024;

#[derive(Debug)]
struct TrackRecord {
    path: String,
    mtime: i64,
    size: i64,
    title: String,
    artist: String,
    album: String,
    album_artist: String,
    track_number: u32,
    disc_number: u32,
    duration: u64,
    added_at: i64,
}

pub struct Library {
    conn: Mutex<Connection>,
    /// Path of the persistent database. A scan uses its own connection so a
    /// long write transaction never blocks the UI's album queries. In-memory
    /// libraries (tests) have no path and share the main connection.
    db_path: Option<PathBuf>,
    albums: Mutex<Option<Arc<Vec<AlbumRecord>>>>,
    scan_generation: AtomicU64,
    /// Held for the duration of a scan. Resetting the database increments the
    /// generation and then waits for this lock, so a scan cannot write rows or
    /// cover files after the reset has started.
    scan_gate: Mutex<()>,
}

#[derive(Debug)]
struct ExistingTrack {
    mtime: i64,
    size: i64,
    album: String,
    album_artist: String,
}

impl std::fmt::Debug for Library {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Library").finish_non_exhaustive()
    }
}

impl Library {
    /// Opens the database in the user's data directory.
    pub fn open_default() -> LibraryResult<Self> {
        let mut data_dir = glib::user_data_dir();
        data_dir.push("amberol");
        fs::create_dir_all(&data_dir)?;

        let db_path = data_dir.join("library.db");
        debug!("Opening music library database at {:?}", db_path);
        Self::from_connection(Connection::open(&db_path)?, Some(db_path))
    }

    /// Useful as a fallback when the persistent database cannot be opened.
    pub fn open_in_memory() -> LibraryResult<Self> {
        Self::from_connection(Connection::open_in_memory()?, None)
    }

    fn from_connection(mut conn: Connection, db_path: Option<PathBuf>) -> LibraryResult<Self> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;

             CREATE TABLE IF NOT EXISTS tracks (
                 id           INTEGER PRIMARY KEY,
                 path         TEXT NOT NULL UNIQUE,
                 mtime        INTEGER NOT NULL,
                 size         INTEGER NOT NULL,
                 title        TEXT NOT NULL DEFAULT '',
                 artist       TEXT NOT NULL DEFAULT '',
                 album        TEXT NOT NULL DEFAULT '',
                 album_artist TEXT NOT NULL DEFAULT '',
                 track_number INTEGER NOT NULL DEFAULT 0,
                 disc_number  INTEGER NOT NULL DEFAULT 0,
                 duration     INTEGER NOT NULL DEFAULT 0,
                 added_at     INTEGER NOT NULL
             );

             CREATE INDEX IF NOT EXISTS tracks_album_idx
                 ON tracks(album, album_artist);
             CREATE INDEX IF NOT EXISTS tracks_mtime_idx
                 ON tracks(mtime DESC);

             -- A credit can list several artists, e.g. A and B.
             -- This index connects each individual name to the albums it
             -- appears on, so an artist page also contains collaborations
             -- where the artist is not the album artist.
             CREATE TABLE IF NOT EXISTS album_artists (
                 path         TEXT NOT NULL REFERENCES tracks(path) ON DELETE CASCADE,
                 artist_key   TEXT NOT NULL,
                 artist       TEXT NOT NULL,
                 album        TEXT NOT NULL,
                 album_artist TEXT NOT NULL,
                 PRIMARY KEY (path, artist_key, album, album_artist)
             );
             CREATE INDEX IF NOT EXISTS album_artists_key_idx
                 ON album_artists(artist_key);

             CREATE TABLE IF NOT EXISTS library_state (
                 id         INTEGER PRIMARY KEY CHECK (id = 1),
                 folder     TEXT NOT NULL,
                 scanned_at INTEGER NOT NULL
             );

             CREATE TABLE IF NOT EXISTS favorite_albums (
                 album        TEXT NOT NULL,
                 album_artist TEXT NOT NULL,
                 added_at     INTEGER NOT NULL,
                 PRIMARY KEY (album, album_artist)
             );",
        )?;

        build_artist_index_if_missing(&mut conn)?;

        Ok(Self {
            conn: Mutex::new(conn),
            db_path,
            albums: Mutex::new(None),
            scan_generation: AtomicU64::new(0),
            scan_gate: Mutex::new(()),
        })
    }

    /// Returns the folder that was successfully indexed by the last scan.
    pub fn indexed_folder(&self) -> LibraryResult<Option<PathBuf>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "library database lock poisoned")?;
        let folder = conn
            .query_row("SELECT folder FROM library_state WHERE id = 1", [], |row| {
                row.get::<_, String>(0)
            })
            .optional()?;
        Ok(folder.map(PathBuf::from))
    }

    /// True when `folder` is already stored in the persistent index.
    pub fn is_folder_indexed(&self, folder: &Path) -> bool {
        let Ok(folder) = fs::canonicalize(folder) else {
            return false;
        };
        self.indexed_folder()
            .ok()
            .flatten()
            .is_some_and(|indexed| indexed == folder)
    }

    fn invalidate_album_cache(&self) {
        if let Ok(mut albums) = self.albums.lock() {
            albums.take();
        }
    }

    /// Deletes every indexed track, favorite and stored folder from the
    /// database. Any running scan is cancelled first, and the method waits
    /// until it has stopped so it cannot repopulate the database or the cover
    /// cache afterwards.
    pub fn reset(&self) -> LibraryResult<()> {
        self.scan_generation.fetch_add(1, Ordering::SeqCst);

        let _gate = self
            .scan_gate
            .lock()
            .map_err(|_| "library scan lock poisoned")?;

        {
            let mut conn = self
                .conn
                .lock()
                .map_err(|_| "library database lock poisoned")?;
            let tx = conn.transaction()?;
            tx.execute("DELETE FROM tracks", [])?;
            tx.execute("DELETE FROM album_artists", [])?;
            tx.execute("DELETE FROM favorite_albums", [])?;
            tx.execute("DELETE FROM library_state", [])?;
            tx.commit()?;

            // Reclaim the space of the deleted rows and fold the WAL back into
            // the database, so a reset does not leave hundreds of megabytes on
            // disk.
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")?;
        }

        self.invalidate_album_cache();
        Ok(())
    }

    fn all_albums(&self) -> LibraryResult<Arc<Vec<AlbumRecord>>> {
        {
            let albums = self
                .albums
                .lock()
                .map_err(|_| "album cache lock poisoned")?;
            if let Some(albums) = albums.as_ref() {
                return Ok(albums.clone());
            }
        }

        let conn = self
            .conn
            .lock()
            .map_err(|_| "library database lock poisoned")?;
        let mut stmt = conn.prepare(
            "SELECT album, album_artist, MIN(path), COUNT(*), MAX(mtime)
             FROM tracks
             GROUP BY album, album_artist
             ORDER BY MAX(mtime) DESC, album COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], album_from_row)?;
        let albums = Arc::new(rows.collect::<Result<Vec<_>, _>>()?);

        let mut cache = self
            .albums
            .lock()
            .map_err(|_| "album cache lock poisoned")?;
        *cache = Some(albums.clone());
        Ok(albums)
    }

    pub fn album_count(&self) -> LibraryResult<u32> {
        Ok(self.all_albums()?.len() as u32)
    }

    /// Searches albums by title or album artist.
    ///
    /// When `priority_artist` is given, albums that contain at least one
    /// track by that artist are returned first, before the rest of the
    /// matching albums. This is used when search is opened from an artist
    /// page, so the albums of that artist are not buried in an alphabetical
    /// list. The ranking happens in SQLite so those albums are still present
    /// when the result set is larger than `limit`.
    pub fn search_albums(
        &self,
        query: &str,
        limit: u32,
        priority_artist: Option<&str>,
    ) -> LibraryResult<Vec<AlbumRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "library database lock poisoned")?;
        let pattern = format!("%{}%", query.trim());
        let mut stmt = conn.prepare(
            "SELECT album, album_artist, MIN(path), COUNT(*), MAX(mtime)
             FROM tracks
             WHERE album LIKE ?1 COLLATE NOCASE
                OR album_artist LIKE ?1 COLLATE NOCASE
             GROUP BY album, album_artist
             ORDER BY
                 CASE WHEN ?3 IS NULL THEN 1
                      WHEN EXISTS (
                          SELECT 1 FROM tracks AS artist_tracks
                          WHERE artist_tracks.album = tracks.album
                            AND artist_tracks.album_artist = tracks.album_artist
                            AND (artist_tracks.artist = ?3 COLLATE NOCASE
                                 OR artist_tracks.album_artist = ?3 COLLATE NOCASE
                                 OR EXISTS (
                                     SELECT 1 FROM album_artists aa
                                     WHERE aa.path = artist_tracks.path
                                       AND aa.artist_key = lower(?3)
                                 ))
                      ) THEN 0
                      ELSE 1
                 END,
                 album COLLATE NOCASE
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![pattern, limit, priority_artist], album_from_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Albums ordered by the newest file modification time in each album.
    pub fn recent_albums(&self, limit: u32) -> LibraryResult<Vec<AlbumRecord>> {
        self.recent_albums_from(limit, 0)
    }

    /// A page of recent albums, skipping the albums already shown. Used by the
    /// infinite scroll on the Recent Albums page; the cached album list makes
    /// each additional page cheap to produce.
    pub fn recent_albums_from(&self, limit: u32, offset: u32) -> LibraryResult<Vec<AlbumRecord>> {
        let albums = self.all_albums()?;
        Ok(albums
            .iter()
            .skip(offset as usize)
            .take(limit as usize)
            .cloned()
            .collect())
    }

    /// A random selection of albums. Each call returns a fresh selection.
    pub fn random_albums(&self, limit: u32) -> LibraryResult<Vec<AlbumRecord>> {
        let albums = self.all_albums()?;
        let mut rng = rand::rng();
        Ok(albums.sample(&mut rng, limit as usize).cloned().collect())
    }

    /// Albums by an artist, including albums where the artist only appears
    /// as the track artist on a compilation.
    pub fn albums_for_artist(&self, artist: &str) -> LibraryResult<Vec<AlbumRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "library database lock poisoned")?;
        let mut stmt = conn.prepare(
            "SELECT album, album_artist, MIN(path), COUNT(*), MAX(mtime)
             FROM tracks
             WHERE artist = ?1 COLLATE NOCASE
                OR album_artist = ?1 COLLATE NOCASE
                OR EXISTS (
                    SELECT 1 FROM album_artists aa
                    WHERE aa.path = tracks.path AND aa.artist_key = lower(?1)
                )
             GROUP BY album, album_artist
             ORDER BY MAX(mtime) DESC, album COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([artist], album_from_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Artist names from the library that are mentioned in `credit`.
    ///
    /// A single artist tag can list several artists using commas, `feat.`,
    /// `/`, `&` and other conventions, sometimes all at once. Guessing the
    /// separator is unreliable: `&` also occurs inside names such as
    /// "King Gizzard & The Lizard Wizard". Instead, look up the names the
    /// library already knows and keep the longest matches that appear in the
    /// credit, so a full band name wins over its parts.
    pub fn artists_in_credit(&self, credit: &str) -> LibraryResult<Vec<String>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "library database lock poisoned")?;
        let mut stmt = conn.prepare(
            "SELECT DISTINCT artist FROM tracks WHERE artist <> ''
             UNION
             SELECT DISTINCT album_artist FROM tracks WHERE album_artist <> ''
             UNION
             SELECT DISTINCT artist FROM album_artists WHERE artist <> ''",
        )?;
        let names = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        drop(stmt);
        drop(conn);

        let mut matches: Vec<String> = names
            .into_iter()
            .filter(|name| contains_artist_name(credit, name))
            .collect();

        // When the whole credit is itself a known artist, prefer individual
        // names so the user can still pick one of them. But if every match is
        // just a differently-cased spelling of the same name (the tags of an
        // artist often mix "Twenty One Pilots" and "twenty one pilots"), keep
        // one of them so that a link is still created.
        if matches
            .iter()
            .any(|name| !name.eq_ignore_ascii_case(credit))
        {
            matches.retain(|name| !name.eq_ignore_ascii_case(credit));
        }

        // Longer names win over the shorter names they contain.
        matches.sort_by(|a, b| b.chars().count().cmp(&a.chars().count()));
        let mut kept: Vec<String> = Vec::new();
        for name in matches {
            if kept
                .iter()
                .any(|longer| contains_artist_name(longer, &name))
            {
                continue;
            }
            kept.push(name);
        }

        // Restore the order in which the names appear in the credit.
        let lower = credit.to_lowercase();
        kept.sort_by_key(|name| {
            lower
                .find(&name.to_lowercase())
                .unwrap_or(usize::MAX)
        });
        Ok(kept)
    }

    /// Number of albums and tracks associated with an artist.
    pub fn artist_stats(&self, artist: &str) -> LibraryResult<(u32, u32)> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "library database lock poisoned")?;
        let albums = conn.query_row(
            "SELECT COUNT(*) FROM (
                 SELECT 1 FROM tracks
                 WHERE artist = ?1 COLLATE NOCASE
                    OR album_artist = ?1 COLLATE NOCASE
                    OR EXISTS (
                        SELECT 1 FROM album_artists aa
                        WHERE aa.path = tracks.path AND aa.artist_key = lower(?1)
                    )
                 GROUP BY album, album_artist
             )",
            [artist],
            |row| row.get::<_, u32>(0),
        )?;
        let tracks = conn.query_row(
            "SELECT COUNT(*) FROM tracks
             WHERE artist = ?1 COLLATE NOCASE
                OR album_artist = ?1 COLLATE NOCASE
                OR EXISTS (
                    SELECT 1 FROM album_artists aa
                    WHERE aa.path = tracks.path AND aa.artist_key = lower(?1)
                )",
            [artist],
            |row| row.get::<_, u32>(0),
        )?;
        Ok((albums, tracks))
    }

    /// Albums marked as favorites, newest favorites first.
    pub fn favorite_albums(&self) -> LibraryResult<Vec<AlbumRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "library database lock poisoned")?;
        let mut stmt = conn.prepare(
            "SELECT t.album, t.album_artist, MIN(t.path), COUNT(*), MAX(t.mtime)
             FROM tracks t
             JOIN favorite_albums f
               ON f.album = t.album AND f.album_artist = t.album_artist
             GROUP BY t.album, t.album_artist
             ORDER BY MAX(f.added_at) DESC, t.album COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], album_from_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Resolves the album and album artist for a track, which may be a
    /// featured artist on a compilation.
    pub fn resolve_album(
        &self,
        album: &str,
        artist: &str,
    ) -> LibraryResult<Option<(String, String)>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "library database lock poisoned")?;
        let mut stmt = conn.prepare(
            "SELECT album, album_artist
             FROM tracks
             WHERE album = ?1 AND (album_artist = ?2 OR artist = ?2)
             ORDER BY (album_artist = ?2) DESC
             LIMIT 1",
        )?;
        let result = stmt
            .query_row(params![album, artist], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .optional()?;
        Ok(result)
    }

    pub fn is_album_favorite(&self, album: &str, album_artist: &str) -> LibraryResult<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "library database lock poisoned")?;
        let count = conn.query_row(
            "SELECT COUNT(*) FROM favorite_albums
             WHERE album = ?1 AND album_artist = ?2",
            params![album, album_artist],
            |row| row.get::<_, u32>(0),
        )?;
        Ok(count > 0)
    }

    /// All favorite album keys, for marking a whole page with one query
    /// instead of running a separate lookup for every tile.
    pub fn favorite_album_keys(&self) -> LibraryResult<HashSet<(String, String)>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "library database lock poisoned")?;
        let mut stmt = conn.prepare("SELECT album, album_artist FROM favorite_albums")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<Result<HashSet<_>, _>>()?)
    }

    pub fn set_album_favorite(
        &self,
        album: &str,
        album_artist: &str,
        favorite: bool,
    ) -> LibraryResult<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "library database lock poisoned")?;
        if favorite {
            let added_at = std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_secs() as i64)
                .unwrap_or_default();
            conn.execute(
                "INSERT OR REPLACE INTO favorite_albums (album, album_artist, added_at)
                 VALUES (?1, ?2, ?3)",
                params![album, album_artist, added_at],
            )?;
        } else {
            conn.execute(
                "DELETE FROM favorite_albums WHERE album = ?1 AND album_artist = ?2",
                params![album, album_artist],
            )?;
        }
        Ok(())
    }

    /// Returns the tracks of an album in disc/track order.
    pub fn tracks_for_album(&self, title: &str, artist: &str) -> LibraryResult<Vec<PathBuf>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "library database lock poisoned")?;
        let mut stmt = conn.prepare(
            "SELECT path
             FROM tracks
             WHERE album = ?1 AND album_artist = ?2
             ORDER BY disc_number, track_number, path COLLATE NOCASE",
        )?;
        let rows = stmt.query_map(params![title, artist], |row| {
            row.get::<_, String>(0).map(PathBuf::from)
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Deletes specific track files from disk and removes their rows from the
    /// index. This is the only operation that intentionally touches files in
    /// the library; metadata inside the files is never modified.
    pub fn delete_tracks(&self, paths: &[PathBuf]) -> LibraryResult<DeleteAlbumStats> {
        let mut stats = DeleteAlbumStats::default();

        for path in paths {
            match fs::remove_file(path) {
                Ok(()) => stats.deleted.push(path.clone()),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    stats.deleted.push(path.clone());
                }
                Err(err) => {
                    warn!("Unable to delete {:?}: {err}", path);
                    stats.failed += 1;
                }
            }
        }

        if !stats.deleted.is_empty() {
            let conn = self
                .conn
                .lock()
                .map_err(|_| "library database lock poisoned")?;
            let mut delete = conn.prepare_cached("DELETE FROM tracks WHERE path = ?1")?;
            for path in &stats.deleted {
                delete.execute([path.to_string_lossy().as_ref()])?;
            }
        }

        self.invalidate_album_cache();
        Ok(stats)
    }

    /// Deletes an album's files from disk and removes its rows from the
    /// index.
    pub fn delete_album(&self, title: &str, artist: &str) -> LibraryResult<DeleteAlbumStats> {
        let paths = self.tracks_for_album(title, artist)?;
        self.delete_tracks(&paths)
    }

    /// Returns the tracks of several albums in disc/track order.
    pub fn tracks_for_albums(&self, albums: &[AlbumRecord]) -> LibraryResult<Vec<PathBuf>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "library database lock poisoned")?;
        let mut stmt = conn.prepare(
            "SELECT path
             FROM tracks
             WHERE album = ?1 AND album_artist = ?2
             ORDER BY disc_number, track_number, path COLLATE NOCASE",
        )?;

        let mut paths = Vec::new();
        for album in albums {
            let rows = stmt.query_map(params![album.title, album.artist], |row| {
                row.get::<_, String>(0).map(PathBuf::from)
            })?;
            paths.extend(rows.collect::<Result<Vec<_>, _>>()?);
        }

        Ok(paths)
    }

    /// Scans a music folder recursively.
    ///
    /// Only files whose mtime/size changed are parsed again. Track rows are
    /// streamed into a single transaction while the scan runs instead of being
    /// collected in memory, and the file list is processed one entry at a
    /// time. On a first scan the working set therefore stays far below
    /// [`SCAN_MEMORY_LIMIT_BYTES`] even for a large music collection.
    pub fn scan_folder<F>(&self, root: &Path, mut progress: F) -> LibraryResult<ScanStats>
    where
        F: FnMut(u32, u32),
    {
        let root = fs::canonicalize(root)?;
        if !root.is_dir() {
            return Err(format!("Not a directory: {}", root.display()).into());
        }

        let _scan_guard = self
            .scan_gate
            .lock()
            .map_err(|_| "library scan lock poisoned")?;
        let generation = self.scan_generation.load(Ordering::SeqCst);

        // The album list is rebuilt after the scan; make sure any UI query
        // running in the meantime does not keep a stale snapshot forever.
        self.invalidate_album_cache();

        let stats = if let Some(db_path) = self.db_path.clone() {
            // A dedicated connection keeps the long scan transaction away
            // from the UI connection, so switching to the library or reading
            // album rows never waits for the scan.
            let mut conn = Connection::open(db_path)?;
            conn.busy_timeout(std::time::Duration::from_secs(5))?;
            conn.execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = NORMAL;
                 PRAGMA foreign_keys = ON;
                 PRAGMA cache_size = -8192;",
            )?;
            self.scan_with_connection(&mut conn, &root, &mut progress, generation)?
        } else {
            let mut conn = self
                .conn
                .lock()
                .map_err(|_| "library database lock poisoned")?;
            self.scan_with_connection(&mut conn, &root, &mut progress, generation)?
        };

        self.invalidate_album_cache();

        debug!("Library scan finished: {:?}", stats);
        Ok(stats)
    }

    fn scan_with_connection<F>(
        &self,
        conn: &mut Connection,
        root: &Path,
        progress: &mut F,
        generation: u64,
    ) -> LibraryResult<ScanStats>
    where
        F: FnMut(u32, u32),
    {
        // A temporary table records which paths were seen during this scan so
        // stale rows can be removed without keeping the whole file list and
        // the previous index in memory.
        conn.execute_batch(
            "PRAGMA temp_store = FILE;
             CREATE TEMP TABLE IF NOT EXISTS scan_seen (path TEXT PRIMARY KEY);
             DELETE FROM scan_seen;",
        )?;

        // First pass: count the files so the progress dialog knows the total.
        let mut total = 0u32;
        let mut cancelled = false;
        walk_audio_files(root, &mut |_path| {
            if self.scan_generation.load(Ordering::SeqCst) != generation {
                cancelled = true;
                return false;
            }
            total = total.saturating_add(1);
            true
        });
        if cancelled {
            return Err(SCAN_CANCELLED.into());
        }
        progress(0, total);

        let now = now_secs();
        let mut failed = 0;
        let mut added = 0;
        let mut updated = 0;
        let mut checked_covers = HashSet::new();
        let mut scan_error: Option<Box<dyn std::error::Error + Send + Sync>> = None;

        let tx = conn.transaction()?;
        {
            let mut upsert = tx.prepare_cached(
                "INSERT INTO tracks (
                     path, mtime, size, title, artist, album, album_artist,
                     track_number, disc_number, duration, added_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(path) DO UPDATE SET
                     mtime = excluded.mtime,
                     size = excluded.size,
                     title = excluded.title,
                     artist = excluded.artist,
                     album = excluded.album,
                     album_artist = excluded.album_artist,
                     track_number = excluded.track_number,
                     disc_number = excluded.disc_number,
                     duration = excluded.duration",
            )?;
            let mut select_existing = tx.prepare_cached(
                "SELECT mtime, size, album, album_artist FROM tracks WHERE path = ?1",
            )?;
            let mut mark_seen =
                tx.prepare_cached("INSERT OR IGNORE INTO scan_seen (path) VALUES (?1)")?;

            let mut index = 0u32;
            let mut process = |path: &Path| -> LibraryResult<()> {
                if self.scan_generation.load(Ordering::SeqCst) != generation {
                    return Err(SCAN_CANCELLED.into());
                }

                let path_string = path.to_string_lossy().into_owned();
                mark_seen.execute([&path_string])?;

                let metadata = match fs::metadata(path) {
                    Ok(metadata) => metadata,
                    Err(err) => {
                        warn!("Unable to read file metadata for {:?}: {err}", path);
                        // The file still exists in the index, so keep its row.
                        failed += 1;
                        return Ok(());
                    }
                };

                let mtime = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map(|duration| duration.as_secs() as i64)
                    .unwrap_or(0);
                let size = metadata.len() as i64;

                let previous = select_existing
                    .query_row([&path_string], |row| {
                        Ok(ExistingTrack {
                            mtime: row.get(0)?,
                            size: row.get(1)?,
                            album: row.get(2)?,
                            album_artist: row.get(3)?,
                        })
                    })
                    .optional()?;

                if let Some(previous) = &previous {
                    if previous.mtime == mtime && previous.size == size {
                        // The tags have not changed. The only file access
                        // needed is a one-time extraction of a missing album
                        // thumbnail (for example after upgrading from a build
                        // without the thumbnail cache).
                        ensure_album_cover(
                            path,
                            &previous.album,
                            &previous.album_artist,
                            None,
                            &mut checked_covers,
                        );
                        return Ok(());
                    }
                }

                let Some(track) = read_track(path, mtime, size, now, &mut checked_covers) else {
                    failed += 1;
                    return Ok(());
                };

                let index_path = track.path.clone();
                let index_artist = track.artist.clone();
                let index_album = track.album.clone();
                let index_album_artist = track.album_artist.clone();
                upsert.execute(params![
                    track.path,
                    track.mtime,
                    track.size,
                    track.title,
                    track.artist,
                    track.album,
                    track.album_artist,
                    track.track_number,
                    track.disc_number,
                    track.duration,
                    track.added_at,
                ])?;
                index_track_artists(
                    &tx,
                    &index_path,
                    &index_artist,
                    &index_album,
                    &index_album_artist,
                )?;
                if previous.is_some() {
                    updated += 1;
                } else {
                    added += 1;
                }

                Ok(())
            };

            walk_audio_files(root, &mut |path| match process(path) {
                Ok(()) => {
                    index = index.saturating_add(1);
                    if index % 16 == 0 || index == total {
                        progress(index, total);
                    }
                    true
                }
                Err(err) => {
                    scan_error = Some(err);
                    false
                }
            });
        }

        if let Some(err) = scan_error {
            return Err(err);
        }

        // Make sure the progress bar reaches 100% even for an empty folder.
        progress(total, total);

        if self.scan_generation.load(Ordering::SeqCst) != generation {
            return Err(SCAN_CANCELLED.into());
        }

        let removed = tx.execute(
            "DELETE FROM tracks
             WHERE NOT EXISTS (
                 SELECT 1 FROM scan_seen WHERE scan_seen.path = tracks.path
             )",
            [],
        )? as u32;

        tx.execute(
            "INSERT INTO library_state (id, folder, scanned_at)
             VALUES (1, ?1, ?2)
             ON CONFLICT(id) DO UPDATE SET
                 folder = excluded.folder,
                 scanned_at = excluded.scanned_at",
            params![root.to_string_lossy(), now],
        )?;

        tx.commit()?;

        // The scan must stay within a small, fixed working set. Rows are
        // streamed to SQLite and only per-album cover bookkeeping remains in
        // memory, so this is a diagnostic for pathological libraries rather
        // than a value that is expected to be reached.
        let bookkeeping = checked_covers
            .len()
            .saturating_mul(std::mem::size_of::<PathBuf>() + 96);
        if bookkeeping > SCAN_MEMORY_LIMIT_BYTES {
            warn!(
                "Library scan bookkeeping used about {} MiB, above the {} MiB budget",
                bookkeeping / (1024 * 1024),
                SCAN_MEMORY_LIMIT_BYTES / (1024 * 1024)
            );
        }

        Ok(ScanStats {
            total,
            added,
            updated,
            removed,
            failed,
        })
    }
}

fn album_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AlbumRecord> {
    Ok(AlbumRecord {
        title: row.get(0)?,
        artist: row.get(1)?,
        cover_path: PathBuf::from(row.get::<_, String>(2)?),
        track_count: row.get(3)?,
        modified_at: row.get(4)?,
    })
}

/// Case-insensitive substring test that only accepts whole names, so "SZA"
/// is not found inside an unrelated word.
fn contains_artist_name(credit: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }

    let credit = credit.to_lowercase();
    let name = name.to_lowercase();
    let mut start = 0;
    while let Some(position) = credit[start..].find(&name) {
        let index = start + position;
        let end = index + name.len();
        let before = index == 0 || !credit[..index].chars().last().is_some_and(|c| c.is_alphanumeric());
        let after = end == credit.len() || !credit[end..].chars().next().is_some_and(|c| c.is_alphanumeric());
        if before && after {
            return true;
        }
        start = index + 1;
    }
    false
}

static CREDIT_SEPARATOR: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:\s+[(]?\s*\b(?:feat|ft|with|vs)(?:\.\s*|\s+|$)|[/&;,])").unwrap()
});

/// Names to associate with an album for one credit. The whole credit is kept
/// as well as each part, so both "King Gizzard & The Lizard Wizard" and
/// "Porter Robinson" from a collaboration resolve to the album.
fn credit_names(credit: &str) -> Vec<String> {
    let credit = credit.trim();
    if credit.is_empty() {
        return Vec::new();
    }

    let mut names = vec![credit.to_owned()];
    for part in CREDIT_SEPARATOR.split(credit) {
        let part = part
            .trim()
            .trim_matches(|c| matches!(c, '(' | ')' | '[' | ']'))
            .trim();
        if part.chars().count() < 2
            || names
                .iter()
                .any(|name: &String| name.eq_ignore_ascii_case(part))
        {
            continue;
        }
        names.push(part.to_owned());
    }
    names
}

fn add_unique_name(names: &mut Vec<String>, name: &str) {
    let name = name.trim();
    if !name.is_empty() && !names.iter().any(|existing| existing.eq_ignore_ascii_case(name)) {
        names.push(name.to_owned());
    }
}

/// Rebuilds the artist index for a single track.
fn index_track_artists(
    tx: &Transaction<'_>,
    path: &str,
    artist: &str,
    album: &str,
    album_artist: &str,
) -> LibraryResult<()> {
    tx.execute("DELETE FROM album_artists WHERE path = ?1", [path])?;

    let mut names = Vec::new();
    add_unique_name(&mut names, artist);
    add_unique_name(&mut names, album_artist);
    for credit in [artist, album_artist] {
        for name in credit_names(credit) {
            add_unique_name(&mut names, &name);
        }
    }

    let mut insert = tx.prepare_cached(
        "INSERT OR IGNORE INTO album_artists
             (path, artist_key, artist, album, album_artist)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for name in names {
        insert.execute(params![
            path,
            name.to_lowercase(),
            name,
            album,
            album_artist
        ])?;
    }
    Ok(())
}

/// Older databases have no artist index; fill it once from the tracks table.
fn build_artist_index_if_missing(conn: &mut Connection) -> LibraryResult<()> {
    let indexed: u64 = conn.query_row("SELECT COUNT(*) FROM album_artists", [], |row| row.get(0))?;
    if indexed > 0 {
        return Ok(());
    }

    let tracks: Vec<(String, String, String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT path, artist, album, album_artist FROM tracks",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    if tracks.is_empty() {
        return Ok(());
    }

    debug!("Building the album artist index for {} tracks", tracks.len());
    let tx = conn.transaction()?;
    for (path, artist, album, album_artist) in tracks {
        index_track_artists(&tx, &path, &artist, &album, &album_artist)?;
    }
    tx.commit()?;
    Ok(())
}

fn embedded_cover(tagged_file: &lofty::file::TaggedFile) -> Option<Vec<u8>> {
    let tag = tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag())?;
    if let Some(picture) = tag.get_picture_type(lofty::picture::PictureType::CoverFront) {
        return Some(picture.data().to_vec());
    }
    tag.pictures()
        .iter()
        .find(|picture| {
            matches!(
                picture.pic_type(),
                lofty::picture::PictureType::Other | lofty::picture::PictureType::BandLogo
            )
        })
        .map(|picture| picture.data().to_vec())
}

fn read_cover(path: &Path) -> Option<Vec<u8>> {
    let embedded = Probe::open(path)
        .ok()
        .and_then(|probe| probe.guess_file_type().ok())
        .and_then(|file_type| file_type.read().ok())
        .and_then(|tagged_file| embedded_cover(&tagged_file));

    embedded.or_else(|| read_external_cover(path))
}

fn read_external_cover(track_path: &Path) -> Option<Vec<u8>> {
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
        .and_then(|path| fs::read(path).ok())
}

fn write_cover_cache(path: &Path, data: &[u8]) {
    let Some(parent) = path.parent() else {
        return;
    };
    if let Err(err) = fs::create_dir_all(parent) {
        warn!("Unable to create cover cache directory {:?}: {err}", parent);
        return;
    }

    // Store compact thumbnails instead of the original embedded artwork. The
    // grid only needs 200px and the player at most 500px, and compact files
    // decode much faster when a playlist is loaded later.
    let Some((large, small)) = utils::cover_thumbnails_from_bytes(data) else {
        warn!("Unable to prepare cover thumbnails for {:?}", path);
        return;
    };

    // Write atomically: cover loading may run on a worker thread while the UI
    // is reading the previous file.
    if utils::write_thumbnail_file(path, &large) {
        utils::write_thumbnail_file(&utils::small_cover_cache_path(path), &small);
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

/// Walks the music folder depth-first and invokes `visit` for every audio
/// file, without collecting the paths into a list. `visit` returns `false` to
/// stop the walk (used for cancellation).
fn walk_audio_files<F>(directory: &Path, visit: &mut F) -> bool
where
    F: FnMut(&Path) -> bool,
{
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(err) => {
            warn!("Unable to read music folder {:?}: {err}", directory);
            return true;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(err) => {
                warn!("Unable to inspect {:?}: {err}", path);
                continue;
            }
        };

        if file_type.is_dir() {
            let hidden = path
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.starts_with('.'))
                .unwrap_or(false);
            if !hidden && !walk_audio_files(&path, visit) {
                return false;
            }
        } else if file_type.is_file() && is_audio_file(&path) && !visit(&path) {
            return false;
        }
    }

    true
}

fn is_audio_file(path: &Path) -> bool {
    const AUDIO_EXTENSIONS: &[&str] = &[
        "aac", "aif", "aiff", "alac", "ape", "dff", "dsf", "flac", "m4a", "m4b", "mka", "mp3",
        "mp4", "mpc", "oga", "ogg", "opus", "wav", "wave", "webm", "wma", "wv",
    ];

    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| AUDIO_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

fn clean_tag_value(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn read_track(
    path: &Path,
    mtime: i64,
    size: i64,
    added_at: i64,
    checked_covers: &mut HashSet<PathBuf>,
) -> Option<TrackRecord> {
    let tagged_file = Probe::open(path)
        .ok()
        .and_then(|probe| probe.guess_file_type().ok())
        .and_then(|file_type| file_type.read().ok());

    let Some(tagged_file) = tagged_file else {
        warn!("Unable to read tags from {:?}", path);
        return None;
    };

    let duration = AudioFile::properties(&tagged_file).duration().as_secs();
    let tag = tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag());

    let fallback_title = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_owned();
    let fallback_album = path
        .parent()
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_owned();

    let (title, artist, album, album_artist, track_number, disc_number) = match tag {
        Some(tag) => {
            let tag_artist = tag
                .artist()
                .map(|artist| artist.trim().to_owned())
                .filter(|artist| !artist.is_empty());
            let tag_album_artist = clean_tag_value(tag.get_string(ItemKey::AlbumArtist))
                .or_else(|| tag_artist.clone());

            (
                clean_tag_value(tag.title().as_deref()).unwrap_or_else(|| fallback_title.clone()),
                tag_artist
                    .or_else(|| tag_album_artist.clone())
                    .unwrap_or_default(),
                clean_tag_value(tag.album().as_deref()).unwrap_or(fallback_album),
                tag_album_artist.unwrap_or_default(),
                tag.track().unwrap_or(0),
                tag.disk().unwrap_or(0),
            )
        }
        None => (
            fallback_title,
            String::new(),
            fallback_album,
            String::new(),
            0,
            0,
        ),
    };

    let track = TrackRecord {
        path: path.to_string_lossy().into_owned(),
        mtime,
        size,
        title,
        artist,
        album,
        album_artist,
        track_number,
        disc_number,
        duration,
        added_at,
    };

    // Extract the cover only for the first track of each album. The tagged
    // file is already in memory here, so the audio file is never parsed twice.
    ensure_album_cover(
        path,
        &track.album,
        &track.album_artist,
        Some(&tagged_file),
        checked_covers,
    );

    Some(track)
}

/// Makes sure the cached 500px and 200px covers exist for an album. When
/// `tagged_file` is available the embedded picture is read from it; otherwise
/// the audio file is parsed again only if a cache entry is missing.
fn ensure_album_cover(
    path: &Path,
    album: &str,
    album_artist: &str,
    tagged_file: Option<&lofty::file::TaggedFile>,
    checked_covers: &mut HashSet<PathBuf>,
) {
    let cover_path = utils::album_cover_cache_path(album, album_artist, path);
    if !checked_covers.insert(cover_path.clone()) {
        return;
    }

    let small_cover_path = utils::small_cover_cache_path(&cover_path);
    let needs_write = !cover_path.is_file();
    let needs_compaction = !needs_write
        && fs::metadata(&cover_path)
            .map(|metadata| metadata.len() > utils::COVER_CACHE_MAX_BYTES)
            .unwrap_or(false);
    let needs_small = !small_cover_path.is_file();

    if needs_write {
        let data = match tagged_file {
            Some(tagged_file) => {
                embedded_cover(tagged_file).or_else(|| read_external_cover(path))
            }
            None => read_cover(path),
        };
        if let Some(data) = data {
            write_cover_cache(&cover_path, &data);
        }
    } else if needs_compaction || needs_small {
        // Both sizes can be rebuilt from the existing cache, without parsing
        // the audio file again.
        utils::ensure_cover_thumbnails(&cover_path, needs_compaction);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_wav(path: &Path, seconds: u32) {
        let sample_rate = 8_000u32;
        let data_len = sample_rate * seconds * 2;
        let mut file = fs::File::create(path).unwrap();

        file.write_all(b"RIFF").unwrap();
        file.write_all(&(36 + data_len).to_le_bytes()).unwrap();
        file.write_all(b"WAVEfmt ").unwrap();
        file.write_all(&16u32.to_le_bytes()).unwrap();
        file.write_all(&1u16.to_le_bytes()).unwrap();
        file.write_all(&1u16.to_le_bytes()).unwrap();
        file.write_all(&sample_rate.to_le_bytes()).unwrap();
        file.write_all(&(sample_rate * 2).to_le_bytes()).unwrap();
        file.write_all(&2u16.to_le_bytes()).unwrap();
        file.write_all(&16u16.to_le_bytes()).unwrap();
        file.write_all(b"data").unwrap();
        file.write_all(&data_len.to_le_bytes()).unwrap();
        file.write_all(&vec![0u8; data_len as usize]).unwrap();
    }

    fn temp_dir(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "amberol-library-test-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn scan_indexes_tracks_and_skips_unchanged_files() {
        let root = temp_dir("scan");
        let album = root.join("Paranoid");
        fs::create_dir_all(&album).unwrap();
        write_wav(&album.join("01.wav"), 1);
        write_wav(&album.join("02.wav"), 1);

        let library = Library::open_in_memory().unwrap();

        let stats = library.scan_folder(&root, |_, _| {}).unwrap();
        assert_eq!(stats.total, 2);
        assert_eq!(stats.added, 2);
        assert_eq!(stats.updated, 0);
        assert_eq!(library.album_count().unwrap(), 1);
        assert!(library.is_folder_indexed(&root));
        let snapshot = library.all_albums().unwrap();
        assert!(Arc::ptr_eq(&snapshot, &library.all_albums().unwrap()));

        let albums = library.recent_albums(10).unwrap();
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].title, "Paranoid");
        assert_eq!(albums[0].track_count, 2);

        let tracks = library.tracks_for_album("Paranoid", "").unwrap();
        assert_eq!(tracks.len(), 2);

        let all_tracks = library.tracks_for_albums(&albums).unwrap();
        assert_eq!(all_tracks.len(), 2);

        // A second scan must not re-read unchanged files.
        let stats = library.scan_folder(&root, |_, _| {}).unwrap();
        assert_eq!(stats.added, 0);
        assert_eq!(stats.updated, 0);
        assert_eq!(stats.failed, 0);
        assert!(library.is_folder_indexed(&root));

        // Removing a file removes it from the index.
        fs::remove_file(album.join("02.wav")).unwrap();
        let stats = library.scan_folder(&root, |_, _| {}).unwrap();
        assert_eq!(stats.removed, 1);
        assert!(!Arc::ptr_eq(&snapshot, &library.all_albums().unwrap()));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn random_albums_are_unique() {
        let root = temp_dir("random");
        for index in 0..5 {
            let album = root.join(format!("Album {index}"));
            fs::create_dir_all(&album).unwrap();
            write_wav(&album.join("01.wav"), 1);
        }

        let library = Library::open_in_memory().unwrap();
        library.scan_folder(&root, |_, _| {}).unwrap();
        assert_eq!(library.album_count().unwrap(), 5);

        let albums = library.random_albums(3).unwrap();
        assert_eq!(albums.len(), 3);

        let mut titles: Vec<_> = albums.iter().map(|album| album.title.clone()).collect();
        titles.sort();
        titles.dedup();
        assert_eq!(titles.len(), 3);

        let _ = fs::remove_dir_all(&root);
    }
}
