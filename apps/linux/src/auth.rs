//! Auth login dialog, rendered from the provider credential schema.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use braindrain_core::ProviderCredentials;
use relm4::{Component, ComponentParts, ComponentSender, adw, gtk};

#[derive(Debug)]
pub enum AuthMsg {
    Show { provider: String },
    Submit,
    Cancel,
    StoreCompleted { provider: String, success: bool },
}

#[allow(dead_code)]
#[derive(Debug)]
pub enum AuthOutput {
    Closed { provider: String, success: bool },
}

#[derive(Debug, Clone)]
enum CredentialEntry {
    Text(adw::EntryRow),
    Password(adw::PasswordEntryRow),
}

impl CredentialEntry {
    fn widget(&self) -> gtk::Widget {
        match self {
            CredentialEntry::Text(row) => row.clone().upcast(),
            CredentialEntry::Password(row) => row.clone().upcast(),
        }
    }

    fn text(&self) -> String {
        match self {
            CredentialEntry::Text(row) => row.text().to_string(),
            CredentialEntry::Password(row) => row.text().to_string(),
        }
    }
}

#[derive(Debug)]
pub struct AuthModel {
    provider: String,
    schema: Option<braindrain_core::ProviderCredentialSchema>,
    /// Live entry rows for the current schema. Created on Show; read on Submit.
    entries: Rc<RefCell<Vec<(String, CredentialEntry)>>>,
    visible: bool,
    submitting: bool,
    last_error: Option<String>,
    /// Bumped whenever the form needs to be re-rendered into the form_box.
    form_version: u32,
}

#[relm4::component(pub)]
impl Component for AuthModel {
    type Init = ();
    type Input = AuthMsg;
    type Output = AuthOutput;
    type CommandOutput = AuthMsg;

    view! {
        adw::Window {
            set_default_width: 380,
            set_modal: true,
            set_hide_on_close: true,
            #[watch]
            set_visible: model.visible,
            #[watch]
            set_title: Some(&format!(
                "{} Login",
                crate::app::provider_title(&model.provider)
            )),

            adw::ToolbarView {
                add_top_bar = &adw::HeaderBar {
                    #[wrap(Some)]
                    set_title_widget = &adw::WindowTitle {
                        #[watch]
                        set_title: &format!(
                            "{} Login",
                            crate::app::provider_title(&model.provider)
                        ),
                    },
                },

                gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,
                    set_margin_top: 18,
                    set_margin_bottom: 18,
                    set_margin_start: 18,
                    set_margin_end: 18,
                    set_spacing: 12,

                    #[name(form_box)]
                    gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,
                        set_spacing: 12,
                    },

                    gtk::Label {
                        #[watch]
                        set_visible: model.last_error.is_some(),
                        #[watch]
                        set_label: model.last_error.as_deref().unwrap_or_default(),
                        add_css_class: "error",
                        set_wrap: true,
                        set_xalign: 0.0,
                    },

                    gtk::Box {
                        set_orientation: gtk::Orientation::Horizontal,
                        set_spacing: 8,
                        set_halign: gtk::Align::End,

                        gtk::Button {
                            set_label: "Cancel",
                            connect_clicked => AuthMsg::Cancel,
                        },
                        gtk::Button {
                            set_label: "Save",
                            add_css_class: "suggested-action",
                            #[watch]
                            set_sensitive: !model.submitting,
                            connect_clicked => AuthMsg::Submit,
                        },
                    },
                }
            },
        }
    }

    fn init(
        _: Self::Init,
        _root: Self::Root,
        _sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let model = AuthModel {
            provider: String::new(),
            schema: None,
            entries: Rc::new(RefCell::new(Vec::new())),
            visible: false,
            submitting: false,
            last_error: None,
            form_version: 0,
        };
        let widgets = view_output!();
        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>, _root: &Self::Root) {
        match msg {
            AuthMsg::Show { provider } => {
                let schema = braindrain_service::credential_schema(&provider);
                self.provider = provider;
                self.schema = schema.clone();
                self.last_error = None;
                self.visible = true;

                // Build entry widgets for the schema.
                let mut entries = Vec::new();
                if let Some(schema) = schema {
                    for field in &schema.fields {
                        let entry = if field.secret {
                            let row = adw::PasswordEntryRow::new();
                            row.set_title(&field.label);
                            CredentialEntry::Password(row)
                        } else {
                            let row = adw::EntryRow::new();
                            row.set_title(&field.label);
                            CredentialEntry::Text(row)
                        };
                        entries.push((field.id.clone(), entry));
                    }
                }
                self.entries.borrow_mut().clear();
                self.entries.borrow_mut().extend(entries);
                self.form_version = self.form_version.wrapping_add(1);
            }

            AuthMsg::Submit => {
                if self.submitting {
                    return;
                }
                let Some(schema) = self.schema.clone() else {
                    self.last_error = Some("No credential schema for this provider".to_owned());
                    return;
                };
                let entries = self.entries.borrow();
                let mut values = HashMap::new();
                let mut missing = None;
                for (id, entry) in entries.iter() {
                    let value = entry.text();
                    if value.is_empty() {
                        missing = Some(id.clone());
                        break;
                    }
                    values.insert(id.clone(), value);
                }
                drop(entries);

                if let Some(missing) = missing {
                    self.last_error = Some(format!("{missing} is required"));
                    return;
                }

                self.submitting = true;
                self.last_error = None;
                let credentials = ProviderCredentials {
                    provider: schema.provider.clone(),
                    values,
                };
                let provider_for_close = self.provider.clone();
                sender.oneshot_command(async move {
                    let success = braindrain_service::store_credentials(credentials)
                        .await
                        .is_ok();
                    AuthMsg::StoreCompleted {
                        provider: provider_for_close,
                        success,
                    }
                });
            }

            AuthMsg::Cancel => {
                self.visible = false;
                self.submitting = false;
                self.last_error = None;
            }

            AuthMsg::StoreCompleted { provider, success } => {
                self.submitting = false;
                self.visible = false;
                let _ = sender.output(AuthOutput::Closed { provider, success });
            }
        }
    }

    fn update_cmd(
        &mut self,
        msg: Self::CommandOutput,
        sender: ComponentSender<Self>,
        root: &Self::Root,
    ) {
        self.update(msg, sender, root);
    }

    fn post_view(&self, widgets: &mut Self::Widgets) {
        while let Some(child) = widgets.form_box.first_child() {
            widgets.form_box.remove(&child);
        }
        let entries = self.entries.borrow();
        for (_, entry) in entries.iter() {
            widgets.form_box.append(&entry.widget());
        }
    }
}
