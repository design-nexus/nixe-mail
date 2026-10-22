//! Nixe-style building blocks.

pub use nixe_ui::widgets::*;

use gtk::prelude::*;

pub struct Page {
    pub root: gtk::ScrolledWindow,
    pub body: gtk::Box,
}

pub fn page(section: &str) -> Page {
    let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
    body.add_css_class("settings-page");
    body.add_css_class(&format!("page-{section}"));
    body.set_hexpand(true);
    let root = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::External)
        .child(&body)
        .vexpand(true)
        .build();
    Page { root, body }
}

pub struct Group {
    pub wrapper: gtk::Box,
    pub list: gtk::Box,
}

impl Page {
    pub fn group(&self, title: &str) -> Group {
        let wrapper = gtk::Box::new(gtk::Orientation::Vertical, 0);
        wrapper.add_css_class("settings-group");
        wrapper.set_widget_name(title);
        if !title.is_empty() {
            let label = gtk::Label::new(Some(&title.to_uppercase()));
            label.add_css_class("group-title");
            label.set_xalign(0.0);
            wrapper.append(&label);
        }
        let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
        list.add_css_class("group-list");
        wrapper.append(&list);
        self.body.append(&wrapper);
        Group { wrapper, list }
    }
}

impl Group {
    pub fn add(&self, widget: &impl IsA<gtk::Widget>) {
        self.list.append(widget);
    }

    pub fn note(&self, text: &str) {
        let label = gtk::Label::new(Some(text));
        label.add_css_class("group-note");
        label.set_xalign(0.0);
        label.set_wrap(true);
        self.wrapper.insert_child_after(&label, self.wrapper.first_child().as_ref());
    }
}
