use adw::prelude::*;
use demo_analysis::lib::algorithm::get_algorithms;
use demo_analysis::lib::parameters::Parameter;
use itertools::Itertools;
use relm4::prelude::*;

use crate::{rcon_manager::RconManager, settings::Settings};

#[derive(Debug)]
pub enum PreferencesMsg {
    Show,
    ConnectionTest(String, u16),
    Close,

    DoubleclickPlay(bool),
    PauseAfterSeek(bool),
    StripConsoleCommands(bool),
    EventSkipOffset(f64),
    TF2FolderPath,
    RConPassword(String),
    RConPort(f64),

    CheatAlgoEnabled(String, bool),
    CheatAlgoParamFloat(String, String, f32),
    CheatAlgoParamInt(String, String, i32),
    CheatAlgoParamBool(String, String, bool),
    CheatAnalysisThreads(f64),

    ProfileLoadRequest,
    ProfileLoad(String),
    ProfileSaveAsRequest,
    ProfileSaveAs(String),
    ProfileImportRequest,
    ProfileImport(String),
    ProfileExport,
}

#[derive(Debug)]
pub enum PreferencesOut {
    Save(Settings),
}

pub struct PreferencesModel {
    parent: adw::Window,

    settings: Settings,
    connection_test_msg: String,
    connection_test_active: bool,
    profile_status: String,
}

#[derive(Debug)]
pub enum PreferencesCmd {
    ConnectionTestResult(String),
    FolderBrowseResult(std::path::PathBuf),
}

#[relm4::component(pub)]
impl Component for PreferencesModel {
    type Init = (Settings, adw::Window);
    type Input = PreferencesMsg;
    type Output = PreferencesOut;
    type CommandOutput = PreferencesCmd;

