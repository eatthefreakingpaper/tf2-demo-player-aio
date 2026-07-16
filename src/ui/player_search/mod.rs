use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use adw::prelude::*;
use itertools::Itertools;
use relm4::{gtk::glib::markup_escape_text, prelude::*};

use crate::demo_manager::DemoManager;

pub struct PlayerSearchModel {
    demo_manager: Arc<Mutex<DemoManager>>,
    loading: bool,
    progress: (usize, usize),
    tps: f32,
    search: String,
    aggregates: Vec<PlayerAggregate>,
    matched_count: usize,
    rows: FactoryVecDeque<PlayerRowModel>,
}

impl PlayerSearchModel {
    const MAX_VISIBLE_ROWS: usize = 200;
}

#[derive(Debug, Clone)]
struct PlayerAggregate {
    steamid: String,
    steamid64: Option<String>,
    names: Vec<String>,
    demos: Vec<String>,
}

#[derive(Debug)]
pub enum PlayerSearchMsg {
    Open,
    SearchChanged(String),
    Reindex,
}

#[derive(Debug)]
pub enum PlayerSearchOut {
    SelectDemo(String),
    CopyDemos(String, Vec<String>),
}

#[derive(Debug)]
pub enum PlayerSearchCmd {
    Progress(usize, usize, f32),
    Done,
}

#[relm4::component(pub)]
impl Component for PlayerSearchModel {
    type Init = Arc<Mutex<DemoManager>>;
    type Input = PlayerSearchMsg;
    type Output = PlayerSearchOut;
    type CommandOutput = PlayerSearchCmd;

    view! {
        adw::Window {
            set_hide_on_close: true,
            set_title: Some("Player Search"),
            set_height_request: 400,
            set_default_size: (600, 700),
            #[wrap(Some)]
            set_content = &adw::ToolbarView {
                add_top_bar = &adw::HeaderBar {
                    #[wrap(Some)]
                    set_title_widget = &adw::WindowTitle {
                        set_title: "Player Search",
                    },
                    pack_start = &gtk::Spinner {
                        #[watch]
                        set_spinning: model.loading,
                    },
                    pack_end = &gtk::Button {
                        set_icon_name: "view-refresh-symbolic",
                        set_tooltip_text: Some("(Re)index players in all demos"),
                        #[watch]
                        set_sensitive: !model.loading,
                        connect_clicked => PlayerSearchMsg::Reindex,
                    },
                },
                #[wrap(Some)]
                set_content = &gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,
                    gtk::SearchEntry {
                        set_margin_all: 10,
                        set_placeholder_text: Some("Search by name or SteamID..."),
                        connect_search_changed[sender] => move |e| {
                            sender.input(PlayerSearchMsg::SearchChanged(e.text().to_string()));
                        },
                    },
                    gtk::Label {
                        set_margin_bottom: 5,
                        #[watch]
                        set_label: &if model.loading {
                            format!(
                                "Indexing demos... {}/{} ({:.0} ticks/sec)",
                                model.progress.0, model.progress.1, model.tps
                            )
                        } else if model.matched_count > PlayerSearchModel::MAX_VISIBLE_ROWS {
                            format!(
                                "Showing first {} of {} matching player(s) - refine your search",
                                PlayerSearchModel::MAX_VISIBLE_ROWS,
                                model.matched_count
                            )
                        } else {
                            format!("{} player(s)", model.matched_count)
                        },
                    },
                    gtk::ScrolledWindow {
                        set_vexpand: true,
                        #[wrap(Some)]
                        set_child = model.rows.widget() -> &gtk::ListBox {
                            set_margin_bottom: 50,
                            set_margin_start: 10,
                            set_margin_end: 10,
                            set_selection_mode: gtk::SelectionMode::None,
                            add_css_class: "boxed-list",
                        }
                    }
                }
            }
        }
    }

    fn init(
        demo_manager: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let model = PlayerSearchModel {
            demo_manager,
            loading: false,
            progress: (0, 0),
            tps: 0.0,
            search: String::new(),
            aggregates: Vec::new(),
            matched_count: 0,
            rows: FactoryVecDeque::builder().launch_default().forward(
                sender.output_sender(),
                |m| match m {
                    PlayerRowOut::SelectDemo(name) => PlayerSearchOut::SelectDemo(name),
                    PlayerRowOut::CopyDemos(display, demos) => {
                        PlayerSearchOut::CopyDemos(display, demos)
                    }
                },
            ),
        };

        let widgets = view_output!();

        ComponentParts { model, widgets }
    }

    fn update(&mut self, message: Self::Input, sender: ComponentSender<Self>, root: &Self::Root) {
        match message {
            PlayerSearchMsg::Open => {
                self.refresh_aggregate();
                self.refresh_rows();
                root.present();
            }
            PlayerSearchMsg::SearchChanged(text) => {
                self.search = text;
                self.refresh_rows();
            }
            PlayerSearchMsg::Reindex => {
                self.loading = true;
                self.progress = (0, 0);
                self.tps = 0.0;
                let dm = self.demo_manager.clone();
                sender.spawn_command(move |s| {
                    dm.lock().unwrap().index_players(|current, total, tps| {
                        s.emit(PlayerSearchCmd::Progress(current, total, tps));
                    });
                    s.emit(PlayerSearchCmd::Done);
                });
            }
        }
    }

    fn update_cmd(
        &mut self,
        message: Self::CommandOutput,
        _sender: ComponentSender<Self>,
        _root: &Self::Root,
    ) {
        match message {
            PlayerSearchCmd::Progress(current, total, tps) => {
                self.progress = (current, total);
                self.tps = tps;
            }
            PlayerSearchCmd::Done => {
                self.loading = false;
                self.refresh_aggregate();
                self.refresh_rows();
            }
        }
    }
}

