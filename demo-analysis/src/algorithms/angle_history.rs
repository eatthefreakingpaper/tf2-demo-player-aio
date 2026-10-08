// Written by Tellta

use std::collections::{HashMap, VecDeque};

use anyhow::Error;
use serde_json::json;
use tf_demo_parser::ParserState;

use crate::base::cheat_analyser_base::{CheatAnalyserState, PlayerState};
use crate::lib::algorithm::{CheatAlgorithm, Detection};
use crate::lib::parameters::{get_parameter_value, Parameter, Parameters};
use crate::util::{helpers::angle_delta, nocrex::jankguard::JankGuard};

#[derive(Clone, Copy)]
struct AngleSnapshot {
    yaw: f32,
    pitch: f32,
}

#[derive(Clone, Copy)]
struct AngleHistoryParams {
    tick_window: usize,
    max_delta_first_third: f32,
    min_delta_second_third: f32,
}

impl Default for AngleHistoryParams {
    fn default() -> Self {
        Self {
            tick_window: 4,
            max_delta_first_third: 0.5,
            min_delta_second_third: 10.0,
        }
    }
}

#[derive(Default)]
pub struct AngleHistory {
    ticks: VecDeque<HashMap<u64, AngleSnapshot>>,
    jg: JankGuard,
    params: Parameters,
    hot_params: AngleHistoryParams,
    detections: Vec<Detection>,
}

impl AngleHistory {
    pub fn new() -> Self {
        Self {
            params: HashMap::from([
                ("tick_window".to_string(), Parameter::Int(4)),
                ("max_delta_first_third".to_string(), Parameter::Float(0.5)),
                ("min_delta_second_third".to_string(), Parameter::Float(10.0)),
            ]),
            ..Default::default()
        }
    }

    fn begin_tick(&mut self) {
        let mut current = if self.ticks.len() >= self.hot_params.tick_window {
            self.ticks.pop_back().unwrap_or_default()
        } else {
            HashMap::new()
        };
        current.clear();
        self.ticks.push_front(current);
    }
}

impl CheatAlgorithm<'_> for AngleHistory {
    fn default(&self) -> bool {
        true
    }

    fn init(&mut self) -> Result<(), Error> {
        self.hot_params = AngleHistoryParams {
            tick_window: get_parameter_value::<i32>(&self.params, "tick_window").max(1) as usize,
            max_delta_first_third: get_parameter_value(&self.params, "max_delta_first_third"),
            min_delta_second_third: get_parameter_value(&self.params, "min_delta_second_third"),
        };
        Ok(())
    }

    fn algorithm_name(&self) -> &str {
        "angle_history"
    }

    fn on_tick(
        &mut self,
        state: &CheatAnalyserState,
        _: &ParserState,
    ) -> Result<Vec<Detection>, Error> {
        self.jg.on_tick(state);
        let ticknum = u32::from(state.tick);
        self.begin_tick();

        for player in state
            .players
            .iter()
            .filter(|player| player.in_pvs && player.state == PlayerState::Alive)
        {
            let Some(steam_id) = player.steam_id() else {
                continue;
            };

            let ticks_since_event = self
                .jg
                .teleported(&steam_id, ticknum)
                .min(self.jg.spawned(&steam_id, ticknum));
            if ticks_since_event < 60 {
                if ticks_since_event == 0 {
                    self.detections.retain(|detection| {
                        detection.player != steam_id || ticknum - detection.tick > 60
                    });
                }
                continue;
            }

            let current = AngleSnapshot {
                yaw: player.view_angle,
                pitch: player.pitch_angle,
            };
            self.ticks.front_mut().unwrap().insert(steam_id, current);
            let current_angle = (current.yaw, current.pitch);

            let mut matching = None;
            for i in 1..self.ticks.len() {
                let Some(past) = self.ticks.get(i).and_then(|tick| tick.get(&steam_id)) else {
                    continue;
                };
                let delta = angle_delta(current_angle, (past.yaw, past.pitch));
                if delta < self.hot_params.max_delta_first_third {
                    matching = Some((i, delta));
                    break;
                }
            }

            let Some((match_index, delta_one)) = matching else {
                continue;
            };
            let first_mid = (1 + match_index) / 2;
            let second_mid = ((match_index - 1) % 2 != 0).then_some(first_mid + 1);

            for middle_index in [Some(first_mid), second_mid].into_iter().flatten() {
                let Some(middle) = self
                    .ticks
                    .get(middle_index)
                    .and_then(|tick| tick.get(&steam_id))
                else {
                    continue;
                };
                let middle_angle = (middle.yaw, middle.pitch);
                let middle_delta = angle_delta(current_angle, middle_angle);
                if middle_delta <= self.hot_params.min_delta_second_third
                    || self.jg.fired(&steam_id, ticknum) > match_index as u32 + 5
                {
                    continue;
                }

                let middle_indices =
                    second_mid.map_or_else(|| vec![first_mid], |second| vec![first_mid, second]);
                self.detections.push(Detection {
                    tick: ticknum,
                    algorithm: "angle_history".to_string(),
                    player: steam_id,
                    data: json!({
                        "class": player.class_name(),
                        "weapon": state.get_player_weapon(player),
                        "angle_current": current_angle,
                        "angle_middle": middle_angle,
                        "angle_trigger": current_angle,
                        "delta_1_3": delta_one,
                        "delta_2_3": middle_delta,
                        "match_index": match_index,
                        "middle_indices": middle_indices,
                        "middle_trigger": middle_index,
                    }),
                });
            }
        }
        Ok(vec![])
    }

    fn handled_messages(&self) -> Result<Vec<tf_demo_parser::MessageType>, bool> {
        self.jg.handled_messages()
    }

    fn on_message(
        &mut self,
        message: &tf_demo_parser::demo::message::Message,
        state: &CheatAnalyserState,
        parser_state: &ParserState,
        tick: tf_demo_parser::demo::data::DemoTick,
    ) -> Result<Vec<Detection>, Error> {
        self.jg.on_message(message, state, parser_state, tick);
        Ok(vec![])
    }

    fn finish(&mut self) -> Result<Vec<Detection>, Error> {
        Ok(self.detections.clone())
    }

    fn params(&mut self) -> Option<&mut Parameters> {
        Some(&mut self.params)
    }
}
