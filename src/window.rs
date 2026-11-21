// SPDX-FileCopyrightText: 2022  Emmanuele Bassi
// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    cell::{Cell, RefCell},
    collections::{HashSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    rc::Rc,
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};

use adw::prelude::{AdwDialogExt, AlertDialogExt, AlertDialogExtManual};
use adw::subclass::prelude::*;
use glib::{clone, closure_local};
use gtk::{gdk, gio, glib, prelude::*, CompositeTemplate};
use log::debug;

use crate::{
    album::Album,
    audio::{
        clear_thumbnail_cache,
        tag_writer::{self, CoverTagChange},
        forget_thumbnail, reclaim, AudioPlayer, CoverCache, RepeatMode, ReplayGainMode, Song,
    },
    config::APPLICATION_ID,
    drag_overlay::DragOverlay,
    i18n::{i18n, i18n_f, i18n_k, ni18n_f, ni18n_k},
    library::{Library, ScanEvent, SCAN_CANCELLED},
    library_view::LibraryView,
    playback_control::PlaybackControl,
    playlist_view::PlaylistView,
    queue_row::QueueRow,
    song_cover::SongCover,
    song_details::SongDetails,
    utils,
    volume_control::VolumeControl,
    waveform_view::WaveformView,
};

pub enum WindowMode {
    InitialView,
    MainView,
    LibraryView,
}

#[derive(Clone)]
pub enum LibraryPage {
    Recent {
        albums: Vec<Album>,
        album_count: u32,
    },
    Random {
        albums: Vec<Album>,
    },
    Favorites {
        albums: Vec<Album>,
    },
    Search {
        albums: Vec<Album>,
    },
    Artist {
        artist: String,
        albums: Vec<Album>,
        album_count: u32,
        track_count: u32,
    },
}

const ATTRIBUTE_HOST_PATH: &str = "xattr::document-portal.host-path";

/// Whether a newly selected cover replaces a single track or the whole album.
#[derive(Clone, Copy, Debug)]
enum CoverScope {
    Track,
    Album,
}

/// What to write to disk after the compression prompt for a large image.
enum CoverCompression {
    Compressed(Vec<u8>),
    Original,
}

/// Writes a custom cover atomically, so a failure never leaves a half-written
/// image that the player would try to decode on the next start.
fn write_cover_file(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let temporary = path.with_extension("tmp");
    fs::write(&temporary, data)?;
    fs::rename(&temporary, path).map_err(|err| {
        let _ = fs::remove_file(&temporary);
        err
    })
}

/// Creates the empty marker that hides the displayed artwork without
/// touching the audio file itself.
fn write_cover_marker(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    fs::write(path, b"")
}

fn remove_cover_file(path: &Path) {
    if let Err(err) = fs::remove_file(path) {
        if err.kind() != std::io::ErrorKind::NotFound {
            debug!("Unable to remove cover file {:?}: {err}", path);
        }
    }
}

/// The bytes and file extension of the artwork shown for `song`. Embedded
/// metadata wins; user overrides, external covers and cached thumbnails are
/// the fallback when a file has no picture in its tags.
fn extract_cover_bytes(audio: Option<PathBuf>, fallback: Vec<PathBuf>) -> Option<(Vec<u8>, String)> {
    if let Some(path) = audio {
        if let Some((data, mime)) = tag_writer::read_cover_image(&path) {
            let extension = mime.ext().unwrap_or("jpg").to_owned();
            return Some((data, extension));
        }
    }

    let path = fallback.into_iter().find(|path| path.is_file())?;
    let data = fs::read(&path).ok()?;
    let extension = tag_writer::mime_type_for_image(&data)
        .ext()
        .unwrap_or("jpg")
        .to_owned();
    Some((data, extension))
}

/// Paths needed to read the displayed artwork, gathered on the main thread
/// because GObjects are not `Send`.
fn cover_source_paths(song: &Song) -> (Option<PathBuf>, Vec<PathBuf>) {
    let audio = song.file().path();
    let mut fallback = Vec::new();
    if let Some((image, _)) = song.cover_override_paths() {
        fallback.push(image);
    }
    if let Some((image, _)) = song.album_cover_override_paths() {
        fallback.push(image);
    }
    if let Some(cache) = song.cover_cache() {
        fallback.push(cache);
    }
    (audio, fallback)
}

/// Reads an image's intrinsic dimensions from its header without decoding
/// the whole picture, and builds the compact description shown under the
/// cover context menu.
fn cover_info_label(data: &[u8]) -> Option<String> {
    let loader = gdk_pixbuf::PixbufLoader::new();
    let size = Rc::new(Cell::new((0, 0)));
    let captured = size.clone();
    loader.connect_size_prepared(move |_, width, height| captured.set((width, height)));
    let header = &data[..data.len().min(256 * 1024)];
    let _ = loader.write_bytes(&glib::Bytes::from(header));
    let (width, height) = size.get();
    if width <= 0 || height <= 0 {
        return None;
    }

    let format = if data.starts_with(&[0xff, 0xd8, 0xff]) {
        "JPEG"
    } else if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        "PNG"
    } else if data.starts_with(b"GIF8") {
        "GIF"
    } else {
        "..."
    };

    let bytes = data.len();
    let size = if bytes >= 1024 * 1024 {
        i18n_f("{} MB", &[&format!("{:.1}", bytes as f64 / (1024.0 * 1024.0))])
    } else {
        i18n_f("{} KB", &[&format!("{}", bytes / 1024)])
    };

    Some(i18n_f(
        "{}×{} · {} · {}",
        &[&width.to_string(), &height.to_string(), &size, format],
    ))
}

fn cover_menu_button(icon: &str, label: &str) -> gtk::Button {
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    content.append(&gtk::Image::from_icon_name(icon));

    let text = gtk::Label::new(Some(label));
    text.set_xalign(0.0);
    content.append(&text);

    let button = gtk::Button::new();
    button.set_child(Some(&content));
    button.add_css_class("flat");
    button.set_halign(gtk::Align::Fill);
    button.set_hexpand(true);
    button
}

/// State shared between `queue_songs_with_options` and its idle loader.
///
/// Files opened from the file manager may arrive as several `open` calls in
/// quick succession. Keeping the pending files in shared state lets those
/// calls extend the load already in progress instead of cancelling it.
#[derive(Clone)]
pub struct QueueLoadState {
    files: Rc<RefCell<VecDeque<gio::File>>>,
    total: Rc<Cell<u32>>,
}

mod imp {
    use glib::{ParamSpec, ParamSpecBoolean, ParamSpecEnum, Value};
    use once_cell::sync::Lazy;

    use super::*;

    #[derive(CompositeTemplate)]
    #[template(resource = "/io/bassi/Amberol/window.ui")]
    pub struct Window {
        // Template widgets
        #[template_child]
        pub drag_overlay: TemplateChild<DragOverlay>,
        #[template_child]
        pub toast_overlay: TemplateChild<adw::ToastOverlay>,
        #[template_child]
        pub main_stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub status_page: TemplateChild<adw::StatusPage>,
        #[template_child]
        pub song_cover: TemplateChild<SongCover>,
        #[template_child]
        pub song_details: TemplateChild<SongDetails>,
        #[template_child]
        pub waveform_view: TemplateChild<WaveformView>,
        #[template_child]
        pub elapsed_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub remaining_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub playback_control: TemplateChild<PlaybackControl>,
        #[template_child]
        pub split_view: TemplateChild<adw::OverlaySplitView>,
        #[template_child]
        pub playlist_view: TemplateChild<PlaylistView>,
        #[template_child]
        pub add_folder_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub restore_playlist_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub library_view: TemplateChild<LibraryView>,

        pub provider: gtk::CssProvider,
        pub settings: gio::Settings,

        pub playlist_shuffled: Cell<bool>,
        pub playlist_visible: Cell<bool>,
        pub playlist_selection: Cell<bool>,
        pub queue_rows: RefCell<Vec<(glib::WeakRef<QueueRow>, glib::WeakRef<gtk::ListItem>)>>,
        pub replaygain_mode: Cell<ReplayGainMode>,
        pub library_scan_running: Cell<bool>,
        pub library_scan_generation: Cell<u64>,
        pub library_processing_dialog: RefCell<Option<adw::Dialog>>,
        pub library_processing_progress: RefCell<Option<gtk::ProgressBar>>,
        pub library_processing_label: RefCell<Option<gtk::Label>>,
        pub library_random: Cell<bool>,
        pub library_favorites: Cell<bool>,
        pub library_albums: RefCell<Vec<Album>>,
        pub library_current_page: RefCell<Option<LibraryPage>>,
        pub library_page_before_search: RefCell<Option<LibraryPage>>,
        /// Artist page the current search was opened from. Albums by this
        /// artist are ranked first while the search stays open.
        pub search_artist: RefCell<Option<String>>,
        pub library_history: RefCell<Vec<LibraryPage>>,
        pub album_limit_submenu: RefCell<Option<gio::Menu>>,
        pub album_limit_popover: RefCell<Option<gtk::Popover>>,
        pub cover_menu: RefCell<Option<gtk::Popover>>,
        pub library_monitors: RefCell<Vec<gio::FileMonitor>>,
        pub library_rescan_source: RefCell<Option<glib::SourceId>>,
        pub library_play_signature: RefCell<Option<Vec<String>>>,
        pub queue_loading: Cell<bool>,
        pub queue_single_album: Cell<Option<bool>>,
        pub queue_load_generation: Cell<u64>,
        pub active_queue_load: RefCell<Option<QueueLoadState>>,
        pub restore_session_checked: Cell<bool>,
        pub pending_restore: RefCell<Option<utils::SessionState>>,
        pub added_songs_count: Cell<u32>,
        pub added_songs_source: RefCell<Option<glib::SourceId>>,
        pub added_songs_toast: RefCell<Option<glib::WeakRef<adw::Toast>>>,

        pub playlist_filtermodel: RefCell<Option<gio::ListModel>>,
        pub playlist_selection_anchor: Cell<Option<u32>>,

        pub notify_playing_id: RefCell<Option<glib::SignalHandlerId>>,
        pub notify_artist_id: RefCell<Option<glib::SignalHandlerId>>,
        pub notify_position_id: RefCell<Option<glib::SignalHandlerId>>,
        pub notify_song_id: RefCell<Option<glib::SignalHandlerId>>,
        pub notify_cover_id: RefCell<Option<glib::SignalHandlerId>>,
        pub notify_nsongs_id: RefCell<Option<glib::SignalHandlerId>>,
        pub notify_current_id: RefCell<Option<glib::SignalHandlerId>>,
        pub notify_peaks_id: RefCell<Option<glib::SignalHandlerId>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Window {
        const NAME: &'static str = "AmberolWindow";
        type Type = super::Window;
        type ParentType = adw::ApplicationWindow;

        fn class_init(klass: &mut Self::Class) {
            Self::bind_template(klass);

            klass.install_action("win.play", None, move |win, _, _| {
                debug!("Window::win.play()");
                if let Some(p) = win.player() {
                    p.toggle_play();
                }
            });
            klass.install_action("win.seek-backwards", None, move |win, _, _| {
                debug!("Window::win.seek-backwards");
                if let Some(p) = win.player() {
                    p.seek_backwards();
                }
            });
            klass.install_action("win.seek-forward", None, move |win, _, _| {
                debug!("Window::win.seek-forward");
                if let Some(p) = win.player() {
                    p.seek_forward();
                }
            });
            klass.install_action("win.previous", None, move |win, _, _| {
                debug!("Window::win.previous()");
                if let Some(p) = win.player() {
                    p.skip_previous();
                }
            });
            klass.install_action("win.next", None, move |win, _, _| {
                debug!("Window::win.next()");
                if let Some(p) = win.player() {
                    p.skip_next();
                }
            });
            klass.install_action("queue.repeat-mode", None, move |win, _, _| {
                debug!("Window::queue.repeat()");
                if let Some(p) = win.player() {
                    p.toggle_repeat_mode();
                }
            });
            klass.install_action_async("queue.add-song", None, |win, _, _| async move {
                debug!("Window::win.add-song()");
                let filters = gio::ListStore::new::<gtk::FileFilter>();
                let filter = gtk::FileFilter::new();
                gtk::FileFilter::set_name(&filter, Some(&i18n("Audio files")));
                filter.add_mime_type("audio/*");
                filters.append(&filter);

                let xdg_music = match glib::user_special_dir(glib::UserDirectory::Music) {
                    Some(p) => p,
                    None => glib::current_dir(),
                };

                let dialog = gtk::FileDialog::builder()
                    .accept_label(&i18n("_Add Song"))
                    .filters(&filters)
                    .initial_folder(&gio::File::for_path(&xdg_music))
                    .modal(true)
                    .title(&i18n("Open File"))
                    .build();

                if let Ok(files) = dialog.open_multiple_future(Some(&win)).await {
                    if files.n_items() == 0 {
                        win.add_toast(i18n("Unable to access files"));
                    } else {
                        win.add_files_to_queue(&files);
                    }
                }
            });
            klass.install_action_async("queue.add-folder", None, |win, _, _| async move {
                debug!("Window::win.add-folder()");
                let xdg_music = match glib::user_special_dir(glib::UserDirectory::Music) {
                    Some(p) => p,
                    None => glib::current_dir(),
                };

                let dialog = gtk::FileDialog::builder()
                    .accept_label(&i18n("_Add Folder"))
                    .initial_folder(&gio::File::for_path(&xdg_music))
                    .modal(true)
                    .title(&i18n("Open Folder"))
                    .build();

                if let Ok(files) = dialog.select_multiple_folders_future(Some(&win)).await {
                    if files.n_items() == 0 {
                        win.add_toast(i18n("Unable to access files"));
                    } else {
                        win.add_files_to_queue(&files);
                    }
                }
            });
            klass.install_action("queue.restore-playlist", None, move |win, _, _| {
                debug!("Window::queue.restore-playlist()");
                win.restore_playlist();
            });
            klass.install_action("win.copy", None, move |win, _, _| {
                debug!("Window::win.copy()");
                win.copy_song();
            });
            klass.install_action_async("win.open-directory", None, async move |win, _, _| {
                debug!("Window::win.open-directory()");
                win.open_song_directory().await
            });
            klass.install_action("queue.clear", None, move |win, _, _| {
                debug!("Window::queue.clear()");
                win.clear_queue();
            });
            klass.install_action("win.show-library", None, move |win, _, _| {
                debug!("Window::win.show-library()");
                win.show_library();
            });
            klass.install_action("win.show-now-playing", None, move |win, _, _| {
                debug!("Window::win.show-now-playing()");
                win.show_now_playing();
            });
            klass.install_action("win.library-back", None, move |win, _, _| {
                debug!("Window::win.library-back()");
                win.library_back();
            });
            klass.install_action("win.recent-albums", None, move |win, _, _| {
                debug!("Window::win.recent-albums()");
                win.refresh_library_view(false);
            });
            klass.install_action("win.random-albums", None, move |win, _, _| {
                debug!("Window::win.random-albums()");
                win.refresh_library_view(true);
            });
            klass.install_action("win.favorite-albums", None, move |win, _, _| {
                debug!("Window::win.favorite-albums()");
                win.refresh_favorites_view();
            });
            klass.install_action("win.toggle-favorite", None, move |win, _, _| {
                debug!("Window::win.toggle-favorite()");
                win.toggle_current_favorite();
            });
            klass.install_action("win.play-albums", None, move |win, _, _| {
                debug!("Window::win.play-albums()");
                win.play_library_albums();
            });
            klass.install_action("win.rescan-library", None, move |win, _, _| {
                debug!("Window::win.rescan-library()");
                win.start_library_scan(true);
            });
            klass.install_action_async("win.choose-library-folder", None, |win, _, _| async move {
                debug!("Window::win.choose-library-folder()");
                win.choose_library_folder().await;
            });
            klass.install_property_action("queue.toggle", "playlist-visible");
            klass.install_property_action("queue.shuffle", "playlist-shuffled");
            klass.install_property_action("queue.select", "playlist-selection");
            klass.install_property_action("win.replaygain", "replaygain-mode");

            klass.install_action(
                "win.skip-to",
                Some(glib::VariantTy::UINT32),
                move |win, _, param| {
                    if let Some(pos) = param.and_then(u32::from_variant) {
                        if let Some(player) = win.player() {
                            player.skip_to(pos);
                            player.play();
                        }
                    }
                },
            );
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            LibraryView::static_type();
            obj.init_template();
        }

        fn new() -> Self {
            Self {
                song_details: TemplateChild::default(),
                song_cover: TemplateChild::default(),
                split_view: TemplateChild::default(),
                toast_overlay: TemplateChild::default(),
                drag_overlay: TemplateChild::default(),
                playback_control: TemplateChild::default(),
                waveform_view: TemplateChild::default(),
                elapsed_label: TemplateChild::default(),
                remaining_label: TemplateChild::default(),
                main_stack: TemplateChild::default(),
                status_page: TemplateChild::default(),
                add_folder_button: TemplateChild::default(),
                restore_playlist_button: TemplateChild::default(),
                playlist_view: TemplateChild::default(),
                library_view: TemplateChild::default(),
                playlist_shuffled: Cell::new(false),
                playlist_visible: Cell::new(true),
                playlist_selection: Cell::new(false),
                queue_rows: RefCell::default(),
                playlist_filtermodel: RefCell::default(),
                playlist_selection_anchor: Cell::new(None),
                replaygain_mode: Cell::new(ReplayGainMode::default()),
                library_scan_running: Cell::new(false),
                library_scan_generation: Cell::new(0),
                library_processing_dialog: RefCell::default(),
                library_processing_progress: RefCell::default(),
                library_processing_label: RefCell::default(),
                library_random: Cell::new(false),
                library_favorites: Cell::new(false),
                library_albums: RefCell::default(),
                library_current_page: RefCell::default(),
                library_page_before_search: RefCell::default(),
                search_artist: RefCell::default(),
                library_history: RefCell::default(),
                album_limit_submenu: RefCell::default(),
                album_limit_popover: RefCell::default(),
                cover_menu: RefCell::default(),
                library_monitors: RefCell::default(),
                library_rescan_source: RefCell::default(),
                library_play_signature: RefCell::default(),
                queue_loading: Cell::new(false),
                queue_single_album: Cell::new(None),
                queue_load_generation: Cell::new(0),
                active_queue_load: RefCell::new(None),
                restore_session_checked: Cell::new(false),
                pending_restore: RefCell::default(),
                added_songs_count: Cell::new(0),
                added_songs_source: RefCell::default(),
                added_songs_toast: RefCell::default(),
                provider: gtk::CssProvider::new(),
                settings: utils::settings_manager(),
                notify_playing_id: RefCell::new(None),
                notify_artist_id: RefCell::new(None),
                notify_position_id: RefCell::new(None),
                notify_song_id: RefCell::new(None),
                notify_cover_id: RefCell::new(None),
                notify_nsongs_id: RefCell::new(None),
                notify_current_id: RefCell::new(None),
                notify_peaks_id: RefCell::new(None),
            }
        }
    }

