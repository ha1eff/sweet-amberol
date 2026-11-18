// SPDX-FileCopyrightText: 2026  Amberol contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::cell::{Cell, RefCell};

use adw::subclass::prelude::*;
use glib::clone;
use gtk::{gdk, glib, prelude::*, CompositeTemplate};
use once_cell::sync::Lazy;

use crate::{album::Album, cover_picture::CoverPicture, i18n::i18n};

mod imp {
    use super::*;

    #[derive(Debug, Default, CompositeTemplate)]
    #[template(resource = "/io/bassi/Amberol/album-tile.ui")]
    pub struct AlbumTile {
        #[template_child]
        pub cover_stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub cover_picture: TemplateChild<CoverPicture>,
        #[template_child]
        pub title_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub artist_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub artist_button: TemplateChild<gtk::Button>,

        pub album: RefCell<Option<Album>>,
        pub album_binding: RefCell<Option<glib::Binding>>,
        pub cover_changed_id: RefCell<Option<glib::SignalHandlerId>>,
        pub cover_loaded: Cell<bool>,
        pub artist_link_enabled: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for AlbumTile {
        const NAME: &'static str = "AmberolAlbumTile";
        type Type = super::AlbumTile;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            Self::bind_template(klass);

            CoverPicture::static_type();
            klass.set_layout_manager_type::<gtk::BinLayout>();
            klass.set_css_name("albumtile");
            klass.set_accessible_role(gtk::AccessibleRole::Group);
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for AlbumTile {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPERTIES: Lazy<Vec<glib::ParamSpec>> =
                Lazy::new(|| vec![glib::ParamSpecBoolean::builder("selected").build()]);
            PROPERTIES.as_ref()
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            match pspec.name() {
                "selected" => self.obj().set_selected(value.get().unwrap_or(false)),
                _ => unimplemented!(),
            }
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "selected" => self.obj().is_selected().to_value(),
                _ => unimplemented!(),
            }
        }

