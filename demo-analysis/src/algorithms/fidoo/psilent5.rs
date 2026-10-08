use std::collections::{HashMap, VecDeque};

use anyhow::Error;
use serde_json::json;
use tf_demo_parser::demo::message::packetentities::{EntityId, UpdateType};
use tf_demo_parser::demo::message::Message;
use tf_demo_parser::demo::sendprop::SendPropIdentifier;
use tf_demo_parser::demo::vector::Vector;
use tf_demo_parser::{MessageType, ParserState};

use crate::base::cheat_analyser_base::{CheatAnalyserState, Player, PlayerState};
use crate::lib::algorithm::{CheatAlgorithm, Detection};
use crate::lib::parameters::{get_parameter_value, Parameter, Parameters};
use crate::util::helpers::{angle_delta, handle_to_entid, viewangle_delta};

const HISTORY_TICKS: u32 = 40;
const TELEPORT_THRESHOLD: f32 = 400.0;

// Standard and transition eye heights: ducking (45), intermediate crouch transitions, and standing (68, 72)
const EYE_HEIGHTS: [f32; 6] = [45.0, 50.0, 56.0, 62.0, 68.0, 72.0];

// Candidate target convergence distances in Hammer Units
const CANDIDATE_DISTANCES: [f32; 9] = [
    24.0, 48.0, 96.0, 192.0, 384.0, 768.0, 1536.0, 2000.0, 4000.0,
];

#[derive(Clone)]
struct PlayerSnapshot {
    tick: u32,
    position: Vector,
    yaw: f32,
    pitch: f32,
    simtime: u16,
    in_pvs: bool,
    state: PlayerState,
    is_taunting: bool,
}

impl PlayerSnapshot {
    fn new(tick: u32, player: &Player) -> Self {
        Self {
            tick,
            position: player.position,
            yaw: player.view_angle,
            pitch: player.pitch_angle,
            simtime: player.simtime,
            in_pvs: player.in_pvs,
            state: player.state,
            is_taunting: player.is_taunting() || (player.cond & (1 << 7)) != 0,
        }
    }
}

struct LaunchCandidate {
    tick: u32,
    player: u64,
    player_class: &'static str,
    weapon_name: String,
    player_ping: u16,
    projectile: String,
    projectile_entity: u32,
    serial: u32,
    origin: Vector,
    initial_velocity: Vector,
    launch_direction: Vector,
    owner_distance: f32,
    pipe_type: Option<i64>,
}

struct TentativeDetection {
    detection: Detection,
    projectile_entity: u32,
    serial: u32,
}

pub struct Psilent5 {
    histories: HashMap<u64, VecDeque<PlayerSnapshot>>,
    pending: Vec<LaunchCandidate>,
    tentative: Vec<TentativeDetection>,
    observations: HashMap<(u32, u32), u32>,
    params: Parameters,
}

impl Psilent5 {
    pub fn new() -> Self {
        Self {
            histories: HashMap::new(),
            pending: Vec::new(),
            tentative: Vec::new(),
            observations: HashMap::new(),
            params: HashMap::from([
                ("enabled".to_string(), Parameter::Bool(true)),
                ("angle_threshold".to_string(), Parameter::Float(6.0)),
                ("lookaround_ticks".to_string(), Parameter::Int(18)),
                ("max_owner_distance".to_string(), Parameter::Float(115.0)),
                ("minimum_samples".to_string(), Parameter::Int(2)),
                ("minimum_suspicious_launches".to_string(), Parameter::Int(2)),
            ]),
        }
    }

    fn record_state(&mut self, state: &CheatAnalyserState) {
        let tick = u32::from(state.tick);
        for player in &state.players {
            let Some(steam_id) = player_id(player) else {
                continue;
            };
            self.record_snapshot(steam_id, PlayerSnapshot::new(tick, player));
        }
        self.histories.retain(|_, history| {
            while history
                .front()
                .is_some_and(|snapshot| tick.saturating_sub(snapshot.tick) > HISTORY_TICKS)
            {
                history.pop_front();
            }
            !history.is_empty()
        });
    }

    fn record_snapshot(&mut self, steam_id: u64, snapshot: PlayerSnapshot) {
        let history = self.histories.entry(steam_id).or_default();
        if let Some(previous) = history.back() {
            if previous.tick == snapshot.tick
                && previous.simtime == snapshot.simtime
                && (previous.yaw - snapshot.yaw).abs() < 0.001
                && (previous.pitch - snapshot.pitch).abs() < 0.001
                && vector_length(previous.position - snapshot.position) < 0.01
            {
                return;
            }
        }
        history.push_back(snapshot);
    }

