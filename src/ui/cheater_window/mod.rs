use std::collections::{HashMap, VecDeque};

use adw::prelude::*;
use anyhow::Result;
use async_std::path::Path;
use demo_analysis::algorithms::firewindow::ALGORITHM_NAME as FIREWINDOW_NAME;
use demo_analysis::algorithms::triggerbot::ALGORITHM_NAME as TRIGGERBOT_NAME;
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

    // Mass analysis work queue. Each queued demo gets one parser pass and the
    // configured worker budget is spent across demos instead of reparsing one
    // demo on every worker.
    queue: VecDeque<Demo>,
    // Settings snapshot taken when the current queue session started, so the
    // next queued demo can be started without the caller re-sending them.
    queue_settings: Option<(
        HashMap<String, bool>,
        demo_analysis::lib::parameters::Config,
        usize,
    )>,
    mass_mode: bool,
    demos_done: usize,
    demos_total: usize,
    current_demo: Option<String>,
    active_demos: usize,
    progress_by_demo: HashMap<String, (u32, u32, f32)>,
    accumulated: Vec<Detection>,
    name_lookup: HashMap<u64, String>,
    queue_errors: Vec<(String, String)>,
}

impl CheaterModel {
    fn progress_text(&self) -> String {
        if self.mass_mode {
            let worker_word = if self.threads == 1 {
                "worker"
            } else {
                "workers"
            };
            return format!(
                "{} / {} demos complete; {} active; {:.0} ticks/sec - {} {}",
                self.demos_done,
                self.demos_total,
                self.active_demos,
                self.tps,
                self.threads,
                worker_word,
            );
        }

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

    fn reset_results(&mut self) {
        self.player_rows.guard().clear();
        self.player_count = 0;
        self.cat_index = rand::random::<usize>() % CAT_TEXTURES.len();
        self.report.clear();
        self.accumulated.clear();
        self.name_lookup.clear();
        self.queue_errors.clear();
        self.current_demo = None;
        self.active_demos = 0;
        self.progress_by_demo.clear();
    }

    // Starts as many queued demos as the worker budget allows. Batch mode uses
    // one parser pass per demo; a single-demo check may use two algorithm
    // workers because that was the useful point in the measured scaling curve.
    fn start_available_demos(&mut self, sender: &ComponentSender<Self>) {
        let Some((enabled_overrides, param_overrides, configured_workers)) =
            self.queue_settings.clone()
        else {
            return;
        };

        let configured_workers = configured_workers.max(1);
        let max_parallel_demos = if self.mass_mode {
            let hardware_cap = std::thread::available_parallelism()
                .map(|threads| (threads.get() + 1) / 2)
                .unwrap_or(1);
            configured_workers.min(hardware_cap).max(1)
        } else {
            1
        };
        self.threads = if self.mass_mode {
            max_parallel_demos
        } else {
            configured_workers.min(2)
        };

        while self.active_demos < max_parallel_demos {
            let Some(mut dem) = self.queue.pop_front() else {
                break;
            };
            let enabled_overrides = enabled_overrides.clone();
            let param_overrides = param_overrides.clone();
            let demo_name = dem.filename.clone();
            let progress_name = demo_name.clone();
            let per_demo_threads = if self.mass_mode { 1 } else { self.threads };

            self.current_demo = Some(demo_name.clone());
            self.active_demos += 1;
            self.progress_by_demo.insert(demo_name.clone(), (0, 0, 0.0));
            self.loading = true;

            let mass_mode = self.mass_mode;
            sender.clone().spawn_command(move |s| {
                let start = std::time::Instant::now();
                let result: Result<(Vec<Detection>, HashMap<u64, String>)> = (|| {
                    let (detections, mut names) = dem.detect_cheaters(
                        &enabled_overrides,
                        &param_overrides,
                        per_demo_threads,
                        |_, current, total| {
                            let elapsed = start.elapsed().as_secs_f32();
                            let tps = if elapsed > 0.0 && current > 0 {
                                current as f32 / elapsed
                            } else {
                                0.0
                            };
                            s.emit(CheaterCmd::Progress(
                                progress_name.clone(),
                                current,
                                total,
                                tps,
                            ));
                        },
                    )?;
                    // Inspection/index data can add historical names, but the
                    // detection pass now supplies the complete primary lookup.
                    names.extend(build_name_lookup(&dem));
                    let detections = if mass_mode {
                        dem.cheat_detections = None;
                        std::sync::Arc::try_unwrap(detections)
                            .unwrap_or_else(|detections| (*detections).clone())
                    } else {
                        (*detections).clone()
                    };
                    Ok((detections, names))
                })();
                s.emit(CheaterCmd::Done(demo_name, dem, result));
            });
        }
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
    QueueCheck(
        Vec<Demo>,
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
    Progress(String, u32, u32, f32),
    Done(String, Demo, Result<(Vec<Detection>, HashMap<u64, String>)>),
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
            queue: VecDeque::new(),
            queue_settings: None,
            mass_mode: false,
            demos_done: 0,
            demos_total: 0,
            current_demo: None,
            active_demos: 0,
            progress_by_demo: HashMap::new(),
            accumulated: Vec::new(),
            name_lookup: HashMap::new(),
            queue_errors: Vec::new(),
        };

        let widgets = view_output!();

        ComponentParts { model, widgets }
    }

    fn update(&mut self, message: Self::Input, sender: ComponentSender<Self>, root: &Self::Root) {
        match message {
            CheaterMsg::Check(demo, enabled_overrides, param_overrides, threads) => {
                self.demo = demo.clone();
                self.reset_results();
                self.mass_mode = false;
                self.queue = VecDeque::from(vec![demo]);
                self.queue_settings = Some((enabled_overrides, param_overrides, threads));
                self.demos_total = 1;
                self.demos_done = 0;
                self.start_available_demos(&sender);
                root.present();
            }
            CheaterMsg::QueueCheck(demos, enabled_overrides, param_overrides, threads) => {
                if demos.is_empty() {
                    return;
                }
                if self.loading {
                    // A queue is already draining: just append and keep going.
                    self.demos_total += demos.len();
                    self.queue.extend(demos);
                    self.start_available_demos(&sender);
                } else {
                    self.demo = demos
                        .last()
                        .cloned()
                        .unwrap_or_else(|| Demo::new(Path::new("empty")));
                    self.reset_results();
                    self.mass_mode = demos.len() > 1;
                    self.queue = demos.into();
                    self.queue_settings = Some((enabled_overrides, param_overrides, threads));
                    self.demos_total = self.queue.len();
                    self.demos_done = 0;
                    self.start_available_demos(&sender);
                }
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
        match message {
            CheaterCmd::Progress(demo_name, current, total, tps) => {
                self.current_demo = Some(demo_name.clone());
                self.progress_by_demo
                    .insert(demo_name, (current, total, tps));
                if self.mass_mode {
                    self.progress = self.progress_by_demo.values().fold(
                        (0_u32, 0_u32),
                        |(current_sum, total_sum), value| {
                            (
                                current_sum.saturating_add(value.0),
                                total_sum.saturating_add(value.1),
                            )
                        },
                    );
                    self.tps = self.progress_by_demo.values().map(|value| value.2).sum();
                } else {
                    self.progress = (current, total);
                    self.tps = tps;
                }
                return;
            }
            CheaterCmd::Done(finished_demo, dem, result) => {
                self.active_demos = self.active_demos.saturating_sub(1);
                self.progress_by_demo.remove(&finished_demo);
                if !self.mass_mode {
                    self.demo = dem;
                }

                match result {
                    Ok((detections, names)) => {
                        if self.mass_mode {
                            // Tag every detection with the demo it came from so the
                            // combined result stays attributable.
                            let mut detections = detections;
                            for det in &mut detections {
                                det.data["demo"] = serde_json::json!(finished_demo);
                            }
                            self.accumulated.extend(detections);
                        } else {
                            self.accumulated = detections;
                        }
                        self.name_lookup.extend(names);
                    }
                    Err(e) => {
                        // A demo that fails to analyse should not sink the rest
                        // of a batch.
                        self.queue_errors
                            .push((finished_demo.clone(), e.to_string()));
                        if !self.mass_mode {
                            util::notice_dialog(
                                &root,
                                "An error occured while analysing the demo",
                                &e.to_string(),
                            );
                        }
                    }
                }
                self.demos_done += 1;

                self.start_available_demos(&sender);
                if !self.queue.is_empty() || self.active_demos > 0 {
                    self.loading = true;
                    return;
                }

                self.loading = false;
                self.current_demo = None;
                self.queue_settings = None;
                self.build_results();
                if !self.mass_mode {
                    let _ = sender.output(CheaterOut::DemoChecked(self.demo.clone()));
                }
            }
        }
    }
}

impl CheaterModel {
    // Turns everything the queue accumulated into on-screen rows + the copyable
    // report. Called once the queue has drained.
    fn build_results(&mut self) {
        let name_lookup = &self.name_lookup;
        let detections = std::mem::take(&mut self.accumulated);
        let mut by_player: HashMap<u64, Vec<Detection>> = HashMap::new();
        for det in detections {
            by_player.entry(det.player).or_default().push(det);
        }

        let mut players: Vec<(u64, Vec<Detection>)> = by_player.into_iter().collect();
        // The triggerbot input check is the loudest verdict, so players it
        // flagged sort above everyone else, then by detection count.
        players.sort_by_key(|(_, dets)| {
            (
                std::cmp::Reverse(dets.iter().any(|d| is_triggerbot_alert(d))),
                std::cmp::Reverse(dets.len()),
            )
        });

        self.player_count = players.len();

        let mut report_rows: Vec<(u64, Option<String>, Vec<Detection>)> = Vec::new();
        let mut guard = self.player_rows.guard();
        for (steamid64, mut dets) in players {
            // Triggerbot alerts first within each player too, then by tick.
            dets.sort_by_key(|d| (!is_triggerbot_alert(d), d.tick));
            let name = name_lookup.get(&steamid64).cloned();
            report_rows.push((steamid64, name.clone(), dets.clone()));
            guard.push_back(CheaterRowInit {
                steamid64,
                name,
                detections: dets,
            });
        }
        drop(guard);
        let report_title = if self.mass_mode {
            format!("{} demos", self.demos_total)
        } else {
            self.demo.filename.clone()
        };
        self.report = detail::full_report(&report_title, &report_rows);
        if !self.queue_errors.is_empty() {
            self.report.push_str(&format!(
                "\n{} demo(s) failed to analyse:\n",
                self.queue_errors.len()
            ));
            for (demo, err) in &self.queue_errors {
                self.report.push_str(&format!("  {demo}: {err}\n"));
            }
        }
    }
}

// Maps SteamID64 -> username for a demo, preferring the lightweight player-index scrape
// (available without a full inspection) and letting a full inspection override it.
// The recorder-input checks (triggerbot / firewindow) are verdicts on their
// own, so their detections are sorted above every other algorithm's in the UI.
fn is_triggerbot_alert(detection: &Detection) -> bool {
    detection.algorithm == TRIGGERBOT_NAME || detection.algorithm == FIREWINDOW_NAME
}

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
                if let Err(e) =
                    opener::open_browser(format!("https://steamhistory.net/id/{}", self.steamid64))
                {
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
            set_subtitle: &markup_escape_text(&{
                // Mass analysis tags each detection with its source demo.
                let demo_tag = self
                    .detection
                    .data
                    .get("demo")
                    .and_then(|v| v.as_str())
                    .map(|d| format!("[{d}] "))
                    .unwrap_or_default();
                format!(
                    "{}{} - {}",
                    demo_tag,
                    self.detection.algorithm,
                    detail::summary(&self.detection.data)
                )
            }),
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
