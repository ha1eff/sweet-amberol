// SPDX-FileCopyrightText: 2022  Emmanuele Bassi
// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    cell::{Cell, RefCell},
    time::{Duration, Instant},
};

use adw::subclass::prelude::*;
use glib::clone;
use gtk::{gdk, gio, glib, prelude::*, CompositeTemplate};

use crate::{audio::Song, cover_picture::CoverPicture, i18n::i18n, utils};

/// The artist label is a link, but it should not steal a click from a row
/// that the pointer merely moves across on its way to another control.
const ARTIST_LINK_DWELL: Duration = Duration::from_millis(500);

mod imp {
    use glib::{ParamSpec, ParamSpecBoolean, ParamSpecObject, ParamSpecString, Value};
    use once_cell::sync::Lazy;

    use super::*;

    #[derive(Debug, Default, CompositeTemplate)]
    #[template(resource = "/io/bassi/Amberol/queue-row.ui")]
    pub struct QueueRow {
        // Template widgets
        #[template_child]
        pub row_stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub song_cover_stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub song_cover_image: TemplateChild<CoverPicture>,
        #[template_child]
        pub song_title_label: TemplateChild<gtk::Inscription>,
        #[template_child]
        pub song_artist_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub song_playing_image: TemplateChild<gtk::Image>,
        #[template_child]
        pub selection_title_label: TemplateChild<gtk::Inscription>,
        #[template_child]
        pub selection_artist_label: TemplateChild<gtk::Inscription>,
        #[template_child]
        pub selected_button: TemplateChild<gtk::CheckButton>,
        #[template_child]
        pub selection_playing_image: TemplateChild<gtk::Image>,
        #[template_child]
        pub favorite_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub selection_favorite_button: TemplateChild<gtk::Button>,

        pub song: RefCell<Option<Song>>,
        pub favorite: Cell<bool>,
        pub playing: Cell<bool>,
        pub selection_mode: Cell<bool>,
        pub cover_visible: Cell<bool>,
        /// When the pointer entered the artist link. Clicks are only accepted
        /// after the pointer has stayed there for a moment, so moving the
        /// mouse across the playlist does not open artist pages by accident.
        pub artist_hover_started: Cell<Option<Instant>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for QueueRow {
        const NAME: &'static str = "AmberolQueueRow";
        type Type = super::QueueRow;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            Self::bind_template(klass);

            klass.set_layout_manager_type::<gtk::BoxLayout>();
            klass.set_css_name("queuerow");
            klass.set_accessible_role(gtk::AccessibleRole::Group);
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for QueueRow {
        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: Lazy<Vec<glib::subclass::Signal>> = Lazy::new(|| {
                vec![
                    glib::subclass::Signal::builder("favorite-toggled")
                        .param_types([Song::static_type()])
                        .build(),
                    glib::subclass::Signal::builder("artist-activated")
                        .param_types([String::static_type()])
                        .build(),
                    glib::subclass::Signal::builder("selection-clicked")
                        .param_types([
                            Song::static_type(),
                            bool::static_type(),
                            bool::static_type(),
                        ])
                        .build(),
                    glib::subclass::Signal::builder("song-context-action")
                        .param_types([Song::static_type(), String::static_type()])
                        .build(),
                ]
            });
            SIGNALS.as_ref()
        }

        fn dispose(&self) {
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }

        fn constructed(&self) {
            self.parent_constructed();

            self.cover_visible.set(true);
            self.obj().init_widgets();
        }

        fn properties() -> &'static [ParamSpec] {
            static PROPERTIES: Lazy<Vec<ParamSpec>> = Lazy::new(|| {
                vec![
                    ParamSpecObject::builder::<Song>("song").build(),
                    ParamSpecString::builder("song-artist").build(),
                    ParamSpecString::builder("song-title").build(),
                    ParamSpecObject::builder::<gdk::Texture>("song-cover").build(),
                    ParamSpecBoolean::builder("playing").build(),
                    ParamSpecBoolean::builder("selection-mode").build(),
                    ParamSpecBoolean::builder("cover-visible").build(),
                    ParamSpecBoolean::builder("selected").build(),
                    ParamSpecBoolean::builder("favorite").build(),
                ]
            });
            PROPERTIES.as_ref()
        }

