// SPDX-FileCopyrightText: 2023  Emmanuele Bassi
// SPDX-License-Identifier: GPL-3.0-or-later

use std::cell::Cell;

use adw::subclass::prelude::*;
use glib::clone;
use gtk::{glib, prelude::*, CompositeTemplate};

use crate::{
    cover_picture::{CoverPicture, CoverSize},
    i18n::i18n,
};

mod imp {
    use once_cell::sync::OnceCell;

    use super::*;

    #[derive(Debug, Default, CompositeTemplate)]
    #[template(resource = "/io/bassi/Amberol/song-cover.ui")]
    pub struct SongCover {
        // Template widgets
        #[template_child]
        pub cover_overlay: TemplateChild<gtk::Overlay>,
        #[template_child]
        pub cover_stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub album_image: TemplateChild<CoverPicture>,
        #[template_child]
        pub expand_button: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub cover_button_revealer: TemplateChild<gtk::Revealer>,

        /// Window height before the cover was expanded, so collapsing the
        /// cover can shrink the window back to its previous size.
        pub collapsed_height: Cell<i32>,
        pub resize_generation: Cell<u32>,
        pub previous_height_request: Cell<i32>,
        pub temporary_request: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SongCover {
        const NAME: &'static str = "AmberolSongCover";
        type Type = super::SongCover;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            Self::bind_template(klass);

            klass.set_layout_manager_type::<gtk::BinLayout>();
            klass.set_css_name("songcover");
            klass.set_accessible_role(gtk::AccessibleRole::Group);
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            CoverPicture::static_type();
            obj.init_template();
        }
    }

    impl ObjectImpl for SongCover {
        fn constructed(&self) {
            self.parent_constructed();

            let obj = self.obj();
            self.expand_button.connect_toggled(clone!(
                #[weak]
                obj,
                move |button| {
                    obj.set_cover_expanded(button.is_active());
                }
            ));

            // The button stays hidden until the pointer is over the cover, and
            // fades in/out thanks to the revealer animation.
            let motion = gtk::EventControllerMotion::new();
            motion.connect_enter(clone!(
                #[weak]
                obj,
                move |_, _, _| {
                    obj.imp().cover_button_revealer.set_reveal_child(true);
                }
            ));
            motion.connect_leave(clone!(
                #[weak]
                obj,
                move |_| {
                    obj.imp().cover_button_revealer.set_reveal_child(false);
                }
            ));
            self.cover_overlay.add_controller(motion);

            // Right-clicking the artwork opens the cover context menu. The
            // press is claimed so the desktop never shows its own menu, and
            // the menu opens on release, otherwise the release would land on
            // a menu row that just appeared under the pointer.
            let context_gesture = gtk::GestureClick::new();
            context_gesture.set_button(gtk::gdk::BUTTON_SECONDARY);
            context_gesture.connect_pressed(|gesture, _, _, _| {
                gesture.set_state(gtk::EventSequenceState::Claimed);
            });
            context_gesture.connect_released(clone!(
                #[weak]
                obj,
                move |gesture, _, x, y| {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    // The gesture coordinates are relative to the overlay,
                    // which is centred inside the cover widget. Translate
                    // them to the widget the popover is parented to, otherwise
                    // the menu ends up far to the left of the pointer.
                    let overlay = obj.imp().cover_overlay.get();
                    let (x, y) = overlay
                        .compute_bounds(&obj)
                        .map(|bounds| {
                            (x + bounds.x() as f64, y + bounds.y() as f64)
                        })
                        .unwrap_or((x, y));
                    obj.emit_by_name::<()>("cover-menu-requested", &[&x, &y]);
                }
            ));
            self.cover_overlay.add_controller(context_gesture);
        }

        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: OnceCell<Vec<glib::subclass::Signal>> = OnceCell::new();
            SIGNALS.get_or_init(|| {
                vec![glib::subclass::Signal::builder("cover-menu-requested")
                    .param_types([f64::static_type(), f64::static_type()])
                    .build()]
            })
        }

