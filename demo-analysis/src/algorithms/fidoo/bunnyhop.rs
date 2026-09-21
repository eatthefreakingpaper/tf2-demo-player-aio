use std::collections::HashMap;

use anyhow::Error;
use serde_json::json;
use steamid_ng::SteamID;
use tf_demo_parser::demo::vector::Vector;
use tf_demo_parser::ParserState;

use crate::base::cheat_analyser_base::{CheatAnalyserState, PlayerState};
use crate::lib::algorithm::{CheatAlgorithm, Detection};
use crate::lib::parameters::{get_parameter_value, Parameter, Parameters};
use crate::util::nocrex::jankguard::JankGuard;

#[derive(Default, Clone)]
struct PlayerBhopState {
    ground_ticks: u32,
    air_ticks: u32,
    air_ticks_at_landing: u32,
    current_streak: u32,
    hop_ground_ticks: Vec<u32>,
    hop_speeds: Vec<f32>,
    last_reported_streak: u32,
    prev_pos: Option<Vector>,
    prev_on_ground: bool,
    last_movement_dz: f32,
    launch_z: f32,
    apex_z: f32,
    last_pos_sample: Option<Vector>,
    last_pos_sample_tick: u32,
    current_speed_hu_s: f32,
}

// Swimming in water alters ground/air flag behavior, and the buoyancy-driven
// movement right after leaving water mimics hop streaks, so hops are also
// ignored for this many ticks after the in-water/swimming flag clears.
const WATER_EXIT_GRACE_TICKS: u32 = 10;

pub struct BunnyHop {
    player_states: HashMap<u64, PlayerBhopState>,
    last_water_tick: HashMap<u64, u32>,
    jg: JankGuard,
    params: Parameters,
    detections: Vec<Detection>,
}

impl Default for BunnyHop {
    fn default() -> Self {
        Self::new()
    }
}

impl BunnyHop {
    pub fn new() -> Self {
        Self {
            player_states: HashMap::new(),
            last_water_tick: HashMap::new(),
            jg: JankGuard::default(),
            params: HashMap::from([
                ("min_streak".to_string(), Parameter::Int(5)),
                ("min_air_ticks".to_string(), Parameter::Int(6)),
                ("max_ground_ticks_bhop".to_string(), Parameter::Int(1)),
                ("min_speed".to_string(), Parameter::Float(150.0)),
            ]),
            detections: Vec::new(),
        }
    }
}

impl<'a> CheatAlgorithm<'a> for BunnyHop {
    fn default(&self) -> bool {
        true
    }

    fn algorithm_name(&self) -> &str {
        "fidoo/bunnyhop"
    }