    fn resolve_pending(&mut self, current_tick: u32, flush: bool) -> Vec<TentativeDetection> {
        let lookaround_ticks =
            get_parameter_value::<i32>(&self.params, "lookaround_ticks").max(0) as u32;
        let mut waiting = Vec::new();
        let mut detections = Vec::new();

        for candidate in self.pending.drain(..) {
            if !self.histories.contains_key(&candidate.player) {
                if !flush {
                    waiting.push(candidate);
                }
                continue;
            }

            let ping_ticks = ((candidate.player_ping as f32 / 15.0).round() as u32).min(20);
            let effective_lookaround = (lookaround_ticks + ping_ticks).max(18).min(32);

            if !flush && current_tick < candidate.tick.saturating_add(effective_lookaround) {
                waiting.push(candidate);
                continue;
            }

            if let Some(detection) = evaluate_candidate(&self.params, &self.histories, candidate) {
                detections.push(detection);
            }
        }
        self.pending = waiting;
        detections
    }
}

impl Default for Psilent5 {
    fn default() -> Self {
        Self::new()
    }
}

impl CheatAlgorithm<'_> for Psilent5 {
    fn default(&self) -> bool {
        true
    }

    fn algorithm_name(&self) -> &str {
        "fidoo/psilent5"
    }

    fn params(&mut self) -> Option<&mut Parameters> {
        Some(&mut self.params)
    }

    fn handled_messages(&self) -> Result<Vec<MessageType>, bool> {
        Ok(vec![MessageType::PacketEntities])
    }

    fn on_tick(
        &mut self,
        state: &CheatAnalyserState,
        _: &ParserState,
    ) -> Result<Vec<Detection>, Error> {
        self.record_state(state);
        let resolved = self.resolve_pending(state.tick.into(), false);
        self.tentative.extend(resolved);
        Ok(vec![])
    }

    fn on_message(
        &mut self,
        message: &Message,
        state: &CheatAnalyserState,
        parser_state: &ParserState,
        _: tf_demo_parser::demo::data::DemoTick,
    ) -> Result<Vec<Detection>, Error> {
        let Message::PacketEntities(message) = message else {
            return Ok(vec![]);
        };
        self.record_state(state);
        let tick = u32::from(state.tick);

        for entity in &message.entities {
            if entity.update_type != UpdateType::Enter {
                continue;
            }
            let Some(class) = parser_state
                .server_classes
                .get(usize::from(entity.server_class))
                .map(|class| class.name.as_str())
                .filter(|class| is_accepted_projectile(class))
            else {
                continue;
            };

            let props = state.entity_props(entity, parser_state);

            // 100% Reliable Networked Owner Verification
            let Some(owner_steam_id) = resolve_owner_steam_id(&props, state) else {
                continue;
            };

            let Some(player) = state.get_player_by_sid(owner_steam_id) else {
                continue;
            };

            let is_taunting = player.is_taunting() || (player.cond & (1 << 7)) != 0;
            if !player.in_pvs || player.state != PlayerState::Alive || is_taunting {
                continue;
            }

            // Extract projectile spawn origin
            let Some(origin) = projectile_vector(
                &props,
                &[
                    ("DT_TFBaseRocket", "m_vecOrigin"),
                    ("DT_TFWeaponBaseGrenadeProj", "m_vecOrigin"),
                    ("DT_BaseEntity", "m_vecOrigin"),
                ],
            ) else {
                continue;
            };

            // Extract projectile launch velocity / direction
            let Some(initial_velocity) = extract_initial_velocity(class, &props) else {
                continue;
            };

            // Filter out deflected or bounced projectiles
            if is_deflected_or_touched(&props) {
                continue;
            }

            let Some(launch_direction) = normalized(initial_velocity) else {
                continue;
            };

            let player_class = player.class_name();
            let weapon_name = state.get_player_weapon(player);
            if is_excluded_weapon(&weapon_name) {
                continue;
            }
            let pipe_type = projectile_integer(&props, &[("DT_TFProjectile_Pipebomb", "m_iType")]);

            let max_owner_distance = get_parameter_value::<f32>(&self.params, "max_owner_distance");
            let offset = origin - player.position;
            let owner_distance = vector_length(offset);
            if !owner_distance.is_finite() || owner_distance > max_owner_distance {
                continue;
            }

            let observation_key = (u32::from(entity.entity_index), entity.serial_number);
            match self.observations.get(&observation_key) {
                None => {
                    self.observations.insert(observation_key, tick);
                }
                Some(&first_tick) if tick.saturating_sub(first_tick) <= 2 => {
                    // Refine candidate within same initial spawn burst across adjacent ticks
                }
                Some(_) => {
                    // Re-entering PVS long after initial spawn (e.g. arrow stuck in wall), ignore
                    continue;
                }
            }

            self.record_snapshot(owner_steam_id, PlayerSnapshot::new(tick, player));
            self.pending.push(LaunchCandidate {
                tick,
                player: owner_steam_id,
                player_class,
                weapon_name,
                player_ping: player.ping,
                projectile: class.to_string(),
                projectile_entity: entity.entity_index.into(),
                serial: entity.serial_number,
                origin,
                initial_velocity,
                launch_direction,
                owner_distance,
                pipe_type,
            });
        }

        Ok(vec![])
    }

    fn finish(&mut self) -> Result<Vec<Detection>, Error> {
        let resolved = self.resolve_pending(u32::MAX, true);
        self.tentative.extend(resolved);

        let mut unique = HashMap::<(u64, u32, u32), TentativeDetection>::new();
        for tentative in self.tentative.drain(..) {
            let key = (
                tentative.detection.player,
                tentative.projectile_entity,
                tentative.serial,
            );
            let new_delta = detection_delta(&tentative.detection);
            match unique.get(&key) {
                Some(existing) if detection_delta(&existing.detection) >= new_delta => {}
                _ => {
                    unique.insert(key, tentative);
                }
            }
        }

        let minimum_launches =
            get_parameter_value::<i32>(&self.params, "minimum_suspicious_launches").max(1) as usize;
        let mut launch_counts = HashMap::<u64, usize>::new();
        for (player, _, _) in unique.keys() {
            *launch_counts.entry(*player).or_default() += 1;
        }

        // Emit if minimum suspicious launches met OR if blatant high-delta single shot (>= 25.0 deg)
        let mut detections: Vec<_> = unique
            .into_values()
            .filter(|tentative| {
                let delta = detection_delta(&tentative.detection);
                delta >= 25.0
                    || launch_counts
                        .get(&tentative.detection.player)
                        .is_some_and(|count| *count >= minimum_launches)
            })
            .map(|tentative| tentative.detection)
            .collect();
        detections.sort_by_key(|detection| (detection.tick, detection.player));
        Ok(detections)
    }
}

