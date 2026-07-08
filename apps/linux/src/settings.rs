//! Settings sub-component: integrations and accounts.

use adw::prelude::*;
use relm4::{
    Component, ComponentController, ComponentParts, ComponentSender, Controller, MessageBroker,
    adw, gtk,
};

use crate::auth::AuthModel;

pub static SETTINGS_BROKER: MessageBroker<SettingsMsg> = MessageBroker::new();

#[derive(Debug)]
pub enum SettingsMsg {
    Show,
    RefreshState,
    DaemonToggle,
    PlasmaToggle,
    AuthOpen(String),
    AuthLogout(String),
    DaemonStateReady { installed: bool },
    PlasmaStateReady { available: bool, installed: bool },
    AuthStateReady(Vec<AuthRow>),
    ShowToast(String),
}

#[derive(Debug, Clone)]
pub struct AuthRow {
    pub provider: String,
    pub supports_credentials: bool,
    pub configured: bool,
}

#[derive(Debug)]
pub enum SettingsOutput {
    /// Emitted after the daemon install/uninstall state may have changed, so
    /// the parent can re-resolve its backend.
    DaemonChanged,
}

#[derive(Debug)]
pub struct SettingsModel {
    parent: gtk::Window,
    daemon_installed: bool,
    plasma_available: bool,
    plasma_installed: bool,
    auth_rows: Vec<AuthRow>,
    auth_dialog: Controller<AuthModel>,
}

#[relm4::component(pub)]
impl Component for SettingsModel {
    type Init = gtk::Window;
    type Input = SettingsMsg;
    type Output = SettingsOutput;
    type CommandOutput = SettingsMsg;

    view! {
        adw::PreferencesDialog {
            set_content_width: 520,
            set_content_height: 560,
            set_search_enabled: false,
            set_title: "BrainDrain Settings",

            add = &adw::PreferencesPage {
                add = &adw::PreferencesGroup {
                    set_title: "Integrations",

                    adw::ActionRow {
                        set_title: "Daemon",
                        #[watch]
                        set_subtitle: if model.daemon_installed { "Installed" } else { "Not installed" },

                        add_suffix = &gtk::Button {
                            set_valign: gtk::Align::Center,
                            #[watch]
                            set_label: if model.daemon_installed { "Uninstall" } else { "Install" },
                            connect_clicked => SettingsMsg::DaemonToggle,
                        },
                    },

                    adw::ActionRow {
                        set_title: "KDE Plasma widget",
                        #[watch]
                        set_subtitle: if model.plasma_installed { "Installed" } else { "Not installed" },
                        #[watch]
                        set_visible: model.plasma_available,

                        add_suffix = &gtk::Button {
                            set_valign: gtk::Align::Center,
                            #[watch]
                            set_label: if model.plasma_installed { "Uninstall" } else { "Install" },
                            connect_clicked => SettingsMsg::PlasmaToggle,
                        },
                    },
                },

                add = &adw::PreferencesGroup {
                    set_title: "Accounts",

                    #[name(auth_list)]
                    gtk::ListBox {
                        add_css_class: "boxed-list",
                        set_selection_mode: gtk::SelectionMode::None,
                    },
                },
            },
        }
    }

