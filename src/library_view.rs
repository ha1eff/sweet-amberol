// SPDX-FileCopyrightText: 2026  Amberol contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::cell::{Cell, RefCell};

use adw::prelude::AnimationExt;
use adw::subclass::prelude::*;
use glib::{clone, closure_local};
use gtk::{gdk, gio, glib, prelude::*, CompositeTemplate};
use once_cell::sync::Lazy;

use crate::{
    album::Album,
    album_tile::AlbumTile,
    i18n::{i18n, ni18n_f},
};

const ALBUM_FADE_OUT_MS: u32 = 120;
const ALBUM_FADE_IN_MS: u32 = 180;

pub struct AlbumRefresh(Box<dyn FnOnce() -> Option<Vec<Album>>>);

impl std::fmt::Debug for AlbumRefresh {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AlbumRefresh")
    }
}

mod imp {
    use super::*;

    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub enum FadePhase {
        #[default]
        Idle,
        FadingOut,
        FadingIn,
    }

    #[derive(Debug, CompositeTemplate)]
    #[template(resource = "/io/bassi/Amberol/library-view.ui")]
    pub struct LibraryView {
        #[template_child]
        pub albums_grid: TemplateChild<gtk::GridView>,
        #[template_child]
        pub albums_scroll: TemplateChild<gtk::ScrolledWindow>,
        #[template_child]
        pub library_stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub title_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub subtitle_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub recent_button: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub random_button: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub favorites_button: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub play_albums_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub title_stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub search_entry: TemplateChild<gtk::SearchEntry>,
        #[template_child]
        pub search_button: TemplateChild<gtk::ToggleButton>,

        pub store: gio::ListStore,
        pub scanning: Cell<bool>,
        pub updating_buttons: Cell<bool>,
        pub random_mode: Cell<bool>,
        pub favorites_mode: Cell<bool>,
        pub artist_mode: Cell<bool>,
        pub selection_anchor: Cell<Option<u32>>,
        pub selected_count: Cell<u32>,
        pub tile_clicked: Cell<bool>,
        pub base_subtitle: RefCell<String>,
        pub search_mode: Cell<bool>,
        pub search_results_mode: Cell<bool>,
        pub suppress_search_signal: Cell<bool>,
        pub search_source: RefCell<Option<glib::SourceId>>,
        /// Infinite scroll is only meaningful on the Recent Albums page.
        pub recent_pagination: Cell<bool>,
        pub loading_more: Cell<bool>,
        pub animation: RefCell<Option<adw::TimedAnimation>>,
        pub animation_generation: Cell<u64>,
        pub fade_phase: Cell<FadePhase>,
        pub pending_refresh: RefCell<Option<AlbumRefresh>>,
    }