fn evaluate_candidate(
    params: &Parameters,
    histories: &HashMap<u64, VecDeque<PlayerSnapshot>>,
    candidate: LaunchCandidate,
) -> Option<TentativeDetection> {
    // Exclude random spread weapons (Beggar's Bazooka) and zero-damage practice weapons (Rocket Jumper, Sticky Jumper)
    if is_excluded_weapon(&candidate.weapon_name) {
        return None;
    }

    let angle_threshold = get_parameter_value::<f32>(params, "angle_threshold");
    let lookaround_ticks = get_parameter_value::<i32>(params, "lookaround_ticks").max(0) as u32;
    let minimum_samples = get_parameter_value::<i32>(params, "minimum_samples").max(1) as usize;
    let history = histories.get(&candidate.player)?;
    let ping_ticks = ((candidate.player_ping as f32 / 15.0).round() as u32).min(20);
    let effective_lookaround = (lookaround_ticks + ping_ticks).max(18).min(32);
    let start_tick = candidate.tick.saturating_sub(effective_lookaround);
    let end_tick = candidate.tick.saturating_add(effective_lookaround);

    let samples: Vec<_> = history
        .iter()
        .filter(|sample| {
            (start_tick..=end_tick).contains(&sample.tick)
                && sample.in_pvs
                && sample.state == PlayerState::Alive
                && !sample.is_taunting
        })
        .collect();

    if samples.len() < minimum_samples {
        return None;
    }

    if history
        .iter()
        .any(|s| s.tick.abs_diff(candidate.tick) <= 2 && s.is_taunting)
    {
        return None;
    }

    for window in samples.windows(2) {
        let (s1, s2) = (window[0], window[1]);
        let pos_delta = s2.position - s1.position;
        let distance = vector_length(pos_delta);
        if distance > TELEPORT_THRESHOLD {
            return None;
        }
    }

    let launch_angles = vector_angles(candidate.launch_direction);
    let speed = vector_length(candidate.initial_velocity);
    let is_pipebomb = candidate.projectile == "CTFGrenadePipebombProjectile";

    let mut raw_delta = f32::INFINITY;
    let mut compensated_delta = f32::INFINITY;
    let mut best_view = (0.0, 0.0);
    let mut best_tick = candidate.tick;

    // Generate test views from recorded snapshots and interpolated sub-tick frames
    let mut test_views = Vec::new();
    for sample in &samples {
        test_views.push((sample.tick, sample.position, sample.yaw, sample.pitch));
    }

    for window in samples.windows(2) {
        let (s1, s2) = (window[0], window[1]);
        let tick_gap = s2.tick.saturating_sub(s1.tick);
        if tick_gap > 16 {
            continue;
        }

        let steps = if tick_gap == 0 {
            4
        } else {
            (tick_gap.min(8) * 2).max(4)
        };
        for step in 1..steps {
            let fraction = step as f32 / steps as f32;
            let (va_diff, pa_diff) = viewangle_delta(s2.yaw, s2.pitch, s1.yaw, s1.pitch, 1);
            let interp_yaw = (s1.yaw + va_diff * fraction).rem_euclid(360.0);
            let interp_pitch = (s1.pitch + pa_diff * fraction).clamp(-89.0, 89.0);
            let interp_pos = Vector {
                x: s1.position.x + (s2.position.x - s1.position.x) * fraction,
                y: s1.position.y + (s2.position.y - s1.position.y) * fraction,
                z: s1.position.z + (s2.position.z - s1.position.z) * fraction,
            };
            let interp_tick_float = s1.tick as f32 + (s2.tick as f32 - s1.tick as f32) * fraction;
            let interp_tick = interp_tick_float.round() as u32;
            test_views.push((interp_tick, interp_pos, interp_yaw, interp_pitch));
        }
    }

    // Continuous Yaw Compensation & Convergence Evaluation
    for (tick, pos, yaw, pitch) in test_views {
        let view = (yaw, pitch);
        let current_raw = angle_delta(view, launch_angles);
        if current_raw < raw_delta {
            raw_delta = current_raw;
        }

        let v_view = angles_to_vector(yaw, pitch);
        let v_up = angles_to_up_vector(yaw, pitch);

        // For CTFGrenadePipebombProjectile, Source engine adds vecUp * 200.0f
        // (CTFGrenadePipebombProjectile::Create). We check both with upward arc and unboosted (e.g. Loose Cannon)
        let arc_boost_candidates: &[(bool, f32)] = if is_pipebomb && speed > 210.0 {
            &[(true, 200.0), (false, 0.0)]
        } else {
            &[(false, 0.0)]
        };

        for &eye_height in &EYE_HEIGHTS {
            let eye = Vector {
                z: pos.z + eye_height,
                ..pos
            };

            for &(use_arc, arc_boost) in arc_boost_candidates {
                // 1. Direct view angle alignment (infinite distance target: d -> infinity)
                let expected_direct = if use_arc {
                    let fwd_speed = (speed * speed - arc_boost * arc_boost).max(0.0).sqrt();
                    let v = Vector {
                        x: v_view.x * fwd_speed + v_up.x * arc_boost,
                        y: v_view.y * fwd_speed + v_up.y * arc_boost,
                        z: v_view.z * fwd_speed + v_up.z * arc_boost,
                    };
                    normalized(v).unwrap_or(v_view)
                } else {
                    v_view
                };
                let direct_angles = vector_angles(expected_direct);
                let direct_delta = angle_delta(direct_angles, launch_angles);
                if direct_delta < compensated_delta {
                    compensated_delta = direct_delta;
                    best_view = view;
                    best_tick = tick;
                }

                // 2. Analytical closest approach distance d* along sightline ray
                let w = eye - candidate.origin;
                let c = dot(candidate.launch_direction, expected_direct);
                let denom = 1.0 - c * c;
                if denom > 1e-6 {
                    let d_star =
                        (c * dot(candidate.launch_direction, w) - dot(expected_direct, w)) / denom;
                    if d_star > 10.0 && d_star < 8000.0 {
                        let target = Vector {
                            x: eye.x + v_view.x * d_star,
                            y: eye.y + v_view.y * d_star,
                            z: eye.z + v_view.z * d_star,
                        };
                        if let Some(mut expected_dir) = normalized(target - candidate.origin) {
                            if use_arc {
                                let fwd_speed =
                                    (speed * speed - arc_boost * arc_boost).max(0.0).sqrt();
                                let v = Vector {
                                    x: expected_dir.x * fwd_speed + v_up.x * arc_boost,
                                    y: expected_dir.y * fwd_speed + v_up.y * arc_boost,
                                    z: expected_dir.z * fwd_speed + v_up.z * arc_boost,
                                };
                                if let Some(n) = normalized(v) {
                                    expected_dir = n;
                                }
                            }
                            let expected_angles = vector_angles(expected_dir);
                            let delta = angle_delta(launch_angles, expected_angles);
                            if delta < compensated_delta {
                                compensated_delta = delta;
                                best_view = view;
                                best_tick = tick;
                            }
                        }
                    }
                }

                // 3. Ground plane impact distance for downward shots / rocket jumps
                if pitch > 20.0 && v_view.z < -0.05 {
                    let d_ground = eye_height / (-v_view.z);
                    if d_ground > 10.0 && d_ground < 1500.0 {
                        let target = Vector {
                            x: eye.x + v_view.x * d_ground,
                            y: eye.y + v_view.y * d_ground,
                            z: pos.z,
                        };
                        if let Some(mut expected_dir) = normalized(target - candidate.origin) {
                            if use_arc {
                                let fwd_speed =
                                    (speed * speed - arc_boost * arc_boost).max(0.0).sqrt();
                                let v = Vector {
                                    x: expected_dir.x * fwd_speed + v_up.x * arc_boost,
                                    y: expected_dir.y * fwd_speed + v_up.y * arc_boost,
                                    z: expected_dir.z * fwd_speed + v_up.z * arc_boost,
                                };
                                if let Some(n) = normalized(v) {
                                    expected_dir = n;
                                }
                            }
                            let expected_angles = vector_angles(expected_dir);
                            let delta = angle_delta(launch_angles, expected_angles);
                            if delta < compensated_delta {
                                compensated_delta = delta;
                                best_view = view;
                                best_tick = tick;
                            }
                        }
                    }
                }

                // 4. Candidate engagement distances covering close to far ranges
                for &dist in &CANDIDATE_DISTANCES {
                    let target = Vector {
                        x: eye.x + v_view.x * dist,
                        y: eye.y + v_view.y * dist,
                        z: eye.z + v_view.z * dist,
                    };
                    if let Some(mut expected_dir) = normalized(target - candidate.origin) {
                        if use_arc {
                            let fwd_speed = (speed * speed - arc_boost * arc_boost).max(0.0).sqrt();
                            let v = Vector {
                                x: expected_dir.x * fwd_speed + v_up.x * arc_boost,
                                y: expected_dir.y * fwd_speed + v_up.y * arc_boost,
                                z: expected_dir.z * fwd_speed + v_up.z * arc_boost,
                            };
                            if let Some(n) = normalized(v) {
                                expected_dir = n;
                            }
                        }
                        let expected_angles = vector_angles(expected_dir);
                        let delta = angle_delta(launch_angles, expected_angles);
                        if delta < compensated_delta {
                            compensated_delta = delta;
                            best_view = view;
                            best_tick = tick;
                        }
                    }
                }
            }
        }
    }

    let is_downward_rocket =
        candidate.launch_direction.z < -0.35 && candidate.projectile == "CTFProjectile_Rocket";
    let effective_threshold = if is_downward_rocket {
        angle_threshold + 1.5
    } else {
        angle_threshold
    };

    if !compensated_delta.is_finite() || compensated_delta < effective_threshold {
        return None;
    }

    let yaw_diff = (launch_angles.0 - best_view.0).rem_euclid(360.0);
    let yaw_delta = if yaw_diff > 180.0 {
        360.0 - yaw_diff
    } else {
        yaw_diff
    };
    let pitch_delta = (launch_angles.1 - best_view.1).abs();

    Some(TentativeDetection {
        projectile_entity: candidate.projectile_entity,
        serial: candidate.serial,
        detection: Detection {
            tick: candidate.tick,
            algorithm: "fidoo/psilent5".to_string(),
            player: candidate.player,
            data: json!({
                "class": candidate.player_class,
                "weapon": candidate.weapon_name,
                "projectile": candidate.projectile,
                "projectile_entity": candidate.projectile_entity,
                "projectile_serial": candidate.serial,
                "origin": candidate.origin,
                "initial_velocity": candidate.initial_velocity,
                "launch_angles": launch_angles,
                "view_angles": best_view,
                "view_tick": best_tick,
                "raw_angle_delta": raw_delta,
                "compensated_angle_delta": compensated_delta,
                "yaw_delta": yaw_delta,
                "pitch_delta": pitch_delta,
                "owner_distance": candidate.owner_distance,
                "pipe_type": candidate.pipe_type,
                "samples": samples.len(),
            }),
        },
    })
}