    view! {
        adw::PreferencesDialog{
            set_search_enabled: false,
            connect_closed[sender] => move |_| {
                sender.input(PreferencesMsg::Close);
            },

            add = &adw::PreferencesPage {
                set_icon_name: Some(relm4_icons::icon_names::SETTINGS),
                set_title: "General",

                adw::PreferencesGroup {
                    set_title: "General",

                    adw::SwitchRow {
                        set_title: "Doubleclick to play demo",
                        set_active: model.settings.doubleclick_play,
                        connect_active_notify[sender] => move |sr| {
                            sender.input(PreferencesMsg::DoubleclickPlay(sr.is_active()));
                        }
                    },

                    adw::SwitchRow {
                        set_title: "Pause demo playback after skipping",
                        set_active: model.settings.pause_after_seek,
                        connect_active_notify[sender] => move |sr| {
                            sender.input(PreferencesMsg::PauseAfterSeek(sr.is_active()));
                        }
                    },

                    adw::SwitchRow {
                        set_title: "Strip console commands from replays",
                        set_subtitle: "Remove recorded console commands (e.g. exec'd configs) before converting a demo to a replay",
                        set_active: model.settings.strip_console_commands,
                        connect_active_notify[sender] => move |sr| {
                            sender.input(PreferencesMsg::StripConsoleCommands(sr.is_active()));
                        }
                    },

                    adw::SpinRow {
                        set_title: "Event skip offset",
                        set_subtitle: "How many seconds before the even the playback should start",
                        set_digits: 1,
                        #[wrap(Some)]
                        set_adjustment = &gtk::Adjustment {
                            set_lower: -300.0,
                            set_upper: 300.0,
                            set_page_increment: 1.0,
                            set_step_increment: 0.1,
                            set_value: model.settings.event_skip_predelay.into(),
                            connect_value_changed[sender] => move |adj| {
                                sender.input(PreferencesMsg::EventSkipOffset(adj.value()));
                            },
                        }
                    },

                    adw::ActionRow {
                        set_title: "TF2 folder",
                        set_tooltip_text: Some("Folder that contains the \"tf\" folder, if set incorrectly replays will not show up in-game!"),
                        #[watch]
                        set_subtitle: model.settings.tf_folder_path.as_ref().map_or("(unset)", |p|p.to_str().unwrap()),
                        set_subtitle_selectable: true,
                        set_activatable_widget: Some(&tf_browse_button),

                        add_suffix: tf_browse_button = &gtk::Button {
                            set_valign: gtk::Align::Center,
                            set_label: "Browse",
                            connect_clicked => PreferencesMsg::TF2FolderPath,
                        }
                    },
                },
                adw::PreferencesGroup {
                    set_title: "RCon",

                    adw::PasswordEntryRow {
                        set_title: "Password",
                        set_text: &model.settings.rcon_pw,
                        connect_changed[sender] => move |per|{
                            sender.input(PreferencesMsg::RConPassword(per.text().as_str().to_owned()))
                        }
                    },

                    adw::SpinRow {
                        set_title: "Port",
                        set_digits: 0,
                        #[wrap(Some)]
                        set_adjustment = &gtk::Adjustment {
                            set_lower: 0.0,
                            set_upper: u16::MAX as f64,
                            set_page_increment: 10.0,
                            set_step_increment: 1.0,
                            set_value: model.settings.rcon_port.into(),
                            connect_value_changed[sender] => move |adj| {
                                sender.input(PreferencesMsg::RConPort(adj.value()));
                            },
                        }
                    },

                    adw::ActionRow {
                        set_title: "Connection Test",
                        set_subtitle_selectable: true,
                        set_activatable_widget: Some(&connection_test_button),
                        #[watch]
                        set_subtitle: &model.connection_test_msg,

                        add_suffix: connection_test_button = &gtk::Button {
                            set_valign: gtk::Align::Center,
                            set_label: "Test",
                            #[watch]
                            set_sensitive: !model.connection_test_active,
                            connect_clicked[sender, pw = model.settings.rcon_pw.clone(), port = model.settings.rcon_port] => move |_|{
                                sender.input(PreferencesMsg::ConnectionTest(pw.clone(), port))
                            }
                        }
                    }
                }
            },

            add = &adw::PreferencesPage {
                set_icon_name: Some(relm4_icons::icon_names::SHIELD_SAFE),
                set_title: "Cheat Detection",

                adw::PreferencesGroup {
                    set_title: "Performance",

                    adw::SpinRow {
                        set_title: "Analysis threads",
                        set_subtitle: "Number of threads used to run detection algorithms concurrently",
                        set_digits: 0,
                        #[wrap(Some)]
                        set_adjustment = &gtk::Adjustment {
                            set_lower: 1.0,
                            set_upper: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f64,
                            set_page_increment: 1.0,
                            set_step_increment: 1.0,
                            set_value: model.settings.cheat_analysis_threads as f64,
                            connect_value_changed[sender] => move |adj| {
                                sender.input(PreferencesMsg::CheatAnalysisThreads(adj.value()));
                            },
                        }
                    },
                },

                adw::PreferencesGroup {
                    set_title: "Profiles",
                    set_description: Some("Save, load, import, and export sets of algorithm parameters."),

                    adw::ActionRow {
                        set_title: "Status",
                        set_subtitle_selectable: true,
                        #[watch]
                        set_visible: !model.profile_status.is_empty(),
                        #[watch]
                        set_subtitle: &model.profile_status,
                    },

                    #[name = "profile_dropdown"]
                    adw::ComboRow {
                        set_title: "Profile",
                        add_suffix = &gtk::Button {
                            set_valign: gtk::Align::Center,
                            set_label: "Load",
                            connect_clicked => PreferencesMsg::ProfileLoadRequest,
                        }
                    },

                    adw::ActionRow {
                        set_title: "Save current parameters",
                        set_activatable_widget: Some(&save_as_button),
                        add_suffix: save_as_button = &gtk::Button {
                            set_valign: gtk::Align::Center,
                            set_label: "Save As...",
                            connect_clicked => PreferencesMsg::ProfileSaveAsRequest,
                        }
                    },

                    adw::ActionRow {
                        set_title: "Import from pasted config",
                        set_activatable_widget: Some(&import_button),
                        add_suffix: import_button = &gtk::Button {
                            set_valign: gtk::Align::Center,
                            set_label: "Import...",
                            connect_clicked => PreferencesMsg::ProfileImportRequest,
                        }
                    },

                    adw::ActionRow {
                        set_title: "Export current parameters",
                        set_subtitle: "Copies the config to your clipboard",
                        set_activatable_widget: Some(&export_button),
                        add_suffix: export_button = &gtk::Button {
                            set_valign: gtk::Align::Center,
                            set_label: "Export",
                            connect_clicked => PreferencesMsg::ProfileExport,
                        }
                    },
                },

                adw::PreferencesGroup {
                    set_title: "Detection Algorithms",
                    set_description: Some("Enable/disable algorithms and tune their thresholds. Changes take effect on the next scan."),

                    #[name = "cheat_algo_list"]
                    gtk::ListBox {
                        set_selection_mode: gtk::SelectionMode::None,
                        add_css_class: "boxed-list",
                    },
                },
            },
        }
    }

