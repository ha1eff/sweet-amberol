// SPDX-FileCopyrightText: 2022  Emmanuele Bassi
// SPDX-License-Identifier: GPL-3.0-or-later

use adw::subclass::prelude::*;
use glib::closure_local;
use gtk::{glib, prelude::*, CompositeTemplate};
use once_cell::sync::Lazy;

use crate::{marquee::Marquee, utils};

mod imp {
    use super::*;

    #[derive(Debug, Default, CompositeTemplate)]
    #[template(resource = "/io/bassi/Amberol/song-details.ui")]
    pub struct SongDetails {
        // Template widgets
        #[template_child]
        pub song_title_label: TemplateChild<Marquee>,
        #[template_child]
        pub song_artist_label: TemplateChild<Marquee>,
        #[template_child]
        pub song_album_label: TemplateChild<Marquee>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SongDetails {
        const NAME: &'static str = "AmberolSongDetails";
        type Type = super::SongDetails;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            Self::bind_template(klass);

            klass.set_layout_manager_type::<gtk::BinLayout>();
            klass.set_css_name("songdetails");
            klass.set_accessible_role(gtk::AccessibleRole::Group);
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for SongDetails {
        fn constructed(&self) {
            self.parent_constructed();

            let obj = self.obj();
            self.song_artist_label.get().connect_closure(
                "link-activated",
                false,
                closure_local!(
                    #[weak]
                    obj,
                    move |_: crate::marquee::Marquee, uri: String| {
                        if let Some(artist) = utils::artist_from_link(&uri) {
                            obj.emit_by_name::<()>("artist-activated", &[&artist]);
                        }
                    }
                ),
            );
            self.song_artist_label.set_cursor_from_name(Some("pointer"));
        }

        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: Lazy<Vec<glib::subclass::Signal>> = Lazy::new(|| {
                vec![glib::subclass::Signal::builder("artist-activated")
                    .param_types([String::static_type()])
                    .build()]
            });
            SIGNALS.as_ref()
        }

        fn dispose(&self) {
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for SongDetails {}
}

glib::wrapper! {
    pub struct SongDetails(ObjectSubclass<imp::SongDetails>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for SongDetails {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl SongDetails {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn artist_label(&self) -> Marquee {
        self.imp().song_artist_label.get()
    }

    /// Shows `markup`, in which each artist is a separate link.
    pub fn set_artist_markup(&self, markup: &str) {
        self.imp().song_artist_label.set_markup(markup);
    }

    pub fn title_label(&self) -> Marquee {
        self.imp().song_title_label.get()
    }

    pub fn album_label(&self) -> Marquee {
        self.imp().song_album_label.get()
    }
}