    impl ObjectImpl for Window {
        fn constructed(&self) {
            self.parent_constructed();

            if APPLICATION_ID.ends_with("Devel") {
                self.obj().add_css_class("devel");
            }
        }

        fn properties() -> &'static [ParamSpec] {
            static PROPERTIES: Lazy<Vec<ParamSpec>> = Lazy::new(|| {
                vec![
                    ParamSpecBoolean::builder("playlist-shuffled").build(),
                    ParamSpecBoolean::builder("playlist-visible").build(),
                    ParamSpecBoolean::builder("playlist-selection").build(),
                    ParamSpecEnum::builder::<ReplayGainMode>("replaygain-mode").build(),
                ]
            });
            PROPERTIES.as_ref()
        }

        fn set_property(&self, _id: usize, value: &Value, pspec: &ParamSpec) {
            let obj = self.obj();
            match pspec.name() {
                "playlist-shuffled" => obj.set_playlist_shuffled(value.get::<bool>().unwrap()),
                "playlist-visible" => obj.set_playlist_visible(value.get::<bool>().unwrap()),
                "playlist-selection" => obj.set_playlist_selection(value.get::<bool>().unwrap()),
                "replaygain-mode" => obj.set_replaygain(value.get::<ReplayGainMode>().unwrap()),
                _ => unimplemented!(),
            }
        }

        fn property(&self, _id: usize, pspec: &ParamSpec) -> Value {
            let obj = self.obj();
            match pspec.name() {
                "playlist-shuffled" => obj.playlist_shuffled().to_value(),
                "playlist-visible" => obj.playlist_visible().to_value(),
                "playlist-selection" => obj.playlist_selection().to_value(),
                "replaygain-mode" => obj.replaygain().to_value(),
                _ => unimplemented!(),
            }
        }
    }

    impl WidgetImpl for Window {}
    impl WindowImpl for Window {}
    impl ApplicationWindowImpl for Window {}
    impl AdwApplicationWindowImpl for Window {}
}

glib::wrapper! {
    pub struct Window(ObjectSubclass<imp::Window>)
        @extends gtk::Widget, gtk::Window, gtk::ApplicationWindow, adw::ApplicationWindow,
        @implements gio::ActionGroup, gio::ActionMap, gtk::Accessible, gtk::Buildable,
                    gtk::ConstraintTarget, gtk::Native, gtk::Root, gtk::ShortcutManager;
}

/// Returns the first and last position of the album containing `position`.
fn model_has_single_album(model: &gio::ListModel) -> bool {
    let mut albums = (0..model.n_items()).filter_map(|position| {
        model
            .item(position)
            .and_then(|item| item.downcast::<Song>().ok())
            .map(|song| song.album())
    });

    let Some(first_album) = albums.next() else {
        return true;
    };

    albums.all(|album| album == first_album)
}

fn update_queue_row_cover(
    row: &QueueRow,
    list_item: &gtk::ListItem,
    model: &gio::ListModel,
    single_album: bool,
) {
    // A playlist with a single album does not need repeated artwork. With
    // multiple albums, only the first track of each album gets a cover;
    // grouping by album alone keeps featured-artist tracks attached to the
    // same cover.
    let position = list_item.position();
    let show_cover = !single_album
        && (position == gtk::INVALID_LIST_POSITION
            || position == 0
            || match (
                list_item
                    .item()
                    .and_then(|item| item.downcast::<Song>().ok()),
                model
                    .item(position - 1)
                    .and_then(|item| item.downcast::<Song>().ok()),
            ) {
                (Some(current), Some(previous)) => current.album() != previous.album(),
                _ => true,
            });

    row.set_cover_visible(show_cover);
}

impl Window {
    pub fn new<P: IsA<gtk::Application>>(application: &P) -> Self {
        let win = glib::Object::builder::<Window>()
            .property("application", application)
            .build();

        win.setup_waveform();
        win.setup_actions();
        win.setup_playlist();
        win.setup_empty_click_deselection();
        win.setup_cover_context_menu();
        win.setup_library();
        win.setup_drop_target();
        win.setup_provider();
        win.bind_state();
        win.bind_queue();
        win.setup_session_autosave();
        win.connect_signals();
        win.restore_window_state();
        win.set_initial_state();

        win
    }

    pub fn save_session(&self) {
        if let Some(player) = self.player() {
            if player.queue().is_empty() {
                utils::clear_cached_playlist();
            } else {
                utils::store_playlist(&player);
            }
        }
    }

    fn player(&self) -> Option<Rc<AudioPlayer>> {
        if let Some(app) = self.application() {
            let player = app
                .downcast::<crate::application::Application>()
                .unwrap()
                .player();
            return Some(player);
        }

        None
    }

    fn library(&self) -> Option<Arc<Library>> {
        if let Some(app) = self.application() {
            return Some(
                app.downcast::<crate::application::Application>()
                    .unwrap()
                    .library(),
            );
        }

        None
    }

    fn setup_library(&self) {
        self.imp().library_view.connect_closure(
            "album-activated",
            false,
            closure_local!(
                #[weak(rename_to = win)]
                self,
                move |_view: LibraryView, album: Album| {
                    win.play_album(&album);
                }
            ),
        );

        self.imp().library_view.connect_closure(
            "mode-changed",
            false,
            closure_local!(
                #[weak(rename_to = win)]
                self,
                move |_view: LibraryView, random: bool| {
                    win.refresh_library_view(random);
                }
            ),
        );

        self.imp().library_view.connect_closure(
            "load-more-albums",
            false,
            closure_local!(
                #[weak(rename_to = win)]
                self,
                move |_view: LibraryView| {
                    win.load_more_recent_albums();
                }
            ),
        );

        self.imp().library_view.connect_closure(
            "favorites-mode-changed",
            false,
            closure_local!(
                #[weak(rename_to = win)]
                self,
                move |_view: LibraryView| {
                    win.refresh_favorites_view();
                }
            ),
        );

        self.imp().library_view.connect_closure(
            "search-changed",
            false,
            closure_local!(
                #[weak(rename_to = win)]
                self,
                move |_view: LibraryView, query: String| {
                    win.search_library(&query);
                }
            ),
        );

        self.imp().library_view.connect_closure(
            "search-toggled",
            false,
            closure_local!(
                #[weak(rename_to = win)]
                self,
                move |_view: LibraryView, active: bool| {
                    if active {
                        // Remember where the search started. Searching from
                        // an artist page must keep that artist's albums at
                        // the top of the results.
                        let artist = match win.imp().library_current_page.borrow().as_ref() {
                            Some(LibraryPage::Artist { artist, .. }) => Some(artist.clone()),
                            _ => None,
                        };
                        win.imp().search_artist.replace(artist);
                    } else {
                        win.imp().search_artist.replace(None);
                        win.restore_after_search();
                    }
                }
            ),
        );

        self.imp().library_view.connect_closure(
            "artist-activated",
            false,
            closure_local!(
                #[weak(rename_to = win)]
                self,
                move |_view: LibraryView, artist: String| {
                    win.show_artist(&artist);
                }
            ),
        );

        self.imp().library_view.connect_closure(
            "albums-context-action",
            false,
            closure_local!(
                #[weak(rename_to = win)]
                self,
                move |_view: LibraryView, albums: gio::ListStore, action: String| {
                    let albums: Vec<Album> = (0..albums.n_items())
                        .filter_map(|position| {
                            albums
                                .item(position)
                                .and_then(|item| item.downcast::<Album>().ok())
                        })
                        .collect();
                    win.handle_albums_context_action(&albums, &action);
                }
            ),
        );

        self.imp().song_details.connect_closure(
            "artist-activated",
            false,
            closure_local!(
                #[weak(rename_to = win)]
                self,
                move |_details: crate::song_details::SongDetails, artist: String| {
                    win.show_artist(&artist);
                }
            ),
        );

        // Loading the library is deferred until the window is up. On a normal
        // start the player is the first page, so the database and the album
        // grid should not delay the first frame.
        glib::idle_add_local_once(clone!(
            #[weak(rename_to = win)]
            self,
            move || {
                if win.imp().main_stack.visible_child_name().as_deref()
                    == Some("library-view")
                    && win.imp().library_albums.borrow().is_empty()
                {
                    if let Some(albums) = win.load_library_albums(false, false) {
                        win.imp().library_view.set_albums(albums);
                    }
                }
            }
        ));

        // A scan reads every file in the music folder. Give the window and the
        // restored playlist a chance to load first.
        glib::timeout_add_local_once(
            Duration::from_secs(2),
            clone!(
                #[weak(rename_to = win)]
                self,
                move || win.start_library_scan(false)
            ),
        );
    }

    fn queue_single_album(&self, model: &gio::ListModel) -> bool {
        if let Some(single_album) = self.imp().queue_single_album.get() {
            return single_album;
        }

        let single_album = model_has_single_album(model);
        self.imp().queue_single_album.set(Some(single_album));
        single_album
    }

    fn refresh_queue_rows(&self, model: &gio::ListModel) {
        let imp = self.imp();
        let single_album = self.queue_single_album(model);
        let mut rows = imp.queue_rows.borrow_mut();
        rows.retain(|(row, list_item)| {
            let (Some(row), Some(list_item)) = (row.upgrade(), list_item.upgrade()) else {
                return false;
            };

            if let Some(child) = list_item
                .child()
                .and_then(|child| child.downcast::<QueueRow>().ok())
            {
                update_queue_row_cover(&child, &list_item, model, single_album);
            }
            let _ = row;
            true
        });
    }

    fn library_folder(&self) -> Option<PathBuf> {
        let stored = self.imp().settings.string("library-folder");
        if !stored.is_empty() {
            return Some(PathBuf::from(stored));
        }

        glib::user_special_dir(glib::UserDirectory::Music)
    }

    fn show_library(&self) {
        // The grid is kept up to date by the scan and file monitors. Rebuilding
        // it here made switching from the player back to the library needlessly
        // reload every album and cover.
        if self.imp().library_albums.borrow().is_empty() {
            self.refresh_current_library_tab();
        }
        self.start_library_scan(false);
        self.switch_mode(WindowMode::LibraryView);
    }

    fn show_now_playing(&self) {
        // The player page is the "Now Playing" destination even before the
        // first song has been queued. Falling back to the initial view here
        // made the menu item look broken right after a library scan.
        if self.player().is_some() {
            self.switch_mode(WindowMode::MainView);
        } else {
            self.switch_mode(WindowMode::InitialView);
        }
    }

    fn album_limit(&self) -> u32 {
        self.imp().settings.int("album-limit").clamp(1, 10_000) as u32
    }

    fn custom_album_limit(&self) -> u32 {
        self.imp()
            .settings
            .int("album-limit-custom")
            .clamp(1, 10_000) as u32
    }

    fn prepare_library_page(&self, page: LibraryPage) -> Vec<Album> {
        let imp = self.imp();
        if !matches!(page, LibraryPage::Search { .. }) {
            imp.library_view.cancel_search();
            imp.search_artist.replace(None);
        }
        let albums = match &page {
            LibraryPage::Recent { albums, .. } => albums.clone(),
            LibraryPage::Random { albums } => albums.clone(),
            LibraryPage::Favorites { albums } => albums.clone(),
            LibraryPage::Search { albums } => albums.clone(),
            LibraryPage::Artist { albums, .. } => albums.clone(),
        };

        match &page {
            LibraryPage::Recent { album_count, .. } => {
                imp.library_random.set(false);
                imp.library_favorites.set(false);
                imp.library_view.set_title_mode(false);
                imp.library_view.set_album_count(*album_count);
            }
            LibraryPage::Random { albums } => {
                imp.library_random.set(true);
                imp.library_favorites.set(false);
                imp.library_view.set_title_mode(true);
                imp.library_view.set_album_count(albums.len() as u32);
            }
            LibraryPage::Favorites { albums } => {
                imp.library_random.set(false);
                imp.library_favorites.set(true);
                imp.library_view.set_favorites_mode(albums.len() as u32);
            }
            LibraryPage::Search { albums } => {
                imp.library_random.set(false);
                imp.library_favorites.set(false);
                imp.library_view.set_search_header(albums.len() as u32);
            }
            LibraryPage::Artist {
                artist,
                album_count,
                track_count,
                ..
            } => {
                imp.library_random.set(false);
                imp.library_favorites.set(false);
                imp.library_view
                    .set_artist_header(artist, *album_count, *track_count);
            }
        }

        if let Some(library) = self.library() {
            let favorites = library.favorite_album_keys().unwrap_or_default();
            for album in &albums {
                let record = album.record();
                album.set_favorite(favorites.contains(&(record.title, record.artist)));
            }
        }

        imp.library_albums.replace(albums.clone());
        imp.library_current_page.replace(Some(page));
        albums
    }

    fn apply_library_page(&self, page: LibraryPage, animated: bool, show: bool) {
        let albums = self.prepare_library_page(page);
        let imp = self.imp();
        if animated {
            imp.library_view.set_albums_animated(albums);
        } else {
            imp.library_view.set_albums(albums);
        }

        if show {
            self.switch_mode(WindowMode::LibraryView);
        }
    }

    /// Opens the page of the artist whose link was activated. The links are
    /// built only for artists found in the library, so the name carried by the
    /// link is already the final destination.
    fn show_artist(&self, artist: &str) {
        let Some(library) = self.library() else {
            return;
        };

        // Artist labels carry a link with the exact name, but other widgets
        // (album tiles) may still emit a whole credit. Handle those as well.
        let known = library
            .albums_for_artist(artist)
            .map(|albums| !albums.is_empty())
            .unwrap_or(false);
        if known {
            self.open_artist_page(&library, artist);
            return;
        }
        if let Ok(names) = library.artists_in_credit(artist) {
            if let Some(name) = names.first() {
                self.open_artist_page(&library, name);
                return;
            }
        }
        self.open_artist_page(&library, artist);
    }

    /// Builds the artist label for the current song, turning every artist that
    /// exists in the library into a separate link.
    fn update_artist_label(&self) {
        let Some(player) = self.player() else {
            return;
        };
        let credit = player.state().artist().unwrap_or_default();
        let markup = self.artist_markup(&credit);
        self.imp().song_details.get().set_artist_markup(&markup);
    }

    /// Same as [`Self::update_artist_label`], for a queue row.
    fn update_queue_row_artist(&self, row: &QueueRow, credit: &str) {
        let markup = self.artist_markup(credit);
        row.set_artist_markup(credit, &markup);
    }

    fn artist_markup(&self, credit: &str) -> String {
        let names = self
            .library()
            .and_then(|library| library.artists_in_credit(credit).ok())
            .unwrap_or_default();
        utils::artist_links_markup(credit, &names)
    }

    fn open_artist_page(&self, library: &Library, artist: &str) {
        let albums = match library.albums_for_artist(artist) {
            Ok(albums) if !albums.is_empty() => albums,
            Ok(_) => {
                self.add_toast(i18n("No albums found for this artist"));
                return;
            }
            Err(err) => {
                debug!("Unable to load albums for artist '{artist}': {err}");
                self.add_toast(i18n("Unable to load the music library"));
                return;
            }
        };

        let album_count = albums.len() as u32;
        let track_count = library
            .artist_stats(artist)
            .map(|(_, tracks)| tracks)
            .unwrap_or_else(|_| albums.iter().map(|album| album.track_count).sum());

        // Keep the page the user came from so the back button can restore
        // exactly the same albums. This is especially important for the
        // random tab: returning must not draw a new random selection.
        if self.imp().main_stack.visible_child_name().as_deref() == Some("library-view") {
            if let Some(page) = self.imp().library_current_page.borrow().clone() {
                self.imp().library_history.borrow_mut().push(page);
            }
        }

        let albums: Vec<Album> = albums.into_iter().map(Album::new).collect();
        self.apply_library_page(
            LibraryPage::Artist {
                artist: artist.to_owned(),
                albums,
                album_count,
                track_count,
            },
            true,
            true,
        );
    }

    fn library_back(&self) {
        if self.imp().library_view.is_artist_mode() {
            if let Some(page) = self.imp().library_history.borrow_mut().pop() {
                self.apply_library_page(page, true, true);
                return;
            }
        }

        self.show_now_playing();
    }

    fn refresh_library_view(&self, random: bool) {
        self.refresh_library_view_with(random, true);
    }

    fn refresh_library_view_with(&self, random: bool, switch_view: bool) {
        self.imp().library_view.refresh_albums_animated(clone!(
            #[weak(rename_to = win)]
            self,
            #[upgrade_or]
            None,
            move || win.load_library_albums(random, switch_view)
        ));
    }

    fn load_library_albums(&self, random: bool, switch_view: bool) -> Option<Vec<Album>> {
        let Some(library) = self.library() else {
            return None;
        };

        let albums = if random {
            library.random_albums(self.album_limit())
        } else {
            library.recent_albums(self.album_limit())
        };

        match albums {
            Ok(albums) => {
                let album_count = library.album_count().unwrap_or(0);
                let has_albums = !albums.is_empty();
                let albums: Vec<Album> = albums.into_iter().map(Album::new).collect();
                let page = if random {
                    LibraryPage::Random { albums }
                } else {
                    LibraryPage::Recent {
                        albums,
                        album_count,
                    }
                };

                // Pressing a tab is an explicit navigation action, so any
                // artist-page history no longer applies.
                self.imp().library_history.borrow_mut().clear();

                let already_library =
                    self.imp().main_stack.visible_child_name().as_deref() == Some("library-view");
                let albums = self.prepare_library_page(page);
                if switch_view && (has_albums || already_library) {
                    self.switch_mode(WindowMode::LibraryView);
                }
                Some(albums)
            }
            Err(err) => {
                debug!("Unable to load albums from the library: {err}");
                self.add_toast(i18n("Unable to load the music library"));
                None
            }
        }
    }

    /// Loads one more page of recent albums after the grid is scrolled to the
    /// bottom. The page size follows the album limit from the settings, and
    /// albums already shown are skipped so nothing is duplicated.
    fn load_more_recent_albums(&self) {
        let imp = self.imp();

        let (offset, total) = match imp.library_current_page.borrow().as_ref() {
            Some(LibraryPage::Recent { albums, album_count }) => {
                (albums.len() as u32, *album_count)
            }
            _ => {
                imp.library_view.finish_load_more();
                return;
            }
        };

        if offset >= total {
            imp.library_view.finish_load_more();
            return;
        }

        let Some(library) = self.library() else {
            imp.library_view.finish_load_more();
            return;
        };

        let records = match library.recent_albums_from(self.album_limit(), offset) {
            Ok(records) if !records.is_empty() => records,
            _ => {
                imp.library_view.finish_load_more();
                return;
            }
        };

        let favorites = library.favorite_album_keys().unwrap_or_default();
        let mut new_albums = Vec::with_capacity(records.len());
        for record in records {
            let album = Album::new(record);
            let key = {
                let record = album.record();
                (record.title, record.artist)
            };
            album.set_favorite(favorites.contains(&key));
            new_albums.push(album);
        }

        if let Some(LibraryPage::Recent { albums, .. }) =
            imp.library_current_page.borrow_mut().as_mut()
        {
            albums.extend(new_albums.iter().cloned());
        }
        imp.library_albums.borrow_mut().extend(new_albums.iter().cloned());
        imp.library_view.append_albums(new_albums);
    }