fn detection_delta(detection: &Detection) -> f64 {
    detection.data["compensated_angle_delta"]
        .as_f64()
        .unwrap_or_default()
}

fn is_excluded_weapon(weapon_name: &str) -> bool {
    weapon_name.contains("Beggar")
        || weapon_name.contains("Rocket Jumper")
        || weapon_name.contains("Sticky Jumper")
}

// 100% Reliable Owner Resolution:
// Resolves entity handles from m_hThrower (grenade/sticky priority) and m_hOwnerEntity (rockets/flares/arrows/etc.)
// Maps to player entity and SteamID64
fn resolve_owner_steam_id(
    props: &[tf_demo_parser::demo::sendprop::SendProp],
    state: &CheatAnalyserState,
) -> Option<u64> {
    const OWNER: SendPropIdentifier = SendPropIdentifier::new("DT_BaseEntity", "m_hOwnerEntity");
    const THROWER: SendPropIdentifier = SendPropIdentifier::new("DT_BaseGrenade", "m_hThrower");

    // Priority 1: m_hThrower (pipes, stickies)
    if let Some(prop) = props.iter().find(|p| p.identifier == THROWER) {
        if let Ok(handle) = i64::try_from(&prop.value) {
            let entid = handle_to_entid(handle as u32);
            if let Some(steam_id) = steam_id_from_entid(entid, state) {
                return Some(steam_id);
            }
        }
    }

    // Priority 2: m_hOwnerEntity (rockets, flares, crossbow bolts, syringes, arrows, energy orbs)
    if let Some(prop) = props.iter().find(|p| p.identifier == OWNER) {
        if let Ok(handle) = i64::try_from(&prop.value) {
            let entid = handle_to_entid(handle as u32);
            if let Some(steam_id) = steam_id_from_entid(entid, state) {
                return Some(steam_id);
            }
        }
    }

    None
}

