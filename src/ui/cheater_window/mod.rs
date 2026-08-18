use std::collections::HashMap;

use adw::prelude::*;
use anyhow::Result;
use async_std::path::Path;
use demo_analysis::lib::algorithm::Detection;
use relm4::{gtk::glib::markup_escape_text, prelude::*};

use crate::demo_manager::Demo;

use super::util;

mod detail;

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
    // Held so "Copy all detections" can hand over every detection's detail, including the ones
    // past the on-screen row cap.
    report: String,
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
    CopyAll,
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
                    },
                    pack_end = &gtk::Button {
                        set_label: "Copy all detections",
                        set_tooltip_text: Some("Copy every flagged player and the full detail of each detection"),
                        #[watch]
                        set_visible: !model.loading && model.player_count > 0,
                        connect_clicked => CheaterMsg::CopyAll,
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
            report: String::new(),
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
                self.report.clear();
                self.loading = true;
                self.progress = (0, 0);
                self.tps = 0.0;
                self.threads = threads.max(1);
                let effective_threads = self.threads;
                let mut dem = self.demo.clone();
                sender.spawn_command(move |s| {
                    let start = std::time::Instant::now();
                    // Track each analysis thread's latest tick so the reported progress reflects
                    // the *slowest* thread rather than whichever happens to report last. Without
                    // this, a fast thread (lighter algorithms) can make the ETA look almost done
                    // while heavier algorithms are still far behind.
                    // Initialized to MAX so threads that don't exist (fewer algorithms than
                    // threads) don't drag the minimum down.
                    let thread_ticks: Vec<std::sync::atomic::AtomicU32> = (0..effective_threads)
                        .map(|_| std::sync::atomic::AtomicU32::new(u32::MAX))
                        .collect();
                    let thread_ticks = std::sync::Arc::new(thread_ticks);
                    let result: Result<(Vec<Detection>, HashMap<u64, String>)> = (|| {
                        let detections = dem.detect_cheaters(
                            &enabled_overrides,
                            &param_overrides,
                            threads,
                            |thread_idx, current, total| {
                                thread_ticks[thread_idx].store(current, std::sync::atomic::Ordering::Relaxed);
                                // Effective progress = the slowest thread's position.
                                let min_current = thread_ticks
                                    .iter()
                                    .map(|t| t.load(std::sync::atomic::Ordering::Relaxed))
                                    .filter(|&v| v != u32::MAX)
                                    .min()
                                    .unwrap_or(current);
                                let elapsed = start.elapsed().as_secs_f32();
                                let tps = if elapsed > 0.0 && min_current > 0 {
                                    min_current as f32 / elapsed
                                } else {
                                    0.0
                                };
                                s.emit(CheaterCmd::Progress(min_current, total, tps));
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
            CheaterMsg::CopyAll => {
                if self.report.is_empty() {
                    return;
                }
                if let Some(display) = gtk::gdk::Display::default() {
                    display.clipboard().set_text(&self.report);
                }
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

        let mut report_rows: Vec<(u64, Option<String>, Vec<Detection>)> = Vec::new();
        let mut guard = self.player_rows.guard();
        for (steamid64, mut dets) in players {
            dets.sort_by_key(|d| d.tick);
            let name = name_lookup.get(&steamid64).cloned();
            report_rows.push((steamid64, name.clone(), dets.clone()));
            guard.push_back(CheaterRowInit {
                steamid64,
                name,
                detections: dets,
            });
        }
        drop(guard);
        self.report = detail::full_report(&self.demo.filename, &report_rows);

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
    detection_rows: FactoryVecDeque<DetectionRowModel>,
    hidden_detections: usize,
}

// A player with thousands of flagged ticks would otherwise build thousands of expander rows up
// front. Only the on-screen list is capped - the clipboard report always carries every detection.
const MAX_DETECTION_ROWS: usize = 200;

impl CheaterRowModel {
    fn subtitle(&self) -> String {
        if self.hidden_detections == 0 {
            return format!("{} detection(s)", self.detections.len());
        }
        format!(
            "{} detection(s) - showing the first {}, use Copy detections for the rest",
            self.detections.len(),
            MAX_DETECTION_ROWS
        )
    }
}

#[derive(Debug, Clone)]
enum CheaterRowMsg {
    CopySteamId,
    CopyDetections,
    OpenProfile,
    OpenSteamhistory,
    GotoTick(u32),
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
            set_subtitle: &self.subtitle(),
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
                        set_label: "Copy detections",
                        set_has_frame: false,
                        set_tooltip_text: Some("Copy this player's detections with the full detail of each one"),
                        connect_clicked => CheaterRowMsg::CopyDetections,
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
            add_row = self.detection_rows.widget() -> &gtk::ListBox {
                set_selection_mode: gtk::SelectionMode::None,
            },
        }
    }

    fn init_model(init: Self::Init, _index: &Self::Index, sender: FactorySender<Self>) -> Self {
        let mut detection_rows = FactoryVecDeque::builder().launch_default().forward(
            sender.input_sender(),
            |m| match m {
                DetectionRowOut::GotoTick(t) => CheaterRowMsg::GotoTick(t),
            },
        );
        {
            let mut guard = detection_rows.guard();
            for detection in init.detections.iter().take(MAX_DETECTION_ROWS) {
                guard.push_back(detection.clone());
            }
        }
        let hidden_detections = init.detections.len().saturating_sub(MAX_DETECTION_ROWS);

        Self {
            steamid64: init.steamid64,
            name: init.name,
            detections: init.detections,
            detection_rows,
            hidden_detections,
        }
    }

    fn update(&mut self, message: Self::Input, sender: FactorySender<Self>) {
        match message {
            CheaterRowMsg::CopySteamId => {
                if let Some(display) = gtk::gdk::Display::default() {
                    display.clipboard().set_text(&self.steamid64.to_string());
                }
            }
            CheaterRowMsg::CopyDetections => {
                if let Some(display) = gtk::gdk::Display::default() {
                    display.clipboard().set_text(&detail::player_report(
                        self.steamid64,
                        self.name.as_deref(),
                        &self.detections,
                    ));
                }
            }
            CheaterRowMsg::GotoTick(tick) => {
                let _ = sender.output(CheaterRowOut::GotoTick(tick));
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

// One flagged tick. Collapsed it shows the algorithm and a gist of the numbers; expanded it shows
// the algorithm's whole payload, which is what actually justifies the flag.
struct DetectionRowModel {
    detection: Detection,
}

#[derive(Debug, Clone)]
enum DetectionRowMsg {
    GotoTick,
}

#[derive(Debug)]
enum DetectionRowOut {
    GotoTick(u32),
}

#[relm4::factory]
impl FactoryComponent for DetectionRowModel {
    type ParentWidget = gtk::ListBox;
    type CommandOutput = ();
    type Input = DetectionRowMsg;
    type Output = DetectionRowOut;
    type Init = Detection;

    view! {
        #[root]
        adw::ExpanderRow {
            set_title: &markup_escape_text(&format!("tick {}", self.detection.tick)),
            set_subtitle: &markup_escape_text(&format!(
                "{} - {}",
                self.detection.algorithm,
                detail::summary(&self.detection.data)
            )),
            add_suffix = &gtk::Button {
                set_label: "Go to tick",
                set_has_frame: false,
                set_valign: gtk::Align::Center,
                connect_clicked => DetectionRowMsg::GotoTick,
            },
            add_row = &adw::ActionRow {
                add_prefix = &gtk::Label {
                    set_margin_top: 6,
                    set_margin_bottom: 6,
                    set_margin_start: 12,
                    set_selectable: true,
                    set_focusable: false,
                    set_wrap: true,
                    set_xalign: 0.0,
                    add_css_class: "monospace",
                    set_label: &detail::detail_block(&self.detection.data),
                }
            },
        }
    }

    fn init_model(init: Self::Init, _index: &Self::Index, _sender: FactorySender<Self>) -> Self {
        Self { detection: init }
    }

    fn update(&mut self, message: Self::Input, sender: FactorySender<Self>) {
        match message {
            DetectionRowMsg::GotoTick => {
                let _ = sender.output(DetectionRowOut::GotoTick(self.detection.tick));
            }
        }
    }
}
