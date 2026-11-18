// SPDX-FileCopyrightText: 2022  Emmanuele Bassi
// SPDX-License-Identifier: GPL-3.0-or-later

use std::cell::RefCell;

use glib::clone;
use gtk::{gio, glib, prelude::*, subclass::prelude::*};
use rand::prelude::*;

mod imp {
    use glib::{ParamSpec, ParamSpecObject, Value};
    use once_cell::sync::Lazy;

    use super::*;

    #[derive(Debug, Default)]
    pub struct ShuffleListModel {
        pub model: RefCell<Option<gio::ListModel>>,
        pub shuffle: RefCell<Option<Vec<u32>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ShuffleListModel {
        const NAME: &'static str = "ShuffleListModel";
        type Type = super::ShuffleListModel;
        type Interfaces = (gio::ListModel,);
    }

    impl ObjectImpl for ShuffleListModel {
        fn properties() -> &'static [ParamSpec] {
            static PROPERTIES: Lazy<Vec<ParamSpec>> = Lazy::new(|| {
                vec![ParamSpecObject::builder::<gio::ListModel>("model")
                    .explicit_notify()
                    .build()]
            });

            PROPERTIES.as_ref()
        }

        fn property(&self, _id: usize, pspec: &ParamSpec) -> Value {
            match pspec.name() {
                "model" => self.model.borrow().to_value(),
                _ => unimplemented!(),
            }
        }

        fn set_property(&self, _id: usize, value: &Value, pspec: &ParamSpec) {
            match pspec.name() {
                "model" => self
                    .obj()
                    .set_model(value.get::<gio::ListModel>().ok().as_ref()),
                _ => unimplemented!(),
            };
        }
    }

    impl ListModelImpl for ShuffleListModel {
        fn item_type(&self) -> glib::Type {
            if let Some(ref model) = *self.model.borrow() {
                return model.item_type();
            }

            glib::Object::static_type()
        }

        fn n_items(&self) -> u32 {
            if let Some(ref model) = *self.model.borrow() {
                return model.n_items();
            }

            0
        }

        fn item(&self, position: u32) -> Option<glib::Object> {
            if let Some(ref model) = *self.model.borrow() {
                if let Some(ref shuffle) = *self.shuffle.borrow() {
                    if let Some(shuffled_pos) = shuffle.get(position as usize) {
                        return model.item(*shuffled_pos);
                    }
                }
                return model.item(position);
            }

            None
        }
    }
}

glib::wrapper! {
    pub struct ShuffleListModel(ObjectSubclass<imp::ShuffleListModel>)
        @implements gio::ListModel;
}

impl Default for ShuffleListModel {
    fn default() -> Self {
        Self::new(gio::ListModel::NONE)
    }
}

impl ShuffleListModel {
    pub fn new(model: Option<&impl IsA<gio::ListModel>>) -> Self {
        glib::Object::builder::<Self>()
            .property("model", model.map(|m| m.as_ref()))
            .build()
    }

    pub fn model(&self) -> Option<gio::ListModel> {
        self.imp().model.borrow().as_ref().cloned()
    }

    pub fn set_model(&self, model: Option<&gio::ListModel>) {
        if let Some(model) = model {
            self.imp().model.replace(Some(model.clone()));
            model.connect_items_changed(clone!(
                #[strong(rename_to = this)]
                self,
                move |_, position, removed, added| {
                    let mapped_position = {
                        let mut shuffle = this.imp().shuffle.borrow_mut();
                        match shuffle.as_mut() {
                            Some(shuffle) if (position as usize) < shuffle.len() => {
                                Some(shuffle[position as usize])
                            }
                            Some(shuffle) => {
                                // New items are appended to the backing model.
                                // Keep the shuffle map valid until the queue
                                // moves those positions to the desired place.
                                let start = shuffle.len() as u32;
                                for index in 0..added {
                                    shuffle.push(start + index);
                                }
                                Some(position)
                            }
                            None => None,
                        }
                    };

                    this.items_changed(mapped_position.unwrap_or(position), removed, added);
                }
            ));
        } else {
            self.imp().model.replace(None);
        }

        self.notify("model");
    }

    pub fn shuffled(&self) -> bool {
        self.imp().shuffle.borrow().is_some()
    }

    pub fn reshuffle(&self, anchor: u32) {
        if let Some(ref model) = *self.imp().model.borrow() {
            let n_songs = model.n_items();
            let mut rng = rand::rng();

            let positions: Vec<u32> = if anchor == 0 {
                let mut before: Vec<u32> = vec![0];
                let mut after: Vec<u32> = (1..n_songs).collect();
                after.shuffle(&mut rng);

                before.extend(after);
                before
            } else if anchor == n_songs - 1 {
                let mut before: Vec<u32> = (0..n_songs - 1).collect();
                let after: Vec<u32> = vec![n_songs - 1];
                before.shuffle(&mut rng);

                before.extend(after);
                before
            } else {
                let mut before: Vec<u32> = (0..anchor).collect();
                let mut after: Vec<u32> = (anchor + 1..n_songs).collect();
                after.shuffle(&mut rng);

                before.push(anchor);
                before.extend(after);
                before
            };

            self.imp().shuffle.replace(Some(positions));
            self.items_changed(0, model.n_items(), model.n_items());
        } else {
            self.imp().shuffle.replace(None);
        }
    }

    /// Moves the last `count` entries of the shuffle map directly after
    /// `model_position`. The backing model has already had those items
    /// appended, so this is used by "Play Next" without reshuffling the
    /// rest of the queue.
    pub fn insert_positions_after(&self, model_position: u32, count: u32) {
        let Some(ref model) = *self.imp().model.borrow() else {
            return;
        };

        let n_items = model.n_items();
        {
            let mut shuffle = self.imp().shuffle.borrow_mut();
            let Some(shuffle) = shuffle.as_mut() else {
                return;
            };

            let count = count as usize;
            if count == 0 || shuffle.len() < count {
                return;
            }

            let start = shuffle.len() - count;
            let moved: Vec<u32> = shuffle.split_off(start);
            let insert_at = ((model_position + 1) as usize).min(shuffle.len());
            for (offset, position) in moved.into_iter().enumerate() {
                shuffle.insert(insert_at + offset, position);
            }
        }

        self.items_changed(0, n_items, n_items);
    }

    pub fn unshuffle(&self) {
        if let Some(ref model) = *self.imp().model.borrow() {
            self.imp().shuffle.replace(None);
            self.items_changed(0, model.n_items(), model.n_items());
        }
    }
}
