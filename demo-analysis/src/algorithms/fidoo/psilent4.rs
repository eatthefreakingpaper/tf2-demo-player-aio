use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::Error;
use serde_json::json;
use steamid_ng::SteamID;
use tf_demo_parser::demo::message::packetentities::{EntityId, UpdateType};
use tf_demo_parser::demo::message::Message;
use tf_demo_parser::demo::sendprop::SendPropIdentifier;
use tf_demo_parser::demo::vector::Vector;
use tf_demo_parser::{MessageType, ParserState};

use crate::base::cheat_analyser_base::{CheatAnalyserState, Player, PlayerState};
use crate::lib::algorithm::{CheatAlgorithm, Detection};
use crate::lib::parameters::{get_parameter_value, Parameter, Parameters};
use crate::util::helpers::{angle_delta, handle_to_entid, viewangle_delta};

const HISTORY_TICKS: u32 = 36;
const PIPE_VERTICAL_BOOST: f32 = 200.0;
const TELEPORT_THRESHOLD: f32 = 350.0;

// Standard and transition eye heights (ducking, crouch-jumping, standing)
const EYE_HEIGHTS: [f32; 6] = [45.0, 50.0, 56.0, 62.0, 68.0, 72.0];

// Candidate aim distances matching TF2 weapon engagement ranges and wall/floor convergence
const AIM_DISTANCES: [f32; 8] = [48.0, 64.0, 96.0, 128.0, 256.0, 512.0, 1024.0, 2000.0];

#[derive(Clone)]
struct PlayerSnapshot {
    tick: u32,
    position: Vector,
    yaw: f32,
    pitch: f32,
    simtime: u16,
    _ping: u16,
    in_pvs: bool,
    state: PlayerState,
}

