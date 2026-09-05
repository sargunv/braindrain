//! Root application component: the main usage viewer window.
#![allow(clippy::needless_borrow)]

use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use braindrain_core::{BalanceSnapshot, RateWindow, ResetCreditSnapshot};
use braindrain_daemon::{CachedProviderState, DaemonStatus};
use relm4::{
    Component, ComponentController, ComponentParts, ComponentSender, Controller, MessageBroker,
    RelmWidgetExt, abstractions::Toaster, adw, gtk,
};

use crate::backend::{self, Backend};
use crate::settings::SettingsModel;

use braindrain_desktop as desktop;

/// How often to auto-refresh usage in the background.
const REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Broker so dynamically-created provider buttons can send input.
pub static APP_BROKER: MessageBroker<AppMsg> = MessageBroker::new();

/// Friendly display names for provider ids, mirroring the Plasma plasmoid.
pub fn provider_title(id: &str) -> &str {
    match id {
        "openai" => "OpenAI",
        "claude" => "Claude Code",
        "cursor" => "Cursor",
        "kimi" => "Kimi Code",
        "zai" => "z.ai",
        "opencode-go" => "OpenCode Go",
        "google" => "Google AI",
        other => other,
    }
}

#[derive(Debug)]
pub struct AppModel {
    backend: Option<Arc<dyn Backend>>,
    status: Option<DaemonStatus>,
    selected: Option<String>,
    refreshing: bool,
    toaster: Toaster,
    settings: Controller<SettingsModel>,
}

#[derive(Debug)]
pub enum AppMsg {
    ResolveBackend,
    BackendReady {
        backend: Option<Arc<dyn Backend>>,
    },
    InstallDaemon,
    InstallResult {
        result: anyhow::Result<()>,
    },
    StatusLoaded {
        result: anyhow::Result<DaemonStatus>,
    },
    RefreshAll,
    Tick,
    SelectProvider(String),
    ShowSettings,
}

#[relm4::component(pub)]
impl Component for AppModel {
    type Init = ();
    type Input = AppMsg;
    type Output = ();
    type CommandOutput = AppMsg;