    fn init(
        (settings, parent): Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let model = PreferencesModel {
            settings,
            parent,
            connection_test_msg: "".to_owned(),
            connection_test_active: false,
            profile_status: "".to_owned(),
        };

        let widgets = view_output!();

        Self::build_cheat_algo_rows(&widgets.cheat_algo_list, &model.settings, &sender);
        Self::set_profile_dropdown_model(
            &widgets.profile_dropdown,
            &crate::cheat_profiles::list_profiles(),
            model.settings.last_selected_profile.as_deref(),
        );

        ComponentParts { model, widgets }
    }

    fn update(&mut self, message: Self::Input, sender: ComponentSender<Self>, root: &Self::Root) {
        match message {
            PreferencesMsg::ConnectionTest(pw, port) => sender.oneshot_command(async move {
                let mut manager = RconManager::new(&pw, port);
                let res = manager.connect().await;
                PreferencesCmd::ConnectionTestResult(match res {
                    Ok(_) => "Connection Successful!".to_owned(),
                    Err(e) => match e.downcast().unwrap() {
                        rcon::Error::Auth => {
                            "Authorization failed, probably incorrect password".to_owned()
                        }
                        rcon::Error::CommandTooLong => "Command too long?".to_owned(),
                        rcon::Error::Io(e) => format!("Connection error: {:?}", e),
                    },
                })
            }),
            PreferencesMsg::Show => {
                self.connection_test_msg = "".to_owned();
                root.present(Some(&self.parent));
            }
            PreferencesMsg::Close => {
                self.settings.save();
                let _ = sender.output(PreferencesOut::Save(self.settings.clone()));
            }
            PreferencesMsg::DoubleclickPlay(p) => self.settings.doubleclick_play = p,
            PreferencesMsg::PauseAfterSeek(p) => self.settings.pause_after_seek = p,
            PreferencesMsg::StripConsoleCommands(p) => self.settings.strip_console_commands = p,
            PreferencesMsg::EventSkipOffset(off) => self.settings.event_skip_predelay = off as f32,
            PreferencesMsg::RConPassword(pass) => self.settings.rcon_pw = pass,
            PreferencesMsg::RConPort(port) => self.settings.rcon_port = port as u16,
            PreferencesMsg::CheatAnalysisThreads(threads) => {
                self.settings.cheat_analysis_threads = (threads as usize).max(1)
            }
            PreferencesMsg::TF2FolderPath => {
                let dia = gtk::FileDialog::new();
                let initial = self
                    .settings
                    .tf_folder_path
                    .as_ref()
                    .map(|p| gtk::gio::File::for_path(p));
                dia.set_initial_folder(initial.as_ref());
                let sender = sender.clone();
                dia.select_folder(
                    Some(&self.parent),
                    None::<&gtk::gio::Cancellable>,
                    move |res| match res {
                        Ok(file) => sender
                            .command_sender()
                            .emit(PreferencesCmd::FolderBrowseResult(file.path().unwrap())),
                        Err(e) => log::warn!("Error while picking folder: {e}"),
                    },
                );
            }
            PreferencesMsg::CheatAlgoEnabled(algo, enabled) => {
                self.settings.cheat_algo_enabled.insert(algo, enabled);
            }
            PreferencesMsg::CheatAlgoParamFloat(algo, param, value) => {
                self.settings
                    .cheat_algo_params
                    .entry(algo)
                    .or_default()
                    .insert(param, Parameter::Float(value));
            }
            PreferencesMsg::CheatAlgoParamInt(algo, param, value) => {
                self.settings
                    .cheat_algo_params
                    .entry(algo)
                    .or_default()
                    .insert(param, Parameter::Int(value));
            }
            PreferencesMsg::CheatAlgoParamBool(algo, param, value) => {
                self.settings
                    .cheat_algo_params
                    .entry(algo)
                    .or_default()
                    .insert(param, Parameter::Bool(value));
            }
            PreferencesMsg::ProfileSaveAsRequest => {
                let entry = gtk::Entry::new();
                let ad = adw::AlertDialog::builder()
                    .default_response("save")
                    .close_response("cancel")
                    .extra_child(&entry)
                    .body("Name for this profile")
                    .heading("Save Profile")
                    .build();
                ad.add_response("cancel", "Cancel");
                ad.add_response("save", "Save");
                ad.set_response_appearance("save", adw::ResponseAppearance::Suggested);
                entry.grab_focus();
                let sender = sender.clone();
                ad.choose(root, None::<&gtk::gio::Cancellable>, move |resp| {
                    if resp == "save" {
                        let name = entry.text().as_str().trim().to_owned();
                        if !name.is_empty() {
                            sender.input(PreferencesMsg::ProfileSaveAs(name));
                        }
                    }
                });
            }
            // Translated into `ProfileLoad(name)` in `update_with_view` before reaching here.
            PreferencesMsg::ProfileLoadRequest => {}
            PreferencesMsg::ProfileImportRequest => {
                let text_view = gtk::TextView::new();
                text_view.set_wrap_mode(gtk::WrapMode::WordChar);
                text_view.set_monospace(true);
                text_view.set_top_margin(6);
                text_view.set_bottom_margin(6);
                text_view.set_left_margin(6);
                text_view.set_right_margin(6);
                let scroller = gtk::ScrolledWindow::new();
                scroller.set_child(Some(&text_view));
                scroller.set_min_content_height(200);
                scroller.set_min_content_width(400);
                scroller.add_css_class("card");
                let buffer = text_view.buffer();
                let ad = adw::AlertDialog::builder()
                    .default_response("import")
                    .close_response("cancel")
                    .extra_child(&scroller)
                    .body("Paste a cheat detection config below (backticks are ignored)")
                    .heading("Import Profile")
                    .build();
                ad.add_response("cancel", "Cancel");
                ad.add_response("import", "Import");
                ad.set_response_appearance("import", adw::ResponseAppearance::Suggested);
                let sender = sender.clone();
                ad.choose(root, None::<&gtk::gio::Cancellable>, move |resp| {
                    if resp == "import" {
                        let start = buffer.start_iter();
                        let end = buffer.end_iter();
                        let text = buffer.text(&start, &end, false).to_string();
                        sender.input(PreferencesMsg::ProfileImport(text));
                    }
                });
            }
            PreferencesMsg::ProfileExport => {
                match crate::cheat_profiles::export_text(&self.settings.cheat_algo_params) {
                    Ok(text) => {
                        if let Some(display) = gtk::gdk::Display::default() {
                            display.clipboard().set_text(&text);
                        }
                        self.profile_status = "Copied current parameters to clipboard".to_owned();
                    }
                    Err(e) => {
                        self.profile_status = format!("Export failed: {e}");
                    }
                }
            }
            PreferencesMsg::ProfileLoad(name) => match crate::cheat_profiles::load_profile(&name) {
                Ok(config) => {
                    self.settings.cheat_algo_params = config;
                    self.profile_status = format!("Loaded profile \"{name}\"");
                }
                Err(e) => {
                    self.profile_status = format!("Failed to load \"{name}\": {e}");
                }
            },
            PreferencesMsg::ProfileSaveAs(name) => {
                match crate::cheat_profiles::save_profile(&name, &self.settings.cheat_algo_params) {
                    Ok(()) => {
                        self.profile_status = format!("Saved profile \"{name}\"");
                        self.settings.last_selected_profile = Some(name);
                    }
                    Err(e) => {
                        self.profile_status = format!("Failed to save \"{name}\": {e}");
                    }
                }
            }
            PreferencesMsg::ProfileImport(text) => match crate::cheat_profiles::import_text(&text) {
                Ok(config) => {
                    self.settings.cheat_algo_params = config;
                    self.profile_status = "Imported profile from pasted config".to_owned();
                }
                Err(e) => {
                    self.profile_status = format!("Import failed: {e}");
                }
            },
        }
    }

    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        message: Self::Input,
        sender: ComponentSender<Self>,
        root: &Self::Root,
    ) {
        let message = match message {
            PreferencesMsg::ProfileLoadRequest => {
                let selected = widgets
                    .profile_dropdown
                    .selected_item()
                    .and_then(|i| i.downcast::<gtk::StringObject>().ok());
                match selected {
                    Some(s) => PreferencesMsg::ProfileLoad(s.string().to_string()),
                    None => {
                        self.profile_status = "No profile selected".to_owned();
                        self.update_view(widgets, sender);
                        return;
                    }
                }
            }
            PreferencesMsg::Close => {
                if let Some(item) = widgets
                    .profile_dropdown
                    .selected_item()
                    .and_then(|i| i.downcast::<gtk::StringObject>().ok())
                {
                    self.settings.last_selected_profile = Some(item.string().to_string());
                }
                PreferencesMsg::Close
            }
            other => other,
        };

        let rebuild_algos = matches!(
            message,
            PreferencesMsg::ProfileLoad(_) | PreferencesMsg::ProfileImport(_)
        );
        let rebuild_profiles = matches!(message, PreferencesMsg::ProfileSaveAs(_));

        self.update(message, sender.clone(), root);

        if rebuild_algos {
            while let Some(row) = widgets.cheat_algo_list.row_at_index(0) {
                widgets.cheat_algo_list.remove(&row);
            }
            Self::build_cheat_algo_rows(&widgets.cheat_algo_list, &self.settings, &sender);
        }
        if rebuild_profiles {
            Self::set_profile_dropdown_model(
                &widgets.profile_dropdown,
                &crate::cheat_profiles::list_profiles(),
                self.settings.last_selected_profile.as_deref(),
            );
        }

        self.update_view(widgets, sender);
    }

    fn update_cmd(
        &mut self,
        message: Self::CommandOutput,
        _: ComponentSender<Self>,
        _: &Self::Root,
    ) {
        match message {
            PreferencesCmd::ConnectionTestResult(msg) => {
                self.connection_test_msg = msg;
                self.connection_test_active = false;
            }
            PreferencesCmd::FolderBrowseResult(path) => {
                if !path.join("tf").is_dir() {
                    crate::ui::util::notice_dialog(
                        &self.parent,
                        "Possibly invalid folder selected",
                        "Please select the folder named \"Team Fortress 2\", which contains the tf2 exe",
                    );
                }
                self.settings.tf_folder_path = Some(path);
            }
        }
    }
}