    impl Default for LibraryView {
        fn default() -> Self {
            Self {
                albums_grid: TemplateChild::default(),
                albums_scroll: TemplateChild::default(),
                library_stack: TemplateChild::default(),
                title_label: TemplateChild::default(),
                subtitle_label: TemplateChild::default(),
                recent_button: TemplateChild::default(),
                random_button: TemplateChild::default(),
                favorites_button: TemplateChild::default(),
                play_albums_button: TemplateChild::default(),
                title_stack: TemplateChild::default(),
                search_entry: TemplateChild::default(),
                search_button: TemplateChild::default(),
                store: gio::ListStore::new::<Album>(),
                scanning: Cell::new(false),
                updating_buttons: Cell::new(false),
                random_mode: Cell::new(false),
                favorites_mode: Cell::new(false),
                artist_mode: Cell::new(false),
                selection_anchor: Cell::new(None),
                selected_count: Cell::new(0),
                tile_clicked: Cell::new(false),
                base_subtitle: RefCell::new(String::new()),
                search_mode: Cell::new(false),
                search_results_mode: Cell::new(false),
                suppress_search_signal: Cell::new(false),
                search_source: RefCell::default(),
                recent_pagination: Cell::new(false),
                loading_more: Cell::new(false),
                animation: RefCell::default(),
                animation_generation: Cell::new(0),
                fade_phase: Cell::new(FadePhase::Idle),
                pending_refresh: RefCell::default(),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for LibraryView {
        const NAME: &'static str = "AmberolLibraryView";
        type Type = super::LibraryView;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            Self::bind_template(klass);

            klass.set_layout_manager_type::<gtk::BinLayout>();
            klass.set_css_name("libraryview");
            klass.set_accessible_role(gtk::AccessibleRole::Group);
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            AlbumTile::static_type();
            obj.init_template();
        }
    }

    impl ObjectImpl for LibraryView {
        fn constructed(&self) {
            self.parent_constructed();

            let obj = self.obj();
            self.random_button
                .set_group(Some(&self.recent_button.get()));
            self.favorites_button
                .set_group(Some(&self.recent_button.get()));
            self.recent_button.set_active(true);

            // Toggle buttons in a group cannot be deactivated by clicking the
            // active one, so `toggled` would never fire on a repeated click.
            // Using `clicked` lets us reshuffle when Random Albums is pressed
            // again while it is already selected.
            self.recent_button.connect_clicked(clone!(
                #[weak]
                obj,
                move |button| {
                    if !button.is_active() {
                        button.set_active(true);
                    }
                    // The Recent button must also leave an artist page, even
                    // though that page keeps the Recent tab selected.
                    if !obj.imp().updating_buttons.get()
                        && (obj.imp().random_mode.get()
                            || obj.imp().favorites_mode.get()
                            || obj.imp().artist_mode.get())
                    {
                        obj.emit_by_name::<()>("mode-changed", &[&false]);
                    }
                }
            ));

            self.random_button.connect_clicked(clone!(
                #[weak]
                obj,
                move |button| {
                    if !button.is_active() {
                        button.set_active(true);
                    }
                    if !obj.imp().updating_buttons.get() {
                        // This also covers pressing the already active
                        // Random Albums button, which reshuffles the grid.
                        obj.emit_by_name::<()>("mode-changed", &[&true]);
                    }
                }
            ));

            self.favorites_button.connect_clicked(clone!(
                #[weak]
                obj,
                move |button| {
                    if !button.is_active() {
                        button.set_active(true);
                    }
                    if !obj.imp().updating_buttons.get() {
                        obj.emit_by_name::<()>("favorites-mode-changed", &[]);
                    }
                }
            ));

            obj.setup_search();
            obj.setup_select_all();
            obj.setup_grid();
        }

        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: Lazy<Vec<glib::subclass::Signal>> = Lazy::new(|| {
                vec![
                    glib::subclass::Signal::builder("album-activated")
                        .param_types([Album::static_type()])
                        .build(),
                    glib::subclass::Signal::builder("mode-changed")
                        .param_types([bool::static_type()])
                        .build(),
                    glib::subclass::Signal::builder("favorites-mode-changed").build(),
                    glib::subclass::Signal::builder("artist-activated")
                        .param_types([String::static_type()])
                        .build(),
                    glib::subclass::Signal::builder("albums-context-action")
                        .param_types([gio::ListStore::static_type(), String::static_type()])
                        .build(),
                    glib::subclass::Signal::builder("search-changed")
                        .param_types([String::static_type()])
                        .build(),
                    glib::subclass::Signal::builder("search-toggled")
                        .param_types([bool::static_type()])
                        .build(),
                    glib::subclass::Signal::builder("load-more-albums").build(),
                ]
            });
            SIGNALS.as_ref()
        }

        fn dispose(&self) {
            self.obj().cancel_album_animation();
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for LibraryView {}
}

glib::wrapper! {
    pub struct LibraryView(ObjectSubclass<imp::LibraryView>)
        @extends gtk::Widget,
        @implements gio::ActionGroup, gio::ActionMap, gtk::Accessible,
                    gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for LibraryView {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl LibraryView {
    pub fn new() -> Self {
        Self::default()
    }

    fn setup_search(&self) {
        let imp = self.imp();

        imp.search_button.connect_clicked(clone!(
            #[weak(rename_to = view)]
            self,
            move |button| {
                if view.imp().suppress_search_signal.get() {
                    return;
                }
                let active = button.is_active();
                view.imp().search_mode.set(active);
                view.imp().title_stack.set_visible_child_name(if active {
                    "search"
                } else {
                    "title"
                });
                if active {
                    view.imp().search_entry.grab_focus();
                } else {
                    view.imp().search_entry.set_text("");
                }
                view.emit_by_name::<()>("search-toggled", &[&active]);
            }
        ));

        imp.search_entry.connect_search_changed(clone!(
            #[weak(rename_to = view)]
            self,
            move |entry| {
                let imp = view.imp();
                if let Some(source) = imp.search_source.borrow_mut().take() {
                    source.remove();
                }

                let text = entry.text().to_string();
                let source = glib::timeout_add_local_once(
                    std::time::Duration::from_secs(1),
                    clone!(
                        #[weak]
                        view,
                        move || {
                            view.imp().search_source.borrow_mut().take();
                            view.emit_by_name::<()>("search-changed", &[&text]);
                        }
                    ),
                );
                imp.search_source.replace(Some(source));
            }
        ));

        imp.search_entry.connect_stop_search(clone!(
            #[weak(rename_to = view)]
            self,
            move |_| {
                if view.imp().search_button.is_active() {
                    view.imp().search_button.set_active(false);
                }
            }
        ));
    }

    fn setup_select_all(&self) {
        let controller = gtk::EventControllerKey::new();
        controller.connect_key_pressed(clone!(
            #[weak(rename_to = view)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, key, _, modifiers| {
                if view.imp().search_mode.get() {
                    return glib::Propagation::Proceed;
                }
                if modifiers.contains(gdk::ModifierType::CONTROL_MASK)
                    && matches!(key, gdk::Key::a | gdk::Key::A)
                {
                    view.select_all();
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            }
        ));
        self.add_controller(controller);
    }

    fn select_all(&self) {
        let imp = self.imp();
        for position in 0..imp.store.n_items() {
            if let Some(album) = imp
                .store
                .item(position)
                .and_then(|item| item.downcast::<Album>().ok())
            {
                album.set_selected(true);
            }
        }
        if imp.store.n_items() > 0 {
            imp.selection_anchor.set(Some(0));
        }
        self.update_selection_count();
    }

    /// Leaves search mode and restores the normal header.
    pub fn cancel_search(&self) {
        let imp = self.imp();
        imp.suppress_search_signal.set(true);
        if imp.search_button.is_active() {
            imp.search_button.set_active(false);
        }
        imp.search_mode.set(false);
        imp.title_stack.set_visible_child_name("title");
        imp.search_entry.set_text("");
        imp.suppress_search_signal.set(false);
    }

    fn setup_grid(&self) {
        let imp = self.imp();

        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(clone!(
            #[weak(rename_to = view)]
            self,
            move |_, item| {
                let list_item = item.downcast_ref::<gtk::ListItem>().unwrap();
                let tile = AlbumTile::new();
                tile.connect_closure(
                    "artist-activated",
                    false,
                    closure_local!(
                        #[weak]
                        view,
                        move |_tile: AlbumTile, artist: String| {
                            if !view.imp().artist_mode.get() {
                                view.emit_by_name::<()>("artist-activated", &[&artist]);
                            }
                        }
                    ),
                );
                tile.connect_closure(
                    "album-clicked",
                    false,
                    closure_local!(
                        #[weak]
                        view,
                        move |_tile: AlbumTile, album: Album| {
                            view.imp().tile_clicked.set(true);
                            view.handle_plain_click(&album);
                        }
                    ),
                );
                tile.connect_closure(
                    "selection-clicked",
                    false,
                    closure_local!(
                        #[weak]
                        view,
                        move |_tile: AlbumTile, album: Album, shift: bool, control: bool| {
                            view.imp().tile_clicked.set(true);
                            view.handle_selection_clicked(&album, shift, control);
                        }
                    ),
                );
                tile.connect_closure(
                    "album-context-action",
                    false,
                    closure_local!(
                        #[weak]
                        view,
                        move |_tile: AlbumTile, album: Album, action: String| {
                            let albums = view.context_albums(&album);
                            let model = gio::ListStore::new::<Album>();
                            for album in &albums {
                                model.append(album);
                            }
                            view.emit_by_name::<()>("albums-context-action", &[&model, &action]);
                        }
                    ),
                );
                list_item.set_child(Some(&tile));
            }
        ));
        factory.connect_bind(clone!(
            #[weak(rename_to = view)]
            self,
            move |_, item| {
                let list_item = item.downcast_ref::<gtk::ListItem>().unwrap();
                let Some(album) = list_item
                    .item()
                    .and_then(|item| item.downcast::<Album>().ok())
                else {
                    return;
                };
                let Some(tile) = list_item
                    .child()
                    .and_then(|child| child.downcast::<AlbumTile>().ok())
                else {
                    return;
                };
                tile.set_album(&album);
                tile.set_artist_link_enabled(!view.imp().artist_mode.get());
            }
        ));
        factory.connect_unbind(|_, item| {
            let list_item = item.downcast_ref::<gtk::ListItem>().unwrap();
            if let Some(tile) = list_item
                .child()
                .and_then(|child| child.downcast::<AlbumTile>().ok())
            {
                tile.clear();
            }
        });

        let selection = gtk::NoSelection::new(Some(imp.store.clone()));
        imp.albums_grid
            .set_model(Some(selection.upcast_ref::<gtk::SelectionModel>()));
        imp.albums_grid
            .set_factory(Some(factory.upcast_ref::<gtk::ListItemFactory>()));

        // A click that does not land on an album tile clears the selection.
        // Album tiles mark `tile_clicked` from their own gesture handlers,
        // which run before this bubble-phase handler on the grid.
        let empty_click = gtk::GestureClick::new();
        empty_click.set_button(gdk::BUTTON_PRIMARY);
        empty_click.connect_pressed(clone!(
            #[weak(rename_to = view)]
            self,
            move |_, _, _, _| {
                view.imp().tile_clicked.set(false);
            }
        ));
        empty_click.connect_released(clone!(
            #[weak(rename_to = view)]
            self,
            move |_, _, _, _| {
                if !view.imp().tile_clicked.get() {
                    view.clear_selection();
                }
            }
        ));
        imp.albums_grid.add_controller(empty_click);

        // Ask the window for the next page of recent albums once the user
        // scrolls close to the bottom. Only `value-changed` is watched: adding
        // a page changes the adjustment's upper bound but not its value, so
        // this cannot trigger a second load by itself.
        imp.albums_scroll.vadjustment().connect_value_changed(clone!(
            #[weak(rename_to = view)]
            self,
            move |adjustment| {
                let imp = view.imp();
                if !imp.recent_pagination.get()
                    || imp.loading_more.get()
                    || imp.search_mode.get()
                {
                    return;
                }

                let upper = adjustment.upper();
                let remaining = upper - adjustment.value() - adjustment.page_size();
                if upper > 0.0 && remaining <= adjustment.page_size() * 0.5 {
                    imp.loading_more.set(true);
                    view.emit_by_name::<()>("load-more-albums", &[]);
                }
            }
        ));
    }

    /// Appends a freshly loaded page without disturbing the current selection.
    pub fn append_albums(&self, albums: Vec<Album>) {
        if albums.is_empty() {
            self.imp().loading_more.set(false);
            return;
        }
        let imp = self.imp();
        let position = imp.store.n_items();
        imp.store.splice(position, 0, &albums);
        self.update_visible_page();
        imp.loading_more.set(false);
    }

    /// Called by the window when a page request could not be fulfilled, so a
    /// failed load does not block later attempts.
    pub fn finish_load_more(&self) {
        self.imp().loading_more.set(false);
    }

    pub fn set_albums(&self, albums: Vec<Album>) {
        self.cancel_album_animation();
        self.replace_albums(albums);
    }

    fn replace_albums(&self, albums: Vec<Album>) {
        let imp = self.imp();
        self.clear_selection();
        imp.loading_more.set(false);
        imp.store.remove_all();
        imp.store.splice(0, 0, &albums);
        self.update_visible_page();
    }

    fn album_position(&self, album: &Album) -> Option<u32> {
        let album = album.record();
        (0..self.imp().store.n_items()).find(|position| {
            self.imp()
                .store
                .item(*position)
                .and_then(|item| item.downcast::<Album>().ok())
                .is_some_and(|candidate| {
                    let candidate = candidate.record();
                    candidate.title == album.title && candidate.artist == album.artist
                })
        })
    }

    fn handle_plain_click(&self, album: &Album) {
        let position = self.album_position(album);
        self.clear_selection();
        // Keep the clicked album as the anchor so a following Shift-click
        // extends the range from here.
        if let Some(position) = position {
            self.imp().selection_anchor.set(Some(position));
        }
        self.emit_by_name::<()>("album-activated", &[album]);
    }

    fn clear_selection(&self) {
        let imp = self.imp();
        for position in 0..imp.store.n_items() {
            if let Some(album) = imp
                .store
                .item(position)
                .and_then(|item| item.downcast::<Album>().ok())
            {
                album.set_selected(false);
            }
        }
        imp.selection_anchor.set(None);
        self.update_selection_count();
    }

    fn update_selection_count(&self) {
        let imp = self.imp();
        let count = (0..imp.store.n_items())
            .filter_map(|position| {
                imp.store
                    .item(position)
                    .and_then(|item| item.downcast::<Album>().ok())
            })
            .filter(Album::is_selected)
            .count() as u32;
        imp.selected_count.set(count);
        self.update_subtitle();
    }

    fn update_subtitle(&self) {
        let imp = self.imp();
        let base = imp.base_subtitle.borrow().clone();
        let count = imp.selected_count.get();
        if count == 0 {
            imp.subtitle_label.set_label(&base);
        } else {
            let selected = ni18n_f("{} selected", "{} selected", count, &[&count.to_string()]);
            imp.subtitle_label
                .set_label(&format!("{base} ({selected})"));
        }
    }

    fn handle_selection_clicked(&self, album: &Album, shift: bool, control: bool) {
        let Some(position) = self.album_position(album) else {
            return;
        };
        let imp = self.imp();

        if shift {
            let anchor = imp.selection_anchor.get().unwrap_or(position);
            if !control {
                self.clear_selection();
                imp.selection_anchor.set(Some(anchor));
            }
            let start = anchor.min(position);
            let end = anchor.max(position);
            for pos in start..=end {
                if let Some(candidate) = imp
                    .store
                    .item(pos)
                    .and_then(|item| item.downcast::<Album>().ok())
                {
                    candidate.set_selected(true);
                }
            }
        } else if control {
            album.set_selected(!album.is_selected());
            imp.selection_anchor.set(Some(position));
        }

        self.update_selection_count();
    }

    fn context_albums(&self, clicked: &Album) -> Vec<Album> {
        let imp = self.imp();
        let selected: Vec<Album> = (0..imp.store.n_items())
            .filter_map(|position| {
                imp.store
                    .item(position)
                    .and_then(|item| item.downcast::<Album>().ok())
            })
            .filter(Album::is_selected)
            .collect();

        let clicked_record = clicked.record();
        let clicked_is_selected = selected.iter().any(|album| {
            let album = album.record();
            album.title == clicked_record.title && album.artist == clicked_record.artist
        });

        if clicked_is_selected && !selected.is_empty() {
            selected
        } else {
            self.clear_selection();
            vec![clicked.clone()]
        }
    }

    /// Replaces the grid contents with a short cross-fade.
    pub fn set_albums_animated(&self, albums: Vec<Album>) {
        self.refresh_albums_animated(move || Some(albums));
    }

    /// Runs album selection and preparation only after the old grid has faded
    /// out. Repeated requests replace the pending work without restarting the
    /// fade. Returning None keeps the current albums (for example on failure).
    pub fn refresh_albums_animated<F>(&self, refresh: F)
    where
        F: FnOnce() -> Option<Vec<Album>> + 'static,
    {
        let imp = self.imp();

        if imp.store.n_items() == 0 || !self.is_mapped() {
            self.cancel_album_animation();
            if let Some(albums) = refresh() {
                self.replace_albums(albums);
            }
            return;
        }

        imp.pending_refresh
            .replace(Some(AlbumRefresh(Box::new(refresh))));
        match imp.fade_phase.get() {
            imp::FadePhase::FadingOut => {}
            imp::FadePhase::Idle | imp::FadePhase::FadingIn => {
                self.start_fade_out();
            }
        }
    }

    fn start_fade_out(&self) {
        let imp = self.imp();
        let generation = imp.animation_generation.get().wrapping_add(1);
        imp.animation_generation.set(generation);
        imp.fade_phase.set(imp::FadePhase::FadingOut);

        if let Some(animation) = imp.animation.borrow_mut().take() {
            animation.pause();
        }

        let grid = imp.albums_grid.get();
        let current_opacity = grid.opacity();

        let fade_out_target = adw::CallbackAnimationTarget::new(clone!(
            #[weak]
            grid,
            move |value| grid.set_opacity(value)
        ));
        let fade_out = adw::TimedAnimation::new(
            &grid,
            current_opacity,
            0.0,
            ALBUM_FADE_OUT_MS,
            fade_out_target,
        );
        fade_out.set_easing(adw::Easing::EaseInCubic);

        fade_out.connect_done(clone!(
            #[weak(rename_to = view)]
            self,
            move |_| {
                let imp = view.imp();
                if imp.animation_generation.get() != generation {
                    return;
                }

                imp.albums_grid.set_opacity(0.0);
                // Finish painting the transparent frame before running the
                // shuffle, favorite lookups and model replacement. Doing this
                // inside `done` would block the final frame of the fade-out.
                glib::idle_add_local_once(clone!(
                    #[weak]
                    view,
                    move || {
                        let imp = view.imp();
                        if imp.animation_generation.get() != generation {
                            return;
                        }
                        let refresh = imp.pending_refresh.borrow_mut().take();
                        if let Some(AlbumRefresh(refresh)) = refresh {
                            if let Some(albums) = refresh() {
                                view.replace_albums(albums);
                            }
                        }
                        view.start_fade_in(generation);
                    }
                ));
            }
        ));

        imp.animation.replace(Some(fade_out.clone()));
        fade_out.play();
    }

    fn start_fade_in(&self, generation: u64) {
        let imp = self.imp();
        imp.fade_phase.set(imp::FadePhase::FadingIn);

        let grid = imp.albums_grid.get();
        let fade_in_target = adw::CallbackAnimationTarget::new(clone!(
            #[weak]
            grid,
            move |value| grid.set_opacity(value)
        ));
        let fade_in = adw::TimedAnimation::new(&grid, 0.0, 1.0, ALBUM_FADE_IN_MS, fade_in_target);
        fade_in.set_easing(adw::Easing::EaseOutCubic);
        fade_in.connect_done(clone!(
            #[weak(rename_to = view)]
            self,
            move |_| {
                let imp = view.imp();
                if imp.animation_generation.get() != generation {
                    return;
                }
                imp.albums_grid.set_opacity(1.0);
                imp.animation.take();
                imp.fade_phase.set(imp::FadePhase::Idle);
            }
        ));

        imp.animation.replace(Some(fade_in.clone()));
        fade_in.play();
    }

    fn cancel_album_animation(&self) {
        let imp = self.imp();
        imp.animation_generation
            .set(imp.animation_generation.get().wrapping_add(1));
        if let Some(animation) = imp.animation.borrow_mut().take() {
            animation.pause();
        }
        imp.pending_refresh.take();
        imp.fade_phase.set(imp::FadePhase::Idle);
        imp.albums_grid.set_opacity(1.0);
    }

    pub fn set_album_count(&self, count: u32) {
        self.imp()
            .base_subtitle
            .replace(album_stats_text(count));
        self.update_subtitle();
    }

    /// Shows an artist as the page title, with album and track totals.
    pub fn set_artist_header(&self, artist: &str, album_count: u32, track_count: u32) {
        let imp = self.imp();

        let albums = album_stats_text(album_count);
        let tracks = ni18n_f(
            "{} track",
            "{} tracks",
            track_count,
            &[&track_count.to_string()],
        );

        imp.updating_buttons.set(true);
        imp.artist_mode.set(true);
        imp.recent_pagination.set(false);
        imp.random_mode.set(false);
        imp.favorites_mode.set(false);
        imp.search_results_mode.set(false);
        // The artist page is not one of the library tabs, so none of the
        // navigation buttons should stay highlighted. Clicking any of them
        // re-activates it and leaves the artist page through the usual
        // handlers above.
        imp.recent_button.set_active(false);
        imp.random_button.set_active(false);
        imp.favorites_button.set_active(false);
        imp.title_label.set_label(artist);
        imp.base_subtitle.replace(format!("{albums} | {tracks}"));
        self.update_subtitle();
        imp.play_albums_button
            .set_tooltip_text(Some(&i18n("Play Artist Albums")));
        imp.updating_buttons.set(false);
    }

    /// Header for the search results page.
    pub fn set_search_header(&self, album_count: u32) {
        let imp = self.imp();

        imp.random_mode.set(false);
        imp.favorites_mode.set(false);
        imp.artist_mode.set(false);
        imp.recent_pagination.set(false);
        imp.search_results_mode.set(true);
        imp.updating_buttons.set(true);
        imp.title_label.set_label(&i18n("Search Results"));
        self.set_album_count(album_count);
        imp.play_albums_button
            .set_tooltip_text(Some(&i18n("Play Search Results")));
        imp.updating_buttons.set(false);
    }

    pub fn is_artist_mode(&self) -> bool {
        self.imp().artist_mode.get()
    }

    pub fn set_title_mode(&self, random: bool) {
        let imp = self.imp();

        imp.random_mode.set(random);
        imp.favorites_mode.set(false);
        imp.artist_mode.set(false);
        imp.search_results_mode.set(false);
        imp.recent_pagination.set(!random);
        imp.loading_more.set(false);
        imp.updating_buttons.set(true);
        if random {
            imp.title_label.set_label(&i18n("Random Albums"));
            imp.random_button.set_active(true);
            imp.play_albums_button
                .set_tooltip_text(Some(&i18n("Play Random Albums")));
        } else {
            imp.title_label.set_label(&i18n("Recent Albums"));
            imp.recent_button.set_active(true);
            imp.play_albums_button
                .set_tooltip_text(Some(&i18n("Play Recent Albums")));
        }
        imp.updating_buttons.set(false);
    }

    pub fn set_favorites_mode(&self, album_count: u32) {
        let imp = self.imp();

        imp.random_mode.set(false);
        imp.favorites_mode.set(true);
        imp.artist_mode.set(false);
        imp.recent_pagination.set(false);
        imp.search_results_mode.set(false);
        imp.updating_buttons.set(true);
        imp.title_label.set_label(&i18n("Favorite Albums"));
        self.set_album_count(album_count);
        imp.favorites_button.set_active(true);
        imp.play_albums_button
            .set_tooltip_text(Some(&i18n("Play Favorite Albums")));
        imp.updating_buttons.set(false);
    }

    pub fn set_scanning(&self, scanning: bool) {
        let imp = self.imp();
        imp.scanning.set(scanning);
        self.update_visible_page();
    }

    fn update_visible_page(&self) {
        let imp = self.imp();
        if imp.store.n_items() > 0 {
            imp.library_stack.set_visible_child_name("albums");
        } else if imp.scanning.get() {
            imp.library_stack.set_visible_child_name("loading");
        } else if imp.search_results_mode.get() {
            imp.library_stack.set_visible_child_name("empty-search");
        } else if imp.favorites_mode.get() {
            imp.library_stack.set_visible_child_name("empty-favorites");
        } else {
            imp.library_stack.set_visible_child_name("empty");
        }
    }
}

fn album_stats_text(album_count: u32) -> String {
    ni18n_f(
        "{} album",
        "{} albums",
        album_count,
        &[&album_count.to_string()],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::AlbumRecord;
    use std::{
        path::PathBuf,
        rc::Rc,
        time::{Duration, Instant},
    };

    fn pump_until(mut ready: impl FnMut() -> bool) {
        let context = glib::MainContext::default();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready() {
            assert!(Instant::now() < deadline, "GTK update timed out");
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    #[ignore = "requires a display and AMBEROL_TEST_RESOURCE pointing to the Meson resource bundle"]
    fn refresh_runs_after_fade_and_cancels_stale_work() {
        adw::init().unwrap();
        let resource =
            gio::Resource::load(std::env::var("AMBEROL_TEST_RESOURCE").unwrap()).unwrap();
        gio::resources_register(&resource);
        let settings = gtk::Settings::default().unwrap();
        settings.set_gtk_enable_animations(true);
        let album = || {
            Album::new(AlbumRecord {
                title: "Test".into(),
                artist: String::new(),
                cover_path: PathBuf::from("/nonexistent-amberol-animation-test/track"),
                track_count: 1,
                modified_at: 0,
            })
        };
        let view = LibraryView::default();
        view.set_albums(vec![album()]);
        let window = gtk::Window::builder()
            .child(&view)
            .default_width(800)
            .default_height(600)
            .build();
        window.present();
        pump_until(|| view.is_mapped());

        let calls = Rc::new(Cell::new(0));
        view.refresh_albums_animated(|| panic!("superseded refresh must not run"));
        let started = Instant::now();
        view.refresh_albums_animated(clone!(
            #[weak]
            view,
            #[strong]
            calls,
            #[upgrade_or]
            None,
            move || {
                assert_eq!(view.imp().albums_grid.opacity(), 0.0);
                assert!(started.elapsed() >= Duration::from_millis(100));
                eprintln!("Refresh started after {:?}", started.elapsed());
                calls.set(calls.get() + 1);
                Some(Vec::new())
            }
        ));
        assert_eq!(calls.get(), 0);
        pump_until(|| view.imp().fade_phase.get() == imp::FadePhase::Idle);
        assert_eq!(calls.get(), 1);
        assert_eq!(view.imp().albums_grid.opacity(), 1.0);
        assert_eq!(view.imp().store.n_items(), 0);

        view.set_albums(vec![album()]);
        view.refresh_albums_animated(|| panic!("cancelled refresh must not run"));
        view.set_albums(vec![album()]);
        assert!(view.imp().pending_refresh.borrow().is_none());
        assert_eq!(view.imp().fade_phase.get(), imp::FadePhase::Idle);

        // Adwaita may complete animations synchronously when disabled.
        settings.set_gtk_enable_animations(false);
        view.refresh_albums_animated(|| Some(Vec::new()));
        pump_until(|| view.imp().fade_phase.get() == imp::FadePhase::Idle);
        assert_eq!(view.imp().store.n_items(), 0);
        assert_eq!(view.imp().albums_grid.opacity(), 1.0);
        window.close();
    }
}