fn steam_id_from_entid(entid: EntityId, state: &CheatAnalyserState) -> Option<u64> {
    if u32::from(entid) >= 2047 || u32::from(entid) == 0 {
        return None;
    }
    state.steam_id_for_entity(entid)
}

fn is_accepted_projectile(class: &str) -> bool {
    match class {
        "CTFProjectile_Rocket"
        | "CTFGrenadePipebombProjectile"
        | "CTFProjectile_Flare"
        | "CTFProjectile_BallOfFire"
        | "CTFProjectile_HealingBolt"
        | "CTFProjectile_Syringe"
        | "CTFProjectile_Arrow"
        | "CTFProjectile_EnergyBall"
        | "CTFProjectile_EnergyRing"
        | "CTFProjectile_MechanicalArmOrb" => true,
        _ => false,
    }
}

fn is_deflected_or_touched(props: &[tf_demo_parser::demo::sendprop::SendProp]) -> bool {
    let deflected = projectile_integer(
        props,
        &[
            ("DT_TFBaseRocket", "m_iDeflected"),
            ("DT_TFWeaponBaseGrenadeProj", "m_iDeflected"),
        ],
    );
    if deflected.is_some_and(|d| d > 0) {
        return true;
    }

    let touched = projectile_integer(props, &[("DT_TFProjectile_Pipebomb", "m_bTouched")]);
    touched.is_some_and(|t| t > 0)
}

