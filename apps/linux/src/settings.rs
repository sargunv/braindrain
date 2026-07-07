//! Settings sub-component: daemon lifecycle, autostart toggle, and auth.

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
    DaemonInstall,
    DaemonUninstall,
    DaemonStart,
    DaemonStop,
    DaemonRestart,
    AutostartToggle(bool),
    AuthOpen(String),
    AuthLogout(String),
    DaemonStateReady {
        installed: bool,
        running: bool,
        enabled: bool,
        error: Option<String>,
    },
    AuthStateReady(Vec<AuthRow>),
}

#[derive(Debug, Clone)]
pub struct AuthRow {
    pub provider: String,
    pub supports_credentials: bool,
    pub configured: bool,
}

#[derive(Debug)]
pub enum SettingsOutput {
    DaemonChanged,
}

#[derive(Debug)]
pub struct SettingsModel {
    parent: gtk::Window,
    installed: bool,
    running: bool,
    enabled: bool,
    auth_rows: Vec<AuthRow>,
    last_error: Option<String>,
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
                    set_title: "Daemon",
                    set_description: Some("Manage the background D-Bus service used by desktop integrations."),

                    adw::ActionRow {
                        set_title: "Daemon service",
                        #[watch]
                        set_subtitle: if model.installed { "Installed" } else { "Not installed" },

                        add_suffix = &gtk::Button {
                            set_valign: gtk::Align::Center,
                            #[watch]
                            set_label: if model.installed { "Uninstall" } else { "Install" },
                            connect_clicked => if model.installed { SettingsMsg::DaemonUninstall } else { SettingsMsg::DaemonInstall },
                        },
                    },

                    adw::ActionRow {
                        set_title: "Service status",
                        #[watch]
                        set_subtitle: if model.running { "Running" } else { "Stopped" },

                        add_suffix = &gtk::Box {
                            set_orientation: gtk::Orientation::Horizontal,
                            set_valign: gtk::Align::Center,
                            set_spacing: 0,
                            add_css_class: "linked",

                            gtk::Button {
                                set_icon_name: "media-playback-start-symbolic",
                                set_tooltip_text: Some("Start"),
                                #[watch]
                                set_sensitive: model.installed && !model.running,
                                connect_clicked => SettingsMsg::DaemonStart,
                            },
                            gtk::Button {
                                set_icon_name: "media-playback-stop-symbolic",
                                set_tooltip_text: Some("Stop"),
                                #[watch]
                                set_sensitive: model.installed && model.running,
                                connect_clicked => SettingsMsg::DaemonStop,
                            },
                            gtk::Button {
                                set_icon_name: "view-refresh-symbolic",
                                set_tooltip_text: Some("Restart"),
                                #[watch]
                                set_sensitive: model.installed,
                                connect_clicked => SettingsMsg::DaemonRestart,
                            },
                        },
                    },

                    adw::SwitchRow {
                        set_title: "Autostart at login",
                        #[watch]
                        set_subtitle: if model.enabled {
                            "The daemon starts when you log in"
                        } else {
                            "The daemon does not autostart"
                        },
                        #[watch]
                        set_sensitive: model.installed,
                        #[watch]
                        set_active: model.enabled,
                        connect_activated => SettingsMsg::AutostartToggle(!model.enabled),
                    },
                },

                add = &adw::PreferencesGroup {
                    set_title: "Accounts",
                    set_description: Some("Manage credentials for providers that support BrainDrain-managed login."),

                    #[name(auth_list)]
                    gtk::ListBox {
                        add_css_class: "boxed-list",
                        set_selection_mode: gtk::SelectionMode::None,
                    },
                },

                add = &adw::PreferencesGroup {
                    #[watch]
                    set_visible: model.last_error.is_some(),
                    add = &adw::ActionRow {
                        set_title: "Error",
                        #[watch]
                        set_subtitle: model.last_error.as_deref().unwrap_or_default(),
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
            installed: false,
            running: false,
            enabled: false,
            auth_rows: Vec::new(),
            last_error: None,
            auth_dialog,
        };

        let widgets = view_output!();
        sender.input(SettingsMsg::RefreshState);
        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>, _root: &Self::Root) {
        match msg {
            SettingsMsg::Show => {
                _root.present(Some(&self.parent));
                sender.input(SettingsMsg::RefreshState);
            }
            SettingsMsg::RefreshState => {
                probe_daemon_state(sender.clone());
                probe_auth_state(sender);
            }
            SettingsMsg::DaemonInstall => spawn_desktop(sender.clone(), "install", || {
                desktop::daemon::install().map(|_| ())
            }),
            SettingsMsg::DaemonUninstall => {
                spawn_desktop(sender.clone(), "uninstall", desktop::daemon::uninstall)
            }
            SettingsMsg::DaemonStart => {
                spawn_desktop(sender.clone(), "start", desktop::daemon::start)
            }
            SettingsMsg::DaemonStop => spawn_desktop(sender.clone(), "stop", desktop::daemon::stop),
            SettingsMsg::DaemonRestart => {
                spawn_desktop(sender.clone(), "restart", desktop::daemon::restart)
            }
            SettingsMsg::AutostartToggle(enable) => {
                spawn_desktop(sender.clone(), "autostart", move || {
                    if enable {
                        desktop::daemon::enable()
                    } else {
                        desktop::daemon::disable()
                    }
                })
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
            SettingsMsg::DaemonStateReady {
                installed,
                running,
                enabled,
                error,
            } => {
                self.installed = installed;
                self.running = running;
                self.enabled = enabled;
                self.last_error = error;
                let _ = sender.output(SettingsOutput::DaemonChanged);
            }
            SettingsMsg::AuthStateReady(rows) => {
                self.auth_rows = rows;
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
        running: desktop::daemon::is_running(),
        enabled: desktop::daemon::is_enabled(),
        error: None,
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
            log::error!("daemon {label} failed: {error:?}");
            SettingsMsg::DaemonStateReady {
                installed: desktop::daemon::is_installed(),
                running: desktop::daemon::is_running(),
                enabled: desktop::daemon::is_enabled(),
                error: Some(error.to_string()),
            }
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
