// Written by Nocrex, Patched for Command Batching by Ciam

use std::collections::{HashMap, VecDeque};

use anyhow::Error;
use serde_json::json;
use tf_demo_parser::ParserState;

use crate::base::cheat_analyser_base::{CheatAnalyserState, PlayerState};
use crate::lib::algorithm::{CheatAlgorithm, Detection};
use crate::lib::parameters::{get_parameter_value, Parameter, Parameters};
use crate::util::{helpers::viewangle_delta, nocrex::jankguard::JankGuard};

#[derive(Clone, Copy)]
struct AngleSnapshot {
    yaw: f32,
    pitch: f32,
}

#[derive(Clone, Copy)]
struct AngleRepeatParams {
    min_angle_diff_ratio: f32,
    min_first_second_angle_delta: f32,
    max_first_third_angle_delta: f32,
}

impl Default for AngleRepeatParams {
    fn default() -> Self {
        Self {
            min_angle_diff_ratio: 0.0,
            min_first_second_angle_delta: 8.0,
            max_first_third_angle_delta: 1.5,
        }
    }
}

#[derive(Default)]
pub struct AngleRepeat {
    ticks: VecDeque<(u32, HashMap<u64, AngleSnapshot>)>,
    jg: JankGuard,
    params: Parameters,
    hot_params: AngleRepeatParams,
    detections: Vec<Detection>,
}

impl AngleRepeat {
    pub fn new() -> Self {
        Self {
            params: HashMap::from([
                ("min_angle_diff_ratio".to_string(), Parameter::Float(0.0)),
                (
                    "min_first_second_angle_delta".to_string(),
                    Parameter::Float(8.0),
                ),
                (
                    "max_first_third_angle_delta".to_string(),
                    Parameter::Float(1.5),
                ),
            ]),
            ..Default::default()
        }
    }

    fn begin_tick(&mut self, tick: u32) {
        let mut current = if self.ticks.len() >= 3 {
            self.ticks.pop_back().unwrap_or_default()
        } else {
            (0, HashMap::new())
        };
        current.0 = tick;
        current.1.clear();
        self.ticks.push_front(current);
    }
}

impl CheatAlgorithm<'_> for AngleRepeat {
    fn default(&self) -> bool {
        true
    }

    fn init(&mut self) -> Result<(), Error> {
        self.hot_params = AngleRepeatParams {
            min_angle_diff_ratio: get_parameter_value(&self.params, "min_angle_diff_ratio"),
            min_first_second_angle_delta: get_parameter_value(
                &self.params,
                "min_first_second_angle_delta",
            ),
            max_first_third_angle_delta: get_parameter_value(
                &self.params,
                "max_first_third_angle_delta",
            ),
        };
        Ok(())
    }

    fn algorithm_name(&self) -> &str {
        "nocrex/angle_repeat"
    }

    fn on_tick(
        &mut self,
        state: &CheatAnalyserState,
        _: &ParserState,
    ) -> Result<Vec<Detection>, Error> {
        self.jg.on_tick(state);
        let ticknum = u32::from(state.tick);
        self.begin_tick(ticknum);

        for player in state
            .players
            .iter()
            .filter(|player| player.in_pvs && player.state == PlayerState::Alive)
        {
            let Some(steam_id) = player.steam_id() else {
                continue;
            };

            let previous = self
                .ticks
                .get(1)
                .and_then(|(tick, players)| players.get(&steam_id).map(|player| (*tick, *player)));
            let first = self
                .ticks
                .get(2)
                .and_then(|(tick, players)| players.get(&steam_id).map(|player| (*tick, *player)));

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
            self.ticks.front_mut().unwrap().1.insert(steam_id, current);

            let (Some((second_tick, second)), Some((first_tick, first))) = (previous, first) else {
                continue;
            };
            let first_angle = (first.yaw, first.pitch);
            let second_angle = (second.yaw, second.pitch);
            let current_angle = (current.yaw, current.pitch);

            let real_delta =
                |old_tick: u32, old_angle: (f32, f32), new_tick: u32, new_angle: (f32, f32)| {
                    let tick_delta = new_tick.saturating_sub(old_tick);
                    let (yaw, pitch) = viewangle_delta(
                        new_angle.0,
                        new_angle.1,
                        old_angle.0,
                        old_angle.1,
                        tick_delta,
                    );
                    (yaw * yaw + pitch * pitch).sqrt()
                };
            let first_second_delta = real_delta(first_tick, first_angle, second_tick, second_angle);
            if first_second_delta < self.hot_params.min_first_second_angle_delta {
                continue;
            }
            let first_third_delta = real_delta(first_tick, first_angle, ticknum, current_angle);
            let ratio = first_second_delta / first_third_delta.max(1.0);

            if first_third_delta <= self.hot_params.max_first_third_angle_delta
                && ratio > self.hot_params.min_angle_diff_ratio
                && self.jg.fired(&steam_id, ticknum) < 3
            {
                let (class_name, weapon_name) = state.get_player_class_and_weapon_by_sid(steam_id);
                self.detections.push(Detection {
                    tick: ticknum,
                    algorithm: "nocrex/angle_repeat".to_string(),
                    player: steam_id,
                    data: json!({
                        "class": class_name,
                        "weapon": weapon_name,
                        "angle_1": first_angle,
                        "angle_2": second_angle,
                        "angle_3": current_angle,
                        "1_3_delta": first_third_delta,
                        "1_2_delta": first_second_delta,
                        "ratio": ratio,
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
