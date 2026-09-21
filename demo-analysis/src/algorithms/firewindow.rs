// Fire-window detection: the recorder's usercmd button state (IN_ATTACK) is
// what actually makes the server fire, and a legit client always pairs it
// with +attack/-attack console commands. A span of held attack button that
// overlaps no console press/release window (within a configurable tick
// tolerance) means the button state was injected - the signature of an
// input-simulating triggerbot.
//
// Both signals only exist for the player who recorded the demo, so every
// detection is attributed to the recorder (matched by the header nickname).

use anyhow::Error;
use serde_json::json;
use tf_demo_parser::ParserState;

use crate::base::cheat_analyser_base::CheatAnalyserState;
use crate::lib::algorithm::{CheatAlgorithm, Detection};
use crate::lib::parameters::{get_parameter_value, Parameter, Parameters};

pub const ALGORITHM_NAME: &str = "firewindow";

pub struct FireWindow {
    next_cmd: usize,
    attack_depth: i32,
    recorder_sid: Option<u64>,
    // Console +attack/-attack windows ("input").
    open_press: Option<u32>,
    windows: Vec<(u32, u32)>,
    // Usercmd IN_ATTACK spans ("firing"). Buttons are delta-encoded, so the
    // last known full value is carried forward.
    last_buttons: Option<u32>,
    attack_held: bool,
    span_start: Option<u32>,
    pending_spans: Vec<(u32, u32)>,
    params: Parameters,
}

impl Default for FireWindow {
    fn default() -> Self {
        Self::new()
    }
}

impl FireWindow {
    pub fn new() -> Self {
        Self {
            next_cmd: 0,
            attack_depth: 0,
            recorder_sid: None,
            open_press: None,
            windows: Vec::new(),
            last_buttons: None,
            attack_held: false,
            span_start: None,
            pending_spans: Vec::new(),
            params: Parameters::from([
                // How far a button span may sit from a console +attack window
                // before it counts as injected input; covers the tick skew
                // between the two streams.
                (
                    "fire_window_tolerance".to_string(),
                    Parameter::Int(8),
                ),
            ]),
        }
    }

    // The recorder's steamid: the demo header stores the nickname the demo
    // was recorded under, which is matched against known player names.
    fn resolve_recorder(&mut self, state: &CheatAnalyserState) {
        if self.recorder_sid.is_some() {
            return;
        }
        let Some(nick) = state.header.as_ref().map(|h| h.nick.clone()) else {
            return;
        };
        if nick.is_empty() {
            return;
        }
        self.recorder_sid = state
            .player_names
            .iter()
            .find(|(_, name)| name.as_str() == nick)
            .map(|(sid, _)| *sid);
    }

    // A held-button span counts as explained when it overlaps a console
    // +attack window within the tolerance. Spans can only be judged once no
    // later window could still explain them, hence the pending queue.
    fn eval_pending_spans(&mut self, state: &CheatAnalyserState, now: u32) -> Vec<Detection> {
        let tol = get_parameter_value::<i32>(&self.params, "fire_window_tolerance").max(0) as u32;
        let mut detections = Vec::new();
        let mut still_pending = Vec::new();

        for span @ (s, e) in self.pending_spans.drain(..) {
            // u32::MAX is the "still held at demo end" sentinel and is always
            // due; ordinary spans wait out the tolerance past their release.
            if e != u32::MAX && now <= e + tol {
                still_pending.push(span);
                continue;
            }
            let explained = self
                .windows
                .iter()
                .any(|(p, r)| s <= r.saturating_add(tol) && e.saturating_add(tol) >= *p)
                || self.open_press.is_some_and(|p| s.saturating_add(tol) >= p);
            if explained {
                continue;
            }

            let player = self.recorder_sid.unwrap_or(0);
            let (class, weapon) = state
                .players
                .iter()
                .find(|p| {
                    p.info.as_ref().is_some_and(|info| {
                        steamid_ng::SteamID::from_steam3(&info.steam_id)
                            .map(u64::from)
                            .ok()
                            == Some(player)
                    })
                })
                .map(|p| (p.class_name(), state.get_player_weapon(p)))
                .unwrap_or(("unknown", "unknown".to_string()));

            detections.push(Detection {
                tick: s,
                algorithm: ALGORITHM_NAME.to_string(),
                player,
                data: json!({
                    "button_span": [s, if e == u32::MAX { "still held at demo end" } else { "released" }],
                    "span_ticks": if e == u32::MAX { serde_json::Value::Null } else { json!(e - s) },
                    "tolerance_ticks": tol,
                    "class": class,
                    "weapon": weapon,
                    "note": "attack button held in usercmds with no +attack console command nearby (input-injected triggerbot)",
                }),
            });
        }
        self.pending_spans = still_pending;
        detections
    }
}

