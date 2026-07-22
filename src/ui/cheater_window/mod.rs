use std::collections::HashMap;

use adw::prelude::*;
use anyhow::Result;
use async_std::path::Path;
use demo_analysis::lib::algorithm::Detection;
use itertools::Itertools;
use relm4::{gtk::glib::markup_escape_text, prelude::*};

use crate::demo_manager::Demo;

use super::util;

lazy_static::lazy_static! {
    static ref CAT_TEXTURES: Vec<gtk::gdk::Texture> = vec![
        gtk::gdk::Texture::from_bytes(&gtk::glib::Bytes::from(include_bytes!(
            "../../img/20230304_155528.jpg"
        )))
        .expect("Failed to load embedded cat image"),
        gtk::gdk::Texture::from_bytes(&gtk::glib::Bytes::from(include_bytes!(
            "../../img/20230425_141804.jpg"
        )))
        .expect("Failed to load embedded cat image"),
        gtk::gdk::Texture::from_bytes(&gtk::glib::Bytes::from(include_bytes!(
            "../../img/20230708_112152.jpg"
        )))
        .expect("Failed to load embedded cat image"),
        gtk::gdk::Texture::from_bytes(&gtk::glib::Bytes::from(include_bytes!(
            "../../img/20240915_024957.jpg"
        )))
        .expect("Failed to load embedded cat image"),
        gtk::gdk::Texture::from_bytes(&gtk::glib::Bytes::from(include_bytes!(
            "../../img/20250222_201432.jpg"
        )))
        .expect("Failed to load embedded cat image"),
        gtk::gdk::Texture::from_bytes(&gtk::glib::Bytes::from(include_bytes!(
            "../../img/20250307_142736.png"
        )))
        .expect("Failed to load embedded cat image"),
        gtk::gdk::Texture::from_bytes(&gtk::glib::Bytes::from(include_bytes!(
            "../../img/20260705_032144.jpg"
        )))
        .expect("Failed to load embedded cat image"),
    ];
}

pub struct CheaterModel {
    demo: Demo,
    loading: bool,
    progress: (u32, u32),
    tps: f32,
    threads: usize,
    player_count: usize,
    cat_index: usize,
    player_rows: FactoryVecDeque<CheaterRowModel>,
}

impl CheaterModel {
    fn progress_text(&self) -> String {
        let (current, total) = self.progress;
        if total == 0 {
            return "Starting up...".to_string();
        }
        let eta = if self.tps > 0.0 {
            format_duration((total.saturating_sub(current)) as f32 / self.tps)
        } else {
            "…".to_string()
        };
        let threads = if self.threads == 1 {
            "1 background thread".to_string()
        } else {
            format!("{} background threads", self.threads)
        };
        format!(
            "tick {}/{} ({:.0} ticks/sec) - ETA {} - {}",
            current, total, self.tps, eta, threads
        )
    }
}