    fn search_library(&self, query: &str) {
        let query = query.trim();
        if query.is_empty() {
            return;
        }
        let Some(library) = self.library() else {
            return;
        };

        if !matches!(
            *self.imp().library_current_page.borrow(),
            Some(LibraryPage::Search { .. })
        ) {
            let previous = self.imp().library_current_page.borrow().clone();
            self.imp().library_page_before_search.replace(previous);
        }

        let priority_artist = self.imp().search_artist.borrow().clone();
        match library.search_albums(query, 300, priority_artist.as_deref()) {
            Ok(albums) => {
                let albums: Vec<Album> = albums.into_iter().map(Album::new).collect();
                let has_albums = !albums.is_empty();
                let already_library =
                    self.imp().main_stack.visible_child_name().as_deref() == Some("library-view");
                self.apply_library_page(
                    LibraryPage::Search { albums },
                    false,
                    has_albums || already_library,
                );
            }
            Err(err) => {
                debug!("Unable to search the library: {err}");
                self.add_toast(i18n("Unable to load the music library"));
            }
        }
    }

    fn restore_after_search(&self) {
        if let Some(page) = self.imp().library_page_before_search.borrow_mut().take() {
            self.apply_library_page(page, true, true);
        }
    }

    fn refresh_favorites_view(&self) {
        let Some(library) = self.library() else {
            return;
        };

        match library.favorite_albums() {
            Ok(albums) => {
                let has_albums = !albums.is_empty();
                let albums: Vec<Album> = albums.into_iter().map(Album::new).collect();
                self.imp().library_history.borrow_mut().clear();
                let already_library =
                    self.imp().main_stack.visible_child_name().as_deref() == Some("library-view");
                self.apply_library_page(
                    LibraryPage::Favorites { albums },
                    true,
                    has_albums || already_library,
                );
            }
            Err(err) => {
                debug!("Unable to load favorite albums: {err}");
                self.add_toast(i18n("Unable to load the music library"));
            }
        }
    }

    fn refresh_current_library_tab(&self) {
        // Do not keep the page borrowed while refreshing: applying the new
        // page needs to replace the same RefCell.
        let page = self.imp().library_current_page.borrow().clone();
        match page {
            Some(LibraryPage::Random { .. }) => self.refresh_library_view(true),
            Some(LibraryPage::Favorites { .. }) => self.refresh_favorites_view(),
            Some(LibraryPage::Artist { .. }) | Some(LibraryPage::Search { .. }) => {}
            _ => self.refresh_library_view(false),
        }
    }

    fn reload_library_page_after_delete(&self, deleted: &[Album]) {
        let Some(library) = self.library() else {
            return;
        };
        let Some(page) = self.imp().library_current_page.borrow().clone() else {
            return;
        };

        let same_album = |candidate: &Album| {
            let candidate = candidate.record();
            deleted.iter().any(|album| {
                let album = album.record();
                candidate.title == album.title && candidate.artist == album.artist
            })
        };

        match page {
            LibraryPage::Recent { .. } => {
                if let Ok(albums) = library.recent_albums(self.album_limit()) {
                    let album_count = library.album_count().unwrap_or(0);
                    let albums = albums.into_iter().map(Album::new).collect();
                    self.apply_library_page(
                        LibraryPage::Recent {
                            albums,
                            album_count,
                        },
                        true,
                        true,
                    );
                }
            }
            LibraryPage::Random { mut albums } => {
                albums.retain(|album| !same_album(album));
                self.apply_library_page(LibraryPage::Random { albums }, true, true);
            }
            LibraryPage::Favorites { mut albums } => {
                albums.retain(|album| !same_album(album));
                self.apply_library_page(LibraryPage::Favorites { albums }, true, true);
            }
            LibraryPage::Search { mut albums } => {
                albums.retain(|album| !same_album(album));
                self.apply_library_page(LibraryPage::Search { albums }, true, true);
            }
            LibraryPage::Artist { artist, .. } => {
                let albums = library.albums_for_artist(&artist).unwrap_or_default();
                if albums.is_empty() {
                    if let Some(previous) = self.imp().library_history.borrow_mut().pop() {
                        self.apply_library_page(previous, true, true);
                    } else {
                        self.show_now_playing();
                    }
                } else {
                    let album_count = albums.len() as u32;
                    let track_count = library
                        .artist_stats(&artist)
                        .map(|(_, tracks)| tracks)
                        .unwrap_or(0);
                    let albums = albums.into_iter().map(Album::new).collect();
                    self.apply_library_page(
                        LibraryPage::Artist {
                            artist,
                            albums,
                            album_count,
                            track_count,
                        },
                        true,
                        true,
                    );
                }
            }
        }
    }

    fn start_library_scan(&self, force: bool) {
        if self.imp().library_scan_running.replace(true) {
            return;
        }

        let Some(library) = self.library() else {
            self.imp().library_scan_running.set(false);
            self.finish_library_processing();
            return;
        };

        let Some(folder) = self.library_folder().filter(|folder| folder.is_dir()) else {
            self.imp().library_scan_running.set(false);
            self.finish_library_processing();
            self.switch_mode(WindowMode::LibraryView);
            self.add_toast(i18n("Choose a folder for your music library"));
            return;
        };

        if self.imp().settings.string("library-folder").is_empty() {
            self.imp()
                .settings
                .set_string("library-folder", &folder.to_string_lossy())
                .expect("Unable to store library-folder setting");
        }

        self.setup_library_monitors(&folder);

        // On a normal start the persistent index is already complete. Only
        // touch the files again when the user picks a folder or a file monitor
        // reports that the folder has changed.
        if !force && library.is_folder_indexed(&folder) {
            self.imp().library_scan_running.set(false);
            self.imp().library_view.set_scanning(false);
            self.finish_library_processing();
            return;
        }

        let scan_generation = self
            .imp()
            .library_scan_generation
            .get()
            .wrapping_add(1);
        self.imp()
            .library_scan_generation
            .set(scan_generation);

        let view = self.imp().library_view.get();
        view.set_scanning(true);

        let (sender, receiver) = async_channel::unbounded::<ScanEvent>();
        let root = folder.clone();
        std::thread::spawn(move || {
            let result = library.scan_folder(&root, |done, total| {
                let _ = sender.send_blocking(ScanEvent::Progress { done, total });
            });

            let event = match result {
                Ok(stats) => ScanEvent::Finished(stats),
                Err(err) => ScanEvent::Failed(err.to_string()),
            };
            let _ = sender.send_blocking(event);
        });

        glib::MainContext::default().spawn_local(clone!(
            #[weak(rename_to = win)]
            self,
            async move {
                use futures::prelude::*;

                let mut receiver = std::pin::pin!(receiver);
                while let Some(event) = receiver.next().await {
                    if win.imp().library_scan_generation.get() != scan_generation {
                        // The database was reset or a newer scan superseded
                        // this one; its queued progress and completion events
                        // must not touch the UI.
                        continue;
                    }

                    match event {
                        ScanEvent::Progress { done, total } => {
                            win.update_library_processing(done, total);
                        }
                        ScanEvent::Finished(stats) => {
                            win.imp().library_scan_running.set(false);
                            win.imp().library_view.set_scanning(false);
                            Album::clear_cover_cache();
                            if let Some(folder) = win.library_folder() {
                                win.setup_library_monitors(&folder);
                            }
                            win.refresh_current_library_tab();
                            win.finish_library_processing();
                            win.switch_mode(WindowMode::LibraryView);

                            let changed = stats.added + stats.updated;
                            if changed > 0 {
                                let changed_str = changed.to_string();
                                win.add_toast(ni18n_f(
                                    "Indexed {} song",
                                    "Indexed {} songs",
                                    changed,
                                    &[&changed_str],
                                ));
                            }
                            if stats.failed > 0 {
                                debug!("{} files could not be indexed", stats.failed);
                            }
                        }
                        ScanEvent::Failed(err) => {
                            win.imp().library_scan_running.set(false);
                            win.imp().library_view.set_scanning(false);
                            win.finish_library_processing();
                            if err != SCAN_CANCELLED {
                                debug!("Unable to scan the music library: {err}");
                                win.add_toast(i18n("Unable to scan the music folder"));
                            }
                        }
                    }
                }
            }
        ));
    }

    fn setup_library_monitors(&self, root: &std::path::Path) {
        let mut monitors = Vec::new();
        let mut pending = vec![root.to_path_buf()];

        while let Some(directory) = pending.pop() {
            if let Ok(entries) = fs::read_dir(&directory) {
                for entry in entries.flatten() {
                    if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                        let path = entry.path();
                        let hidden = path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .map(|name| name.starts_with('.'))
                            .unwrap_or(false);
                        if !hidden {
                            pending.push(path);
                        }
                    }
                }
            }

            if let Ok(monitor) = gio::File::for_path(&directory)
                .monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
            {
                monitor.connect_changed(clone!(
                    #[weak(rename_to = win)]
                    self,
                    move |_, _file, _other_file, event| {
                        if matches!(
                            event,
                            gio::FileMonitorEvent::Created
                                | gio::FileMonitorEvent::Deleted
                                | gio::FileMonitorEvent::Moved
                                | gio::FileMonitorEvent::Renamed
                                | gio::FileMonitorEvent::MovedIn
                                | gio::FileMonitorEvent::MovedOut
                                | gio::FileMonitorEvent::Changed
                        ) {
                            win.schedule_library_rescan();
                        }
                    }
                ));
                monitors.push(monitor);
            }
        }