impl<'a> CheatAlgorithm<'a> for FireWindow {
    fn default(&self) -> bool {
        true
    }

    fn algorithm_name(&self) -> &str {
        ALGORITHM_NAME
    }

    fn params(&mut self) -> Option<&mut Parameters> {
        Some(&mut self.params)
    }

    fn on_tick(
        &mut self,
        state: &CheatAnalyserState,
        _: &ParserState,
    ) -> Result<Vec<Detection>, Error> {
        self.resolve_recorder(state);
        let tick = u32::from(state.tick);

        while self.next_cmd < state.console_cmds.len() {
            let (t, command) = &state.console_cmds[self.next_cmd];
            self.next_cmd += 1;
            let t = u32::from(*t);
            match command.split_whitespace().next().unwrap_or("") {
                "+attack" => {
                    if self.attack_depth == 0 {
                        self.open_press = Some(t);
                    }
                    self.attack_depth += 1;
                }
                "-attack" => {
                    self.attack_depth = (self.attack_depth - 1).max(0);
                    if self.attack_depth == 0 {
                        if let Some(p) = self.open_press.take() {
                            self.windows.push((p, t));
                        }
                    }
                }
                _ => continue,
            }
        }

        // user_cmds accumulate between NetTicks and are cleared right after
        // this loop, so they are read here. Buttons are delta-encoded: None
        // keeps the previous command's state.
        for cmd in &state.user_cmds {
            if let Some(buttons) = cmd.cmd.buttons {
                self.last_buttons = Some(buttons);
            }
            let held = self.last_buttons.is_some_and(|b| b & 1 != 0);
            let t = u32::from(cmd.tick);
            if held && !self.attack_held {
                self.span_start = Some(t);
            } else if !held && self.attack_held {
                if let Some(s) = self.span_start.take() {
                    self.pending_spans.push((s, t));
                }
            }
            self.attack_held = held;
        }

        Ok(self.eval_pending_spans(state, tick))
    }