impl PreferencesModel {
    fn set_profile_dropdown_model(dropdown: &adw::ComboRow, names: &[String], selected: Option<&str>) {
        let items: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        dropdown.set_model(Some(&gtk::StringList::new(&items)));
        if let Some(name) = selected {
            if let Some(index) = names.iter().position(|n| n == name) {
                dropdown.set_selected(index as u32);
            }
        }
    }

    fn build_cheat_algo_rows(list: &gtk::ListBox, settings: &Settings, sender: &ComponentSender<Self>) {
        for mut algo in get_algorithms()
            .into_iter()
            .sorted_by_key(|a| a.algorithm_name().to_string())
        {
            let name = algo.algorithm_name().to_string();
            let enabled = settings
                .cheat_algo_enabled
                .get(&name)
                .copied()
                .unwrap_or_else(|| algo.default());

            let row = adw::ExpanderRow::new();
            row.set_title(&name);
            row.set_show_enable_switch(false);

            let switch = gtk::Switch::new();
            switch.set_valign(gtk::Align::Center);
            switch.set_active(enabled);
            {
                let sender = sender.clone();
                let name = name.clone();
                switch.connect_active_notify(move |sw| {
                    sender.input(PreferencesMsg::CheatAlgoEnabled(name.clone(), sw.is_active()));
                });
            }
            row.add_suffix(&switch);

            let overrides = settings
                .cheat_algo_params
                .get(&name)
                .cloned()
                .unwrap_or_default();
            if let Some(params) = algo.params() {
                for (param_name, default_value) in params.iter().sorted_by_key(|p| p.0.clone()) {
                    let value = overrides
                        .get(param_name)
                        .cloned()
                        .unwrap_or_else(|| default_value.clone());
                    match value {
                        Parameter::Float(f) => {
                            let adjustment = gtk::Adjustment::new(
                                f as f64, -100000.0, 100000.0, 0.001, 1.0, 0.0,
                            );
                            let param_row = adw::SpinRow::new(Some(&adjustment), 0.001, 3);
                            param_row.set_title(param_name);
                            let sender = sender.clone();
                            let algo_name = name.clone();
                            let param_name = param_name.clone();
                            adjustment.connect_value_changed(move |adj| {
                                sender.input(PreferencesMsg::CheatAlgoParamFloat(
                                    algo_name.clone(),
                                    param_name.clone(),
                                    adj.value() as f32,
                                ));
                            });
                            row.add_row(&param_row);
                        }
                        Parameter::Int(i) => {
                            let adjustment = gtk::Adjustment::new(
                                i as f64, -1000000.0, 1000000.0, 1.0, 10.0, 0.0,
                            );
                            let param_row = adw::SpinRow::new(Some(&adjustment), 1.0, 0);
                            param_row.set_title(param_name);
                            let sender = sender.clone();
                            let algo_name = name.clone();
                            let param_name = param_name.clone();
                            adjustment.connect_value_changed(move |adj| {
                                sender.input(PreferencesMsg::CheatAlgoParamInt(
                                    algo_name.clone(),
                                    param_name.clone(),
                                    adj.value() as i32,
                                ));
                            });
                            row.add_row(&param_row);
                        }
                        Parameter::Bool(b) => {
                            let param_row = adw::SwitchRow::new();
                            param_row.set_title(param_name);
                            param_row.set_active(b);
                            let sender = sender.clone();
                            let algo_name = name.clone();
                            let param_name = param_name.clone();
                            param_row.connect_active_notify(move |sr| {
                                sender.input(PreferencesMsg::CheatAlgoParamBool(
                                    algo_name.clone(),
                                    param_name.clone(),
                                    sr.is_active(),
                                ));
                            });
                            row.add_row(&param_row);
                        }
                    }
                }
            }

            list.append(&row);
        }
    }
}