    fn init(
        parent: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let auth_dialog = AuthModel::builder()
            .transient_for(&parent)
            .launch(())
            .forward(sender.input_sender(), |_| SettingsMsg::RefreshState);

        let model = SettingsModel {
            parent,
            daemon_installed: false,
            plasma_available: false,
            plasma_installed: false,
            auth_rows: Vec::new(),
            auth_dialog,
        };

        let widgets = view_output!();
        sender.input(SettingsMsg::RefreshState);
        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>, root: &Self::Root) {
        match msg {
            SettingsMsg::Show => {
                root.present(Some(&self.parent));
                sender.input(SettingsMsg::RefreshState);
            }
            SettingsMsg::RefreshState => {
                probe_daemon_state(sender.clone());
                probe_plasma_state(sender.clone());
                probe_auth_state(sender);
            }
            SettingsMsg::DaemonToggle => {
                if self.daemon_installed {
                    spawn_desktop(sender.clone(), "uninstall", desktop::daemon::uninstall)
                } else {
                    spawn_desktop(sender.clone(), "install", || {
                        let exe = std::env::current_exe()?;
                        desktop::daemon::install(&exe, "--daemon-run").map(|_| ())
                    })
                }
            }
            SettingsMsg::PlasmaToggle => {
                if self.plasma_installed {
                    spawn_desktop(
                        sender.clone(),
                        "plasma uninstall",
                        desktop::plasma::uninstall,
                    )
                } else {
                    spawn_desktop(sender.clone(), "plasma install", desktop::plasma::install)
                }
            }
            SettingsMsg::AuthOpen(provider) => {
                self.auth_dialog
                    .emit(crate::auth::AuthMsg::Show { provider });
            }
            SettingsMsg::AuthLogout(provider) => {
                sender.oneshot_command(async move {
                    let _ = braindrain_service::delete_credentials(&provider).await;
                    SettingsMsg::RefreshState
                });
            }
            SettingsMsg::DaemonStateReady { installed } => {
                self.daemon_installed = installed;
                let _ = sender.output(SettingsOutput::DaemonChanged);
            }
            SettingsMsg::PlasmaStateReady {
                available,
                installed,
            } => {
                self.plasma_available = available;
                self.plasma_installed = installed;
            }
            SettingsMsg::AuthStateReady(rows) => {
                self.auth_rows = rows;
            }
            SettingsMsg::ShowToast(message) => {
                root.add_toast(adw::Toast::new(&message));
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
        rebuild_auth_rows(&widgets.auth_list, self);
    }
}

fn probe_daemon_state(sender: ComponentSender<SettingsModel>) {
    sender.spawn_oneshot_command(|| SettingsMsg::DaemonStateReady {
        installed: desktop::daemon::is_installed(),
    });
}

fn probe_plasma_state(sender: ComponentSender<SettingsModel>) {
    sender.spawn_oneshot_command(|| SettingsMsg::PlasmaStateReady {
        available: desktop::plasma::is_session_plasma(),
        installed: desktop::plasma::is_installed(),
    });
}

fn probe_auth_state(sender: ComponentSender<SettingsModel>) {
    sender.oneshot_command(async move {
        let mut rows = Vec::new();
        for provider in braindrain_service::provider_ids() {
            let id = provider.as_str().to_owned();
            let supports = braindrain_service::credential_schema(&id).is_some();
            let configured = if supports {
                braindrain_service::info_provider(&id)
                    .await
                    .map(|i| {
                        i.fields
                            .iter()
                            .any(|f| f.key == "auth_found" && f.value == "true")
                    })
                    .unwrap_or(false)
            } else {
                false
            };
            rows.push(AuthRow {
                provider: id,
                supports_credentials: supports,
                configured,
            });
        }
        SettingsMsg::AuthStateReady(rows)
    });
}

fn spawn_desktop<F>(sender: ComponentSender<SettingsModel>, label: &'static str, f: F)
where
    F: FnOnce() -> anyhow::Result<()> + Send + 'static,
{
    sender.spawn_oneshot_command(move || match f() {
        Ok(()) => SettingsMsg::RefreshState,
        Err(error) => {
            log::error!("{label} failed: {error:?}");
            SettingsMsg::ShowToast(error.to_string())
        }
    });
}

fn rebuild_auth_rows(auth_list: &gtk::ListBox, model: &SettingsModel) {
    while let Some(child) = auth_list.first_child() {
        auth_list.remove(&child);
    }
    for row in &model.auth_rows {
        if !row.supports_credentials {
            continue;
        }

        let action_row = adw::ActionRow::builder()
            .title(crate::app::provider_title(&row.provider))
            .subtitle(if row.configured {
                "Configured"
            } else {
                "Not configured"
            })
            .build();

        let login_btn = gtk::Button::with_label(if row.configured { "Re-login" } else { "Login" });
        login_btn.set_valign(gtk::Align::Center);
        let id_for_login = row.provider.clone();
        login_btn.connect_clicked(move |_| {
            SETTINGS_BROKER.send(SettingsMsg::AuthOpen(id_for_login.clone()));
        });
        action_row.add_suffix(&login_btn);

        if row.configured {
            let logout_btn = gtk::Button::new();
            logout_btn.set_icon_name("user-trash-symbolic");
            logout_btn.set_tooltip_text(Some("Log out"));
            logout_btn.set_valign(gtk::Align::Center);
            logout_btn.add_css_class("destructive-action");
            let id_for_logout = row.provider.clone();
            logout_btn.connect_clicked(move |_| {
                SETTINGS_BROKER.send(SettingsMsg::AuthLogout(id_for_logout.clone()));
            });
            action_row.add_suffix(&logout_btn);
        }

        auth_list.append(&action_row);
    }
}

use braindrain_desktop as desktop;