    view! {
        adw::ApplicationWindow {
            set_default_width: 420,
            set_default_height: 560,
            set_title: Some("BrainDrain"),

            #[local_ref]
            toast_overlay -> adw::ToastOverlay {
                adw::ToolbarView {
                    add_top_bar = &adw::HeaderBar {
                        #[wrap(Some)]
                        set_title_widget = &adw::WindowTitle {
                            set_title: "BrainDrain",
                            #[watch]
                            set_subtitle: &header_subtitle(&model),
                        },

                        pack_start = &gtk::Button {
                            set_icon_name: "view-refresh-symbolic",
                            set_tooltip_text: Some("Refresh"),
                            #[watch]
                            set_sensitive: !model.refreshing && model.backend.is_some(),
                            connect_clicked => AppMsg::RefreshAll,
                        },

                        pack_end = &gtk::Button {
                            set_icon_name: "emblem-system-symbolic",
                            set_tooltip_text: Some("Settings"),
                            connect_clicked => AppMsg::ShowSettings,
                        },
                    },

                    gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,
                        set_margin_top: 12,
                        set_margin_bottom: 12,
                        set_margin_start: 12,
                        set_margin_end: 12,
                        set_spacing: 12,

                        #[name(provider_group)]
                        adw::ToggleGroup {
                            set_halign: gtk::Align::Fill,
                            set_homogeneous: true,
                            set_can_shrink: true,
                            #[watch]
                            set_visible: model.backend.is_some(),
                        },

                        gtk::ScrolledWindow {
                            set_hscrollbar_policy: gtk::PolicyType::Never,
                            set_vscrollbar_policy: gtk::PolicyType::Automatic,
                            set_expand: true,

                            adw::Clamp {
                                set_maximum_size: 560,
                                set_tightening_threshold: 420,

                                #[name(content_box)]
                                gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,
                                    set_margin_top: 18,
                                    set_margin_bottom: 18,
                                    set_margin_start: 18,
                                    set_margin_end: 18,
                                    set_spacing: 18,
                                },
                            },
                        },

                    }
                }
            }
        }
    }

    fn init(
        _: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let settings = SettingsModel::builder()
            .launch_with_broker(
                root.clone().upcast::<gtk::Window>(),
                &crate::settings::SETTINGS_BROKER,
            )
            .forward(sender.input_sender(), |msg| match msg {
                crate::settings::SettingsOutput::DaemonChanged => AppMsg::ResolveBackend,
            });

        let model = AppModel {
            backend: None,
            status: None,
            selected: None,
            refreshing: false,
            toaster: Toaster::default(),
            settings,
        };

        let toast_overlay = model.toaster.overlay_widget();

        let widgets = view_output!();
        widgets.provider_group.connect_active_name_notify(|group| {
            if let Some(name) = group.active_name() {
                APP_BROKER.send(AppMsg::SelectProvider(name.to_string()));
            }
        });

        sender.input(AppMsg::ResolveBackend);

        sender.command(|out, shutdown| async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(REFRESH_INTERVAL) => {
                        let _ = out.send(AppMsg::Tick);
                    }
                    _ = shutdown.clone().wait() => break,
                }
            }
        });

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>, _root: &Self::Root) {
        match msg {
            AppMsg::ResolveBackend => {
                sender.oneshot_command(async move {
                    let backend = backend::resolve().await.map(Arc::from);
                    AppMsg::BackendReady { backend }
                });
            }

            AppMsg::BackendReady { backend } => {
                self.backend = backend;
                self.status = None;
                self.selected = None;
                if let Some(b) = &self.backend {
                    fetch_status(Arc::clone(b), sender);
                }
            }

            AppMsg::InstallDaemon => {
                sender.spawn_oneshot_command(|| {
                    let exe = match std::env::current_exe() {
                        Ok(p) => p,
                        Err(error) => {
                            log::error!("daemon install failed: {error:?}");
                            return AppMsg::InstallResult {
                                result: Err(anyhow::Error::from(error)),
                            };
                        }
                    };
                    match desktop::daemon::install(&exe, "--daemon-run").map(|_| ()) {
                        Ok(()) => AppMsg::InstallResult { result: Ok(()) },
                        Err(error) => {
                            log::error!("daemon install failed: {error:?}");
                            AppMsg::InstallResult { result: Err(error) }
                        }
                    }
                });
            }

            AppMsg::InstallResult { result } => match result {
                Ok(()) => sender.input(AppMsg::ResolveBackend),
                Err(error) => self.toaster.toast(&error.to_string()),
            },

            AppMsg::StatusLoaded { result } => {
                self.refreshing = false;
                match result {
                    Ok(status) => self.apply_status(status),
                    Err(error) => self.toaster.toast(&error.to_string()),
                }
            }

            AppMsg::RefreshAll | AppMsg::Tick => {
                if !self.refreshing
                    && let Some(backend) = self.backend.clone()
                {
                    self.refreshing = true;
                    sender.oneshot_command(async move {
                        backend.refresh_all().await.ok();
                        let status = backend.status().await;
                        AppMsg::StatusLoaded { result: status }
                    });
                }
            }

            AppMsg::SelectProvider(id) => {
                if self.selected.as_deref() != Some(id.as_str()) {
                    self.selected = Some(id);
                }
            }

            AppMsg::ShowSettings => {
                self.settings.emit(crate::settings::SettingsMsg::Show);
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
        rebuild_provider_group(&widgets.provider_group, self);
        rebuild_content(&widgets.content_box, self);
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn fetch_status(backend: Arc<dyn Backend>, sender: ComponentSender<AppModel>) {
    sender.oneshot_command(async move {
        AppMsg::StatusLoaded {
            result: backend.status().await,
        }
    });
}

fn header_subtitle(model: &AppModel) -> String {
    if model.backend.is_none() {
        return "Daemon not installed".to_owned();
    }
    if model.refreshing {
        return "Refreshing…".to_owned();
    }
    if let Some(status) = &model.status
        && let Some(first) = status.providers.first()
        && let Some(t) = first.last_success_at
    {
        return format!("Updated {}", format_time(t));
    }
    String::new()
}

fn format_time(t: time::OffsetDateTime) -> String {
    let format = time::macros::format_description!("[hour]:[minute]");
    t.format(format).unwrap_or_else(|_| "?".to_owned())
}

fn relative_time(t: time::OffsetDateTime) -> String {
    let seconds = (t - time::OffsetDateTime::now_utc()).whole_seconds();
    if let Ok(seconds) = u64::try_from(seconds) {
        if seconds == 0 {
            return "now".to_owned();
        }
        return format!(
            "in {}",
            humantime::format_duration(Duration::from_secs(seconds))
        );
    }
    format!(
        "{} ago",
        humantime::format_duration(Duration::from_secs(seconds.unsigned_abs()))
    )
}

impl AppModel {
    fn apply_status(&mut self, status: DaemonStatus) {
        let providers: Vec<String> = status
            .providers
            .iter()
            .map(|p| p.provider.clone())
            .collect();
        match &self.selected {
            Some(sel) if providers.iter().any(|p| p == sel) => {}
            _ => self.selected = providers.first().cloned(),
        }
        self.status = Some(status);
    }
}

fn rebuild_provider_group(group: &adw::ToggleGroup, model: &AppModel) {
    let Some(status) = &model.status else { return };

    if !provider_group_matches(group, status) {
        group.remove_all();
        for state in &status.providers {
            let id = state.provider.clone();
            let toggle = adw::Toggle::new();
            toggle.set_name(Some(&id));
            toggle.set_label(Some(provider_title(&id)));
            group.add(toggle);
        }
    }

    if let Some(selected) = &model.selected {
        let active = group.active_name().map(|name| name.to_string());
        if active.as_deref() != Some(selected.as_str()) {
            group.set_active_name(Some(selected));
        }
    }
}

fn provider_group_matches(group: &adw::ToggleGroup, status: &DaemonStatus) -> bool {
    if group.n_toggles() as usize != status.providers.len() {
        return false;
    }

    status.providers.iter().enumerate().all(|(index, state)| {
        group
            .toggle(index as u32)
            .is_some_and(|toggle| toggle.name().as_str() == state.provider)
    })
}

fn rebuild_content(content_box: &gtk::Box, model: &AppModel) {
    while let Some(child) = content_box.first_child() {
        content_box.remove(&child);
    }

    if model.backend.is_none() {
        content_box.append(&install_prompt());
        return;
    }

    let Some(status) = &model.status else {
        content_box.append(&status_page("Loading", None));
        return;
    };

    let Some(selected) = &model.selected else {
        content_box.append(&status_page("No Providers", None));
        return;
    };

    let Some(state) = status.providers.iter().find(|p| &p.provider == selected) else {
        return;
    };

    content_box.append(&build_provider_content(state));
}

fn install_prompt() -> adw::StatusPage {
    let page = adw::StatusPage::new();
    page.set_title("Daemon not installed");
    page.set_description(Some(
        "Install the BrainDrain daemon to start tracking usage. It runs on demand whenever any client needs it.",
    ));
    page.set_icon_name(Some("emblem-system-symbolic"));

    let button = gtk::Button::with_label("Install daemon");
    button.add_css_class("suggested-action");
    button.add_css_class("pill");
    button.set_halign(gtk::Align::Center);
    button.connect_clicked(|_| APP_BROKER.send(AppMsg::InstallDaemon));
    page.set_child(Some(&button));

    page
}

fn build_provider_content(state: &CachedProviderState) -> gtk::Box {
    let container = gtk::Box::new(gtk::Orientation::Vertical, 18);
    let group = adw::PreferencesGroup::builder()
        .title(provider_title(&state.provider))
        .build();

    if let Some(error) = &state.error {
        let row = adw::ActionRow::builder()
            .title("Error")
            .subtitle(error)
            .subtitle_lines(3)
            .build();
        group.add(&row);
    }

    if state.snapshot.is_none() && state.error.is_none() {
        container.append(&status_page("No Usage Data", None));
        return container;
    }

    if let Some(snap) = &state.snapshot {
        if let Some(identity) = &snap.identity {
            let row = adw::ActionRow::builder().title("Account").build();
            if let Some(email) = &identity.email {
                row.set_subtitle(email);
            }
            if let Some(plan) = &identity.plan {
                let label = gtk::Label::new(Some(plan));
                label.set_valign(gtk::Align::Center);
                label.add_css_class("caption");
                label.add_css_class("dim-label");
                row.add_suffix(&label);
            }
            group.add(&row);
        }

        for window in &snap.usage.windows {
            group.add(&build_rate_window(window));
        }
        for balance in &snap.usage.balances {
            group.add(&build_balance(balance));
        }
        for credit in &snap.usage.reset_credits {
            group.add(&build_reset_credit(credit));
        }
    }

    container.append(&group);
    container
}

fn build_rate_window(window: &RateWindow) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(&window.label).build();
    if let Some(resets_at) = window.resets_at {
        row.set_subtitle(&format!("Resets {}", relative_time(resets_at)));
    }
    let bar = gtk::ProgressBar::new();
    bar.set_fraction(window.used_percent.clamp(0.0, 100.0) / 100.0);
    bar.set_valign(gtk::Align::Center);
    bar.set_size_request(120, -1);
    row.add_suffix(&bar);

    let percent = gtk::Label::new(Some(&format!("{:.0}%", window.used_percent)));
    percent.set_valign(gtk::Align::Center);
    percent.set_width_chars(4);
    percent.set_xalign(1.0);
    percent.add_css_class("numeric");
    row.add_suffix(&percent);
    row
}

fn build_balance(balance: &BalanceSnapshot) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(&balance.label).build();
    let value = gtk::Label::new(Some(&format!("{:.0} {}", balance.remaining, balance.unit)));
    value.set_valign(gtk::Align::Center);
    value.add_css_class("numeric");
    row.add_suffix(&value);
    row
}

fn build_reset_credit(credit: &ResetCreditSnapshot) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title("Quota reset credit")
        .build();
    if let Some(expires) = credit.expires_at {
        row.set_subtitle(&format!("Expires {}", relative_time(expires)));
    }
    row
}

fn status_page(title: &str, description: Option<&str>) -> adw::StatusPage {
    let page = adw::StatusPage::new();
    page.set_title(title);
    if let Some(description) = description {
        page.set_description(Some(description));
    }
    page
}