    // Judge spans still open or pending at the end of the demo.
    fn finish(&mut self) -> Result<Vec<Detection>, Error> {
        if let Some(s) = self.span_start.take() {
            self.pending_spans.push((s, u32::MAX));
        }
        let state = CheatAnalyserState::default();
        Ok(self.eval_pending_spans(&state, u32::MAX))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::base::cheat_analyser_base::Player;
    use tf_demo_parser::demo::data::DemoTick;
    use tf_demo_parser::demo::header::Header;
    use tf_demo_parser::demo::message::packetentities::EntityId;
    use tf_demo_parser::demo::parser::analyser::{Class, Team, UserId, UserInfo};

    fn test_parser_state() -> ParserState {
        ParserState::new(24, |_| false, false)
    }

    fn recorder_player(state: &mut CheatAnalyserState) {
        state.players = vec![Player {
            entity: EntityId::from(1u32),
            position: Default::default(),
            health: 125,
            max_health: 125,
            class: Class::Scout,
            team: Team::Red,
            view_angle: 0.0,
            pitch_angle: 0.0,
            state: crate::base::cheat_analyser_base::PlayerState::Alive,
            info: Some(UserInfo {
                classes: Default::default(),
                name: "Recorder".to_string(),
                user_id: UserId::from(1u16),
                steam_id: "[U:1:12345678]".to_string(),
                entity_id: EntityId::from(1u32),
                team: Team::Red,
            }),
            charge: 0,
            simtime: 100,
            ping: 25,
            in_pvs: true,
            active_weapon: None,
            cond: 0,
            cond_ex: 0,
            cond_ex2: 0,
            invis_change_complete_time: 0.0,
            flags: 1,
        }];
        state.player_names.insert(
            u64::from(steamid_ng::SteamID::from_steam3("[U:1:12345678]").unwrap()),
            "Recorder".to_string(),
        );
        state.header = Some(Header {
            demo_type: "HL2DEMO".to_string(),
            version: 3,
            protocol: 4,
            server: String::new(),
            nick: "Recorder".to_string(),
            map: "ctf_2fort".to_string(),
            game: "tf".to_string(),
            duration: 60.0,
            ticks: 4000,
            frames: 4000,
            signon: 0,
        });
    }

    fn push_cmds(state: &mut CheatAnalyserState, cmds: &[(u32, &str)]) {
        for (tick, cmd) in cmds {
            state
                .console_cmds
                .push((DemoTick::from(*tick), cmd.to_string()));
        }
    }

    fn push_usercmds(state: &mut CheatAnalyserState, cmds: &[(u32, Option<u32>)]) {
        use tf_demo_parser::demo::packet::usercmd::{UserCmd, UserCmdPacket};
        for (tick, buttons) in cmds {
            state.user_cmds.push(UserCmdPacket {
                tick: DemoTick::from(*tick),
                sequence_out: 0,
                cmd: UserCmd {
                    command_number: Some(0),
                    tick_count: Some(0),
                    view_angles: [None; 3],
                    movement: [None; 3],
                    buttons: *buttons,
                    impulse: None,
                    weapon_select: None,
                    mouse_dx: None,
                    mouse_dy: None,
                },
            });
        }
    }

    fn run(algo: &mut FireWindow, state: &CheatAnalyserState) -> Vec<Detection> {
        algo.on_tick(state, &test_parser_state()).unwrap()
    }

    #[test]
    fn button_span_inside_console_window_is_ignored() {
        let mut algo = FireWindow::new();
        let mut state = CheatAnalyserState::default();
        recorder_player(&mut state);
        push_cmds(&mut state, &[(100, "+attack 45"), (200, "-attack 46")]);
        push_usercmds(&mut state, &[(100, Some(1)), (200, Some(0))]);
        state.tick = 100.into();
        assert!(run(&mut algo, &state).is_empty());
        state.user_cmds.clear();
        state.tick = 250.into();
        assert!(run(&mut algo, &state).is_empty());
    }

    // The testdemo_noinput.dem pattern: button state held with no console
    // attack command anywhere near it.
    #[test]
    fn button_span_without_console_command_flags() {
        let mut algo = FireWindow::new();
        let mut state = CheatAnalyserState::default();
        recorder_player(&mut state);
        push_usercmds(&mut state, &[(100, Some(1)), (200, Some(0))]);
        state.tick = 100.into();
        assert!(run(&mut algo, &state).is_empty());
        state.user_cmds.clear();
        state.tick = 250.into();
        let detections = run(&mut algo, &state);
        assert_eq!(detections.len(), 1);
        assert_eq!(detections[0].tick, 100);
        assert_eq!(detections[0].algorithm, "firewindow");
        assert_eq!(
            detections[0].player,
            u64::from(steamid_ng::SteamID::from_steam3("[U:1:12345678]").unwrap())
        );
    }

    #[test]
    fn press_within_tolerance_of_span_still_explains() {
        let mut algo = FireWindow::new();
        let mut state = CheatAnalyserState::default();
        recorder_player(&mut state);
        // The console press trails the span start by exactly the tolerance
        // (8), so the skew is forgiven.
        push_usercmds(&mut state, &[(100, Some(1)), (200, Some(0))]);
        push_cmds(&mut state, &[(108, "+attack 45"), (210, "-attack 46")]);
        state.tick = 250.into();
        assert!(run(&mut algo, &state).is_empty());
    }

    #[test]
    fn press_far_after_the_span_flags() {
        let mut algo = FireWindow::new();
        let mut state = CheatAnalyserState::default();
        recorder_player(&mut state);
        push_usercmds(&mut state, &[(100, Some(1)), (200, Some(0))]);
        state.tick = 205.into();
        assert!(run(&mut algo, &state).is_empty());
        state.user_cmds.clear();
        // A press only much later cannot explain the earlier held span.
        push_cmds(&mut state, &[(300, "+attack 45"), (320, "-attack 46")]);
        state.tick = 400.into();
        assert_eq!(run(&mut algo, &state).len(), 1);
    }

    #[test]
    fn still_held_at_demo_end_flags_in_finish() {
        let mut algo = FireWindow::new();
        let mut state = CheatAnalyserState::default();
        recorder_player(&mut state);
        push_usercmds(&mut state, &[(100, Some(1))]);
        state.tick = 100.into();
        assert!(run(&mut algo, &state).is_empty());
        let detections = algo.finish().unwrap();
        assert_eq!(detections.len(), 1);
        assert_eq!(detections[0].tick, 100);
    }

    #[test]
    fn console_commands_without_button_state_never_flag() {
        let mut algo = FireWindow::new();
        let mut state = CheatAnalyserState::default();
        recorder_player(&mut state);
        push_cmds(&mut state, &[(100, "+attack 45"), (200, "-attack 46")]);
        state.tick = 250.into();
        assert!(run(&mut algo, &state).is_empty());
    }
}