        self.imp().library_monitors.replace(monitors);
    }

    fn schedule_library_rescan(&self) {
        if let Some(source) = self.imp().library_rescan_source.take() {
            source.remove();
        }

        let source = glib::timeout_add_seconds_local_once(
            2,
            clone!(
                #[weak(rename_to = win)]
                self,
                move || {
                    win.imp().library_rescan_source.replace(None);
                    if win.imp().library_scan_running.get() {
                        win.schedule_library_rescan();
                    } else {
                        win.start_library_scan(true);
                    }
                }
            ),
        );

        self.imp().library_rescan_source.replace(Some(source));
    }

    async fn choose_library_folder(&self) {
        let initial_folder = self
            .library_folder()
            .filter(|folder| folder.is_dir())
            .map(|folder| gio::File::for_path(folder))
            .unwrap_or_else(|| gio::File::for_path(glib::home_dir()));

        let dialog = gtk::FileDialog::builder()
            .accept_label(&i18n("_Choose Folder"))
            .initial_folder(&initial_folder)
            .modal(true)
            .title(&i18n("Choose Music Folder"))
            .build();

        if let Ok(folder) = dialog.select_folder_future(Some(self)).await {
            let Some(path) = folder.path() else {
                self.add_toast(i18n("Unable to access the selected folder"));
                return;
            };

            // A folder that is not part of the current index needs a full
            // first scan. While it runs, the user gets a modal, non-dismissible
            // progress dialog instead of an empty library grid.
            let needs_processing = self
                .library()
                .map(|library| !library.is_folder_indexed(&path))
                .unwrap_or(true);

            self.imp()
                .settings
                .set_string("library-folder", &path.to_string_lossy())
                .expect("Unable to store library-folder setting");
            self.imp().library_monitors.borrow_mut().clear();
            self.setup_library_monitors(&path);

            if needs_processing {
                self.show_library_processing();
            }

            self.switch_mode(WindowMode::LibraryView);
            self.refresh_library_view(false);

            if self.imp().library_scan_running.get() {
                // A scan of the previous folder is still running. Invalidate
                // it and start the scan for the folder that was just chosen,
                // so the processing dialog follows the new scan.
                self.imp()
                    .library_scan_generation
                    .set(self.imp().library_scan_generation.get().wrapping_add(1));
                self.imp().library_scan_running.set(false);
            }
            self.start_library_scan(true);
        }
    }

    /// Shows a modal progress dialog for the first scan of a folder. The
    /// dialog has no close button and ignores Escape, so it cannot be
    /// dismissed while the library is being prepared.
    fn show_library_processing(&self) {
        let imp = self.imp();
        if imp.library_processing_dialog.borrow().is_some() {
            return;
        }

        let spinner = gtk::Spinner::new();
        spinner.set_size_request(32, 32);
        spinner.start();

        let title = gtk::Label::new(Some(&i18n("Preparing Your Music Library")));
        title.add_css_class("title-2");

        let description = gtk::Label::new(Some(&i18n(
            "Amberol is scanning your music folder and preparing cover artwork. This only happens the first time.",
        )));
        description.set_wrap(true);
        description.set_justify(gtk::Justification::Center);
        description.add_css_class("dim-label");

        let progress = gtk::ProgressBar::new();
        progress.set_show_text(false);
        progress.set_hexpand(true);

        let counter = gtk::Label::new(Some(&i18n("Scanning…")));
        counter.add_css_class("dim-label");

        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_margin_top(28);
        content.set_margin_bottom(28);
        content.set_margin_start(28);
        content.set_margin_end(28);
        content.append(&spinner);
        content.append(&title);
        content.append(&description);
        content.append(&progress);
        content.append(&counter);

        let dialog = adw::Dialog::builder()
            .can_close(false)
            .content_width(420)
            .child(&content)
            .build();
        dialog.present(Some(self));

        imp.library_processing_dialog.replace(Some(dialog));
        imp.library_processing_progress.replace(Some(progress));
        imp.library_processing_label.replace(Some(counter));
    }

    fn update_library_processing(&self, done: u32, total: u32) {
        let imp = self.imp();
        if let Some(progress) = imp.library_processing_progress.borrow().as_ref() {
            let fraction = if total == 0 {
                0.0
            } else {
                done as f64 / total as f64
            };
            progress.set_fraction(fraction);
        }
        if let Some(label) = imp.library_processing_label.borrow().as_ref() {
            let total_str = total.to_string();
            let tracks = ni18n_f("{} track", "{} tracks", total, &[&total_str]);
            label.set_label(&format!("{done} / {tracks}"));
        }
    }

    fn finish_library_processing(&self) {
        let imp = self.imp();
        imp.library_processing_progress.borrow_mut().take();
        imp.library_processing_label.borrow_mut().take();
        if let Some(dialog) = imp.library_processing_dialog.borrow_mut().take() {
            dialog.force_close();
        }
    }

    fn play_album(&self, album: &Album) {
        let Some(library) = self.library() else {
            return;
        };
        let record = album.record();

        // In the random tab, playing an album queues every album that follows
        // it in the current shuffle, so playback continues through the same
        // selection the user is looking at.
        let records = if self.imp().library_random.get() {
            let displayed = self.imp().library_albums.borrow();
            match displayed.iter().position(|candidate| {
                let candidate = candidate.record();
                candidate.title == record.title && candidate.artist == record.artist
            }) {
                Some(position) => displayed[position..]
                    .iter()
                    .map(Album::record)
                    .collect::<Vec<_>>(),
                None => vec![record],
            }
        } else {
            vec![record]
        };

        match library.tracks_for_albums(&records) {
            Ok(paths) if !paths.is_empty() => self.play_paths_if_changed(paths),
            Ok(_) => self.add_toast(i18n("No songs found in this album")),
            Err(err) => {
                debug!("Unable to load album tracks: {err}");
                self.add_toast(i18n("Unable to load album"));
            }
        }
    }

    fn play_library_albums(&self) {
        let Some(library) = self.library() else {
            return;
        };

        let albums = self.imp().library_albums.borrow().clone();
        let records: Vec<_> = albums.iter().map(Album::record).collect();

        match library.tracks_for_albums(&records) {
            Ok(paths) if !paths.is_empty() => self.play_paths_if_changed(paths),
            Ok(_) => self.add_toast(i18n("No songs found")),
            Err(err) => {
                debug!("Unable to load library albums: {err}");
                self.add_toast(i18n("Unable to load the music library"));
            }
        }
    }

    fn update_favorite_button(&self) {
        let button = self.imp().playback_control.favorite_button();
        let Some(player) = self.player() else {
            button.set_sensitive(false);
            return;
        };
        let Some(song) = player.state().current_song() else {
            button.set_icon_name("non-starred-symbolic");
            button.set_tooltip_text(Some(&i18n("Add to Favorites")));
            button.set_sensitive(false);
            return;
        };
        let Some(library) = self.library() else {
            button.set_sensitive(false);
            return;
        };

        let favorite = library
            .resolve_album(&song.album(), &song.artist())
            .ok()
            .flatten()
            .is_some_and(|(album, artist)| {
                library.is_album_favorite(&album, &artist).unwrap_or(false)
            });

        button.set_sensitive(true);
        if favorite {
            button.set_icon_name("starred-symbolic");
            button.set_tooltip_text(Some(&i18n("Remove from Favorites")));
        } else {
            button.set_icon_name("non-starred-symbolic");
            button.set_tooltip_text(Some(&i18n("Add to Favorites")));
        }
    }

    fn song_album_key(&self, song: &Song) -> Option<(String, String)> {
        let library = self.library()?;
        library
            .resolve_album(&song.album(), &song.artist())
            .ok()
            .flatten()
    }

    fn is_song_album_favorite(&self, song: &Song) -> bool {
        let Some(library) = self.library() else {
            return false;
        };
        self.song_album_key(song).is_some_and(|(album, artist)| {
            library.is_album_favorite(&album, &artist).unwrap_or(false)
        })
    }

    fn update_queue_row_favorite(&self, row: &QueueRow, song: &Song) {
        row.set_favorite(self.is_song_album_favorite(song));
    }

    fn refresh_queue_favorites(&self) {
        let rows = self.imp().queue_rows.borrow().clone();
        for (row, _) in rows {
            let Some(row) = row.upgrade() else {
                continue;
            };
            if let Some(song) = row.song() {
                self.update_queue_row_favorite(&row, &song);
            }
        }
    }

    fn toggle_favorite_for_song(&self, song: &Song) {
        let Some(library) = self.library() else {
            return;
        };
        let Some((album, artist)) = self.song_album_key(song) else {
            return;
        };

        let favorite = library.is_album_favorite(&album, &artist).unwrap_or(false);
        if library
            .set_album_favorite(&album, &artist, !favorite)
            .is_ok()
        {
            self.favorite_state_changed();
        }
    }

    fn toggle_current_favorite(&self) {
        let Some(player) = self.player() else {
            return;
        };
        let Some(song) = player.state().current_song() else {
            return;
        };
        self.toggle_favorite_for_song(&song);
    }

    fn favorite_state_changed(&self) {
        self.update_favorite_button();
        self.refresh_queue_favorites();
        if matches!(
            *self.imp().library_current_page.borrow(),
            Some(LibraryPage::Favorites { .. })
        ) {
            self.refresh_favorites_view();
        }
    }

    /// Adds the selected albums to favorites, or removes them when every
    /// selected album is already a favorite.
    ///
    /// Albums that are already in the requested state are left untouched, so
    /// selecting a mix of favorite and non-favorite albums only writes the
    /// ones that actually need to change.
    fn toggle_albums_favorite(&self, albums: &[Album]) {
        let Some(library) = self.library() else {
            return;
        };

        let mut all_favorite = true;
        for album in albums {
            let record = album.record();
            if !library
                .is_album_favorite(&record.title, &record.artist)
                .unwrap_or(false)
            {
                all_favorite = false;
                break;
            }
        }

        let favorite = !all_favorite;
        let mut changed = false;
        for album in albums {
            let record = album.record();
            let current = library
                .is_album_favorite(&record.title, &record.artist)
                .unwrap_or(false);
            if current == favorite {
                album.set_favorite(favorite);
                continue;
            }

            if library
                .set_album_favorite(&record.title, &record.artist, favorite)
                .is_ok()
            {
                album.set_favorite(favorite);
                changed = true;
            }
        }

        if changed {
            self.favorite_state_changed();
        }
    }

    fn albums_tracks(&self, albums: &[Album]) -> Option<Vec<PathBuf>> {
        let library = self.library()?;
        let records: Vec<_> = albums.iter().map(Album::record).collect();
        match library.tracks_for_albums(&records) {
            Ok(paths) if !paths.is_empty() => Some(paths),
            Ok(_) => {
                self.add_toast(i18n("No songs found in this album"));
                None
            }
            Err(err) => {
                debug!("Unable to load album tracks: {err}");
                self.add_toast(i18n("Unable to load album"));
                None
            }
        }
    }

    fn songs_from_paths(&self, paths: &[PathBuf]) -> Vec<Song> {
        paths
            .iter()
            .filter_map(|path| {
                let uri = gio::File::for_path(path).uri();
                Song::from_uri(uri.as_str()).ok()
            })
            .collect()
    }

    fn handle_albums_context_action(&self, albums: &[Album], action: &str) {
        if albums.is_empty() {
            return;
        }
        match action {
            "play" => self.play_albums_only(albums),
            "queue" => self.add_albums_to_queue(albums),
            "play-next" => self.play_albums_next(albums),
            "favorite" => self.toggle_albums_favorite(albums),
            "delete" => self.delete_albums_from_device(albums),
            _ => debug!("Unknown album context action: {action}"),
        }
    }

    /// Plays exactly these albums, replacing whatever is in the queue.
    ///
    /// The context menu starts playback without leaving the library page, so
    /// the album grid stays visible while the music starts.
    fn play_albums_only(&self, albums: &[Album]) {
        if let Some(paths) = self.albums_tracks(albums) {
            self.play_paths_if_changed_with(paths, false);
        }
    }

    fn add_albums_to_queue(&self, albums: &[Album]) {
        let Some(paths) = self.albums_tracks(albums) else {
            return;
        };
        let Some(player) = self.player() else {
            return;
        };

        let queue = player.queue();
        let was_empty = queue.is_empty();
        let songs = self.songs_from_paths(&paths);
        if songs.is_empty() {
            return;
        }

        let added = songs.len() as u32;
        queue.add_songs(&songs);
        utils::store_playlist(&player);

        if was_empty {
            player.skip_to(0);
            player.play();
        } else {
            self.add_toast(ni18n_f(
                "Added one song",
                "Added {} songs",
                added,
                &[&added.to_string()],
            ));
        }
    }

    fn play_albums_next(&self, albums: &[Album]) {
        let Some(paths) = self.albums_tracks(albums) else {
            return;
        };
        let Some(player) = self.player() else {
            return;
        };

        let queue = player.queue();
        if queue.is_empty() || queue.current_song().is_none() {
            self.play_albums_only(albums);
            return;
        }

        let songs = self.songs_from_paths(&paths);
        if songs.is_empty() {
            return;
        }

        if queue.insert_songs_after_current(&songs) {
            utils::store_playlist(&player);
            self.add_toast(i18n("Will Play Next"));
        }
    }

    fn delete_albums_from_device(&self, albums: &[Album]) {
        let Some(library) = self.library() else {
            return;
        };

        let mut deleted = Vec::new();
        let mut failed = 0;
        let mut error = None;
        for album in albums {
            let record = album.record();
            match library.delete_album(&record.title, &record.artist) {
                Ok(stats) => {
                    deleted.extend(stats.deleted);
                    failed += stats.failed;
                }
                Err(err) => {
                    debug!("Unable to delete album: {err}");
                    error = Some(err);
                }
            }
        }

        if let Some(err) = error {
            debug!("Unable to delete album: {err}");
            self.add_toast(i18n("Unable to delete album"));
            return;
        }

        if deleted.is_empty() {
            self.add_toast(i18n("No songs found in this album"));
            return;
        }

        let uris: HashSet<String> = deleted
            .iter()
            .map(|path| gio::File::for_path(path).uri().to_string())
            .collect();

        if let Some(player) = self.player() {
            let queue = player.queue();
            let songs_to_remove: Vec<Song> = (0..queue.n_songs())
                .rev()
                .filter_map(|position| queue.song_at(position))
                .filter(|song| uris.contains(&song.uri()))
                .collect();
            for song in songs_to_remove {
                player.remove_song(&song);
            }
            // Dropping the songs from the queue is not enough: their artwork
            // lives in the shared texture caches. Release it so removing a
            // playlist actually lowers the process footprint.
            reclaim();
            utils::store_playlist(&player);
        }

        self.reload_library_page_after_delete(albums);

        if failed > 0 {
            self.add_toast(i18n("Some files could not be deleted"));
        } else {
            self.add_toast(i18n("Album deleted from device"));
        }
    }

    fn delete_songs_from_device(&self, songs: &[Song]) {
        let Some(library) = self.library() else {
            return;
        };

        let paths: Vec<PathBuf> = songs
            .iter()
            .filter_map(|song| song.file().path())
            .collect();
        if paths.is_empty() {
            self.add_toast(i18n("Unable to delete songs"));
            return;
        }

        let stats = match library.delete_tracks(&paths) {
            Ok(stats) => stats,
            Err(err) => {
                debug!("Unable to delete songs: {err}");
                self.add_toast(i18n("Unable to delete songs"));
                return;
            }
        };

        if stats.deleted.is_empty() {
            self.add_toast(i18n("Unable to delete songs"));
            return;
        }

        let uris: HashSet<String> = stats
            .deleted
            .iter()
            .map(|path| gio::File::for_path(path).uri().to_string())
            .collect();

        if let Some(player) = self.player() {
            let queue = player.queue();
            let songs_to_remove: Vec<Song> = (0..queue.n_songs())
                .rev()
                .filter_map(|position| queue.song_at(position))
                .filter(|song| uris.contains(&song.uri()))
                .collect();
            for song in songs_to_remove {
                player.remove_song(&song);
            }
            // Dropping the songs from the queue is not enough: their artwork
            // lives in the shared texture caches. Release it so removing a
            // playlist actually lowers the process footprint.
            reclaim();
            utils::store_playlist(&player);
        }

        self.imp().playlist_selection_anchor.set(None);
        self.refresh_current_library_tab();
        self.update_selected_count();
        self.set_playlist_selection(false);

        if stats.failed > 0 {
            self.add_toast(i18n("Some files could not be deleted"));
        } else {
            self.add_toast(i18n("Songs deleted from device"));
        }
    }

    /// Starts a queue unless it is already exactly what we would build.
    ///
    /// Rebuilding the queue means re-reading every file to create its `Song`
    /// objects, so repeatedly pressing the play button must be a no-op, even
    /// while the first request is still being loaded.
    fn play_paths_if_changed(&self, paths: Vec<PathBuf>) {
        self.play_paths_if_changed_with(paths, true);
    }

    /// Starts playing `paths`, optionally switching to the player view.
    fn play_paths_if_changed_with(&self, paths: Vec<PathBuf>, switch_view: bool) {
        let mut signature: Vec<String> = paths
            .iter()
            .map(|path| gio::File::for_path(path).uri().to_string())
            .collect();
        signature.sort();

        let same_request = self
            .imp()
            .library_play_signature
            .borrow()
            .as_ref()
            .is_some_and(|current| current == &signature);

        if same_request && self.imp().queue_loading.get() {
            debug!("The same playlist is already loading; not rebuilding it");
            if switch_view {
                self.switch_mode(WindowMode::MainView);
            }
            return;
        }

        if self.queue_matches_signature(&signature) {
            debug!("Playlist is already loaded; not rebuilding it");
            if switch_view {
                self.switch_mode(WindowMode::MainView);
            }
            if let Some(player) = self.player() {
                if player.queue().current_song().is_none() {
                    player.skip_to(0);
                }
                player.play();
            }
            return;
        }

        self.imp().library_play_signature.replace(Some(signature));

        let files = paths
            .into_iter()
            .map(gio::File::for_path)
            .collect::<Vec<_>>();
        self.queue_songs_with_options(files, true, true, switch_view);
    }

    fn queue_matches_signature(&self, expected: &[String]) -> bool {
        let Some(player) = self.player() else {
            return false;
        };
        let queue = player.queue();
        if queue.is_empty() || queue.n_songs() as usize != expected.len() {
            return false;
        }

        let mut actual: Vec<String> = (0..queue.n_songs())
            .filter_map(|position| queue.song_at(position).map(|song| song.uri()))
            .collect();
        actual.sort();
        actual == expected
    }

    fn setup_actions(&self) {
        let enable_recoloring = self.imp().settings.boolean("enable-recoloring");
        let show_track_info = self.imp().settings.boolean("show-track-info");
        let restore_session = self.imp().settings.boolean("restore-session");
        self.imp()
            .playback_control
            .set_track_info_visible(show_track_info);

        let album_limit = self.album_limit();
        let album_limit_state = if album_limit == 25 {
            "25"
        } else if album_limit == 50 {
            "50"
        } else {
            "custom"
        };
        let album_limit_action = gio::SimpleAction::new_stateful(
            "album-limit",
            Some(glib::VariantTy::STRING),
            &album_limit_state.to_variant(),
        );
        album_limit_action.connect_activate(clone!(
            #[weak(rename_to = win)]
            self,
            move |action, parameter| {
                let Some(value) = parameter.and_then(|value| value.get::<String>()) else {
                    return;
                };

                if value == "custom" {
                    win.show_album_limit_popover();
                    return;
                }

                let Ok(limit) = value.parse::<u32>() else {
                    return;
                };
                win.set_album_limit(limit, false);
                action.set_state(&value.to_variant());
            }
        ));
        self.add_action(&album_limit_action);
        self.setup_album_limit_menu();

        self.add_action_entries([
            gio::ActionEntry::builder("enable-recoloring")
                .state(enable_recoloring.to_variant())
                .activate(|this: &Window, action, _| {
                    let state = action.state().unwrap();
                    let action_state: bool = state.get().unwrap();
                    let enable_recoloring = !action_state;
                    action.set_state(&enable_recoloring.to_variant());

                    this.imp()
                        .settings
                        .set_boolean("enable-recoloring", enable_recoloring)
                        .expect("Unable to store setting");
                })
                .build(),
            gio::ActionEntry::builder("show-track-info")
                .state(show_track_info.to_variant())
                .activate(|this: &Window, action, _| {
                    let state = action.state().unwrap();
                    let action_state: bool = state.get().unwrap();
                    let show_track_info = !action_state;
                    action.set_state(&show_track_info.to_variant());

                    this.imp()
                        .settings
                        .set_boolean("show-track-info", show_track_info)
                        .expect("Unable to store setting");
                    this.imp()
                        .playback_control
                        .set_track_info_visible(show_track_info);
                })
                .build(),
            gio::ActionEntry::builder("reset-database")
                .activate(|this: &Window, _, _| {
                    this.confirm_reset_database();
                })
                .build(),
            gio::ActionEntry::builder("restore-session")
                .state(restore_session.to_variant())
                .activate(|this: &Window, action, _| {
                    let state = action.state().unwrap();
                    let action_state: bool = state.get().unwrap();
                    let restore_session = !action_state;
                    action.set_state(&restore_session.to_variant());

                    this.imp()
                        .settings
                        .set_boolean("restore-session", restore_session)
                        .expect("Unable to store setting");
                })
                .build(),
        ])
    }

    fn setup_album_limit_menu(&self) {
        let menu_button = self.imp().playback_control.menu_button();
        let Some(model) = menu_button.menu_model() else {
            return;
        };
        let Ok(menu) = model.downcast::<gio::Menu>() else {
            return;
        };

        let submenu = gio::Menu::new();
        submenu.append(Some("25"), Some("win.album-limit('25')"));
        submenu.append(Some("50"), Some("win.album-limit('50')"));
        self.imp()
            .album_limit_submenu
            .replace(Some(submenu.clone()));
        self.update_album_limit_menu_label();

        // Keep the album count and ReplayGain together in a single menu
        // section, so there is only one separator after them. The ReplayGain
        // section is found by looking for the submenu that contains the
        // "win.replaygain" action.
        let replaygain_section = (0..menu.n_items()).find_map(|index| {
            let section = menu
                .item_link(index, gio::MENU_LINK_SECTION)?
                .downcast::<gio::Menu>()
                .ok()?;

            let has_replaygain = (0..section.n_items()).any(|item_index| {
                section
                    .item_link(item_index, "submenu")
                    .map(|submenu| {
                        (0..submenu.n_items()).any(|submenu_index| {
                            submenu
                                .item_attribute_value(submenu_index, "action", None)
                                .and_then(|action| action.get::<String>())
                                .map(|action| action == "win.replaygain")
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false)
            });

            has_replaygain.then_some(section)
        });

        if let Some(section) = replaygain_section {
            section.insert_submenu(0, Some(&i18n("Album Count")), &submenu);
        } else {
            menu.append_submenu(Some(&i18n("Album Count")), &submenu);
        }
    }

    fn update_album_limit_menu_label(&self) {
        let Some(submenu) = self.imp().album_limit_submenu.borrow().clone() else {
            return;
        };

        while submenu.n_items() > 2 {
            submenu.remove(2);
        }

        let label = format!("{} ({})", i18n("Custom"), self.custom_album_limit());
        submenu.append(Some(&label), Some("win.album-limit('custom')"));
    }

    fn set_album_limit(&self, limit: u32, custom: bool) {
        let limit = limit.clamp(1, 10_000);
        let settings = &self.imp().settings;
        settings
            .set_int("album-limit", limit as i32)
            .expect("Unable to store album-limit setting");
        if custom {
            settings
                .set_int("album-limit-custom", limit as i32)
                .expect("Unable to store album-limit-custom setting");
        }
        self.update_album_limit_menu_label();

        if let Some(action) = self
            .lookup_action("album-limit")
            .and_then(|action| action.downcast::<gio::SimpleAction>().ok())
        {
            let state = if custom {
                "custom".to_string()
            } else {
                limit.to_string()
            };
            action.set_state(&state.to_variant());
        }

        // The setting controls only the Random Albums tab. Refresh it only
        // when that page is actually visible; otherwise the Recent and
        // artist pages keep their current contents untouched.
        let on_library =
            self.imp().main_stack.visible_child_name().as_deref() == Some("library-view");
        let on_random = matches!(
            *self.imp().library_current_page.borrow(),
            Some(LibraryPage::Random { .. })
        );
        if on_library && on_random {
            self.refresh_library_view(true);
        }
    }

    fn show_album_limit_popover(&self) {
        let menu_button = self.imp().playback_control.menu_button();
        if let Some(popover) = self.imp().album_limit_popover.borrow_mut().take() {
            popover.popdown();
            popover.unparent();
        }

        let popover = gtk::Popover::new();
        popover.add_css_class("album-limit-popover");
        popover.set_parent(&menu_button);
        popover.set_has_arrow(true);

        let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        content.set_margin_top(6);
        content.set_margin_bottom(6);
        content.set_margin_start(6);
        content.set_margin_end(6);

        let entry = gtk::Entry::new();
        entry.set_width_chars(6);
        entry.set_input_purpose(gtk::InputPurpose::Digits);
        entry.set_text(&self.custom_album_limit().to_string());
        content.append(&entry);

        let confirm = gtk::Button::from_icon_name("object-select-symbolic");
        confirm.add_css_class("flat");
        confirm.set_tooltip_text(Some(&i18n("Apply")));
        confirm.update_property(&[gtk::accessible::Property::Label(&i18n("Apply"))]);
        content.append(&confirm);

        popover.set_child(Some(&content));

        let commit = clone!(
            #[weak(rename_to = win)]
            self,
            #[weak]
            popover,
            #[weak]
            entry,
            move || {
                let Ok(limit) = entry.text().trim().parse::<u32>() else {
                    entry.add_css_class("error");
                    return;
                };
                if limit == 0 {
                    entry.add_css_class("error");
                    return;
                }
                win.set_album_limit(limit, true);
                popover.popdown();
            }
        );
        confirm.connect_clicked(clone!(
            #[strong]
            commit,
            move |_| commit()
        ));
        entry.connect_activate(clone!(
            #[strong]
            commit,
            move |_| commit()
        ));
        entry.connect_changed(clone!(
            #[weak]
            entry,
            move |_| entry.remove_css_class("error")
        ));

        popover.connect_closed(clone!(
            #[weak(rename_to = win)]
            self,
            #[weak]
            popover,
            move |_| {
                win.imp().album_limit_popover.take();
                popover.unparent();
            }
        ));

        self.imp()
            .album_limit_popover
            .replace(Some(popover.clone()));
        popover.popup();
        entry.grab_focus();
    }

    fn setup_waveform(&self) {
        if let Some(player) = self.player() {
            let peaks = player.waveform_generator().peaks();
            self.imp().waveform_view.set_peaks(peaks);

            let notify_peaks_id = player.waveform_generator().connect_notify_local(
                Some("has-peaks"),
                clone!(
                    #[weak(rename_to = win)]
                    self,
                    move |gen, _| {
                        let peaks = gen.peaks();
                        win.imp().waveform_view.set_peaks(peaks);
                    }
                ),
            );
            self.imp().notify_peaks_id.replace(Some(notify_peaks_id));
        }
    }

    fn unbind_waveform(&self) {
        if let Some(player) = self.player() {
            if let Some(id) = self.imp().notify_peaks_id.take() {
                player.waveform_generator().disconnect(id);
            }
        }
    }

    fn restore_window_state(&self) {
        let settings = utils::settings_manager();
        let width = settings.int("window-width");
        let height = settings.int("window-height");
        // Keep the album grid usable even if an older settings value was
        // stored when the window was smaller.
        self.set_default_size(width.max(900), height.max(600));
    }

    fn reset_queue(&self) {
        self.set_playlist_visible(false);
        self.set_playlist_shuffled(false);
        self.set_playlist_selection(false);
        self.update_style(None);
    }

    fn clear_queue(&self) {
        if let Some(p) = self.player() {
            p.clear_queue();
            utils::clear_cached_playlist();
        }
    }

    fn confirm_reset_database(&self) {
        let dialog = adw::AlertDialog::builder()
            .heading(&i18n("Reset Database"))
            .body(&i18n(
                "This will delete the music library index, favorites, saved session and all cached artwork. This cannot be undone.",
            ))
            .build();
        dialog.add_response("cancel", &i18n("Cancel"));
        dialog.add_response("reset", &i18n("Reset"));
        dialog.set_response_appearance("reset", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        dialog.connect_response(
            None,
            clone!(
                #[weak(rename_to = win)]
                self,
                move |dialog, response| {
                    dialog.close();
                    if response == "reset" {
                        win.reset_database();
                    }
                }
            ),
        );
        dialog.present(Some(self));
    }

    fn reset_database(&self) {
        let imp = self.imp();

        // Invalidate any queue that is still being parsed and drop whatever
        // is currently playing before the caches disappear from disk.
        imp.queue_load_generation
            .set(imp.queue_load_generation.get().wrapping_add(1));
        imp.active_queue_load.replace(None);
        imp.library_scan_generation
            .set(imp.library_scan_generation.get().wrapping_add(1));
        self.finish_library_processing();
        imp.queue_loading.set(false);
        imp.playlist_view.end_loading();
        self.action_set_enabled("queue.add-song", true);
        self.action_set_enabled("queue.add-folder", true);
        self.action_set_enabled("queue.clear", true);
        self.clear_queue();
        imp.pending_restore.take();
        imp.queue_single_album.set(None);

        // A pending rescan or a file monitor on the old folder would rebuild
        // the index immediately after the reset.
        if let Some(source) = imp.library_rescan_source.borrow_mut().take() {
            source.remove();
        }
        imp.library_monitors.borrow_mut().clear();

        if let Some(library) = self.library() {
            if let Err(err) = library.reset() {
                log::warn!("Unable to reset the music library: {err}");
                self.add_toast(i18n("Unable to reset the database"));
                return;
            }
        }

        // The folder itself is part of the library state; resetting must let
        // the user choose it again instead of silently rebuilding the old
        // index on the next start.
        imp.settings.set_string("library-folder", "").ok();

        let mut cache_dir = glib::user_cache_dir();
        cache_dir.push("amberol");
        if let Err(err) = fs::remove_dir_all(&cache_dir) {
            if err.kind() != std::io::ErrorKind::NotFound {
                log::warn!("Unable to remove the cover cache {:?}: {err}", cache_dir);
            }
        }

        Album::clear_cover_cache();
        clear_thumbnail_cache();
        if let Ok(mut covers) = CoverCache::global().lock() {
            covers.clear();
        }

        imp.library_albums.borrow_mut().clear();
        imp.library_current_page.borrow_mut().take();
        imp.library_page_before_search.borrow_mut().take();
        imp.library_history.borrow_mut().clear();
        imp.library_view.set_scanning(false);
        imp.library_view.set_albums(Vec::new());
        imp.library_scan_running.set(false);

        self.set_initial_state();
        self.switch_mode(WindowMode::InitialView);
        self.add_toast(i18n("Database and caches have been reset"));
    }

    fn playlist_visible(&self) -> bool {
        self.imp().playlist_visible.get()
    }

    fn set_playlist_visible(&self, visible: bool) {
        if visible != self.imp().playlist_visible.replace(visible) {
            self.imp().split_view.set_show_sidebar(visible);
            self.notify("playlist-visible");
        }
    }

    fn playlist_shuffled(&self) -> bool {
        self.imp().playlist_shuffled.get()
    }

    fn set_playlist_shuffled(&self, shuffled: bool) {
        let imp = self.imp();

        if shuffled != imp.playlist_shuffled.replace(shuffled) {
            if let Some(player) = self.player() {
                let queue = player.queue();
                let state = player.state();
                let reset_song = queue.is_first_song() && !state.playing();

                queue.set_shuffled(shuffled);

                if reset_song {
                    player.skip_to(0);
                }
            }

            self.notify("playlist-shuffled");
        }
    }

    fn playlist_selection(&self) -> bool {
        self.imp().playlist_selection.get()
    }

    fn set_playlist_selection(&self, selection: bool) {
        let imp = self.imp();

        if selection != imp.playlist_selection.replace(selection) {
            if !selection {
                if let Some(player) = self.player() {
                    let queue = player.queue();
                    queue.unselect_all_songs();
                }
            }

            self.imp()
                .playlist_view
                .queue_actionbar()
                .set_revealed(selection);

            self.notify("playlist-selection");
        }
    }

    fn restore_playlist(&self) {
        if let Some((songs, state)) = utils::load_cached_session() {
            self.imp().pending_restore.replace(Some(state));
            self.queue_songs(songs);
        }
    }

    fn apply_restored_session(&self, player: Rc<AudioPlayer>, session: utils::SessionState) {
        let queue = player.queue();

        player.set_volume(session.volume);
        player.set_repeat_mode(session.repeat_mode);

        // The freshly loaded queue is not shuffled yet; rebuild the shuffle
        // map so the restored session keeps its shuffle mode.
        if session.shuffled && !queue.is_shuffled() {
            queue.set_shuffled(true);
        }
        self.imp().playlist_shuffled.set(session.shuffled);
        self.notify("playlist-shuffled");
        self.imp()
            .playback_control
            .set_repeat_mode(queue.repeat_mode());

        let index = session.current_uri.as_ref().and_then(|uri| {
            (0..queue.n_songs())
                .find(|&index| queue.song_at(index).is_some_and(|song| song.uri() == *uri))
        });

        if let Some(index) = index {
            player.skip_to(index);
            // Prepare the pipeline while staying paused, so the seek below
            // does not start playing from the beginning of the song.
            player.prepare();
            // Show the stored time immediately; the pipeline can take a
            // moment to load and acknowledge the seek.
            player.preview_position(session.position);
            self.restore_position(player, session.position, session.playing);
        } else if !queue.is_empty() {
            player.skip_to(0);
        }
    }

    fn restore_position(&self, player: Rc<AudioPlayer>, target: u64, resume: bool) {
        if target == 0 {
            if resume {
                player.play();
            }
            return;
        }

        // GstPlayer drops seek requests issued before the media is loaded, so
        // wait for the backend to report a duration, then seek and verify the
        // result.
        let mut attempts = 0_u32;
        glib::timeout_add_local(Duration::from_millis(200), move || {
            if attempts >= 40 {
                player.clear_position_preview();
                if resume {
                    player.play();
                }
                return glib::ControlFlow::Break;
            }
            attempts += 1;

            // While the media is being prepared the backend may still report
            // position zero. Keep the restored time visible instead of
            // letting the seek bar jump back to the start.
            if player.media_position().unwrap_or(0).abs_diff(target) > 2 {
                player.preview_position(target);
            }

            if player.media_duration().unwrap_or(0) == 0 {
                return glib::ControlFlow::Continue;
            }

            if player.media_position().unwrap_or(0).abs_diff(target) <= 2 {
                // Persist the restored position so an immediate quit does
                // not lose it again.
                utils::store_playlist(&player);
                if resume {
                    player.play();
                }
                return glib::ControlFlow::Break;
            }

            player.seek_restored_position(target);
            glib::ControlFlow::Continue
        });
    }

    fn queue_songs(&self, queue: Vec<gio::File>) {
        self.queue_songs_with_options(queue, false, false, true);
    }

    fn queue_songs_with_options(
        &self,
        queue: Vec<gio::File>,
        replace: bool,
        autoplay: bool,
        switch_view: bool,
    ) {
        let imp = self.imp();

        if queue.is_empty() {
            if !imp.queue_loading.get() {
                self.add_toast(i18n("No available song found"));
            }
            return;
        }

        // The file manager may deliver several opened files as separate
        // `open` calls in quick succession. Fold them into the load that is
        // already running: starting a new one would invalidate the pending
        // idle batch and silently drop the files that arrived just before.
        if !replace && imp.queue_loading.get() {
            if let Some(active) = imp.active_queue_load.borrow().as_ref().cloned() {
                let added = queue.len() as u32;
                active.total.set(active.total.get() + added);
                active.files.borrow_mut().extend(queue);
                debug!("Appended {added} files to the ongoing queue load");
                return;
            }
        }

        // A reset invalidates pending idle batches, so a queue that was
        // still loading cannot repopulate the player after the reset.
        let load_generation = imp.queue_load_generation.get().wrapping_add(1);
        imp.queue_load_generation.set(load_generation);
        imp.active_queue_load.replace(None);

        if replace {
            self.clear_queue();
        }

        imp.queue_loading.set(true);
        if switch_view {
            self.switch_mode(WindowMode::MainView);
            self.set_playlist_visible(true);
        }

        // Disable actions on the queue; loading is "atomic"
        self.action_set_enabled("queue.add-song", false);
        self.action_set_enabled("queue.add-folder", false);
        self.action_set_enabled("queue.clear", false);
        imp.playlist_view.begin_loading();

        // Begin the trace
        let now = Instant::now();

        // Turn the list of files into songs on idle, in small time-budgeted
        // batches. One file per idle callback made loading thousands of
        // tracks artificially slow.
        let files = Rc::new(RefCell::new(queue.into_iter().collect::<VecDeque<_>>()));
        let total = Rc::new(Cell::new(files.borrow().len() as u32));

        imp.active_queue_load.replace(Some(QueueLoadState {
            files: files.clone(),
            total: total.clone(),
        }));

        let mut known_keys: HashSet<String> = HashSet::new();
        if let Some(player) = self.player() {
            let queue = player.queue();
            for position in 0..queue.n_songs() {
                if let Some(song) = queue.song_at(position) {
                    known_keys.insert(format!("f:{}", song.uri()));
                    if let Some(uuid) = song.uuid() {
                        known_keys.insert(format!("u:{uuid}"));
                    }
                }
            }
        }

        let mut songs = Vec::new();
        let mut scanned: u32 = 0;
        let mut duplicates: u32 = 0;

        glib::idle_add_local(clone!(
            #[weak(rename_to = win)]
            self,
            #[upgrade_or]
            glib::ControlFlow::Break,
            move || {
                if win.imp().queue_load_generation.get() != load_generation {
                    win.imp().active_queue_load.replace(None);
                    return glib::ControlFlow::Break;
                }

                const BATCH_BUDGET: Duration = Duration::from_millis(8);
                let batch_start = Instant::now();

                while batch_start.elapsed() < BATCH_BUDGET {
                    let Some(file) = files.borrow_mut().pop_front() else {
                        break;
                    };
                    scanned += 1;

                    if let Ok(song) = Song::from_uri(file.uri().as_str()) {
                        let uri_key = format!("f:{}", song.uri());
                        let uuid_key = song.uuid().map(|uuid| format!("u:{uuid}"));
                        let duplicate = known_keys.contains(&uri_key)
                            || uuid_key
                                .as_ref()
                                .is_some_and(|key| known_keys.contains(key));

                        if duplicate {
                            duplicates += 1;
                        } else {
                            known_keys.insert(uri_key);
                            if let Some(uuid_key) = uuid_key {
                                known_keys.insert(uuid_key);
                            }
                            songs.push(song);
                        }
                    }
                }

                win.imp().playlist_view.update_loading(scanned, total.get());

                // More files may have been appended while this batch was
                // parsing; keep the same loader alive until all are done.
                if !files.borrow().is_empty() {
                    return glib::ControlFlow::Continue;
                }

                win.imp().active_queue_load.replace(None);
                win.imp().queue_loading.set(false);
                debug!(
                    "Total loading time for {} files: {} ms",
                    total.get(),
                    now.elapsed().as_millis()
                );

                // Re-enable the actions
                win.action_set_enabled("queue.add-song", true);
                win.action_set_enabled("queue.add-folder", true);
                win.action_set_enabled("queue.clear", true);

                if songs.is_empty() {
                    win.imp().playlist_view.end_loading();
                    if duplicates == 0 {
                        win.add_toast(i18n("No songs found"));
                    }
                } else if let Some(player) = win.player() {
                    let queue = player.queue();
                    let was_empty = queue.is_empty();

                    win.imp().playlist_view.end_loading();

                    let pending_restore = win.imp().pending_restore.take();

                    // Bulk add to avoid hammering the UI with list model updates.
                    // The queue sorts every album by disc and track number from
                    // the tags, so files opened in any order end up in order.
                    queue.add_songs(&songs);

                    debug!(
                        "Queue was empty: {}, new size: {}",
                        was_empty,
                        queue.n_songs()
                    );

                    if let Some(session) = pending_restore {
                        win.apply_restored_session(player, session);
                    } else {
                        // Store the current state of the playlist
                        utils::store_playlist(&player);

                        if was_empty || autoplay {
                            player.skip_to(0);
                        }

                        if autoplay {
                            player.play();
                        } else if songs.len() == 1 {
                            // If the queue was empty, start playing the new song
                            // immediately; otherwise just report it once.
                            if was_empty {
                                player.play();
                            } else {
                                win.queue_added_songs_toast(1);
                            }
                        } else {
                            win.queue_added_songs_toast(songs.len() as u32);
                        }
                    }
                }

                glib::ControlFlow::Break
            }
        ));
    }

    fn add_files_to_queue(&self, model: &gio::ListModel) {
        let mut queue: Vec<gio::File> = vec![];

        for pos in 0..model.n_items() {
            let file = model.item(pos).unwrap().downcast::<gio::File>().unwrap();

            if let Ok(info) = file.query_info(
                "standard::name,standard::display-name,standard::type,standard::content-type",
                gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
                gio::Cancellable::NONE,
            ) {
                match info.file_type() {
                    gio::FileType::Regular => {
                        if let Some(content_type) = info.content_type() {
                            if gio::content_type_is_mime_type(&content_type, "audio/*") {
                                debug!("Adding file '{}' to the queue", file.uri());
                                queue.push(file);
                            }
                        }
                    }
                    gio::FileType::Directory => {
                        debug!("Adding folder '{}' to the queue", file.uri());
                        let files = utils::load_files_from_folder(&file, true);
                        queue.extend(files);
                    }
                    _ => (),
                }
            }
        }

        self.queue_songs(queue);
    }

    // Bind the PlayerState to the UI
    fn setup_session_autosave(&self) {
        // Keep the stored position reasonably fresh while playing, so a
        // restart (or a crash) restores the song near the place it stopped.
        glib::timeout_add_seconds_local(5, clone!(
            #[weak(rename_to = win)]
            self,
            #[upgrade_or]
            glib::ControlFlow::Break,
            move || {
                if let Some(player) = win.player() {
                    if !player.queue().is_empty() && player.state().playing() {
                        utils::store_playlist(&player);
                    }
                }
                glib::ControlFlow::Continue
            }
        ));
    }

    fn bind_state(&self) {
        let imp = self.imp();
        if let Some(player) = self.player() {
            let state = player.state();

            // Use the PlayerState:playing property to control the play/pause button
            self.update_play_button();
            let notify_playing_id = state.connect_notify_local(
                Some("playing"),
                clone!(
                    #[weak(rename_to = win)]
                    self,
                    move |_, _| {
                        win.set_playlist_selection(false);
                        win.update_play_button();
                    }
                ),
            );
            imp.notify_playing_id.replace(Some(notify_playing_id));

            // Update the position labels
            self.update_position_labels();
            let notify_position_id = state.connect_notify_local(
                Some("position"),
                clone!(
                    #[weak(rename_to = win)]
                    self,
                    move |_, _| {
                        win.update_position_labels();
                    }
                ),
            );
            imp.notify_position_id.replace(Some(notify_position_id));

            // Update the UI
            self.update_song();
            let notify_song_id = state.connect_notify_local(
                Some("song"),
                clone!(
                    #[weak(rename_to = win)]
                    self,
                    move |_, _| {
                        win.update_song();
                        if let Some(player) = win.player() {
                            if !player.queue().is_empty() {
                                utils::store_playlist(&player);
                            }
                        }
                    }
                ),
            );
            imp.notify_song_id.replace(Some(notify_song_id));

            // Update the cover, if any is available
            self.update_cover();
            let notify_cover_id = state.connect_notify_local(
                Some("cover"),
                clone!(
                    #[weak(rename_to = win)]
                    self,
                    move |_, _| {
                        win.update_cover();
                    }
                ),
            );
            imp.notify_cover_id.replace(Some(notify_cover_id));

            // Bind the song properties to the UI
            state
                .bind_property("title", &imp.song_details.get().title_label(), "label")
                .sync_create()
                .build();
            // The artist credit is rendered as individual links, so it cannot
            // use a plain property binding; rebuild it whenever the song changes.
            self.update_artist_label();
            let notify_artist_id = state.connect_notify_local(
                Some("artist"),
                clone!(
                    #[weak(rename_to = win)]
                    self,
                    move |_, _| {
                        win.update_artist_label();
                    }
                ),
            );
            imp.notify_artist_id.replace(Some(notify_artist_id));
            state
                .bind_property("album", &imp.song_details.get().album_label(), "label")
                .sync_create()
                .build();
            state
                .bind_property(
                    "track-info",
                    &imp.playback_control.get().track_info_label(),
                    "label",
                )
                .sync_create()
                .build();
            state
                .bind_property(
                    "volume",
                    &imp.playback_control.get().volume_control(),
                    "volume",
                )
                .sync_create()
                .build();
        }
    }

    fn unbind_state(&self) {
        if let Some(player) = self.player() {
            let state = player.state();

            if let Some(id) = self.imp().notify_playing_id.take() {
                state.disconnect(id);
            }
            if let Some(id) = self.imp().notify_artist_id.take() {
                state.disconnect(id);
            }
            if let Some(id) = self.imp().notify_position_id.take() {
                state.disconnect(id);
            }
            if let Some(id) = self.imp().notify_song_id.take() {
                state.disconnect(id);
            }
            if let Some(id) = self.imp().notify_cover_id.take() {
                state.disconnect(id);
            }
        }
    }

    // Bind the Queue to the UI
    fn bind_queue(&self) {
        if let Some(player) = self.player() {
            let queue = player.queue();

            let notify_nsongs_id = queue.connect_notify_local(
                Some("n-songs"),
                clone!(
                    #[weak(rename_to = win)]
                    self,
                    move |queue, _| {
                        debug!("queue.n_songs() = {}", queue.n_songs());
                        if queue.is_empty() {
                            win.set_initial_state();
                            win.reset_queue();
                        } else {
                            win.action_set_enabled("queue.toggle", true);
                            win.action_set_enabled("queue.shuffle", queue.n_songs() > 1);

                            win.action_set_enabled("win.play", true);
                            win.action_set_enabled("win.previous", true);
                            win.action_set_enabled("win.next", queue.n_songs() > 1);
                        }

                        if queue.n_songs() == 1 {
                            if let Some(p) = win.player() {
                                p.skip_next();
                            }
                        }

                        win.update_playlist_time();
                    }
                ),
            );
            self.imp().notify_nsongs_id.replace(Some(notify_nsongs_id));

            queue.connect_notify_local(
                Some("repeat-mode"),
                clone!(
                    #[weak(rename_to = win)]
                    self,
                    move |queue, _| {
                        win.imp()
                            .playback_control
                            .set_repeat_mode(queue.repeat_mode());
                    }
                ),
            );

            let notify_current_id = queue.connect_notify_local(
                Some("current"),
                clone!(
                    #[weak(rename_to = win)]
                    self,
                    move |queue, _| {
                        if queue.is_last_song() {
                            match queue.repeat_mode() {
                                RepeatMode::Consecutive => {
                                    win.action_set_enabled("win.next", false);
                                }
                                _ => {
                                    win.action_set_enabled("win.next", true);
                                }
                            }
                        } else {
                            win.action_set_enabled("win.next", true);
                        }
                    }
                ),
            );
            self.imp()
                .notify_current_id
                .replace(Some(notify_current_id));
        }
    }

    fn unbind_queue(&self) {
        if let Some(player) = self.player() {
            let queue = player.queue();
            if let Some(id) = self.imp().notify_nsongs_id.take() {
                queue.disconnect(id);
            }
            if let Some(id) = self.imp().notify_current_id.take() {
                queue.disconnect(id);
            }
        }
    }

    fn connect_signals(&self) {
        self.imp().split_view.connect_notify_local(
            Some("collapsed"),
            clone!(
                #[weak(rename_to = win)]
                self,
                move |split_view, _| {
                    win.set_playlist_visible(split_view.shows_sidebar());
                    win.imp()
                        .playlist_view
                        .back_button()
                        .set_visible(split_view.is_collapsed());
                }
            ),
        );

        self.imp().split_view.connect_notify_local(
            Some("show-sidebar"),
            clone!(
                #[weak(rename_to = win)]
                self,
                move |split_view, _| {
                    win.set_playlist_visible(split_view.shows_sidebar());
                }
            ),
        );

        self.imp().waveform_view.connect_closure(
            "position-changed",
            false,
            closure_local!(
                #[watch(rename_to = win)]
                self,
                move |_wv: WaveformView, position: f64| {
                    debug!("New position: {}", position);
                    if let Some(player) = win.player() {
                        let state = player.state();
                        if state.current_song().is_some() {
                            player.seek_position_rel(position);
                            player.play();
                        }
                    }
                }
            ),
        );

        self.imp()
            .playback_control
            .volume_control()
            .connect_closure(
                "volume-changed",
                false,
                closure_local!(
                    #[watch(rename_to = win)]
                    self,
                    move |_vc: VolumeControl, volume: f64| {
                        debug!("Volume changed: {}", volume);
                        if let Some(p) = win.player() {
                            p.set_volume(volume);
                        }
                    }
                ),
            );

        self.imp()
            .playlist_view
            .queue_select_all_button()
            .connect_clicked(clone!(
                #[weak(rename_to = win)]
                self,
                move |_| {
                    win.select_all_playlist_songs();
                }
            ));

        self.imp()
            .playlist_view
            .queue_remove_button()
            .connect_clicked(clone!(
                #[weak(rename_to = win)]
                self,
                move |_| {
                    if let Some(player) = win.player() {
                        let queue = player.queue();
                        let mut remove_songs: Vec<Song> = Vec::new();
                        // Collect all songs to be removed first, since we can't
                        // remove objects from the model while we're iterating it
                        for idx in 0..queue.n_songs() {
                            let song = queue.song_at(idx).unwrap();
                            if song.selected() {
                                remove_songs.push(song);
                            }
                        }

                        for song in remove_songs {
                            win.remove_song(&song);
                        }

                        // Removing many tracks at once leaves all of their
                        // cover textures cached; give that memory back now.
                        reclaim();

                        // Store the current state of the playlist
                        utils::store_playlist(&player);
                    }
                }
            ));

        self.imp().settings.connect_changed(
            Some("enable-recoloring"),
            clone!(
                #[weak(rename_to = this)]
                self,
                move |settings, _| {
                    let enable_recoloring = settings.boolean("enable-recoloring");
                    debug!("GSettings:enable-recoloring: {enable_recoloring}");

                    // Keep the menu check mark in sync when the setting is
                    // changed from anywhere else.
                    if let Some(action) = this
                        .lookup_action("enable-recoloring")
                        .and_then(|action| action.downcast::<gio::SimpleAction>().ok())
                    {
                        action.set_state(&enable_recoloring.to_variant());
                    }

                    if let Some(player) = this.player() {
                        let state = player.state();
                        this.update_style(state.current_song().as_ref());
                    } else {
                        this.update_style(None);
                    }
                }
            ),
        );
        let _dummy = self.imp().settings.boolean("enable-recoloring");

        self.connect_close_request(move |window| {
            debug!("Saving window state");
            let width = window.default_size().0;
            let height = window.default_size().1;

            let settings = utils::settings_manager();
            settings
                .set_int("window-width", width)
                .expect("Unable to store window-width");
            settings
                .set_int("window-height", height)
                .expect("Unable to stop window-height");

            if let Some(player) = window.player() {
                let queue = player.queue();
                if queue.is_empty() {
                    utils::clear_cached_playlist();
                } else {
                    utils::store_playlist(&player);
                }
            }

            window.unbind_queue();
            window.unbind_state();
            window.unbind_waveform();

            if let Some(source) = window.imp().library_rescan_source.take() {
                source.remove();
            }
            window.imp().library_monitors.borrow_mut().clear();

            glib::Propagation::Proceed
        });
    }

    // The initial state of the playback actions
    fn set_initial_state(&self) {
        if let Some(player) = self.player() {
            let queue = player.queue();
            self.action_set_enabled("win.play", !queue.is_empty());
            self.action_set_enabled("win.previous", !queue.is_empty());
            self.action_set_enabled("win.next", !queue.is_last_song());

            self.action_set_enabled("queue.toggle", !queue.is_empty());
            self.action_set_enabled("queue.shuffle", queue.n_songs() > 1);
            self.action_set_enabled("win.replaygain", player.replaygain_available());

            let replaygain = self.imp().settings.enum_("replay-gain").into();
            self.set_replaygain(replaygain);

            // Manually set player state, because set_replaygain
            // only updates player state when the value changes.
            player.set_replaygain(replaygain);

            self.imp()
                .playback_control
                .set_repeat_mode(queue.repeat_mode());
            self.set_playlist_shuffled(queue.is_shuffled());

            // Manually update the icon on the initial empty state
            // to avoid generating the UI definition file at build
            // time
            self.imp().status_page.set_icon_name(Some(APPLICATION_ID));

            if utils::has_cached_playlist() {
                self.imp().restore_playlist_button.set_visible(true);
                self.imp()
                    .restore_playlist_button
                    .add_css_class("suggested-action");
                self.imp()
                    .add_folder_button
                    .remove_css_class("suggested-action");
            } else {
                self.imp().restore_playlist_button.set_visible(false);
                self.imp()
                    .restore_playlist_button
                    .remove_css_class("suggested-action");
                self.imp()
                    .add_folder_button
                    .add_css_class("suggested-action");
            }

            // Restore the previous session once, when the application starts.
            // The guard is important: after a manual "Clear" the queue becomes
            // empty, and set_initial_state() is called again; without it the
            // playlist would immediately come back.
            if !self.imp().restore_session_checked.replace(true)
                && queue.is_empty()
                && self.imp().settings.boolean("restore-session")
                && !self.imp().queue_loading.get()
                && utils::has_cached_playlist()
            {
                self.restore_playlist();
                return;
            }

            let state = player.state();
            if state.playing() || !queue.is_empty() {
                self.switch_mode(WindowMode::MainView);
            } else if self
                .library()
                .and_then(|library| library.album_count().ok())
                .unwrap_or(0)
                > 0
            {
                self.switch_mode(WindowMode::LibraryView);
            } else {
                self.switch_mode(WindowMode::InitialView);
            }
        }
    }

    fn setup_playlist(&self) {
        let imp = self.imp();
        let Some(player) = self.player() else {
            return;
        };
        let queue = player.queue();

        // The playlist is shown in queue order. The queue itself keeps albums
        // together and orders their tracks by the disc/track tags, so no
        // filename-based sort model is allowed to override it here.
        let queue_model = queue.model().clone();
        let selection = gtk::NoSelection::new(Some(queue_model.clone()));

        self.refresh_queue_rows(queue_model.upcast_ref());
        queue_model.connect_items_changed(clone!(
            #[weak(rename_to = win)]
            self,
            #[weak]
            queue_model,
            move |_, _, _, _| {
                win.imp().queue_single_album.set(None);
                win.refresh_queue_rows(queue_model.upcast_ref());
            }
        ));

        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(clone!(
            #[weak(rename_to = win)]
            self,
            #[weak]
            queue_model,
            move |_, item| {
                let row = QueueRow::default();
                let list_item = item.downcast_ref::<gtk::ListItem>().unwrap();
                list_item.set_child(Some(&row));

                win.imp()
                    .queue_rows
                    .borrow_mut()
                    .push((row.downgrade(), list_item.downgrade()));

                row.connect_notify_local(
                    Some("selected"),
                    clone!(
                        #[weak]
                        win,
                        move |_, _| {
                            win.update_selected_count();
                        }
                    ),
                );

                row.connect_closure(
                    "favorite-toggled",
                    false,
                    closure_local!(
                        #[weak]
                        win,
                        move |_row: QueueRow, song: Song| {
                            win.toggle_favorite_for_song(&song);
                        }
                    ),
                );

                row.connect_closure(
                    "artist-activated",
                    false,
                    closure_local!(
                        #[weak]
                        win,
                        move |_row: QueueRow, artist: String| {
                            win.show_artist(&artist);
                        }
                    ),
                );

                row.connect_closure(
                    "selection-clicked",
                    false,
                    closure_local!(
                        #[weak]
                        win,
                        move |_row: QueueRow, song: Song, shift: bool, control: bool| {
                            win.select_playlist_song(&song, shift, control);
                        }
                    ),
                );

                row.connect_closure(
                    "song-context-action",
                    false,
                    closure_local!(
                        #[weak]
                        win,
                        move |_row: QueueRow, song: Song, action: String| {
                            win.handle_song_context_action(&song, &action);
                        }
                    ),
                );

                win.bind_property("playlist-selection", &row, "selection-mode")
                    .sync_create()
                    .build();

                list_item
                    .bind_property("item", &row, "song")
                    .sync_create()
                    .build();

                list_item
                    .property_expression("item")
                    .chain_property::<Song>("title")
                    .bind(&row, "song-title", gtk::Widget::NONE);
                list_item
                    .property_expression("item")
                    .chain_property::<Song>("cover")
                    .bind(&row, "song-cover", gtk::Widget::NONE);
                list_item
                    .property_expression("item")
                    .chain_property::<Song>("playing")
                    .bind(&row, "playing", gtk::Widget::NONE);
                list_item
                    .property_expression("item")
                    .chain_property::<Song>("selected")
                    .bind(&row, "selected", gtk::Widget::NONE);

                let single_album = win.queue_single_album(queue_model.upcast_ref());
                update_queue_row_cover(&row, list_item, queue_model.upcast_ref(), single_album);

                // Reordering the queue moves rows without necessarily
                // rebinding them; keep the cover grouping correct.
                list_item.connect_notify_local(
                    Some("position"),
                    clone!(
                        #[weak]
                        row,
                        #[weak]
                        queue_model,
                        #[weak]
                        win,
                        move |list_item, _| {
                            let single_album =
                                win.queue_single_album(queue_model.upcast_ref());
                            update_queue_row_cover(
                                &row,
                                list_item,
                                queue_model.upcast_ref(),
                                single_album,
                            );
                        }
                    ),
                );
            }
        ));
        factory.connect_bind(clone!(
            #[weak(rename_to = win)]
            self,
            #[weak]
            queue_model,
            move |_, item| {
                let list_item = item.downcast_ref::<gtk::ListItem>().unwrap();
                let Some(row) = list_item
                    .child()
                    .and_then(|child| child.downcast::<QueueRow>().ok())
                else {
                    return;
                };
                let single_album = win.queue_single_album(queue_model.upcast_ref());
                update_queue_row_cover(&row, list_item, queue_model.upcast_ref(), single_album);
                if let Some(song) = list_item
                    .item()
                    .and_then(|item| item.downcast::<Song>().ok())
                {
                    win.update_queue_row_artist(&row, &song.artist());
                    win.update_queue_row_favorite(&row, &song);
                }
            }
        ));
        imp.playlist_view
            .queue_view()
            .set_factory(Some(&factory.upcast::<gtk::ListItemFactory>()));

        imp.playlist_view
            .queue_view()
            .set_model(Some(selection.upcast_ref::<gtk::SelectionModel>()));

        let key_controller = gtk::EventControllerKey::new();
        key_controller.set_propagation_phase(gtk::PropagationPhase::Capture);
        key_controller.connect_key_pressed(clone!(
            #[weak(rename_to = win)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, key, _, modifiers| {
                if modifiers.contains(gdk::ModifierType::CONTROL_MASK)
                    && matches!(key, gdk::Key::a | gdk::Key::A)
                {
                    win.select_all_playlist_songs();
                    return glib::Propagation::Stop;
                }
                if key == gdk::Key::Escape && win.playlist_selection() {
                    win.set_playlist_selection(false);
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            }
        ));
        imp.playlist_view.queue_view().add_controller(key_controller);

        imp.playlist_view.queue_view().connect_activate(clone!(
            #[weak(rename_to = win)]
            self,
            #[weak]
            selection,
            move |_, pos| {
                let song = selection
                    .upcast::<gio::ListModel>()
                    .item(pos)
                    .unwrap()
                    .downcast::<Song>()
                    .unwrap();

                if let Some(player) = win.player() {
                    let queue = player.queue();

                    let mut real_pos = None;
                    for i in 0..queue.model().n_items() {
                        if let Some(item) = queue.model().item(i) {
                            let s = item.downcast::<Song>().unwrap();
                            if s.equals(&song) {
                                real_pos = Some(i);
                                break;
                            }
                        }
                    }

                    if let Some(real_pos) = real_pos {
                        if win.playlist_selection() {
                            queue.select_song_at(real_pos);
                        } else if queue.current_song_index() != Some(real_pos) {
                            player.skip_to(real_pos);
                            player.play();
                        } else if !player.state().playing() {
                            player.play();
                        }
                    }
                }
            }
        ));

        imp.playlist_filtermodel
            .replace(Some(queue_model.upcast::<gio::ListModel>()));

    }

    /// Clears the playlist selection when the user clicks anywhere that is
    /// not an interactive control or a song row.
    fn setup_empty_click_deselection(&self) {
        let gesture = gtk::GestureClick::new();
        gesture.set_propagation_phase(gtk::PropagationPhase::Capture);
        gesture.connect_pressed(clone!(
            #[weak(rename_to = win)]
            self,
            move |gesture, _, x, y| {
                if !win.playlist_selection() {
                    return;
                }

                let Some(widget) = gesture.widget() else {
                    return;
                };
                let Some(picked) = widget.pick(x, y, gtk::PickFlags::DEFAULT) else {
                    return;
                };

                let mut node = Some(picked);
                while let Some(widget) = node {
                    if widget.is::<QueueRow>()
                        || widget.is::<gtk::Button>()
                        || widget.is::<gtk::ToggleButton>()
                        || widget.is::<gtk::MenuButton>()
                        || widget.is::<gtk::CheckButton>()
                        || widget.is::<gtk::Editable>()
                        || widget.is::<gtk::Range>()
                        || widget.is::<gtk::Switch>()
                        || widget.is::<WaveformView>()
                    {
                        return;
                    }
                    node = widget.parent();
                }

                win.set_playlist_selection(false);
                win.update_selected_count();
            }
        ));
        self.imp().main_stack.add_controller(gesture);
    }

    fn setup_drop_target(&self) {
        let drop_target = gtk::DropTarget::builder()
            .name("file-drop-target")
            .actions(gdk::DragAction::COPY)
            .formats(&gdk::ContentFormats::for_type(gdk::FileList::static_type()))
            .build();

        drop_target.connect_drop(clone!(
            #[weak(rename_to = win)]
            self,
            #[upgrade_or]
            false,
            move |_, value, _, _| {
                if let Ok(file_list) = value.get::<gdk::FileList>() {
                    if file_list.files().is_empty() {
                        win.add_toast(i18n("Unable to access dropped files"));
                        return false;
                    }

                    let model = gio::ListStore::new::<gio::File>();
                    for f in file_list.files() {
                        model.append(&f);
                    }
                    win.add_files_to_queue(model.upcast_ref::<gio::ListModel>());
                    return true;
                }

                false
            }
        ));

        self.imp().drag_overlay.set_drop_target(&drop_target);
    }

    fn update_play_button(&self) {
        if let Some(player) = self.player() {
            let state = player.state();
            let play_button = self.imp().playback_control.play_button();
            if state.playing() {
                play_button.set_icon_name("media-playback-pause-symbolic");
            } else {
                play_button.set_icon_name("media-playback-start-symbolic");
            }
        }
    }

    fn update_position_labels(&self) {
        if let Some(player) = self.player() {
            let state = player.state();
            if state.current_song().is_some() {
                let elapsed = state.position();
                let duration = state.duration();
                let remaining = duration.checked_sub(elapsed).unwrap_or_default();
                self.set_song_time(Some(elapsed), Some(remaining));

                let position = if duration > 0 {
                    state.position() as f64 / duration as f64
                } else {
                    0.0
                };
                self.set_song_position(position);
            } else {
                self.set_song_time(None, None);
                self.set_song_position(0.0);
            }
        }
    }

    fn update_song(&self) {
        if let Some(player) = self.player() {
            let state = player.state();
            self.scroll_playlist_to_song();
            self.update_playlist_time();
            self.update_title(state.current_song().as_ref());
            self.update_style(state.current_song().as_ref());
            self.update_favorite_button();
        }
    }

    fn update_cover(&self) {
        if let Some(player) = self.player() {
            let state = player.state();
            let song_cover = self.imp().song_cover.get();
            if let Some(cover) = state.cover() {
                song_cover.album_image().set_cover(Some(&cover));
                song_cover.show_cover_image(true);
            } else {
                song_cover.album_image().set_cover(None);
                song_cover.show_cover_image(false);
            }
        }
    }

    /// The song the cover context menu works on: whatever is playing now.
    fn current_cover_song(&self) -> Option<Song> {
        self.player()?.state().current_song()
    }

    fn setup_cover_context_menu(&self) {
        self.imp().song_cover.get().connect_closure(
            "cover-menu-requested",
            false,
            closure_local!(
                #[weak(rename_to = win)]
                self,
                move |_cover: SongCover, x: f64, y: f64| {
                    win.show_cover_context_menu(x, y);
                }
            ),
        );
    }

    fn show_cover_context_menu(&self, x: f64, y: f64) {
        // Without a song there is nothing to change or remove, so the menu
        // never appears over the empty artwork placeholder.
        let Some(song) = self.current_cover_song() else {
            return;
        };
        let Some(player) = self.player() else {
            return;
        };
        let has_cover = player.state().cover().is_some();

        let song_cover = self.imp().song_cover.get();
        // The guard must be dropped before popdown(): closing the previous
        // popover emits a signal that borrows this same field again.
        let previous = self.imp().cover_menu.borrow_mut().take();
        if let Some(previous) = previous {
            previous.popdown();
        }

        let popover = gtk::Popover::new();
        popover.set_has_arrow(true);
        popover.set_parent(&song_cover);
        popover.add_css_class("cover-context-menu");

        let content = gtk::Box::new(gtk::Orientation::Vertical, 2);
        content.set_size_request(190, -1);
        content.set_margin_top(6);
        content.set_margin_bottom(6);
        content.set_margin_start(6);
        content.set_margin_end(6);

        let change_button = cover_menu_button("document-edit-symbolic", &i18n("Change Cover…"));
        content.append(&change_button);

        let save_button = cover_menu_button("document-save-symbolic", &i18n("Save Cover"));
        save_button.set_sensitive(has_cover);
        content.append(&save_button);

        let remove_button = cover_menu_button("user-trash-symbolic", &i18n("Remove Cover"));
        // There is nothing to remove when only the embedded artwork is used
        // and the user has not hidden it yet.
        remove_button.set_sensitive(has_cover);
        content.append(&remove_button);

        let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
        separator.set_margin_top(4);
        separator.set_margin_bottom(2);
        content.append(&separator);

        let info_label = gtk::Label::new(Some(&i18n("Cover information…")));
        info_label.add_css_class("dim-label");
        info_label.add_css_class("caption");
        info_label.set_halign(gtk::Align::Center);
        info_label.set_xalign(0.5);
        info_label.set_margin_top(2);
        info_label.set_wrap(true);
        content.append(&info_label);

        let (sender, receiver) = async_channel::bounded(1);
        let (audio, fallback) = cover_source_paths(&song);
        std::thread::spawn(move || {
            let _ = sender.send_blocking(extract_cover_bytes(audio, fallback));
        });
        let label = info_label.downgrade();
        glib::MainContext::default().spawn_local(async move {
            let Ok(result) = receiver.recv().await else {
                return;
            };
            let Some(label) = label.upgrade() else {
                return;
            };
            let text = result
                .and_then(|(data, _)| cover_info_label(&data))
                .unwrap_or_else(|| i18n("Cover information unavailable"));
            label.set_label(&text);
        });

        popover.set_child(Some(&content));

        change_button.connect_clicked(clone!(
            #[weak(rename_to = win)]
            self,
            #[weak]
            popover,
            move |_| {
                popover.popdown();
                win.choose_cover_image();
            }
        ));
        save_button.connect_clicked(clone!(
            #[weak(rename_to = win)]
            self,
            #[weak]
            popover,
            move |_| {
                popover.popdown();
                win.save_current_cover();
            }
        ));
        remove_button.connect_clicked(clone!(
            #[weak(rename_to = win)]
            self,
            #[weak]
            popover,
            move |_| {
                popover.popdown();
                win.remove_current_cover();
            }
        ));

        popover.connect_closed(clone!(
            #[weak(rename_to = win)]
            self,
            move |popover| {
                let mut current = win.imp().cover_menu.borrow_mut();
                if current.as_ref().is_some_and(|open| open == popover) {
                    current.take();
                }
                popover.unparent();
            }
        ));

        self.imp().cover_menu.replace(Some(popover.clone()));
        let rectangle = gdk::Rectangle::new(x as i32, y as i32, 1, 1);
        popover.set_pointing_to(Some(&rectangle));
        popover.popup();
    }

    /// Opens a PNG/JPEG picker and, after the file has been chosen, asks
    /// where the new artwork should be used: one song or the whole album,
    /// with Cancel as the last button.
    fn choose_cover_image(&self) {
        let Some(song) = self.current_cover_song() else {
            return;
        };

        let window = self.downgrade();
        glib::MainContext::default().spawn_local(async move {
            let Some(window) = window.upgrade() else {
                return;
            };

            let filters = gio::ListStore::new::<gtk::FileFilter>();
            let filter = gtk::FileFilter::new();
            gtk::FileFilter::set_name(&filter, Some(&i18n("Image files")));
            filter.add_mime_type("image/png");
            filter.add_mime_type("image/jpeg");
            filters.append(&filter);

            let pictures = glib::user_special_dir(glib::UserDirectory::Pictures)
                .unwrap_or_else(glib::current_dir);
            let dialog = gtk::FileDialog::builder()
                .accept_label(&i18n("_Select"))
                .filters(&filters)
                .initial_folder(&gio::File::for_path(pictures))
                .modal(true)
                .title(&i18n("Choose Cover Image"))
                .build();

            let file = match dialog.open_future(Some(&window)).await {
                Ok(file) => file,
                // Dismissing the picker is not an error.
                Err(_) => return,
            };

            let Some(path) = file.path() else {
                window.add_toast(i18n("Unable to load image"));
                return;
            };
            let Some(image) = utils::load_selected_cover_image(&path) else {
                window.add_toast(i18n("Unable to load image"));
                return;
            };

            let has_album = song.cover_album_info().is_some();
            let Some(scope) = window.choose_cover_scope(has_album).await else {
                return;
            };

            let data = if image.width >= utils::COVER_IMPORT_LARGE_DIMENSION
                || image.height >= utils::COVER_IMPORT_LARGE_DIMENSION
            {
                match window.choose_cover_compression(&image).await {
                    Some(CoverCompression::Compressed(bytes)) => Some(bytes),
                    Some(CoverCompression::Original) => fs::read(&path).ok(),
                    // Cancelled: nothing is written.
                    None => return,
                }
            } else {
                fs::read(&path).ok()
            };

            let Some(data) = data else {
                window.add_toast(i18n("Unable to load image"));
                return;
            };

            window.apply_cover_data(&song, scope, &data).await;
        });
    }

    /// Extracts the artwork from the current song's metadata and lets the
    /// user pick where to save it as an image file.
    fn save_current_cover(&self) {
        let Some(song) = self.current_cover_song() else {
            return;
        };

        let window = self.downgrade();
        glib::MainContext::default().spawn_local(async move {
            let Some(window) = window.upgrade() else {
                return;
            };

            let (sender, receiver) = async_channel::bounded(1);
            let (audio, fallback) = cover_source_paths(&song);
            std::thread::spawn(move || {
                let _ = sender.send_blocking(extract_cover_bytes(audio, fallback));
            });

            let Some((data, extension)) = receiver.recv().await.ok().flatten() else {
                window.add_toast(i18n("No cover art found"));
                return;
            };

            let filters = gio::ListStore::new::<gtk::FileFilter>();
            let filter = gtk::FileFilter::new();
            gtk::FileFilter::set_name(&filter, Some(&i18n("PNG and JPEG images")));
            filter.add_mime_type("image/png");
            filter.add_mime_type("image/jpeg");
            filters.append(&filter);

            let name: String = format!("{} - {}.{}", song.title(), song.artist(), extension)
                .chars()
                .map(|c| match c {
                    '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
                    _ => c,
                })
                .collect();

            let pictures = glib::user_special_dir(glib::UserDirectory::Pictures)
                .unwrap_or_else(glib::current_dir);
            let dialog = gtk::FileDialog::builder()
                .accept_label(&i18n("_Save"))
                .filters(&filters)
                .initial_folder(&gio::File::for_path(pictures))
                .initial_name(name)
                .modal(true)
                .title(&i18n("Save Cover"))
                .build();

            let Ok(file) = dialog.save_future(Some(&window)).await else {
                return;
            };
            let Some(path) = file.path() else {
                return;
            };

            if let Err(err) = fs::write(&path, &data) {
                debug!("Unable to save cover image {:?}: {err}", path);
                window.add_toast(i18n("Unable to save the cover image"));
            }
        });
    }

    /// Asks whether the artwork applies to a single track or to the entire
    /// album. The Cancel button is added last so it sits at the end.
    async fn choose_cover_scope(&self, has_album: bool) -> Option<CoverScope> {
        if !has_album {
            return Some(CoverScope::Track);
        }

        let dialog = adw::AlertDialog::builder()
            .heading(&i18n("Apply Cover"))
            .body(&i18n("Where should the new cover be used?"))
            .build();
        dialog.add_response("song", &i18n("One Song"));
        dialog.add_response("album", &i18n("Entire Album"));
        dialog.add_response("cancel", &i18n("Cancel"));
        dialog.set_response_appearance("song", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("song"));
        dialog.set_close_response("cancel");

        match dialog.choose_future(Some(self)).await.as_str() {
            "song" => Some(CoverScope::Track),
            "album" => Some(CoverScope::Album),
            _ => None,
        }
    }

    /// Offers a compact JPEG copy for large photos. Cancel keeps the whole
    /// operation from writing anything.
    async fn choose_cover_compression(
        &self,
        image: &utils::SelectedCoverImage,
    ) -> Option<CoverCompression> {
        let size = format!("{} × {}", image.width, image.height);
        let body = i18n_f(
            "The selected image is {} pixels. Compress it to an 800×800 JPEG of about 100 KB? Choosing “Use Original” keeps the file untouched.",
            &[&size],
        );

        let dialog = adw::AlertDialog::builder()
            .heading(&i18n("Compress Cover"))
            .body(&body)
            .build();
        dialog.add_response("compress", &i18n("Compress"));
        dialog.add_response("original", &i18n("Use Original"));
        dialog.add_response("cancel", &i18n("Cancel"));
        dialog.set_response_appearance("compress", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("compress"));
        dialog.set_close_response("cancel");

        let response = dialog.choose_future(Some(self)).await;
        if response == "compress" {
            match utils::square_jpeg_cover(
                &image.pixbuf,
                utils::COVER_IMPORT_OUTPUT_SIZE,
                utils::COVER_IMPORT_TARGET_BYTES,
            ) {
                Some(bytes) => Some(CoverCompression::Compressed(bytes)),
                None => {
                    self.add_toast(i18n("Unable to compress the image"));
                    None
                }
            }
        } else if response == "original" {
            Some(CoverCompression::Original)
        } else {
            None
        }
    }

    /// Drops stale on-disk album thumbnails so the library grid does not keep
    /// showing the previous artwork after a tag rewrite.
    fn remove_cover_cache_files(&self, paths: &[PathBuf]) {
        let Some((album, artist)) = self
            .current_cover_song()
            .and_then(|song| song.cover_album_info())
        else {
            return;
        };

        for path in paths {
            let large = utils::album_cover_cache_path(&album, &artist, path);
            let small = utils::small_cover_cache_path(&large);
            remove_cover_file(&large);
            remove_cover_file(&small);
        }
    }

    /// Tag rewriting can touch dozens of files for an album, so it runs on a
    /// worker thread and the result is delivered back to the main context.
    async fn apply_cover_tags(
        &self,
        paths: Vec<PathBuf>,
        change: CoverTagChange,
    ) -> Result<(), String> {
        let (sender, receiver) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let mut error = None;
            for path in &paths {
                if let Err(err) = tag_writer::write_cover_tag(path, &change) {
                    error = Some(format!("{}: {err}", path.display()));
                }
            }
            let _ = sender.send_blocking(error);
        });

        match receiver.recv().await {
            Ok(None) => Ok(()),
            Ok(Some(err)) => Err(err),
            Err(err) => Err(err.to_string()),
        }
    }

    /// Writes the chosen artwork into the audio file tags and updates the
    /// local cover cache and everything that displays it.
    async fn apply_cover_data(&self, song: &Song, scope: CoverScope, data: &[u8]) {
        let paths: Vec<PathBuf> = match scope {
            CoverScope::Track => song.file().path().into_iter().collect(),
            CoverScope::Album => {
                let Some((album, artist)) = song.cover_album_info() else {
                    self.add_toast(i18n("Unable to save the cover"));
                    return;
                };
                self.album_track_paths(&album, &artist)
                    .into_iter()
                    .collect()
            }
        };

        if paths.is_empty() {
            self.add_toast(i18n("Unable to save the cover"));
            return;
        }

        let mime = tag_writer::mime_type_for_image(data);
        let change = CoverTagChange::Set {
            data: data.to_vec(),
            mime,
        };
        if let Err(err) = self.apply_cover_tags(paths.clone(), change).await {
            debug!("Unable to write cover tags: {err}");
            self.add_toast(i18n("Unable to update the audio file tags"));
        }
        // The local override still shows the new artwork even if one file
        // could not be written (for example because it is read-only).
        if let Ok(mut covers) = CoverCache::global().lock() {
            covers.clear();
        }
        // Drop stale on-disk thumbnails; they would otherwise keep showing
        // the old artwork in the library grid after the tags were rewritten.
        self.remove_cover_cache_files(&paths);

        match scope {
            CoverScope::Track => {
                let Some((image, hidden)) = song.cover_override_paths() else {
                    self.add_toast(i18n("Unable to save the cover"));
                    return;
                };

                if let Err(err) = write_cover_file(&image, data) {
                    debug!("Unable to write cover {:?}: {err}", image);
                    self.add_toast(i18n("Unable to save the cover"));
                    return;
                }

                remove_cover_file(&hidden);
                forget_thumbnail(&image);
                song.refresh_cover();
                self.refresh_song_display(&song);
            }
            CoverScope::Album => {
                let Some((image, hidden)) = song.album_cover_override_paths() else {
                    self.add_toast(i18n("Unable to save the cover"));
                    return;
                };
                let Some((album, artist)) = song.cover_album_info() else {
                    self.add_toast(i18n("Unable to save the cover"));
                    return;
                };

                if let Err(err) = write_cover_file(&image, data) {
                    debug!("Unable to write cover {:?}: {err}", image);
                    self.add_toast(i18n("Unable to save the cover"));
                    return;
                }

                remove_cover_file(&hidden);
                forget_thumbnail(&image);

                // Per-track artwork would win over the album cover and hide
                // the change on most songs, so it is dropped together with
                // the album-wide one.
                self.remove_album_track_overrides(&album, &artist);

                song.refresh_cover();
                self.refresh_queue_album(&image);
                self.invalidate_album_cover(&album, &artist);
                self.refresh_song_display(&song);
            }
        }
    }

    /// Confirms the removal and asks which scope to hide. Cancel is the last
    /// button in the dialog.
    fn remove_current_cover(&self) {
        let Some(song) = self.current_cover_song() else {
            return;
        };
        let has_album = song.cover_album_info().is_some();

        let window = self.downgrade();
        glib::MainContext::default().spawn_local(async move {
            let Some(window) = window.upgrade() else {
                return;
            };
            if let Some(scope) = window.choose_cover_removal(has_album).await {
                window.remove_cover_for(&song, scope).await;
            }
        });
    }

    async fn choose_cover_removal(&self, has_album: bool) -> Option<CoverScope> {
        let dialog = adw::AlertDialog::builder()
            .heading(&i18n("Remove Cover"))
            .body(&i18n(
                "Hide the artwork for this song only, or for the whole album? The cover will be removed from the audio file tags; audio data is not changed.",
            ))
            .build();
        dialog.add_response("song", &i18n("Remove from Song"));
        dialog.add_response("album", &i18n("Remove from Entire Album"));
        dialog.add_response("cancel", &i18n("Cancel"));
        if !has_album {
            dialog.set_response_enabled("album", false);
        }
        dialog.set_default_response(Some("song"));
        dialog.set_close_response("cancel");

        match dialog.choose_future(Some(self)).await.as_str() {
            "song" => Some(CoverScope::Track),
            "album" => Some(CoverScope::Album),
            _ => None,
        }
    }

    /// Hides the artwork in the scope chosen in the confirmation dialog. The
    /// front cover is removed from the audio file tags; local markers keep
    /// folder-level cover files (`Cover.jpg`) from reappearing.
    async fn remove_cover_for(&self, song: &Song, scope: CoverScope) {
        let paths: Vec<PathBuf> = match scope {
            CoverScope::Track => song.file().path().into_iter().collect(),
            CoverScope::Album => {
                let Some((album, artist)) = song.cover_album_info() else {
                    self.add_toast(i18n("Unable to remove the cover"));
                    return;
                };
                self.album_track_paths(&album, &artist)
                    .into_iter()
                    .collect()
            }
        };

        if paths.is_empty() {
            self.add_toast(i18n("Unable to remove the cover"));
            return;
        }

        if let Err(err) = self
            .apply_cover_tags(paths.clone(), CoverTagChange::Remove)
            .await
        {
            debug!("Unable to remove cover tags: {err}");
            self.add_toast(i18n("Unable to update the audio file tags"));
        }
        // Drop stale on-disk thumbnails; they would otherwise keep showing
        // the old artwork in the library grid after the tags were rewritten.
        self.remove_cover_cache_files(&paths);
        if let Ok(mut covers) = CoverCache::global().lock() {
            covers.clear();
        }

        match scope {
            CoverScope::Track => {
                let Some((image, hidden)) = song.cover_override_paths() else {
                    self.add_toast(i18n("Unable to remove the cover"));
                    return;
                };

                remove_cover_file(&image);
                forget_thumbnail(&image);
                if let Err(err) = write_cover_marker(&hidden) {
                    debug!("Unable to hide cover {:?}: {err}", hidden);
                    self.add_toast(i18n("Unable to remove the cover"));
                    return;
                }

                song.refresh_cover();
                self.refresh_song_display(song);
            }
            CoverScope::Album => {
                let Some((album_image, album_hidden)) = song.album_cover_override_paths() else {
                    self.add_toast(i18n("Unable to remove the cover"));
                    return;
                };
                let Some((album, artist)) = song.cover_album_info() else {
                    self.add_toast(i18n("Unable to remove the cover"));
                    return;
                };

                remove_cover_file(&album_image);
                forget_thumbnail(&album_image);
                if let Err(err) = write_cover_marker(&album_hidden) {
                    debug!("Unable to hide cover {:?}: {err}", album_hidden);
                    self.add_toast(i18n("Unable to remove the cover"));
                    return;
                }

                // An individual track override would win over the album
                // marker, so every track of this album is hidden as well.
                self.hide_album_tracks(&album, &artist);

                song.refresh_cover();
                self.refresh_queue_album(&album_image);
                self.invalidate_album_cover(&album, &artist);
                self.refresh_song_display(song);
            }
        }
    }

    /// Every track of an album, both from the library index and from the
    /// current queue (which may contain files the index has not seen yet).
    fn album_track_paths(&self, album: &str, artist: &str) -> HashSet<PathBuf> {
        let mut paths: HashSet<PathBuf> = HashSet::new();

        if let Some(library) = self.library() {
            if let Ok(tracks) = library.tracks_for_album(album, artist) {
                paths.extend(tracks);
            }
        }

        if let Some(player) = self.player() {
            let queue = player.queue();
            for position in 0..queue.n_songs() {
                if let Some(song) = queue.song_at(position) {
                    let belongs = song
                        .cover_album_info()
                        .is_some_and(|(song_album, song_artist)| {
                            song_album == album && song_artist == artist
                        });
                    if belongs {
                        if let Some(path) = song.file().path() {
                            paths.insert(path);
                        }
                    }
                }
            }
        }

        paths
    }

    /// Drops the per-track overrides of every song in an album, so an
    /// album-wide cover is not shadowed by the older track-level one.
    fn remove_album_track_overrides(&self, album: &str, artist: &str) {
        for path in self.album_track_paths(album, artist) {
            let image = utils::track_cover_override_path(&path);
            let hidden = utils::track_cover_hidden_marker_path(&path);
            if image.is_file() || hidden.is_file() {
                forget_thumbnail(&image);
                remove_cover_file(&image);
                remove_cover_file(&hidden);
            }
        }
    }

    /// Hides the cover for every track of an album, including tracks that
    /// currently have their own individual override.
    fn hide_album_tracks(&self, album: &str, artist: &str) {
        for path in self.album_track_paths(album, artist) {
            let image = utils::track_cover_override_path(&path);
            let hidden = utils::track_cover_hidden_marker_path(&path);
            if image.is_file() {
                forget_thumbnail(&image);
                remove_cover_file(&image);
            }
            if let Err(err) = write_cover_marker(&hidden) {
                debug!("Unable to hide cover {:?}: {err}", hidden);
            }
        }
    }

    /// Reloads the queued songs that share an album cover path.
    fn refresh_queue_album(&self, album_image: &Path) {
        let Some(player) = self.player() else {
            return;
        };

        let queue = player.queue();
        for position in 0..queue.n_songs() {
            if let Some(song) = queue.song_at(position) {
                let belongs = song
                    .album_cover_override_paths()
                    .is_some_and(|(image, _)| image == album_image);
                if belongs {
                    song.refresh_cover();
                }
            }
        }
    }

    /// Drops the cached album artwork and tells the visible grid tiles to
    /// load the new cover.
    fn invalidate_album_cover(&self, album: &str, artist: &str) {
        Album::clear_cover_cache();

        for object in self.imp().library_albums.borrow().iter() {
            let record = object.record();
            if record.title == album && record.artist == artist {
                object.notify_cover_changed();
            }
        }
    }

    /// Redraws the player cover and the palette after the artwork changed.
    fn refresh_song_display(&self, song: &Song) {
        let Some(player) = self.player() else {
            return;
        };

        let current = player.state().current_song();
        let is_current = current
            .as_ref()
            .is_some_and(|current| current.uri() == song.uri());
        if !is_current {
            return;
        }

        if let Some(current) = current.as_ref() {
            current.refresh_cover();
        }
        self.update_cover();
        self.update_style(current.as_ref());
    }

    fn update_playlist_time(&self) {
        if let Some(player) = self.player() {
            let queue = player.queue();
            let n_songs = queue.n_songs();
            if let Some(current) = queue.current_song_index() {
                let mut remaining_time = 0;
                for pos in 0..n_songs {
                    let song = queue.song_at(pos).unwrap();
                    if pos >= current {
                        remaining_time += song.duration();
                    }
                }

                let remaining_min = ((remaining_time - (remaining_time % 60)) / 60) as u32;
                let remaining_hrs = (remaining_min - (remaining_min % 60)) / 60;

                let remaining_str = if remaining_hrs > 0 {
                    // Translators: the `{}` must be left unmodified, and
                    // it will be replaced by the number of minutes remaining
                    // in the string "N hours M minutes remaining"
                    let minutes = ni18n_f(
                        "{} minute",
                        "{} minutes",
                        remaining_min % 60,
                        &[&(remaining_min % 60).to_string()],
                    );

                    // Translators: `{hours}` and `{minutes}` must be left
                    // unmodified, and they will be replaced by number of
                    // hours and by the translated number of minutes
                    // remaining, respectively
                    ni18n_k(
                        "{hours} hour {minutes} remaining",
                        "{hours} hours {minutes} remaining",
                        remaining_hrs,
                        &[("hours", &remaining_hrs.to_string()), ("minutes", &minutes)],
                    )
                } else {
                    ni18n_f(
                        // Translators: the '{}' must be left unmodified, and
                        // it will be replaced by the number of minutes remaining
                        // in the playlist
                        "{} minute remaining",
                        "{} minutes remaining",
                        remaining_min,
                        &[&remaining_min.to_string()],
                    )
                };

                self.imp()
                    .playlist_view
                    .queue_length_label()
                    .set_label(&remaining_str);
                self.imp()
                    .playlist_view
                    .queue_length_label()
                    .set_visible(true);
            } else {
                self.imp()
                    .playlist_view
                    .queue_length_label()
                    .set_visible(false);
            }
        }
    }

    fn scroll_playlist_to_song(&self) {
        let queue_view = self.imp().playlist_view.queue_view();
        if let Some(player) = self.player() {
            if let Some(current_idx) = player.queue().current_song_index() {
                debug!("Scrolling playlist to {}", current_idx);
                queue_view
                    .upcast_ref::<gtk::Widget>()
                    .activate_action("list.scroll-to-item", Some(&current_idx.to_variant()))
                    .expect("Failed to activate action");
            }
        }
    }

    fn setup_provider(&self) {
        let imp = self.imp();
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(&display, &imp.provider, 400);
        }
    }

    fn update_style(&self, song: Option<&Song>) {
        let imp = self.imp();

        if !imp.settings.boolean("enable-recoloring") {
            imp.provider.load_from_string("");
            imp.main_stack.remove_css_class("main-window");
            return;
        }

        if let Some(song) = song {
            if let Some(bg_colors) = song.cover_palette() {
                let mut css = String::new();

                // GTK 4.14 (and libadwaita 1.5) do not support CSS custom
                // properties, so the palette is exposed as named colors.
                let n_colors = bg_colors.len();
                for (i, color) in bg_colors.iter().enumerate().take(n_colors) {
                    css.push_str(&format!("@define-color background_color_{i} {color};"));
                }

                for i in n_colors..5 {
                    css.push_str(&format!(
                        "@define-color background_color_{i} @window_bg_color;"
                    ));
                }

                imp.provider.load_from_string(&css);
                if !imp.main_stack.has_css_class("main-window") {
                    imp.main_stack.add_css_class("main-window");
                }

                return;
            }
        }

        imp.provider.load_from_string("");
        imp.main_stack.remove_css_class("main-window");
    }

    fn update_title(&self, song: Option<&Song>) {
        if let Some(song) = song {
            self.set_title(Some(&format!("{} - {}", song.artist(), song.title())));
        } else {
            self.set_title(Some("Amberol"));
        }
    }

    fn update_selected_count(&self) {
        if let Some(player) = self.player() {
            let queue = player.queue();
            let n_selected = queue.n_selected_songs();

            let selected_str = if n_selected == 0 {
                i18n("No song selected")
            } else {
                ni18n_f(
                    // Translators: The '{}' must be left unmodified, and
                    // it is expanded to the number of songs selected
                    "{} song selected",
                    "{} songs selected",
                    n_selected,
                    &[&n_selected.to_string()],
                )
            };

            self.imp()
                .playlist_view
                .queue_selected_label()
                .set_label(&selected_str);
        }
    }

    fn playlist_song_position(&self, song: &Song) -> Option<u32> {
        let model = self.imp().playlist_filtermodel.borrow().clone()?;
        (0..model.n_items()).find(|&position| {
            model
                .item(position)
                .and_then(|item| item.downcast::<Song>().ok())
                .is_some_and(|candidate| candidate.as_ptr() == song.as_ptr())
        })
    }

    fn select_playlist_song(&self, song: &Song, shift: bool, control: bool) {
        let Some(model) = self.imp().playlist_filtermodel.borrow().clone() else {
            return;
        };
        let Some(position) = self.playlist_song_position(song) else {
            return;
        };

        let imp = self.imp();
        if shift {
            let anchor = imp.playlist_selection_anchor.get().unwrap_or(position);
            if !control {
                if let Some(player) = self.player() {
                    player.queue().unselect_all_songs();
                }
            }
            let start = anchor.min(position);
            let end = anchor.max(position);
            for pos in start..=end {
                if let Some(candidate) = model
                    .item(pos)
                    .and_then(|item| item.downcast::<Song>().ok())
                {
                    candidate.set_selected(true);
                }
            }
            imp.playlist_selection_anchor.set(Some(position));
        } else if control {
            song.set_selected(!song.selected());
            imp.playlist_selection_anchor.set(Some(position));
        }

        let has_selection = self
            .player()
            .is_some_and(|player| player.queue().n_selected_songs() > 0);
        self.set_playlist_selection(has_selection);
        self.update_selected_count();
    }

    fn select_all_playlist_songs(&self) {
        let Some(player) = self.player() else {
            return;
        };
        let queue = player.queue();
        let n_songs = queue.n_songs();
        if n_songs == 0 {
            return;
        }

        // The button is a toggle: pressing it again while every song is
        // selected clears the selection and leaves selection mode.
        if queue.n_selected_songs() == n_songs {
            queue.unselect_all_songs();
            self.imp().playlist_selection_anchor.set(None);
            self.set_playlist_selection(false);
            self.update_selected_count();
            return;
        }

        for position in 0..n_songs {
            if let Some(song) = queue.song_at(position) {
                song.set_selected(true);
            }
        }

        self.imp().playlist_selection_anchor.set(Some(0));
        self.set_playlist_selection(true);
        self.update_selected_count();
    }

    /// Songs affected by a playlist context-menu action. If the clicked song
    /// is part of the current selection, the whole selection is targeted;
    /// otherwise only the clicked song is affected.
    fn context_songs(&self, clicked: &Song) -> Vec<Song> {
        let Some(player) = self.player() else {
            return vec![clicked.clone()];
        };
        let queue = player.queue();
        let selected: Vec<Song> = (0..queue.n_songs())
            .filter_map(|position| queue.song_at(position))
            .filter(Song::selected)
            .collect();

        if selected
            .iter()
            .any(|song| song.as_ptr() == clicked.as_ptr())
        {
            selected
        } else {
            vec![clicked.clone()]
        }
    }

    fn toggle_favorite_for_songs(&self, songs: &[Song]) {
        let Some(library) = self.library() else {
            return;
        };

        let mut albums: Vec<(String, String)> = Vec::new();
        for song in songs {
            if let Some(key) = self.song_album_key(song) {
                if !albums.contains(&key) {
                    albums.push(key);
                }
            }
        }
        if albums.is_empty() {
            return;
        }

        let all_favorite = albums.iter().all(|(album, artist)| {
            library
                .is_album_favorite(album, artist)
                .unwrap_or(false)
        });
        let favorite = !all_favorite;
        let mut changed = false;

        for (album, artist) in &albums {
            let current = library
                .is_album_favorite(album, artist)
                .unwrap_or(false);
            if current == favorite {
                continue;
            }
            if library
                .set_album_favorite(album, artist, favorite)
                .is_ok()
            {
                changed = true;
            }
        }

        if changed {
            self.favorite_state_changed();
        }
    }

    fn remove_songs_from_playlist(&self, songs: &[Song]) {
        let Some(player) = self.player() else {
            return;
        };
        for song in songs {
            self.remove_song(song);
        }
        // The removed tracks' artwork is held in shared caches, not in the
        // queue objects themselves, so it must be released separately.
        reclaim();
        utils::store_playlist(&player);
        self.imp().playlist_selection_anchor.set(None);
        self.update_selected_count();
    }

    fn play_playlist_song(&self, song: &Song) {
        let Some(player) = self.player() else {
            return;
        };
        let queue = player.queue();
        for position in 0..queue.n_songs() {
            if queue
                .song_at(position)
                .is_some_and(|candidate| candidate.as_ptr() == song.as_ptr())
            {
                if queue.current_song_index() != Some(position) {
                    player.skip_to(position);
                }
                player.play();
                return;
            }
        }
    }

    fn add_songs_to_queue_end(&self, songs: &[Song]) {
        let Some(player) = self.player() else {
            return;
        };
        let queue = player.queue();
        queue.add_songs(songs);
        utils::store_playlist(&player);
    }

    fn play_songs_next(&self, songs: &[Song]) {
        let Some(player) = self.player() else {
            return;
        };
        let queue = player.queue();
        if queue.is_empty() || queue.current_song().is_none() {
            if let Some(first) = songs.first() {
                self.play_playlist_song(first);
            }
            return;
        }
        if queue.insert_songs_after_current(songs) {
            utils::store_playlist(&player);
            self.add_toast(i18n("Will Play Next"));
        }
    }

    fn handle_song_context_action(&self, song: &Song, action: &str) {
        match action {
            "play" => self.play_playlist_song(song),
            "queue" => {
                let songs = self.context_songs(song);
                self.add_songs_to_queue_end(&songs);
            }
            "play-next" => {
                let songs = self.context_songs(song);
                self.play_songs_next(&songs);
            }
            "favorite" => {
                let songs = self.context_songs(song);
                self.toggle_favorite_for_songs(&songs);
            }
            "remove" => {
                let songs = self.context_songs(song);
                self.remove_songs_from_playlist(&songs);
            }
            "delete" => {
                let songs = self.context_songs(song);
                self.delete_songs_from_device(&songs);
            }
            _ => debug!("Unknown song context action: {action}"),
        }
    }

    pub fn open_files(&self, files: &[gio::File]) {
        if files.is_empty() {
            self.add_toast(i18n("Unable to access files"));
            return;
        }

        let model = gio::ListStore::new::<gio::File>();
        for f in files {
            model.append(f);
        }
        self.add_files_to_queue(model.upcast_ref::<gio::ListModel>());
    }

    pub fn remove_song(&self, song: &Song) {
        if let Some(p) = self.player() {
            p.remove_song(song);
        }
        self.update_selected_count();
        self.update_playlist_time();
    }

    pub fn add_toast(&self, msg: String) {
        let toast = adw::Toast::new(&msg);
        self.imp().toast_overlay.add_toast(toast);
    }

    /// Shows a single "songs added" toast even when several files are opened
    /// in quick succession. Rapid additions are coalesced into one plural
    /// notification instead of stacking one toast per file.
    fn queue_added_songs_toast(&self, count: u32) {
        let imp = self.imp();
        imp.added_songs_count
            .set(imp.added_songs_count.get().saturating_add(count));

        // Reset the debounce on every new batch. When several files are opened
        // at once, only the final count is announced.
        if let Some(source) = imp.added_songs_source.borrow_mut().take() {
            source.remove();
        }

        let source = glib::timeout_add_local_once(
            Duration::from_millis(800),
            clone!(
                #[weak(rename_to = win)]
                self,
                move || {
                    let count = win.imp().added_songs_count.replace(0);
                    win.imp().added_songs_source.replace(None);
                    if count == 0 {
                        return;
                    }

                    let msg = if count == 1 {
                        i18n("Added a new song")
                    } else {
                        i18n("Added new songs")
                    };

                    // Never leave more than one "songs added" toast on screen.
                    if let Some(toast) = win.imp().added_songs_toast.borrow_mut().take() {
                        if let Some(toast) = toast.upgrade() {
                            toast.dismiss();
                        }
                    }
                    let toast = adw::Toast::new(&msg);
                    win.imp()
                        .added_songs_toast
                        .replace(Some(toast.downgrade()));
                    win.imp().toast_overlay.add_toast(toast);
                }
            ),
        );
        imp.added_songs_source.replace(Some(source));
    }

    fn copy_song(&self) {
        if let Some(player) = self.player() {
            let state = player.state();
            if let Some(song) = state.current_song() {
                let s = i18n_k(
                    // Translators: `{title}` and `{artist}` must be left
                    // untranslated; they will expand to the title and
                    // artist of the currently playing song, respectively
                    "Currently playing “{title}” by “{artist}”",
                    &[("title", &song.title()), ("artist", &song.artist())],
                );
                self.clipboard().set_text(&s);
            }
        }
    }

    async fn open_song_directory(&self) {
        if let Some(player) = self.player() {
            let state = player.state();

            if let Some(song) = state.current_song() {
                let file = song.file();

                match file
                    .query_info_future(
                        &ATTRIBUTE_HOST_PATH,
                        gio::FileQueryInfoFlags::NONE,
                        glib::Priority::DEFAULT,
                    )
                    .await
                {
                    Err(err) => debug!("Unable to get host-path attribute: {err}"),
                    Ok(info) => {
                        let host_path = info
                            .attribute_as_string(&ATTRIBUTE_HOST_PATH)
                            .and_then(|x| PathBuf::from_str(x.as_str()).ok());

                        if let Some(path) = host_path.or_else(|| file.path()) {
                            let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(path)));
                            match launcher
                                .open_containing_folder_future(Some(
                                    self.upcast_ref::<gtk::Window>(),
                                ))
                                .await
                            {
                                Err(_e) => self.add_toast(i18n("Unable to open folder")),
                                Ok(()) => {}
                            }
                        }
                    }
                }
            }
        }
    }

    pub fn switch_mode(&self, mode: WindowMode) {
        let stack = self.imp().main_stack.get();
        match mode {
            WindowMode::InitialView => {
                stack.set_visible_child_name("initial-view");
                self.set_default_widget(Some(&self.imp().add_folder_button.get()));
            }
            WindowMode::MainView => {
                stack.set_visible_child_name("main-view");
                self.set_default_widget(Some(&self.imp().playback_control.play_button()));
            }
            WindowMode::LibraryView => {
                stack.set_visible_child_name("library-view");
                self.set_default_widget(None::<&gtk::Widget>);
            }
        };
    }

    pub fn set_replaygain(&self, replaygain: ReplayGainMode) {
        let imp = self.imp();

        if replaygain != imp.replaygain_mode.replace(replaygain) {
            if let Some(p) = self.player() {
                p.set_replaygain(replaygain);
            }
            self.imp()
                .settings
                .set_enum("replay-gain", replaygain.into())
                .expect("Unable to store setting");

            self.notify("replaygain-mode");
        }
    }

    pub fn replaygain(&self) -> ReplayGainMode {
        self.imp().replaygain_mode.get()
    }

    pub fn set_song_time(&self, elapsed: Option<u64>, remaining: Option<u64>) {
        if let Some(elapsed) = elapsed {
            self.imp()
                .elapsed_label
                .set_text(&utils::format_time(elapsed as i64));
        } else {
            self.imp().elapsed_label.set_text("0:00");
        }

        if let Some(remaining) = remaining {
            self.imp()
                .remaining_label
                .set_text(&utils::format_remaining_time(remaining as i64));
        } else {
            self.imp().remaining_label.set_text("0:00");
        }
    }

    pub fn set_song_position(&self, position: f64) {
        self.imp().waveform_view.set_position(position);
    }
}