impl PlayerSearchModel {
    fn refresh_aggregate(&mut self) {
        use std::collections::HashSet;

        struct Building {
            names: HashSet<String>,
            demos: HashSet<String>,
        }

        let dm = self.demo_manager.lock().unwrap();
        let mut by_steamid: HashMap<String, Building> = HashMap::new();
        for demo in dm.get_demos().values() {
            let Some(players) = &demo.players else {
                continue;
            };
            for (name, steamid) in players {
                let building = by_steamid.entry(steamid.clone()).or_insert_with(|| Building {
                    names: HashSet::new(),
                    demos: HashSet::new(),
                });
                building.names.insert(name.clone());
                building.demos.insert(demo.filename.clone());
            }
        }
        let mut aggregates: Vec<PlayerAggregate> = by_steamid
            .into_iter()
            .map(|(steamid, building)| {
                let mut names: Vec<String> = building.names.into_iter().collect();
                names.sort();
                let mut demos: Vec<String> = building.demos.into_iter().collect();
                demos.sort();
                PlayerAggregate {
                    steamid64: crate::util::steamid_32_to_64(&steamid),
                    steamid,
                    names,
                    demos,
                }
            })
            .collect();
        aggregates.sort_by_key(|a| std::cmp::Reverse(a.demos.len()));
        self.aggregates = aggregates;
    }

    fn refresh_rows(&mut self) {
        let search = self.search.to_lowercase();
        let matched: Vec<&PlayerAggregate> = self
            .aggregates
            .iter()
            .filter(|agg| {
                if search.is_empty() {
                    return true;
                }
                agg.names.iter().any(|n| n.to_lowercase().contains(&search))
                    || agg.steamid.to_lowercase().contains(&search)
                    || agg.steamid64.as_ref().is_some_and(|s| s.contains(&search))
            })
            .collect();
        self.matched_count = matched.len();

        let mut guard = self.rows.guard();
        guard.clear();
        for agg in matched.into_iter().take(Self::MAX_VISIBLE_ROWS) {
            guard.push_back(agg.clone());
        }
    }
}