fn extract_initial_velocity(
    class: &str,
    props: &[tf_demo_parser::demo::sendprop::SendProp],
) -> Option<Vector> {
    if class == "CTFProjectile_MechanicalArmOrb" || class == "CTFProjectile_BallOfFire" {
        let ang_rotation = projectile_vector(
            props,
            &[
                ("DT_TFBaseRocket", "m_angRotation"),
                ("DT_BaseEntity", "m_angRotation"),
            ],
        );
        if let Some(rot) = ang_rotation {
            let speed = if class == "CTFProjectile_BallOfFire" {
                3000.0
            } else {
                1100.0
            };
            let dir = angles_to_vector(rot.y, rot.x);
            return Some(Vector {
                x: dir.x * speed,
                y: dir.y * speed,
                z: dir.z * speed,
            });
        }
    }

    let vel = projectile_vector(
        props,
        &[
            ("DT_TFBaseRocket", "m_vInitialVelocity"),
            ("DT_TFWeaponBaseGrenadeProj", "m_vInitialVelocity"),
        ],
    );
    if let Some(v) = vel {
        if vector_length(v) > 1.0 {
            return Some(v);
        }
    }

    // Fallback to m_angRotation if initial velocity is missing/zero
    let ang_rotation = projectile_vector(
        props,
        &[
            ("DT_TFBaseRocket", "m_angRotation"),
            ("DT_BaseEntity", "m_angRotation"),
        ],
    );
    if let Some(rot) = ang_rotation {
        let dir = angles_to_vector(rot.y, rot.x);
        return Some(Vector {
            x: dir.x * 1100.0,
            y: dir.y * 1100.0,
            z: dir.z * 1100.0,
        });
    }

    None
}