impl PlayerSnapshot {
    fn new(tick: u32, player: &Player) -> Self {
        Self {
            tick,
            position: player.position,
            yaw: player.view_angle,
            pitch: player.pitch_angle,
            simtime: player.simtime,
            _ping: player.ping,
            in_pvs: player.in_pvs,
            state: player.state,
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

pub struct Psilent4 {
    histories: HashMap<u64, VecDeque<PlayerSnapshot>>,
    pending: Vec<LaunchCandidate>,
    tentative: Vec<TentativeDetection>,
    observations: HashMap<(u32, u32), u32>,
    params: Parameters,
}

impl Psilent4 {
    pub fn new() -> Self {
        Self {
            histories: HashMap::new(),
            pending: Vec::new(),
            tentative: Vec::new(),
            observations: HashMap::new(),
            params: HashMap::from([
                ("angle_threshold".to_string(), Parameter::Float(12.0)),
                ("lookaround_ticks".to_string(), Parameter::Int(4)),
                ("max_owner_distance".to_string(), Parameter::Float(105.0)),
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

            // Support players with 150-200ms latency (up to 20 ticks)
            let ping_ticks = ((candidate.player_ping as f32 / 15.0).round() as u32).min(20);
            let effective_lookaround = (lookaround_ticks + ping_ticks).max(16).min(32);

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

impl Default for Psilent4 {
    fn default() -> Self {
        Self::new()
    }
}

impl CheatAlgorithm<'_> for Psilent4 {
    fn default(&self) -> bool {
        true
    }

    fn algorithm_name(&self) -> &str {
        "fidoo/psilent4"
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
        let max_owner_distance = get_parameter_value::<f32>(&self.params, "max_owner_distance");

        for entity in &message.entities {
            if entity.update_type != UpdateType::Enter {
                continue;
            }
            let Some(class) = parser_state
                .server_classes
                .get(usize::from(entity.server_class))
                .map(|class| class.name.as_str())
                .filter(|class| supported_projectile(class))
            else {
                continue;
            };
            let props: Vec<_> = entity.props(parser_state).collect();
            let Some(owner_entity) = resolve_owner(&props, state) else {
                continue;
            };
            let Some(user_id) = state.get_userid_from_entid(owner_entity) else {
                continue;
            };
            let Some(steam_id) = state.get_id64_from_userid(user_id) else {
                continue;
            };
            let Some(player) = state
                .players
                .iter()
                .find(|player| player_id(player) == Some(steam_id))
            else {
                continue;
            };
            if !player.in_pvs || player.state != PlayerState::Alive {
                continue;
            }

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

            let initial_velocity = if class == "CTFProjectile_MechanicalArmOrb"
                || class == "CTFProjectile_BallOfFire"
            {
                let ang_rotation = projectile_vector(
                    &props,
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
                    Vector {
                        x: dir.x * speed,
                        y: dir.y * speed,
                        z: dir.z * speed,
                    }
                } else if let Some(vel) = projectile_vector(
                    &props,
                    &[
                        ("DT_TFBaseRocket", "m_vInitialVelocity"),
                        ("DT_TFWeaponBaseGrenadeProj", "m_vInitialVelocity"),
                    ],
                ) {
                    vel
                } else {
                    continue;
                }
            } else {
                let Some(vel) = projectile_vector(
                    &props,
                    &[
                        ("DT_TFBaseRocket", "m_vInitialVelocity"),
                        ("DT_TFWeaponBaseGrenadeProj", "m_vInitialVelocity"),
                    ],
                ) else {
                    continue;
                };
                vel
            };
            if projectile_integer(
                &props,
                &[
                    ("DT_TFBaseRocket", "m_iDeflected"),
                    ("DT_TFWeaponBaseGrenadeProj", "m_iDeflected"),
                ],
            )
            .is_some_and(|deflected| deflected > 0)
                || projectile_integer(&props, &[("DT_TFProjectile_Pipebomb", "m_bTouched")])
                    .is_some_and(|touched| touched > 0)
            {
                continue;
            }
            let pipe_type = projectile_integer(&props, &[("DT_TFProjectile_Pipebomb", "m_iType")]);
            let velocity = if arcing_projectile(class) && pipe_type != Some(1) {
                Vector {
                    z: initial_velocity.z - PIPE_VERTICAL_BOOST,
                    ..initial_velocity
                }
            } else {
                initial_velocity
            };
            let Some(launch_direction) = normalized(velocity) else {
                continue;
            };

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
                    // Entity re-entering PVS long after initial spawn (e.g. arrow stuck in wall), ignore
                    continue;
                }
            }
            let player_class = player.class_name();
            let weapon_name = state.get_player_weapon(player);
            self.record_snapshot(steam_id, PlayerSnapshot::new(tick, player));
            self.pending.push(LaunchCandidate {
                tick,
                player: steam_id,
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

        // Emit if minimum suspicious launches met OR if blatant high-delta single shot (>= 30.0 deg)
        let mut detections: Vec<_> = unique
            .into_values()
            .filter(|tentative| {
                let delta = detection_delta(&tentative.detection);
                delta >= 30.0
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
    if is_spread_rocket_launcher(&candidate.weapon_name) {
        return None;
    }

    let angle_threshold = get_parameter_value::<f32>(params, "angle_threshold");
    let lookaround_ticks = get_parameter_value::<i32>(params, "lookaround_ticks").max(0) as u32;
    let minimum_samples = get_parameter_value::<i32>(params, "minimum_samples").max(1) as usize;
    let history = histories.get(&candidate.player)?;
    let ping_ticks = ((candidate.player_ping as f32 / 15.0).round() as u32).min(20);
    let effective_lookaround = (lookaround_ticks + ping_ticks).max(16).min(32);
    let start_tick = candidate.tick.saturating_sub(effective_lookaround);
    let end_tick = candidate.tick.saturating_add(effective_lookaround);
    let samples: Vec<_> = history
        .iter()
        .filter(|sample| {
            (start_tick..=end_tick).contains(&sample.tick)
                && sample.in_pvs
                && sample.state == PlayerState::Alive
        })
        .collect();

    if samples.len() < minimum_samples
        || samples.first()?.tick > candidate.tick
        || samples.last()?.tick < candidate.tick
    {
        return None;
    }

    let simtimes: HashSet<_> = samples.iter().map(|sample| sample.simtime).collect();
    if simtimes.len() < 2 {
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
    let mut raw_delta = f32::INFINITY;
    let mut compensated_delta = f32::INFINITY;
    let mut best_view = (0.0, 0.0);
    let mut best_tick = candidate.tick;

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

    // Check if launch is a steep downward shot (ground rocket jump / feet shot)
    let is_downward_shot = candidate.launch_direction.z < -0.35;

    for (tick, pos, yaw, pitch) in test_views {
        let view = (yaw, pitch);
        raw_delta = raw_delta.min(angle_delta(view, launch_angles));

        for eye_height in EYE_HEIGHTS {
            if let Some(opt_angles) = optimal_muzzle_compensation(
                candidate.origin,
                candidate.launch_direction,
                pos,
                eye_height,
                yaw,
                pitch,
            ) {
                let delta = angle_delta(view, opt_angles);
                if delta < compensated_delta {
                    compensated_delta = delta;
                    best_view = view;
                    best_tick = tick;
                }
            }

            if (pitch > 35.0 || is_downward_shot) && candidate.launch_direction.z < -0.01 {
                let eye = Vector {
                    z: pos.z + eye_height,
                    ..pos
                };
                let ground_z = pos.z;
                let t_ground = (ground_z - candidate.origin.z) / candidate.launch_direction.z;
                if t_ground > 0.0 && t_ground < 1000.0 {
                    let impact = Vector {
                        x: candidate.origin.x + candidate.launch_direction.x * t_ground,
                        y: candidate.origin.y + candidate.launch_direction.y * t_ground,
                        z: ground_z,
                    };
                    let v_view = angles_to_vector(yaw, pitch);
                    if v_view.z < -0.01 {
                        let t_eye = (ground_z - eye.z) / v_view.z;
                        if t_eye > 0.0 {
                            let eye_impact = Vector {
                                x: eye.x + v_view.x * t_eye,
                                y: eye.y + v_view.y * t_eye,
                                z: ground_z,
                            };
                            let diff = impact - eye_impact;
                            if diff.x * diff.x + diff.y * diff.y <= 2500.0 {
                                if let Some(ground_dir) = normalized(impact - eye) {
                                    let ground_angles = vector_angles(ground_dir);
                                    let delta = angle_delta(view, ground_angles);
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
            }

            for &aim_distance in &AIM_DISTANCES {
                let Some(compensated_angles) = compensate_muzzle_offset(
                    candidate.origin,
                    candidate.launch_direction,
                    pos,
                    eye_height,
                    aim_distance,
                ) else {
                    continue;
                };
                let delta = angle_delta(view, compensated_angles);
                if delta < compensated_delta {
                    compensated_delta = delta;
                    best_view = view;
                    best_tick = tick;
                }

                let eye = Vector {
                    z: pos.z + eye_height,
                    ..pos
                };
                let v_view = angles_to_vector(yaw, pitch);
                let target = Vector {
                    x: eye.x + v_view.x * aim_distance,
                    y: eye.y + v_view.y * aim_distance,
                    z: eye.z + v_view.z * aim_distance,
                };
                if let Some(dir) = normalized(target - candidate.origin) {
                    let expected_angles = vector_angles(dir);
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

    // Dynamic threshold: for steep downward rocket jumping shots with high muzzle parallax, allow ground tolerance buffer
    let effective_threshold = if is_downward_shot && candidate.weapon_name.contains("Rocket") {
        angle_threshold + 6.0
    } else {
        angle_threshold
    };

    if !compensated_delta.is_finite() || compensated_delta < effective_threshold {
        return None;
    }

    Some(TentativeDetection {
        projectile_entity: candidate.projectile_entity,
        serial: candidate.serial,
        detection: Detection {
            tick: candidate.tick,
            algorithm: "fidoo/psilent4".to_string(),
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
                "owner_distance": candidate.owner_distance,
                "pipe_type": candidate.pipe_type,
                "samples": samples.len(),
                "simtime_samples": simtimes.len(),
            }),
        },
    })
}

fn detection_delta(detection: &Detection) -> f64 {
    detection.data["compensated_angle_delta"]
        .as_f64()
        .unwrap_or_default()
}

fn is_spread_rocket_launcher(weapon_name: &str) -> bool {
    weapon_name == "Beggar's Bazooka"
        || weapon_name == "Air Strike"
        || weapon_name.contains("Beggar")
        || weapon_name.contains("Air Strike")
        || weapon_name.contains("Airstrike")
}

fn supported_projectile(class: &str) -> bool {
    (class == "CTFGrenadePipebombProjectile" || class.starts_with("CTFProjectile_"))
        && class != "CTFProjectile_SentryRocket"
}

fn arcing_projectile(class: &str) -> bool {
    matches!(
        class,
        "CTFGrenadePipebombProjectile"
            | "CTFProjectile_Cleaver"
            | "CTFProjectile_Jar"
            | "CTFProjectile_JarMilk"
            | "CTFProjectile_ThrowableBreadMonster"
    )
}

fn resolve_owner(
    props: &[tf_demo_parser::demo::sendprop::SendProp],
    state: &CheatAnalyserState,
) -> Option<EntityId> {
    const OWNER: SendPropIdentifier = SendPropIdentifier::new("DT_BaseEntity", "m_hOwnerEntity");
    const THROWER: SendPropIdentifier = SendPropIdentifier::new("DT_BaseGrenade", "m_hThrower");

    props
        .iter()
        .filter(|prop| matches!(prop.identifier, OWNER | THROWER))
        .filter_map(|prop| i64::try_from(&prop.value).ok())
        .map(|handle| handle_to_entid(handle as u32))
        .find(|entity| state.get_userid_from_entid(*entity).is_some())
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
    let info = player.info.as_ref()?;
    if info.steam_id == "BOT" {
        return None;
    }
    SteamID::from_steam3(&info.steam_id).ok().map(u64::from)
}

fn optimal_muzzle_compensation(
    origin: Vector,
    direction: Vector,
    player_origin: Vector,
    eye_height: f32,
    view_yaw: f32,
    view_pitch: f32,
) -> Option<(f32, f32)> {
    let eye = Vector {
        z: player_origin.z + eye_height,
        ..player_origin
    };
    let v_view = angles_to_vector(view_yaw, view_pitch);
    let x = origin - eye;
    let v_dot_d = dot(v_view, direction);
    let denom = 1.0 - v_dot_d * v_dot_d;
    if denom.abs() < 1e-6 {
        return None;
    }
    let v_dot_x = dot(v_view, x);
    let x_dot_d = dot(x, direction);
    let t = (v_dot_x * v_dot_d - x_dot_d) / denom;
    if t <= 0.0 || t > 4000.0 {
        return None;
    }
    let s = v_dot_x + t * v_dot_d;
    if s <= 0.0 {
        return None;
    }
    let p_proj = Vector {
        x: origin.x + direction.x * t,
        y: origin.y + direction.y * t,
        z: origin.z + direction.z * t,
    };
    let p_eye = Vector {
        x: eye.x + v_view.x * s,
        y: eye.y + v_view.y * s,
        z: eye.z + v_view.z * s,
    };
    let diff = p_proj - p_eye;
    let miss_dist_sq = diff.x * diff.x + diff.y * diff.y + diff.z * diff.z;
    if miss_dist_sq > 400.0 {
        return None;
    }
    let target = p_proj - eye;
    normalized(target).map(vector_angles)
}

fn compensate_muzzle_offset(
    origin: Vector,
    direction: Vector,
    player_origin: Vector,
    eye_height: f32,
    aim_distance: f32,
) -> Option<(f32, f32)> {
    let eye = Vector {
        z: player_origin.z + eye_height,
        ..player_origin
    };
    let offset = origin - eye;
    let along = dot(offset, direction);
    let discriminant = along * along - (dot(offset, offset) - aim_distance * aim_distance);
    if discriminant < 0.0 {
        return None;
    }
    let travel = -along + discriminant.sqrt();
    if travel <= 0.0 {
        return None;
    }
    let target_direction = Vector {
        x: origin.x + direction.x * travel - eye.x,
        y: origin.y + direction.y * travel - eye.y,
        z: origin.z + direction.z * travel - eye.z,
    };
    normalized(target_direction).map(vector_angles)
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