    fn params(&mut self) -> Option<&mut Parameters> {
        Some(&mut self.params)
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

    fn on_tick(
        &mut self,
        state: &CheatAnalyserState,
        _: &ParserState,
    ) -> Result<Vec<Detection>, Error> {
        self.jg.on_tick(state);
        let ticknum = u32::from(state.tick);
        let players = &state.players;

        let min_streak: i32 = get_parameter_value(&self.params, "min_streak");
        let min_air_ticks: i32 = get_parameter_value(&self.params, "min_air_ticks");
        let max_ground_ticks: i32 =
            get_parameter_value(&self.params, "max_ground_ticks_bhop");
        let min_speed: f32 = get_parameter_value(&self.params, "min_speed");

        let mut tick_detections = Vec::new();
        let algo_name = self.algorithm_name().to_string();

        for player in players.iter().filter(|p| {
            p.in_pvs
                && p.state == PlayerState::Alive
                && p.info.as_ref().is_some_and(|info| info.steam_id != "BOT")
        }) {
            let info = match &player.info {
                Some(info) => info,
                None => continue,
            };

            let steam_id: u64 = match SteamID::from_steam3(&info.steam_id) {
                Ok(sid) => u64::from(sid),
                Err(_) => continue,
            };

            let ticks_since_event = self
                .jg
                .teleported(&steam_id, ticknum)
                .min(self.jg.spawned(&steam_id, ticknum));

            // Ignore detections within 60 ticks of spawn or teleport
            if ticks_since_event < 60 {
                self.player_states.remove(&steam_id);
                continue;
            }

            // Swimming in water alters ground/air flag behavior
            if player.is_in_water() {
                self.last_water_tick.insert(steam_id, ticknum);
                self.player_states.remove(&steam_id);
                continue;
            }

            // Water exit grace: buoyancy pops for a few ticks after leaving
            // water still look like hops, so keep ignoring the player here too
            if self
                .last_water_tick
                .get(&steam_id)
                .is_some_and(|&water_tick| ticknum.saturating_sub(water_tick) <= WATER_EXIT_GRACE_TICKS)
            {
                self.player_states.remove(&steam_id);
                continue;
            }

            let pstate = self.player_states.entry(steam_id).or_default();

            let on_ground = player.is_on_ground();
            let prev_z = pstate.prev_pos.map_or(player.position.z, |p| p.z);
            let dz = player.position.z - prev_z;

            let (dx, dy) = if let Some(prev_pos) = pstate.prev_pos {
                (player.position.x - prev_pos.x, player.position.y - prev_pos.y)
            } else {
                (0.0, 0.0)
            };
            let pos_delta_sq = dx * dx + dy * dy;

            // When a position packet arrives, compute accurate speed across update interval
            let pos_updated = dz.abs() > 0.001 || pos_delta_sq > 0.001;

            if pos_updated {
                let tick_delta = if pstate.last_pos_sample_tick > 0 {
                    (ticknum - pstate.last_pos_sample_tick).max(1)
                } else {
                    1
                };

                if let Some(last_sample) = pstate.last_pos_sample {
                    let sample_dx = player.position.x - last_sample.x;
                    let sample_dy = player.position.y - last_sample.y;
                    let dist = (sample_dx * sample_dx + sample_dy * sample_dy).sqrt();
                    pstate.current_speed_hu_s = (dist / (tick_delta as f32)) * 66.66667;
                }

                pstate.last_pos_sample = Some(player.position);
                pstate.last_pos_sample_tick = ticknum;
            }

            let current_speed = pstate.current_speed_hu_s;

            if on_ground {
                pstate.launch_z = player.position.z;
                pstate.apex_z = player.position.z;

                if !pstate.prev_on_ground {
                    pstate.air_ticks_at_landing = pstate.air_ticks;
                    pstate.ground_ticks = 1;
                    pstate.air_ticks = 0;
                } else {
                    pstate.ground_ticks += 1;
                    if pstate.ground_ticks > (max_ground_ticks.max(1) + 3) as u32 {
                        // Walking on ground breaks the bunnyhop streak
                        pstate.current_streak = 0;
                        pstate.hop_ground_ticks.clear();
                        pstate.hop_speeds.clear();
                        pstate.last_reported_streak = 0;
                    }
                }
            } else {
                pstate.apex_z = pstate.apex_z.max(player.position.z);

                // Check for zero-tick ground bounce (instant jump impulse without FL_ONGROUND toggle)
                // Filter out Scout mid-air double/triple jumps:
                // Must have fallen >= 20 units from apex down to ground level (prev_z) before jumping up
                let fallen_from_apex = pstate.apex_z - prev_z;
                let near_ground_level = prev_z <= pstate.launch_z + 25.0;

                let is_ground_bounce = pos_updated
                    && !pstate.prev_on_ground
                    && pstate.last_movement_dz < -1.0
                    && dz > 5.0
                    && pstate.air_ticks >= min_air_ticks as u32
                    && fallen_from_apex >= 20.0
                    && near_ground_level;

                if is_ground_bounce {
                    // Check if 0 ground ticks satisfies the configured max_ground_ticks limit
                    if 0 <= max_ground_ticks && current_speed >= min_speed {
                        pstate.current_streak += 1;
                        pstate.hop_ground_ticks.push(0);
                        pstate.hop_speeds.push(current_speed);

                        pstate.air_ticks_at_landing = pstate.air_ticks;
                        pstate.air_ticks = 1;
                        pstate.ground_ticks = 0;
                        pstate.launch_z = player.position.z;
                        pstate.apex_z = player.position.z;

                        if pstate.current_streak >= min_streak as u32
                            && pstate.current_streak > pstate.last_reported_streak
                            && pstate.hop_ground_ticks.iter().all(|&g| g <= max_ground_ticks as u32)
                        {
                            pstate.last_reported_streak = pstate.current_streak;
                            let avg_speed = pstate.hop_speeds.iter().sum::<f32>()
                                / pstate.hop_speeds.len() as f32;
                            let max_speed = pstate
                                .hop_speeds
                                .iter()
                                .cloned()
                                .fold(0.0_f32, f32::max);
                            let detection = Detection {
                                tick: ticknum,
                                algorithm: algo_name.clone(),
                                player: steam_id,
                                data: json!({
                                    "class": player.class_name(),
                                    "weapon": state.get_player_weapon(player),
                                    "streak": pstate.current_streak,
                                    "ground_ticks": pstate.hop_ground_ticks.clone(),
                                    "avg_speed": avg_speed,
                                    "max_speed": max_speed,
                                    "speed": current_speed,
                                }),
                            };
                            tick_detections.push(detection.clone());
                            self.detections.push(detection);
                        }
                    } else {
                        pstate.current_streak = 0;
                        pstate.hop_ground_ticks.clear();
                        pstate.hop_speeds.clear();
                        pstate.last_reported_streak = 0;
                    }
                } else if pstate.prev_on_ground {
                    let hopped_ground_ticks = pstate.ground_ticks;
                    let air_at_landing = pstate.air_ticks_at_landing;
                    pstate.ground_ticks = 0;
                    pstate.air_ticks = 1;
                    pstate.launch_z = player.position.z;
                    pstate.apex_z = player.position.z;

                    if air_at_landing >= min_air_ticks as u32 && current_speed >= min_speed {
                        // Strictly enforce that this hop does not exceed max_ground_ticks
                        if (hopped_ground_ticks as i32) <= max_ground_ticks {
                            pstate.current_streak += 1;
                            pstate.hop_ground_ticks.push(hopped_ground_ticks);
                            pstate.hop_speeds.push(current_speed);

                            if pstate.current_streak >= min_streak as u32
                                && pstate.current_streak > pstate.last_reported_streak
                                && pstate.hop_ground_ticks.iter().all(|&g| g <= max_ground_ticks as u32)
                            {
                                pstate.last_reported_streak = pstate.current_streak;
                                let avg_speed = pstate.hop_speeds.iter().sum::<f32>()
                                    / pstate.hop_speeds.len() as f32;
                                let max_speed = pstate
                                    .hop_speeds
                                    .iter()
                                    .cloned()
                                    .fold(0.0_f32, f32::max);
                                let detection = Detection {
                                    tick: ticknum,
                                    algorithm: algo_name.clone(),
                                    player: steam_id,
                                    data: json!({
                                        "class": player.class_name(),
                                        "weapon": state.get_player_weapon(player),
                                        "streak": pstate.current_streak,
                                        "ground_ticks": pstate.hop_ground_ticks.clone(),
                                        "avg_speed": avg_speed,
                                        "max_speed": max_speed,
                                        "speed": current_speed,
                                    }),
                                };
                                tick_detections.push(detection.clone());
                                self.detections.push(detection);
                            }
                        } else {
                            // Ground contact exceeded max_ground_ticks -> break streak
                            pstate.current_streak = 0;
                            pstate.hop_ground_ticks.clear();
                            pstate.hop_speeds.clear();
                            pstate.last_reported_streak = 0;
                        }
                    }
                } else {
                    pstate.air_ticks += 1;
                }
            }

            if pos_updated {
                pstate.last_movement_dz = dz;
            }
            pstate.prev_pos = Some(player.position);
            pstate.prev_on_ground = on_ground;
        }

        Ok(tick_detections)
    }

    fn finish(&mut self) -> Result<Vec<Detection>, Error> {
        Ok(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::base::cheat_analyser_base::Player;
    use tf_demo_parser::demo::message::packetentities::EntityId;
    use tf_demo_parser::demo::parser::analyser::{Class, Team, UserId, UserInfo};

    const FL_ONGROUND: u32 = 1;
    // TF2's networked in-water bit; see Player::is_in_water.
    const FL_INWATER: u32 = 1 << 10;
    const STEAM_ID: &str = "[U:1:12345678]";

    fn test_parser_state() -> ParserState {
        ParserState::new(24, |_| false, false)
    }

    fn test_player(x: f32, flags: u32) -> Player {
        Player {
            entity: EntityId::from(1u32),
            position: Vector { x, y: 0.0, z: 0.0 },
            health: 125,
            max_health: 125,
            class: Class::Scout,
            team: Team::Red,
            view_angle: 0.0,
            pitch_angle: 0.0,
            state: PlayerState::Alive,
            info: Some(UserInfo {
                classes: Default::default(),
                name: "TestSwimmer".to_string(),
                user_id: UserId::from(1u16),
                steam_id: STEAM_ID.to_string(),
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
            flags,
        }
    }

    // Drives on_tick once per flags entry, moving ~333 HU/s along x so hops
    // clear min_speed. Ticks start at 100 to clear the 60-tick spawn grace.
    fn run(algo: &mut BunnyHop, flags_seq: &[u32]) -> Vec<Detection> {
        let mut state = CheatAnalyserState::default();
        let parser_state = test_parser_state();
        let mut detections = Vec::new();
        for (i, &flags) in flags_seq.iter().enumerate() {
            state.tick = (100 + i as u32).into();
            state.players = vec![test_player(i as f32 * 5.0, flags)];
            detections.extend(algo.on_tick(&state, &parser_state).unwrap());
        }
        detections
    }

    fn hop_cycles(cycles: usize) -> Vec<u32> {
        let mut seq = Vec::new();
        for _ in 0..cycles {
            seq.push(FL_ONGROUND); // landing
            seq.extend(std::iter::repeat(0).take(7)); // air, so the next landing sees >= 6 air ticks
        }
        seq
    }

    #[test]
    fn hop_pattern_without_water_is_detected() {
        let mut algo = BunnyHop::new();
        let detections = run(&mut algo, &hop_cycles(8));
        assert!(
            !detections.is_empty(),
            "plain hop streak should still be detected"
        );
    }

    // The 2fort false positive: the networked in-water bit flickers while the
    // player bobs at the surface, and each bob reads as a 1-ground-tick hop
    // landing. Without the exit grace this builds a streak.
    #[test]
    fn water_bobbing_does_not_build_hop_streaks() {
        let mut seq = Vec::new();
        for _ in 0..12 {
            seq.push(FL_INWATER); // dip
            seq.push(FL_ONGROUND); // landing
            seq.extend(std::iter::repeat(0).take(6)); // air
            seq.push(FL_ONGROUND); // landing with 6 air ticks
            seq.push(0); // hop
            seq.push(0);
        }
        let mut algo = BunnyHop::new();
        let detections = run(&mut algo, &seq);
        assert!(
            detections.is_empty(),
            "hops within the water-exit grace must not count, got {detections:?}"
        );
    }

    #[test]
    fn wading_water_flag_counts_as_water() {
        let mut seq = Vec::new();
        for _ in 0..12 {
            seq.push(FL_INWATER | FL_ONGROUND); // wading, feet on the bottom
            seq.push(FL_ONGROUND);
            seq.extend(std::iter::repeat(0).take(6));
            seq.push(FL_ONGROUND);
            seq.push(0);
            seq.push(0);
        }
        let mut algo = BunnyHop::new();
        let detections = run(&mut algo, &seq);
        assert!(
            detections.is_empty(),
            "wading (water + on ground) must be treated like in-water, got {detections:?}"
        );
    }

    // The grace has to expire: a real hop streak well after leaving water is
    // still detected.
    #[test]
    fn grace_expires_after_leaving_water() {
        let mut seq = vec![FL_INWATER; 3];
        seq.extend(std::iter::repeat(FL_ONGROUND).take(12)); // walk it off
        seq.extend(hop_cycles(10));
        let mut algo = BunnyHop::new();
        let detections = run(&mut algo, &seq);
        assert!(
            !detections.is_empty(),
            "hops long after water exit should still be detected"
        );
    }
}
