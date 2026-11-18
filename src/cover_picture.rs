// SPDX-FileCopyrightText: 2022  Emmanuele Bassi
// SPDX-License-Identifier: GPL-3.0-or-later

use std::cell::{Cell, RefCell};

use glib::clone;
use gtk::{gdk, gio, glib, graphene, gsk, prelude::*, subclass::prelude::*};

use crate::i18n::i18n;

#[derive(Clone, Copy, Debug, glib::Enum, PartialEq, Default)]
#[enum_type(name = "AmberolCoverSize")]
pub enum CoverSize {
    #[default]
    Large = 0,
    Small = 1,
    Album = 2,
    Expanded = 3,
}

impl CoverSize {
    /// Side of the square allocated to this cover size, in application pixels.
    pub fn pixels(&self) -> i32 {
        match self {
            CoverSize::Large => 192,
            CoverSize::Small => 48,
            CoverSize::Album => 200,
            CoverSize::Expanded => 250,
        }
    }
}

impl AsRef<str> for CoverSize {
    fn as_ref(&self) -> &str {
        match self {
            CoverSize::Large => "large",
            CoverSize::Small => "small",
            CoverSize::Album => "album",
            CoverSize::Expanded => "expanded",
        }
    }
}

mod imp {
    use glib::{ParamSpec, ParamSpecEnum, ParamSpecObject, Value};
    use once_cell::sync::Lazy;

    use super::*;

    #[derive(Debug, Default)]
    pub struct CoverPicture {
        pub cover: RefCell<Option<gdk::Texture>>,
        pub cover_size: Cell<CoverSize>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for CoverPicture {
        const NAME: &'static str = "AmberolCoverPicture";
        type Type = super::CoverPicture;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("picture");
            klass.set_accessible_role(gtk::AccessibleRole::Img);
        }
    }

    impl ObjectImpl for CoverPicture {
        fn constructed(&self) {
            self.parent_constructed();

            self.obj().add_css_class("cover");
            self.obj().set_overflow(gtk::Overflow::Hidden);

            self.obj().connect_notify_local(
                Some("scale-factor"),
                clone!(
                    #[weak(rename_to = _obj)]
                    self,
                    move |picture, _| {
                        picture.queue_draw();
                    }
                ),
            );

            self.obj()
                .upcast_ref::<gtk::Accessible>()
                .update_property(&[(gtk::accessible::Property::Label(&i18n("Cover image")))]);
        }

        fn properties() -> &'static [ParamSpec] {
            static PROPERTIES: Lazy<Vec<ParamSpec>> = Lazy::new(|| {
                vec![
                    ParamSpecObject::builder::<gdk::Texture>("cover").build(),
                    ParamSpecEnum::builder::<CoverSize>("cover-size").build(),
                ]
            });
            PROPERTIES.as_ref()
        }

        fn property(&self, _id: usize, pspec: &ParamSpec) -> Value {
            match pspec.name() {
                "cover" => self.cover.borrow().to_value(),
                "cover-size" => self.cover_size.get().to_value(),
                _ => unimplemented!(),
            }
        }

        fn set_property(&self, _id: usize, value: &Value, pspec: &ParamSpec) {
            match pspec.name() {
                "cover" => self
                    .obj()
                    .set_cover(value.get::<gdk::Texture>().ok().as_ref()),
                "cover-size" => self
                    .obj()
                    .set_cover_size(value.get::<CoverSize>().expect("Required CoverSize")),
                _ => unimplemented!(),
            };
        }
    }

    impl WidgetImpl for CoverPicture {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::ConstantSize
        }

        fn measure(&self, _orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let size = self.cover_size.get().pixels();
            (size, size, -1, -1)
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            if let Some(ref cover) = *self.cover.borrow() {
                let widget = self.obj();
                let width = widget.width() as f32;
                let height = widget.height() as f32;
                if width <= 0.0 || height <= 0.0 {
                    return;
                }

                let raw_ratio = cover.intrinsic_aspect_ratio() as f32;
                let ratio = if raw_ratio.is_finite() && raw_ratio > 0.0 {
                    raw_ratio
                } else {
                    1.0
                };

                // Always fill the square frame from its centre: scale the
                // artwork uniformly until it covers the whole widget and let
                // the clip discard the excess. A 1:1 photo is simply drawn
                // square at 1:1, and no aspect ratio is ever stretched at the
                // edges. This applies to the player, the queue and the grid.
                let (w, h) = if ratio > 1.0 {
                    (height * ratio, height)
                } else {
                    (width, width / ratio)
                };

                let x = (width - w) / 2.0;
                let y = (height - h) / 2.0;

                snapshot.push_clip(&graphene::Rect::new(0.0, 0.0, width, height));
                snapshot.save();
                snapshot.translate(&graphene::Point::new(x, y));
                snapshot.append_scaled_texture(
                    cover,
                    gsk::ScalingFilter::Trilinear,
                    &graphene::Rect::new(0.0, 0.0, w, h),
                );
                snapshot.restore();
                snapshot.pop();
            }
        }
    }
}

glib::wrapper! {
    pub struct CoverPicture(ObjectSubclass<imp::CoverPicture>)
        @extends gtk::Widget,
        @implements gio::ActionGroup, gio::ActionMap, gtk::Accessible,
                    gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for CoverPicture {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl CoverPicture {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cover(&self) -> Option<gdk::Texture> {
        (*self.imp().cover.borrow()).as_ref().cloned()
    }

    pub fn set_cover(&self, cover: Option<&gdk::Texture>) {
        if let Some(cover) = cover {
            self.imp().cover.replace(Some(cover.clone()));
        } else {
            self.imp().cover.replace(None);
        }

        self.queue_draw();
        self.notify("cover");
    }

    pub fn set_cover_size(&self, cover_size: CoverSize) {
        self.imp().cover_size.replace(cover_size);
        self.queue_resize();
        self.notify("cover-size");
    }
}