        fn constructed(&self) {
            self.parent_constructed();

            self.cover_picture.set_overflow(gtk::Overflow::Hidden);
            self.artist_button.set_cursor_from_name(Some("pointer"));
            self.artist_link_enabled.set(true);

            let obj = self.obj();
            let tile = obj.downgrade();
            obj.connect_map(move |_| {
                if let Some(tile) = tile.upgrade() {
                    tile.load_cover();
                }
            });

            self.artist_button.connect_clicked(clone!(
                #[weak]
                obj,
                move |_| {
                    if !obj.imp().artist_link_enabled.get() {
                        return;
                    }
                    if let Some(album) = obj.imp().album.borrow().as_ref() {
                        let artist = album.artist();
                        obj.emit_by_name::<()>("artist-activated", &[&artist]);
                    }
                }
            ));

            let click_gesture = gtk::GestureClick::new();
            click_gesture.set_button(gdk::BUTTON_PRIMARY);
            // Capture the event before the GridView's own click handling, so
            // Shift/Ctrl selection cannot also activate (play) the album.
            click_gesture.set_propagation_phase(gtk::PropagationPhase::Capture);
            click_gesture.connect_released(clone!(
                #[weak]
                obj,
                move |gesture, _, x, y| {
                    if obj.is_artist_click(x, y) {
                        return;
                    }

                    let Some(album) = obj.imp().album.borrow().clone() else {
                        return;
                    };
                    let modifiers = gesture.current_event_state();
                    let shift = modifiers.contains(gdk::ModifierType::SHIFT_MASK);
                    let control = modifiers.contains(gdk::ModifierType::CONTROL_MASK);
                    if shift || control {
                        obj.emit_by_name::<()>("selection-clicked", &[&album, &shift, &control]);
                    } else {
                        obj.emit_by_name::<()>("album-clicked", &[&album]);
                    }
                }
            ));
            obj.add_controller(click_gesture);

            let context_gesture = gtk::GestureClick::new();
            context_gesture.set_button(gdk::BUTTON_SECONDARY);
            // Claim the press so the event is not forwarded to the window
            // manager / desktop. The menu itself is opened on release,
            // otherwise the release would be delivered to the menu row that
            // appears under the pointer and activate it immediately.
            context_gesture.connect_pressed(|gesture, _, _, _| {
                gesture.set_state(gtk::EventSequenceState::Claimed);
            });
            context_gesture.connect_released(clone!(
                #[weak]
                obj,
                move |gesture, _, x, y| {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    obj.show_context_menu(x, y);
                }
            ));
            obj.add_controller(context_gesture);
        }

        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: Lazy<Vec<glib::subclass::Signal>> = Lazy::new(|| {
                vec![
                    glib::subclass::Signal::builder("artist-activated")
                        .param_types([String::static_type()])
                        .build(),
                    glib::subclass::Signal::builder("album-clicked")
                        .param_types([Album::static_type()])
                        .build(),
                    glib::subclass::Signal::builder("selection-clicked")
                        .param_types([
                            Album::static_type(),
                            bool::static_type(),
                            bool::static_type(),
                        ])
                        .build(),
                    glib::subclass::Signal::builder("album-context-action")
                        .param_types([Album::static_type(), String::static_type()])
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
    }

    impl WidgetImpl for AlbumTile {}
}

glib::wrapper! {
    pub struct AlbumTile(ObjectSubclass<imp::AlbumTile>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for AlbumTile {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl AlbumTile {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_album(&self, album: &Album) {
        let imp = self.imp();
        self.disconnect_cover_changed();
        if let Some(binding) = imp.album_binding.borrow_mut().take() {
            binding.unbind();
        }
        imp.album.replace(Some(album.clone()));
        let cover_changed_id = album.connect_closure(
            "cover-changed",
            false,
            glib::closure_local!(
                #[weak(rename_to = tile)]
                self,
                move |_: Album| {
                    tile.imp().cover_loaded.set(false);
                    if tile.is_mapped() {
                        tile.load_cover();
                    }
                }
            ),
        );
        imp.cover_changed_id.replace(Some(cover_changed_id));
        imp.album_binding.replace(Some(
            album
                .bind_property("selected", self, "selected")
                .sync_create()
                .build(),
        ));
        imp.cover_loaded.set(false);
        imp.cover_picture.set_cover(None);
        imp.cover_stack.set_visible_child_name("no-cover");

        imp.title_label.set_label(&album.title());
        imp.artist_label.set_label(&album.artist());

        if self.is_mapped() {
            self.load_cover();
        }
    }

    pub fn clear(&self) {
        let imp = self.imp();
        self.disconnect_cover_changed();
        if let Some(binding) = imp.album_binding.borrow_mut().take() {
            binding.unbind();
        }
        imp.album.replace(None);
        imp.cover_loaded.set(false);
        imp.cover_picture.set_cover(None);
        imp.cover_stack.set_visible_child_name("no-cover");
        imp.title_label.set_label("");
        imp.artist_label.set_label("");
    }

    fn disconnect_cover_changed(&self) {
        let imp = self.imp();
        if let Some(id) = imp.cover_changed_id.borrow_mut().take() {
            if let Some(album) = imp.album.borrow().clone() {
                album.disconnect(id);
            }
        }
    }

    fn is_artist_click(&self, x: f64, y: f64) -> bool {
        let artist_button = self.imp().artist_button.get();
        let Some(mut widget) = self.pick(x, y, gtk::PickFlags::DEFAULT) else {
            return false;
        };

        loop {
            if widget == artist_button {
                return true;
            }
            if widget == *self {
                return false;
            }
            let Some(parent) = widget.parent() else {
                return false;
            };
            widget = parent;
        }
    }

    pub fn is_selected(&self) -> bool {
        self.has_css_class("selected")
    }

    pub fn set_selected(&self, selected: bool) {
        if selected {
            self.add_css_class("selected");
        } else {
            self.remove_css_class("selected");
        }
    }

    pub fn set_artist_link_enabled(&self, enabled: bool) {
        let imp = self.imp();
        imp.artist_link_enabled.set(enabled);
        imp.artist_button.set_sensitive(enabled);
        imp.artist_button
            .set_cursor_from_name(if enabled { Some("pointer") } else { None });
        if enabled {
            let tooltip = i18n("Show artist");
            imp.artist_button.set_tooltip_text(Some(&tooltip));
        } else {
            imp.artist_button.set_tooltip_text(None);
        }
    }

    fn show_context_menu(&self, x: f64, y: f64) {
        let Some(album) = self.imp().album.borrow().clone() else {
            return;
        };

        let tile = self.downgrade();
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

        // Three icon-only actions share one compact row.
        let primary_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        primary_row.set_halign(gtk::Align::Center);

        let play_button = context_icon_button(
            "media-playback-start-symbolic",
            &i18n("Play"),
            &popover,
            &tile,
            &album,
            "play",
        );
        primary_row.append(&play_button);

        let queue_button = context_icon_button(
            "list-add-symbolic",
            &i18n("Add to Queue"),
            &popover,
            &tile,
            &album,
            "queue",
        );
        primary_row.append(&queue_button);

        let favorite_icon = if album.is_favorite() {
            "starred-symbolic"
        } else {
            "non-starred-symbolic"
        };
        let favorite_tooltip = if album.is_favorite() {
            i18n("Remove from Favorites")
        } else {
            i18n("Add to Favorites")
        };
        let favorite_button = context_icon_button(
            favorite_icon,
            &favorite_tooltip,
            &popover,
            &tile,
            &album,
            "favorite",
        );
        primary_row.append(&favorite_button);

        content.append(&primary_row);

        let next_button = gtk::Button::with_label(&i18n("Play Next"));
        next_button.add_css_class("flat");
        next_button.set_halign(gtk::Align::Fill);
        if let Some(label) = next_button
            .child()
            .and_then(|c| c.downcast::<gtk::Label>().ok())
        {
            label.set_xalign(0.5);
            label.set_halign(gtk::Align::Center);
        }
        next_button.connect_clicked(clone!(
            #[weak]
            popover,
            #[strong]
            tile,
            #[strong]
            album,
            move |_| {
                popover.popdown();
                if let Some(tile) = tile.upgrade() {
                    tile.emit_context_action(&album, "play-next");
                }
            }
        ));
        content.append(&next_button);

        // Keep the confirmation in a stack instead of replacing the child of
        // a clickable button. Nesting the confirmation buttons inside the
        // delete button made GTK re-activate the parent button, so clicking
        // "No" immediately opened the confirmation again.
        let delete_stack = gtk::Stack::new();
        delete_stack.set_transition_type(gtk::StackTransitionType::Crossfade);
        delete_stack.set_transition_duration(120);
        delete_stack.set_hhomogeneous(false);
        delete_stack.set_vhomogeneous(false);

        let delete_button = gtk::Button::with_label(&i18n("Delete from Device"));
        delete_button.add_css_class("flat");
        delete_button.set_halign(gtk::Align::Fill);
        if let Some(label) = delete_button
            .child()
            .and_then(|c| c.downcast::<gtk::Label>().ok())
        {
            label.set_xalign(0.5);
        }
        delete_button.connect_clicked(clone!(
            #[weak]
            delete_stack,
            move |_| delete_stack.set_visible_child_name("confirm")
        ));
        delete_stack.add_named(&delete_button, Some("delete"));

        let confirmation = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        confirmation.set_margin_start(6);
        confirmation.set_margin_end(6);
        confirmation.set_halign(gtk::Align::Fill);

        let label = gtk::Label::new(Some(&i18n("Are you sure?")));
        label.add_css_class("album-context-confirm-label");
        label.set_xalign(0.5);
        label.set_halign(gtk::Align::Center);
        label.set_hexpand(true);
        confirmation.append(&label);

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
            tile,
            #[strong]
            album,
            move |_| {
                popover.popdown();
                if let Some(tile) = tile.upgrade() {
                    tile.emit_context_action(&album, "delete");
                }
            }
        ));
        no_button.connect_clicked(clone!(
            #[weak]
            delete_stack,
            move |_| delete_stack.set_visible_child_name("delete")
        ));

        delete_stack.add_named(&confirmation, Some("confirm"));
        delete_stack.set_visible_child_name("delete");
        delete_stack.set_size_request(172, -1);
        content.append(&delete_stack);

        popover.set_child(Some(&content));
        popover.connect_closed(|popover| {
            popover.unparent();
        });

        let rect = gdk::Rectangle::new(x as i32, y as i32, 1, 1);
        popover.set_pointing_to(Some(&rect));
        popover.popup();
    }

    fn emit_context_action(&self, album: &Album, action: &str) {
        let action = action.to_string();
        self.emit_by_name::<()>("album-context-action", &[album, &action]);
    }

    fn load_cover(&self) {
        let imp = self.imp();
        if imp.cover_loaded.replace(true) {
            return;
        }

        let Some(album) = imp.album.borrow().clone() else {
            return;
        };
        let tile = self.downgrade();
        album.clone().cover_texture_async(move |texture| {
            let Some(tile) = tile.upgrade() else {
                return;
            };
            let imp = tile.imp();
            // The tile may have been recycled while the artwork was decoding.
            let is_current = imp
                .album
                .borrow()
                .as_ref()
                .is_some_and(|current| current.record() == album.record());
            if !is_current {
                return;
            }

            if let Some(texture) = texture {
                imp.cover_picture.set_cover(Some(&texture));
                imp.cover_stack.set_visible_child_name("cover");
            } else {
                imp.cover_picture.set_cover(None);
                imp.cover_stack.set_visible_child_name("no-cover");
            }
        });
    }
}

fn context_icon_button(
    icon_name: &str,
    tooltip: &str,
    popover: &gtk::Popover,
    tile: &glib::WeakRef<AlbumTile>,
    album: &Album,
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
        tile,
        #[strong]
        album,
        move |_| {
            popover.popdown();
            if let Some(tile) = tile.upgrade() {
                tile.emit_context_action(&album, &action);
            }
        }
    ));

    button
}