fn format_duration(seconds: f32) -> String {
    let seconds = seconds.max(0.0).round() as u32;
    if seconds >= 60 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

#[derive(Debug)]
pub enum CheaterMsg {
    Check(
        Demo,
        HashMap<String, bool>,
        demo_analysis::lib::parameters::Config,
        usize,
    ),
}

#[derive(Debug)]
pub enum CheaterOut {
    GotoTick(u32),
    DemoChecked(Demo),
}

#[derive(Debug)]
pub enum CheaterCmd {
    Progress(u32, u32, f32),
    Done(Result<(Vec<Detection>, HashMap<u64, String>)>),
}

#[relm4::component(pub)]
impl Component for CheaterModel {
    type Init = ();
    type Input = CheaterMsg;
    type Output = CheaterOut;
    type CommandOutput = CheaterCmd;

    view! {
        adw::Window {
            set_hide_on_close: true,
            set_title: Some("Cheater Detection"),
            set_height_request: 400,
            set_default_size: (700, 700),
            #[wrap(Some)]
            set_content = &adw::ToolbarView {
                add_top_bar = &adw::HeaderBar {
                    #[wrap(Some)]
                    set_title_widget = &adw::WindowTitle {
                        #[watch]
                        set_title: if model.loading { "" } else { &model.demo.filename },
                    },
                    pack_start = &gtk::Spinner {
                        #[watch]
                        set_spinning: model.loading,
                    }
                },
                #[wrap(Some)]
                set_content = &gtk::ScrolledWindow {
                    #[wrap(Some)]
                    set_child = &gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,
                        adw::Clamp {
                            set_maximum_size: 650,
                            #[wrap(Some)]
                            set_child = &gtk::Box {
                                set_orientation: gtk::Orientation::Vertical,
                                gtk::Label {
                                    set_margin_top: 10,
                                    add_css_class: "title-3",
                                    #[watch]
                                    set_label: &if model.loading {
                                        "Analysing demo...".to_string()
                                    } else if model.player_count == 0 {
                                        "No suspicious activity detected :(".to_string()
                                    } else {
                                        format!("{} player(s) flagged", model.player_count)
                                    },
                                },
                                gtk::Picture {
                                    #[watch]
                                    set_visible: !model.loading && model.player_count == 0,
                                    #[watch]
                                    set_paintable: Some(&CAT_TEXTURES[model.cat_index]),
                                    set_content_fit: gtk::ContentFit::Contain,
                                    set_halign: gtk::Align::Center,
                                    set_margin_top: 10,
                                    set_margin_bottom: 10,
                                    set_size_request: (300, 300),
                                },
                                gtk::Label {
                                    set_margin_bottom: 10,
                                    add_css_class: "dim-label",
                                    add_css_class: "caption",
                                    #[watch]
                                    set_visible: model.loading,
                                    #[watch]
                                    set_label: &model.progress_text(),
                                },
                                model.player_rows.widget() -> &gtk::ListBox {
                                    set_margin_bottom: 50,
                                    set_selection_mode: gtk::SelectionMode::None,
                                    add_css_class: "boxed-list",
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let model = CheaterModel {
            demo: Demo::new(Path::new("empty")),
            loading: false,
            progress: (0, 0),
            tps: 0.0,
            threads: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1),
            player_count: 0,
            cat_index: rand::random::<usize>() % CAT_TEXTURES.len(),
            player_rows: FactoryVecDeque::builder().launch_default().forward(
                sender.output_sender(),
                |m| match m {
                    CheaterRowOut::GotoTick(t) => CheaterOut::GotoTick(t),
                },
            ),
        };

        let widgets = view_output!();

        ComponentParts { model, widgets }
    }

    fn update(&mut self, message: Self::Input, sender: ComponentSender<Self>, root: &Self::Root) {
        match message {
            CheaterMsg::Check(demo, enabled_overrides, param_overrides, threads) => {
                self.demo = demo;
                self.player_rows.guard().clear();
                self.player_count = 0;
                self.cat_index = rand::random::<usize>() % CAT_TEXTURES.len();
                self.loading = true;
                self.progress = (0, 0);
                self.tps = 0.0;
                self.threads = threads.max(1);
                let mut dem = self.demo.clone();
                sender.spawn_command(move |s| {
                    let start = std::time::Instant::now();
                    let result: Result<(Vec<Detection>, HashMap<u64, String>)> = (|| {
                        let detections = dem.detect_cheaters(
                            &enabled_overrides,
                            &param_overrides,
                            threads,
                            |current, total| {
                                let elapsed = start.elapsed().as_secs_f32();
                                let tps = if elapsed > 0.0 {
                                    current as f32 / elapsed
                                } else {
                                    0.0
                                };
                                s.emit(CheaterCmd::Progress(current, total, tps));
                            },
                        )?;
                        let detections = (*detections).clone();
                        // Make sure we have names to show alongside each flagged SteamID. The
                        // detection pass doesn't collect usernames, so scrape the player list
                        // here if it wasn't already indexed.
                        if dem.players.is_none() {
                            let _ = pollster::block_on(dem.index_players());
                        }
                        Ok((detections, build_name_lookup(&dem)))
                    })();
                    s.emit(CheaterCmd::Done(result));
                });
                root.present();
            }
        }
    }

    fn update_cmd(
        &mut self,
        message: Self::CommandOutput,
        sender: ComponentSender<Self>,
        root: &Self::Root,
    ) {
        let (detections, name_lookup) = match message {
            CheaterCmd::Progress(current, total, tps) => {
                self.progress = (current, total);
                self.tps = tps;
                return;
            }
            CheaterCmd::Done(result) => {
                self.loading = false;
                match result {
                    Ok(d) => d,
                    Err(e) => {
                        util::notice_dialog(
                            &root,
                            "An error occured while analysing the demo",
                            &e.to_string(),
                        );
                        return;
                    }
                }
            }
        };

        let mut by_player: HashMap<u64, Vec<Detection>> = HashMap::new();
        for det in detections {
            by_player.entry(det.player).or_default().push(det);
        }

        let mut players: Vec<(u64, Vec<Detection>)> = by_player.into_iter().collect();
        players.sort_by_key(|(_, dets)| std::cmp::Reverse(dets.len()));

        self.player_count = players.len();

        let mut guard = self.player_rows.guard();
        for (steamid64, mut dets) in players {
            dets.sort_by_key(|d| d.tick);
            guard.push_back(CheaterRowInit {
                steamid64,
                name: name_lookup.get(&steamid64).cloned(),
                detections: dets,
            });
        }

        let _ = sender.output(CheaterOut::DemoChecked(self.demo.clone()));
    }
}

// Maps SteamID64 -> username for a demo, preferring the lightweight player-index scrape
// (available without a full inspection) and letting a full inspection override it.
fn build_name_lookup(demo: &Demo) -> HashMap<u64, String> {
    let mut name_lookup: HashMap<u64, String> = HashMap::new();
    if let Some(players) = &demo.players {
        for (name, steamid) in players {
            if name.is_empty() {
                continue;
            }
            if let Some(id) = crate::util::steamid_32_to_64(steamid).and_then(|s| s.parse().ok()) {
                name_lookup.entry(id).or_insert_with(|| name.clone());
            }
        }
    }
    if let Some(insp) = demo.inspection.as_ref() {
        for u in &insp.users {
            let Some(sid64) = u
                .steam_id
                .as_ref()
                .and_then(|s| crate::util::steamid_32_to_64(s))
            else {
                continue;
            };
            let Some(id) = sid64.parse::<u64>().ok() else {
                continue;
            };
            if let Some(name) = &u.name {
                if !name.is_empty() {
                    name_lookup.insert(id, name.clone());
                }
            }
        }
    }
    name_lookup
}

struct CheaterRowInit {
    steamid64: u64,
    name: Option<String>,
    detections: Vec<Detection>,
}

struct CheaterRowModel {
    steamid64: u64,
    name: Option<String>,
    detections: Vec<Detection>,
}

#[derive(Debug, Clone)]
enum CheaterRowMsg {
    CopySteamId,
    OpenProfile,
    OpenSteamhistory,
}

#[derive(Debug)]
enum CheaterRowOut {
    GotoTick(u32),
}

#[relm4::factory]
impl FactoryComponent for CheaterRowModel {
    type ParentWidget = gtk::ListBox;
    type CommandOutput = ();
    type Input = CheaterRowMsg;
    type Output = CheaterRowOut;
    type Init = CheaterRowInit;

    view! {
        #[root]
        adw::ExpanderRow {
            set_title_selectable: true,
            set_title: &markup_escape_text(&match &self.name {
                Some(n) if !n.is_empty() => format!("{} ({})", self.steamid64, n),
                _ => self.steamid64.to_string(),
            }),
            set_subtitle: &format!("{} detection(s)", self.detections.len()),
            add_row = &gtk::CenterBox {
                #[wrap(Some)]
                set_center_widget = &gtk::Box {
                    set_spacing: 10,
                    gtk::Button {
                        set_label: "Copy SteamID",
                        set_has_frame: false,
                        connect_clicked => CheaterRowMsg::CopySteamId,
                    },
                    gtk::Button {
                        set_label: "Profile",
                        set_has_frame: false,
                        connect_clicked => CheaterRowMsg::OpenProfile,
                    },
                    gtk::Button {
                        set_label: "SteamHistory",
                        set_has_frame: false,
                        connect_clicked => CheaterRowMsg::OpenSteamhistory,
                    },
                }
            },
            add_row = &adw::ActionRow {
                set_title: "Detections",
                add_suffix = &gtk::Label {
                    set_margin_top: 10,
                    set_margin_bottom: 10,
                    set_selectable: true,
                    set_focusable: false,
                    set_wrap: true,
                    set_justify: gtk::Justification::Right,
                    set_use_markup: true,
                    set_label: &self.detections.iter()
                        .map(|d| format!("<a href=\"{0}\">{0}</a>: {1}", d.tick, markup_escape_text(&d.algorithm)))
                        .join("\n"),
                    connect_activate_link[sender] => move |_, tick| {
                        let _ = sender.output(CheaterRowOut::GotoTick(tick.parse().unwrap()));
                        gtk::glib::Propagation::Stop
                    },
                }
            },
        }
    }

    fn init_model(init: Self::Init, _index: &Self::Index, _sender: FactorySender<Self>) -> Self {
        Self {
            steamid64: init.steamid64,
            name: init.name,
            detections: init.detections,
        }
    }

    fn update(&mut self, message: Self::Input, _sender: FactorySender<Self>) {
        match message {
            CheaterRowMsg::CopySteamId => {
                if let Some(display) = gtk::gdk::Display::default() {
                    display.clipboard().set_text(&self.steamid64.to_string());
                }
            }
            CheaterRowMsg::OpenProfile => {
                if let Err(e) = opener::open_browser(format!(
                    "https://steamcommunity.com/profiles/{}",
                    self.steamid64
                )) {
                    log::warn!("Failed to open browser, {e}");
                }
            }
            CheaterRowMsg::OpenSteamhistory => {
                if let Err(e) = opener::open_browser(format!(
                    "https://steamhistory.net/id/{}",
                    self.steamid64
                )) {
                    log::warn!("Failed to open browser, {e}");
                }
            }
        }
    }
}