        fn set_property(&self, _id: usize, value: &Value, pspec: &ParamSpec) {
            match pspec.name() {
                "song" => {
                    let song = value.get::<Option<Song>>().unwrap();
                    self.song.replace(song);
                }
                "song-artist" => {
                    let p = value.get::<&str>().expect("The value needs to be a string");
                    self.obj().set_song_artist(p);
                }
                "song-title" => {
                    let p = value.get::<&str>().expect("The value needs to be a string");
                    self.obj().set_song_title(p);
                }
                "song-cover" => {
                    let p = value.get::<gdk::Texture>().ok();
                    self.obj().set_song_cover(p);
                }
                "playing" => {
                    let p = value
                        .get::<bool>()
                        .expect("The value needs to be a boolean");
                    self.obj().set_playing(p);
                }
                "selection-mode" => {
                    let p = value
                        .get::<bool>()
                        .expect("The value needs to be a boolean");
                    self.obj().set_selection_mode(p);
                }
                "cover-visible" => {
                    let p = value
                        .get::<bool>()
                        .expect("The value needs to be a boolean");
                    self.obj().set_cover_visible(p);
                }
                "selected" => {
                    let p = value
                        .get::<bool>()
                        .expect("The value needs to be a boolean");
                    self.selected_button.set_active(p);
                }
                "favorite" => {
                    let p = value
                        .get::<bool>()
                        .expect("The value needs to be a boolean");
                    self.obj().set_favorite(p);
                }
                _ => unimplemented!(),
            }
        }

        fn property(&self, _id: usize, pspec: &ParamSpec) -> Value {
            match pspec.name() {
                "song" => self.song.borrow().to_value(),
                "song-artist" => self.song_artist_label.label().to_value(),
                "song-title" => self.song_title_label.text().to_value(),
                "song-cover" => self.song_cover_image.cover().to_value(),
                "playing" => self.playing.get().to_value(),
                "selection-mode" => self.selection_mode.get().to_value(),
                "cover-visible" => self.cover_visible.get().to_value(),
                "selected" => self.selected_button.is_active().to_value(),
                "favorite" => self.favorite.get().to_value(),
                _ => unimplemented!(),
            }
        }
    }

    impl WidgetImpl for QueueRow {}
}