fn projectile_vector(
    props: &[tf_demo_parser::demo::sendprop::SendProp],
    identifiers: &[(&str, &str)],
) -> Option<Vector> {
    identifiers.iter().find_map(|(table, name)| {
        let identifier = SendPropIdentifier::new(table, name);
        props
            .iter()
            .find(|prop| prop.identifier == identifier)
            .and_then(|prop| Vector::try_from(&prop.value).ok())
    })
}

fn projectile_integer(
    props: &[tf_demo_parser::demo::sendprop::SendProp],
    identifiers: &[(&str, &str)],
) -> Option<i64> {
    identifiers.iter().find_map(|(table, name)| {
        let identifier = SendPropIdentifier::new(table, name);
        props
            .iter()
            .find(|prop| prop.identifier == identifier)
            .and_then(|prop| i64::try_from(&prop.value).ok())
    })
}

fn player_id(player: &Player) -> Option<u64> {
    player.steam_id()
}

fn normalized(vector: Vector) -> Option<Vector> {
    let length = vector_length(vector);
    if !length.is_finite() || length < 100.0 * f32::EPSILON {
        return None;
    }
    Some(Vector {
        x: vector.x / length,
        y: vector.y / length,
        z: vector.z / length,
    })
}

fn vector_angles(vector: Vector) -> (f32, f32) {
    let horizontal = (vector.x * vector.x + vector.y * vector.y).sqrt();
    (
        vector.y.atan2(vector.x).to_degrees(),
        (-vector.z).atan2(horizontal).to_degrees(),
    )
}

fn vector_length(vector: Vector) -> f32 {
    dot(vector, vector).sqrt()
}

fn dot(first: Vector, second: Vector) -> f32 {
    first.x * second.x + first.y * second.y + first.z * second.z
}

fn angles_to_vector(yaw_deg: f32, pitch_deg: f32) -> Vector {
    let yaw_rad = yaw_deg.to_radians();
    let pitch_rad = pitch_deg.to_radians();
    Vector {
        x: yaw_rad.cos() * pitch_rad.cos(),
        y: yaw_rad.sin() * pitch_rad.cos(),
        z: -pitch_rad.sin(),
    }
}

fn angles_to_up_vector(yaw_deg: f32, pitch_deg: f32) -> Vector {
    let yaw_rad = yaw_deg.to_radians();
    let pitch_rad = pitch_deg.to_radians();
    Vector {
        x: pitch_rad.sin() * yaw_rad.cos(),
        y: pitch_rad.sin() * yaw_rad.sin(),
        z: pitch_rad.cos(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_angles_roundtrip() {
        let dir = angles_to_vector(45.0, 15.0);
        let (yaw, pitch) = vector_angles(dir);
        assert!((yaw - 45.0).abs() < 0.01);
        assert!((pitch - 15.0).abs() < 0.01);
    }

    #[test]
    fn test_forward_up_orthogonality() {
        let fwd = angles_to_vector(35.0, 20.0);
        let up = angles_to_up_vector(35.0, 20.0);
        let d = dot(fwd, up);
        assert!(d.abs() < 1e-5, "forward and up must be orthogonal");
    }
}