struct PlayerRowModel {
    steamid: String,
    steamid64: Option<String>,
    names: Vec<String>,
    demos: Vec<String>,
}

#[derive(Debug, Clone)]
enum PlayerRowMsg {
    OpenProfile,
    OpenSteamhistory,
    CopyDemos,
}

#[derive(Debug)]
enum PlayerRowOut {
    SelectDemo(String),
    CopyDemos(String, Vec<String>),
}

#[relm4::factory]
impl FactoryComponent for PlayerRowModel {
    type ParentWidget = gtk::ListBox;
    type CommandOutput = ();
    type Input = PlayerRowMsg;
    type Output = PlayerRowOut;
    type Init = PlayerAggregate;

    view! {
        #[root]
        adw::ExpanderRow {
            set_title_selectable: true,
            set_title: &markup_escape_text(&self.names.join(" / ")),
            set_subtitle: &format!("{} - {} demo(s)", self.steamid, self.demos.len()),
            add_row = &gtk::CenterBox {
                #[wrap(Some)]
                set_center_widget = &gtk::Box {
                    set_spacing: 10,
                    gtk::Button {
                        set_label: "Profile",
                        set_has_frame: false,
                        set_sensitive: self.steamid64.is_some(),
                        connect_clicked => PlayerRowMsg::OpenProfile,
                    },
                    gtk::Button {
                        set_label: "SteamHistory",
                        set_has_frame: false,
                        set_sensitive: self.steamid64.is_some(),
                        connect_clicked => PlayerRowMsg::OpenSteamhistory,
                    },
                    gtk::Button {
                        set_label: "Copy demos to folder",
                        set_has_frame: false,
                        connect_clicked => PlayerRowMsg::CopyDemos,
                    },
                }
            },
            add_row = &adw::ActionRow {
                set_title: "Seen in",
                add_suffix = &gtk::Label {
                    set_margin_top: 10,
                    set_margin_bottom: 10,
                    set_selectable: true,
                    set_focusable: false,
                    set_wrap: true,
                    set_justify: gtk::Justification::Right,
                    set_use_markup: true,
                    set_label: &self.demos.iter()
                        .map(|d| {
                            let esc = markup_escape_text(d);
                            format!("<a href=\"{esc}\">{esc}</a>")
                        })
                        .join("\n"),
                    connect_activate_link[sender] => move |_, name| {
                        let _ = sender.output(PlayerRowOut::SelectDemo(name.to_string()));
                        gtk::glib::Propagation::Stop
                    },
                }
            },
        }
    }

    fn init_model(init: Self::Init, _index: &Self::Index, _sender: FactorySender<Self>) -> Self {
        Self {
            steamid: init.steamid,
            steamid64: init.steamid64,
            names: init.names,
            demos: init.demos,
        }
    }

    fn update(&mut self, message: Self::Input, sender: FactorySender<Self>) {
        match message {
            PlayerRowMsg::OpenProfile => {
                if let Some(id64) = &self.steamid64 {
                    if let Err(e) =
                        opener::open_browser(format!("https://steamcommunity.com/profiles/{id64}"))
                    {
                        log::warn!("Failed to open browser, {e}");
                    }
                }
            }
            PlayerRowMsg::OpenSteamhistory => {
                if let Some(id64) = &self.steamid64 {
                    if let Err(e) =
                        opener::open_browser(format!("https://steamhistory.net/id/{id64}"))
                    {
                        log::warn!("Failed to open browser, {e}");
                    }
                }
            }
            PlayerRowMsg::CopyDemos => {
                let display = self.names.first().cloned().unwrap_or_else(|| self.steamid.clone());
                let _ = sender.output(PlayerRowOut::CopyDemos(display, self.demos.clone()));
            }
        }
    }
}