glib::wrapper! {
    pub struct QueueRow(ObjectSubclass<imp::QueueRow>)
        @extends gtk::Widget,
        @implements gio::ActionGroup, gio::ActionMap, gtk::Accessible,
                    gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for QueueRow {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl QueueRow {
    pub fn new() -> Self {
        Self::default()
    }

    fn init_widgets(&self) {
        self.imp().selected_button.connect_active_notify(clone!(
            #[strong(rename_to = this)]
            self,
            move |button| {
                if let Some(ref song) = *this.imp().song.borrow() {
                    song.set_selected(button.is_active());
                }
                this.notify("selected");
            }
        ));

        for button in [
            self.imp().favorite_button.get(),
            self.imp().selection_favorite_button.get(),
        ] {
            button.connect_clicked(clone!(
                #[strong(rename_to = this)]
                self,
                move |_| {
                    if let Some(song) = this.song() {
                        this.emit_by_name::<()>("favorite-toggled", &[&song]);
                    }
                }
            ));
        }

        // Each artist in the credit is its own Pango link, so the name under
        // the pointer decides which artist page opens. The 500 ms dwell is
        // kept: clicking while just moving across the playlist does nothing.
        self.imp().song_artist_label.set_can_target(true);
        self.imp().song_artist_label.connect_activate_link(clone!(
            #[weak(rename_to = this)]
            self,
            #[upgrade_or]
            glib::Propagation::Stop,
            move |_, uri| {
                if this.imp().selection_mode.get() {
                    return glib::Propagation::Stop;
                }

                let hovered_long_enough = this
                    .imp()
                    .artist_hover_started
                    .get()
                    .is_some_and(|started| started.elapsed() >= ARTIST_LINK_DWELL);
                if hovered_long_enough {
                    if let Some(artist) = utils::artist_from_link(uri) {
                        this.emit_by_name::<()>("artist-activated", &[&artist]);
                    }
                }

                glib::Propagation::Stop
            }
        ));

        let artist_motion = gtk::EventControllerMotion::new();
        artist_motion.connect_enter(clone!(
            #[weak(rename_to = this)]
            self,
            move |_, _, _| {
                this.imp().artist_hover_started.set(Some(Instant::now()));
            }
        ));
        artist_motion.connect_motion(clone!(
            #[weak(rename_to = this)]
            self,
            move |_, _, _| {
                if this.imp().artist_hover_started.get().is_none() {
                    this.imp().artist_hover_started.set(Some(Instant::now()));
                }
            }
        ));
        artist_motion.connect_leave(clone!(
            #[weak(rename_to = this)]
            self,
            move |_| {
                this.imp().artist_hover_started.set(None);
            }
        ));
        self.imp().song_artist_label.add_controller(artist_motion);

        self.imp()
            .song_artist_label
            .set_cursor_from_name(Some("pointer"));

        // Ctrl/Shift click selects rows instead of activating them. The
        // gesture runs in the capture phase so the ListView's single-click
        // activation does not start playback as well.
        let selection_gesture = gtk::GestureClick::new();
        selection_gesture.set_button(gdk::BUTTON_PRIMARY);
        selection_gesture.set_propagation_phase(gtk::PropagationPhase::Capture);
        selection_gesture.connect_released(clone!(
            #[weak(rename_to = this)]
            self,
            move |gesture, _, _, _| {
                let modifiers = gesture.current_event_state();
                let shift = modifiers.contains(gdk::ModifierType::SHIFT_MASK);
                let control = modifiers.contains(gdk::ModifierType::CONTROL_MASK);
                if !shift && !control {
                    return;
                }

                if let Some(song) = this.song() {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    this.emit_by_name::<()>("selection-clicked", &[&song, &shift, &control]);
                }
            }
        ));
        self.add_controller(selection_gesture);

        let context_gesture = gtk::GestureClick::new();
        context_gesture.set_button(gdk::BUTTON_SECONDARY);
        context_gesture.connect_pressed(|gesture, _, _, _| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
        });
        context_gesture.connect_released(clone!(
            #[weak(rename_to = this)]
            self,
            move |gesture, _, x, y| {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                this.show_context_menu(x, y);
            }
        ));
        self.add_controller(context_gesture);

    }

    pub fn set_favorite(&self, favorite: bool) {
        if favorite == self.imp().favorite.replace(favorite) {
            return;
        }

        let icon = if favorite {
            "starred-symbolic"
        } else {
            "non-starred-symbolic"
        };
        let tooltip = if favorite {
            i18n("Remove from Favorites")
        } else {
            i18n("Add to Favorites")
        };

        if favorite {
            self.add_css_class("favorite");
        } else {
            self.remove_css_class("favorite");
        }

        for button in [
            self.imp().favorite_button.get(),
            self.imp().selection_favorite_button.get(),
        ] {
            button.set_icon_name(icon);
            button.set_tooltip_text(Some(&tooltip));
        }
        self.notify("favorite");
    }

    fn set_playing(&self, playing: bool) {
        if playing != self.imp().playing.replace(playing) {
            if playing {
                self.add_css_class("current");
            } else {
                self.remove_css_class("current");
            }
            self.update_mode();
            self.notify("playing");
        }
    }

    fn set_selection_mode(&self, selection_mode: bool) {
        if selection_mode != self.imp().selection_mode.replace(selection_mode) {
            self.update_mode();
            self.notify("selection-mode");
        }
    }

    /// Consecutive tracks from the same album reuse the first track's cover,
    /// so the cover column is hidden for them and their text shifts left
    /// instead of leaving an empty gap.
    pub fn set_cover_visible(&self, visible: bool) {
        if visible != self.imp().cover_visible.replace(visible) {
            self.imp().song_cover_stack.set_visible(visible);
            self.notify("cover-visible");
        }
    }

    fn update_mode(&self) {
        let imp = self.imp();
        if imp.selection_mode.get() {
            imp.row_stack.set_visible_child_name("selection-mode");
            let opacity = if imp.playing.get() { 1.0 } else { 0.0 };
            imp.selection_playing_image.set_opacity(opacity);
        } else {
            imp.row_stack.set_visible_child_name("song-details");
            let opacity = if imp.playing.get() { 1.0 } else { 0.0 };
            imp.song_playing_image.set_opacity(opacity);
        }
    }

    fn set_song_title(&self, title: &str) {
        let imp = self.imp();
        imp.song_title_label.set_text(Some(title));
        imp.selection_title_label.set_text(Some(title));
    }

    fn set_song_artist(&self, artist: &str) {
        let imp = self.imp();
        // A plain label sizes itself exactly to its text, so the clickable
        // artist area never grows past the artist name (and it still
        // ellipsizes when the row is too narrow).
        let displayed = utils::display_artist_name(artist);
        imp.song_artist_label.set_text(&displayed);
        imp.selection_artist_label.set_text(Some(&displayed));
    }

    fn set_song_cover(&self, cover: Option<gdk::Texture>) {
        let imp = self.imp();
        if let Some(texture) = cover {
            imp.song_cover_image.set_cover(Some(&texture));
            imp.song_cover_stack.set_visible_child_name("cover");
        } else {
            imp.song_cover_image.set_cover(None);
            imp.song_cover_stack.set_visible_child_name("no-cover");
        }
    }

    fn show_context_menu(&self, x: f64, y: f64) {
        let Some(song) = self.song() else {
            return;
        };

        let row = self.downgrade();
        let popover = gtk::Popover::new();
        popover.add_css_class("album-context-menu");
        popover.set_has_arrow(true);
        popover.set_parent(self);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 2);
        content.set_size_request(184, -1);
        content.set_margin_top(6);
        content.set_margin_bottom(6);
        content.set_margin_start(6);
        content.set_margin_end(6);

        let stack = gtk::Stack::new();
        stack.set_transition_type(gtk::StackTransitionType::Crossfade);
        stack.set_transition_duration(120);
        stack.set_hhomogeneous(false);
        stack.set_vhomogeneous(false);

        let primary_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        primary_row.set_halign(gtk::Align::Center);

        primary_row.append(&song_context_icon_button(
            "media-playback-start-symbolic",
            &i18n("Play"),
            &popover,
            &row,
            &song,
            "play",
        ));
        primary_row.append(&song_context_icon_button(
            "list-add-symbolic",
            &i18n("Add to Queue"),
            &popover,
            &row,
            &song,
            "queue",
        ));

        let (favorite_icon, favorite_tooltip) = if self.imp().favorite.get() {
            ("starred-symbolic", i18n("Remove from Favorites"))
        } else {
            ("non-starred-symbolic", i18n("Add to Favorites"))
        };
        primary_row.append(&song_context_icon_button(
            favorite_icon,
            &favorite_tooltip,
            &popover,
            &row,
            &song,
            "favorite",
        ));
        primary_row.append(&song_context_delete_button(
            &i18n("Delete from Device"),
            &stack,
            &row,
            &song,
        ));
        content.append(&primary_row);

        content.append(&song_context_text_button(
            &i18n("Play Next"),
            &popover,
            &row,
            &song,
            "play-next",
        ));
        content.append(&song_context_text_button(
            &i18n("Remove from Playlist"),
            &popover,
            &row,
            &song,
            "remove",
        ));

        stack.add_named(&content, Some("menu"));

        let confirmation = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        confirmation.set_margin_start(6);
        confirmation.set_margin_end(6);
        confirmation.set_margin_top(6);
        confirmation.set_margin_bottom(6);
        confirmation.set_halign(gtk::Align::Fill);

        let confirm_label = gtk::Label::new(Some(&i18n("Are you sure?")));
        confirm_label.add_css_class("album-context-confirm-label");
        confirm_label.set_xalign(0.5);
        confirm_label.set_halign(gtk::Align::Center);
        confirm_label.set_hexpand(true);
        confirmation.append(&confirm_label);

        let yes_button = gtk::Button::from_icon_name("object-select-symbolic");
        yes_button.add_css_class("flat");
        yes_button.set_tooltip_text(Some(&i18n("Yes")));
        yes_button.update_property(&[gtk::accessible::Property::Label(&i18n("Yes"))]);
        confirmation.append(&yes_button);

        let no_button = gtk::Button::from_icon_name("window-close-symbolic");
        no_button.add_css_class("flat");
        no_button.set_tooltip_text(Some(&i18n("No")));
        no_button.update_property(&[gtk::accessible::Property::Label(&i18n("No"))]);
        confirmation.append(&no_button);

        yes_button.connect_clicked(clone!(
            #[weak]
            popover,
            #[strong]
            row,
            #[strong]
            song,
            move |_| {
                popover.popdown();
                if let Some(row) = row.upgrade() {
                    row.emit_context_action(&song, "delete");
                }
            }
        ));
        no_button.connect_clicked(clone!(
            #[weak]
            stack,
            move |_| stack.set_visible_child_name("menu")
        ));
        stack.add_named(&confirmation, Some("confirm"));
        stack.set_visible_child_name("menu");

        popover.set_child(Some(&stack));
        popover.connect_closed(|popover| {
            popover.unparent();
        });

        let rect = gdk::Rectangle::new(x as i32, y as i32, 1, 1);
        popover.set_pointing_to(Some(&rect));
        popover.popup();
    }

    fn emit_context_action(&self, song: &Song, action: &str) {
        self.emit_by_name::<()>("song-context-action", &[song, &action.to_string()]);
    }

    pub fn song(&self) -> Option<Song> {
        self.imp().song.borrow().clone()
    }

    /// Shows the credit with one link per known artist. The plain credit is
    /// kept for the inscription used in selection mode, which cannot contain
    /// links.
    pub fn set_artist_markup(&self, credit: &str, markup: &str) {
        let imp = self.imp();
        imp.song_artist_label.set_markup(markup);
        imp.selection_artist_label
            .set_text(Some(&utils::display_artist_name(credit)));
    }
}