        fn dispose(&self) {
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for SongCover {}
}

glib::wrapper! {
    pub struct SongCover(ObjectSubclass<imp::SongCover>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for SongCover {
    fn default() -> Self {
        glib::Object::new::<Self>()
    }
}

impl SongCover {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn album_image(&self) -> CoverPicture {
        self.imp().album_image.get()
    }

    pub fn show_cover_image(&self, has_image: bool) {
        let imp = self.imp();
        let cover_stack = imp.cover_stack.get();
        if has_image {
            cover_stack.set_visible_child_name("cover-image");
            imp.expand_button.set_visible(true);
        } else {
            // Without artwork there is nothing to enlarge, so collapse the
            // cover back to its normal size.
            imp.expand_button.set_active(false);
            imp.expand_button.set_visible(false);
            imp.cover_button_revealer.set_reveal_child(false);
            cover_stack.set_visible_child_name("no-image");
        }
    }

    /// Switches the cover between its regular size and the slightly larger,
    /// expanded size used by the overlay button.
    fn set_cover_expanded(&self, expanded: bool) {
        let imp = self.imp();

        imp.album_image.set_cover_size(if expanded {
            CoverSize::Expanded
        } else {
            CoverSize::Large
        });

        self.adjust_window_height(expanded);

        let (icon_name, tooltip, a11y) = if expanded {
            (
                "view-restore-symbolic",
                i18n("Restore Cover Size"),
                i18n("Restore cover size"),
            )
        } else {
            (
                "view-fullscreen-symbolic",
                i18n("Enlarge Cover"),
                i18n("Enlarge cover"),
            )
        };

        imp.expand_button.set_icon_name(icon_name);
        imp.expand_button.set_tooltip_text(Some(tooltip.as_str()));
        imp.expand_button
            .update_property(&[gtk::accessible::Property::Label(&a11y)]);
    }

    /// How much vertical space the main view is missing for its content to be
    /// fully visible, in application pixels.
    fn content_height_deficit(&self) -> i32 {
        let mut parent = self.parent();
        while let Some(widget) = parent {
            if let Some(scrolled) = widget.downcast_ref::<gtk::ScrolledWindow>() {
                let Some(child) = scrolled.child() else {
                    return 0;
                };
                let (_, natural, _, _) = child.measure(gtk::Orientation::Vertical, scrolled.width().max(1));
                return (natural - scrolled.height()).max(0);
            }
            parent = widget.parent();
        }

        0
    }

    /// Grows the window by the space the enlarged cover needs, so the rest of
    /// the interface is not pushed out of the window; collapsing the cover
    /// restores the previous height.
    fn adjust_window_height(&self, expanded: bool) {
        let Some(root) = self.root() else {
            return;
        };
        let Some(window) = root.downcast_ref::<gtk::Window>() else {
            return;
        };

        let imp = self.imp();
        let generation = imp.resize_generation.get().wrapping_add(1);
        imp.resize_generation.set(generation);

        if window.is_maximized() || window.is_fullscreen() {
            imp.collapsed_height.set(0);
            return;
        }

        let current_height = window.height();
        if window.width() <= 0 || current_height <= 0 {
            return;
        }

        if expanded {
            imp.collapsed_height.set(current_height);
            if !imp.temporary_request.replace(true) {
                imp.previous_height_request.set(window.height_request());
            }
            // The expanded layout must always have enough room for the
            // controls below it. Keep this minimum request for the whole
            // expanded state, not just until the next relayout.
            let minimum_height = current_height.max(700);
            window.set_size_request(window.width_request(), minimum_height);
            window.set_default_size(window.width().max(1), minimum_height);
            let window = window.clone();
            glib::idle_add_local_once(clone!(
                #[weak(rename_to = this)]
                self,
                move || {
                    let imp = this.imp();
                    if imp.resize_generation.get() != generation || window.is_maximized() || window.is_fullscreen() {
                        return;
                    }
                    let deficit = this.content_height_deficit();
                    if deficit > 0 {
                        let target = (window.height() + deficit).max(700);
                        window.set_size_request(window.width_request(), target);
                        window.set_default_size(window.width(), target);
                    }
                }
            ));
        } else {
            let stored_height = imp.collapsed_height.replace(0);
            if imp.temporary_request.replace(false) {
                window.set_size_request(window.width_request(), imp.previous_height_request.get());
            }
            if stored_height > 0 && stored_height != current_height {
                let window = window.clone();
                glib::idle_add_local_once(clone!(
                    #[weak(rename_to = this)]
                    self,
                    move || {
                        if this.imp().resize_generation.get() == generation && !window.is_maximized() && !window.is_fullscreen() {
                            window.set_default_size(window.width(), stored_height);
                        }
                    }
                ));
            }
        }
    }
}