fn song_context_icon_button(
    icon_name: &str,
    tooltip: &str,
    popover: &gtk::Popover,
    row: &glib::WeakRef<QueueRow>,
    song: &Song,
    action: &str,
) -> gtk::Button {
    let button = gtk::Button::from_icon_name(icon_name);
    button.add_css_class("flat");
    button.add_css_class("album-context-action");
    button.set_halign(gtk::Align::Center);
    button.set_valign(gtk::Align::Center);
    button.set_size_request(40, 34);
    button.set_tooltip_text(Some(tooltip));
    button.update_property(&[gtk::accessible::Property::Label(tooltip)]);

    let action = action.to_owned();
    button.connect_clicked(clone!(
        #[weak]
        popover,
        #[strong]
        row,
        #[strong]
        song,
        move |_| {
            popover.popdown();
            if let Some(row) = row.upgrade() {
                row.emit_context_action(&song, &action);
            }
        }
    ));
    button
}

fn song_context_delete_button(
    tooltip: &str,
    stack: &gtk::Stack,
    row: &glib::WeakRef<QueueRow>,
    song: &Song,
) -> gtk::Button {
    let button = gtk::Button::from_icon_name("user-trash-symbolic");
    button.add_css_class("flat");
    button.add_css_class("album-context-action");
    button.set_halign(gtk::Align::Center);
    button.set_valign(gtk::Align::Center);
    button.set_size_request(40, 34);
    button.set_tooltip_text(Some(tooltip));
    button.update_property(&[gtk::accessible::Property::Label(tooltip)]);

    button.connect_clicked(clone!(
        #[weak]
        stack,
        #[strong]
        row,
        #[strong]
        song,
        move |_| {
            // Deleting files is destructive, so the context menu switches to
            // an inline confirmation instead of acting immediately.
            let _ = (&row, &song);
            stack.set_visible_child_name("confirm");
        }
    ));
    button
}

fn song_context_text_button(
    label: &str,
    popover: &gtk::Popover,
    row: &glib::WeakRef<QueueRow>,
    song: &Song,
    action: &str,
) -> gtk::Button {
    let button = gtk::Button::with_label(label);
    button.add_css_class("flat");
    button.set_halign(gtk::Align::Fill);
    if let Some(label) = button
        .child()
        .and_then(|child| child.downcast::<gtk::Label>().ok())
    {
        label.set_xalign(0.5);
        label.set_halign(gtk::Align::Center);
    }

    let action = action.to_owned();
    button.connect_clicked(clone!(
        #[weak]
        popover,
        #[strong]
        row,
        #[strong]
        song,
        move |_| {
            popover.popdown();
            if let Some(row) = row.upgrade() {
                row.emit_context_action(&song, &action);
            }
        }
    ));
    button
}
