use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::Error;
use serde_json::json;
use steamid_ng::SteamID;
use tf_demo_parser::demo::gameevent_gen::GameEvent;
use tf_demo_parser::demo::message::packetentities::{EntityId, UpdateType};
use tf_demo_parser::demo::message::Message;
use tf_demo_parser::demo::parser::analyser::Class;
use tf_demo_parser::demo::sendprop::SendPropIdentifier;
use tf_demo_parser::demo::vector::Vector;
use tf_demo_parser::{MessageType, ParserState};

use crate::base::cheat_analyser_base::{CheatAnalyserState, Player, PlayerState};
use crate::lib::algorithm::{CheatAlgorithm, Detection};
use crate::lib::parameters::{get_parameter_value, Parameter, Parameters};
use crate::util::helpers::handle_to_entid;
use crate::util::nocrex::jankguard::JankGuard;

const ATTACK_HISTORY_MAX_TICKS: u32 = 32;
const SNAPSHOT_HISTORY_MAX_TICKS: u32 = 64;

#[derive(Clone, Debug)]
struct PlayerSnapshot {
    tick: u32,
    position: Vector,
    view_angle: f32, // yaw
    velocity: Vector,
}

#[derive(Clone, Debug, Default)]
struct KnifeInfo {
    owner_entity: Option<EntityId>,
    ready_to_backstab: bool,
    last_update_tick: u32,
}

#[derive(Clone, Debug, Default)]
struct SpyBackstabTracker {
    ready_to_backstab: bool,
    ready_start_tick: Option<u32>,
    last_closed_window: Option<(u32, u32)>, // (start_tick, end_tick)
    recent_attacks: VecDeque<u32>,
}

impl SpyBackstabTracker {
    fn record_attack(&mut self, tick: u32) {
        if self.recent_attacks.back().copied() != Some(tick) {
            self.recent_attacks.push_back(tick);
        }
        while self
            .recent_attacks
            .front()
            .is_some_and(|&t| tick.saturating_sub(t) > ATTACK_HISTORY_MAX_TICKS)
        {
            self.recent_attacks.pop_front();
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
struct BackstabRecord {
    tick: u32,
    player: u64,
    reaction_ticks: Option<u32>,
    reaction_time_ms: Option<f32>,
    window_duration_ticks: Option<u32>,
    was_preswinging: bool,
    had_prior_tracking: bool,
    victim_was_moving: bool,
    approach_quality: f32,
    victim_id: u32,
    victim_class: &'static str,
    victim_pos: [f32; 3],
    spy_pos: [f32; 3],
    dist2d: f32,
    dist3d: f32,
    angle_diff: f32,
    aim_fov: f32,
    flick_2tick: f32,
    damage: u16,
    is_instant_trigger: bool,
    is_fleeting_exploit: bool,
    is_snap_backstab: bool,
    is_impossible_angle: bool,
    is_teleport_distance: bool,
    is_blind_triggerbot: bool,
    data: serde_json::Value,
}

pub struct AutoBackstab {
    params: Parameters,
    jg: JankGuard,
    spy_states: HashMap<u64, SpyBackstabTracker>,
    knife_entities: HashMap<EntityId, KnifeInfo>,
    player_histories: HashMap<u64, VecDeque<PlayerSnapshot>>,
    backstab_records: HashMap<u64, Vec<BackstabRecord>>,
    logged_detections: HashSet<(u64, u32)>,
    detections: Vec<Detection>,
}

impl Default for AutoBackstab {
    fn default() -> Self {
        Self::new()
    }
}

impl AutoBackstab {
    pub fn new() -> Self {
        Self {
            params: HashMap::from([
                ("max_instant_trigger_ticks".to_string(), Parameter::Int(1)),
                ("fleeting_window_max_ticks".to_string(), Parameter::Int(3)),
                ("fleeting_reaction_max_ticks".to_string(), Parameter::Int(2)),
                ("min_stabs_aggregate".to_string(), Parameter::Int(3)),
                ("max_mean_reaction_ticks".to_string(), Parameter::Float(1.5)),
                ("max_std_dev_ticks".to_string(), Parameter::Float(0.8)),
                ("min_tracking_ticks".to_string(), Parameter::Int(4)),
                ("tracking_fov_threshold".to_string(), Parameter::Float(60.0)),
                ("tracking_lookback_ticks".to_string(), Parameter::Int(15)),
                ("aimbot_snap_threshold".to_string(), Parameter::Float(40.0)),
                ("teleport_distance_threshold".to_string(), Parameter::Float(180.0)),
                ("max_teleport_distance".to_string(), Parameter::Float(600.0)),
                ("impossible_angle_threshold".to_string(), Parameter::Float(115.0)),
                ("blind_fov_threshold".to_string(), Parameter::Float(85.0)),
                ("preswing_check_ticks".to_string(), Parameter::Int(3)),
            ]),
            jg: JankGuard::default(),
            spy_states: HashMap::new(),
            knife_entities: HashMap::new(),
            player_histories: HashMap::new(),
            backstab_records: HashMap::new(),
            logged_detections: HashSet::new(),
            detections: Vec::new(),
        }
    }

    fn update_spy_ready_state(&mut self, sid: u64, is_ready: bool, tick: u32) {
        let tracker = self.spy_states.entry(sid).or_default();
        if is_ready && !tracker.ready_to_backstab {
            tracker.ready_to_backstab = true;
            tracker.ready_start_tick = Some(tick);
        } else if !is_ready && tracker.ready_to_backstab {
            if let Some(t_ready) = tracker.ready_start_tick {
                tracker.last_closed_window = Some((t_ready, tick));
            }
            tracker.ready_to_backstab = false;
            tracker.ready_start_tick = None;
        }
    }

    fn reset_spy_state(&mut self, sid: u64) {
        if let Some(tracker) = self.spy_states.get_mut(&sid) {
            tracker.ready_to_backstab = false;
            tracker.ready_start_tick = None;
            tracker.last_closed_window = None;
            tracker.recent_attacks.clear();
        }
    }

    fn record_player_snapshots(&mut self, state: &CheatAnalyserState) {
        let tick = u32::from(state.tick);
        for player in &state.players {
            if !player.in_pvs || player.state != PlayerState::Alive {
                continue;
            }
            let Some(sid) = player_id(player) else {
                continue;
            };

            let velocity = self
                .player_histories
                .get(&sid)
                .and_then(|history| {
                    history.iter().rev().nth(0).map(|prev| {
                        let dt = (tick.saturating_sub(prev.tick)) as f32 * 0.015;
                        if dt > 0.001 {
                            Vector {
                                x: (player.position.x - prev.position.x) / dt,
                                y: (player.position.y - prev.position.y) / dt,
                                z: (player.position.z - prev.position.z) / dt,
                            }
                        } else {
                            Vector { x: 0.0, y: 0.0, z: 0.0 }
                        }
                    })
                })
                .unwrap_or(Vector { x: 0.0, y: 0.0, z: 0.0 });

            let snapshot = PlayerSnapshot {
                tick,
                position: player.position,
                view_angle: player.view_angle,
                velocity,
            };

            let history = self.player_histories.entry(sid).or_default();
            if history.back().is_some_and(|s| s.tick == tick) {
                history.pop_back();
            }
            history.push_back(snapshot);
        }

        self.player_histories.retain(|_, history| {
            while history
                .front()
                .is_some_and(|s| tick.saturating_sub(s.tick) > SNAPSHOT_HISTORY_MAX_TICKS)
            {
                history.pop_front();
            }
            !history.is_empty()
        });
    }

    /// Check if the Spy maintained visual tracking of the victim prior to the attack
    fn check_prior_tracking(
        &self,
        spy_sid: u64,
        victim_sid: u64,
        t_ref: u32,
    ) -> (bool, f32, f32) {
        let Some(spy_history) = self.player_histories.get(&spy_sid) else {
            return (false, 0.0, 0.0);
        };
        let Some(victim_history) = self.player_histories.get(&victim_sid) else {
            return (false, 0.0, 0.0);
        };

        let lookback_ticks =
            get_parameter_value::<i32>(&self.params, "tracking_lookback_ticks").max(5) as u32;
        let fov_threshold =
            get_parameter_value::<f32>(&self.params, "tracking_fov_threshold");
        let min_tracking =
            get_parameter_value::<i32>(&self.params, "min_tracking_ticks").max(1) as u32;

        let start_tick = t_ref.saturating_sub(lookback_ticks);
        let end_tick = t_ref.saturating_sub(1);

        if end_tick < start_tick {
            return (false, 0.0, 0.0);
        }

        let mut valid_tracking_ticks = 0u32;
        let mut total_ticks = 0u32;
        let mut sum_fov = 0.0f32;
        let mut last_fov = 0.0f32;

        for tick in start_tick..=end_tick {
            let spy_snap = spy_history.iter().find(|s| s.tick == tick);
            let victim_snap = victim_history.iter().find(|s| s.tick == tick);

            if let (Some(spy), Some(victim)) = (spy_snap, victim_snap) {
                total_ticks += 1;
                let dx = victim.position.x - spy.position.x;
                let dy = victim.position.y - spy.position.y;
                let dist = (dx * dx + dy * dy).sqrt();

                if dist <= 300.0 {
                    let aim_yaw = dy.atan2(dx).to_degrees();
                    let aim_fov =
                        ((aim_yaw - spy.view_angle + 180.0).rem_euclid(360.0) - 180.0).abs();
                    sum_fov += aim_fov;
                    last_fov = aim_fov;

                    if aim_fov <= fov_threshold {
                        valid_tracking_ticks += 1;
                    }
                }
            }
        }

        let avg_fov = if total_ticks > 0 {
            sum_fov / total_ticks as f32
        } else {
            0.0
        };

        let had_recent_alignment = total_ticks > 0 && last_fov <= 45.0;
        let had_tracking = valid_tracking_ticks >= min_tracking && had_recent_alignment;
        (had_tracking, avg_fov, last_fov)
    }

    /// Check if the victim was moving in the 10 ticks before the attack
    fn check_victim_movement(&self, victim_sid: u64, t_attack: u32) -> bool {
        let Some(victim_history) = self.player_histories.get(&victim_sid) else {
            return false;
        };

        let start_tick = t_attack.saturating_sub(10);
        let mut total_speed = 0.0f32;
        let mut count = 0u32;

        for snap in victim_history.iter() {
            if snap.tick >= start_tick && snap.tick < t_attack {
                let speed = (snap.velocity.x.powi(2) + snap.velocity.y.powi(2)).sqrt();
                total_speed += speed;
                count += 1;
            }
        }

        if count > 0 {
            (total_speed / count as f32) > 40.0
        } else {
            false
        }
    }

    /// Detect abrupt aimbot flick / snap within 5 ticks of the attack
    fn detect_aimbot_snap(&self, spy_sid: u64, t_attack: u32) -> (bool, f32) {
        let Some(spy_history) = self.player_histories.get(&spy_sid) else {
            return (false, 0.0);
        };

        let snap_threshold = get_parameter_value::<f32>(&self.params, "aimbot_snap_threshold");
        let start_tick = t_attack.saturating_sub(5);

        let snaps: Vec<_> = spy_history
            .iter()
            .filter(|s| s.tick >= start_tick && s.tick <= t_attack)
            .collect();

        let mut max_flick = 0.0f32;

        for window in snaps.windows(2) {
            let angle_delta =
                ((window[1].view_angle - window[0].view_angle + 180.0).rem_euclid(360.0) - 180.0).abs();
            if angle_delta > max_flick {
                max_flick = angle_delta;
            }
        }

        (max_flick >= snap_threshold, max_flick)
    }

    /// Calculate approach quality score (0.0 = bot-like, 1.0 = human-like)
    fn calculate_approach_quality(
        &self,
        had_tracking: bool,
        victim_moving: bool,
        had_snap: bool,
        avg_fov: f32,
    ) -> f32 {
        let mut score = 0.0f32;
        if had_tracking {
            score += 0.4;
        }
        if victim_moving {
            score += 0.3;
        }
        if !had_snap {
            score += 0.2;
        }
        if avg_fov > 0.0 && avg_fov < 45.0 {
            score += 0.1;
        }
        score.clamp(0.0, 1.0)
    }
}

impl<'a> CheatAlgorithm<'a> for AutoBackstab {
    fn default(&self) -> bool {
        true
    }

    fn algorithm_name(&self) -> &str {
        "fidoo/auto_backstab"
    }

    fn params(&mut self) -> Option<&mut Parameters> {
        Some(&mut self.params)
    }

    fn handled_messages(&self) -> Result<Vec<MessageType>, bool> {
        let mut types = self.jg.handled_messages()?;
        if !types.contains(&MessageType::PacketEntities) {
            types.push(MessageType::PacketEntities);
        }
        if !types.contains(&MessageType::GameEvent) {
            types.push(MessageType::GameEvent);
        }
        types.sort_by_key(|a| format!("{:?}", a));
        types.dedup();
        Ok(types)
    }

    fn on_message(
        &mut self,
        message: &Message,
        state: &CheatAnalyserState,
        parser_state: &ParserState,
        tick: tf_demo_parser::demo::data::DemoTick,
    ) -> Result<Vec<Detection>, Error> {
        self.jg.on_message(message, state, parser_state, tick);
        let ticknum = u32::from(tick);
        let mut new_detections = Vec::new();

        match message {
            Message::PacketEntities(msg) => {
                for entity in &msg.entities {
                    if entity.update_type == UpdateType::Delete {
                        self.knife_entities.remove(&entity.entity_index);
                        continue;
                    }

                    let class_name: &str = parser_state
                        .server_classes
                        .get(usize::from(entity.server_class))
                        .map(|class| class.name.as_str())
                        .unwrap_or("");

                    let is_knife_class = class_name == "CTFKnife"
                        || class_name.contains("Knife")
                        || class_name == "CTFWeaponKnife";

                    let mut ready_prop = None;
                    let mut owner_prop = None;

                    for prop in entity.props(parser_state) {
                        if let Some((_, prop_name)) = prop.identifier.names() {
                            match prop_name.as_str() {
                                "m_bReadyToBackstab" => {
                                    if let Ok(val) = i64::try_from(&prop.value) {
                                        ready_prop = Some(val != 0);
                                    }
                                }
                                "m_hOwnerEntity" | "m_hOwner" => {
                                    if let Ok(val) = i64::try_from(&prop.value) {
                                        let handle = val as u32;
                                        let ent_id = handle_to_entid(handle);
                                        if u32::from(ent_id) != 0x7FF && u32::from(ent_id) != 0 {
                                            owner_prop = Some(ent_id);
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }

                    if is_knife_class || ready_prop.is_some() {
                        let knife_entry = self.knife_entities.entry(entity.entity_index).or_default();
                        if let Some(owner) = owner_prop {
                            knife_entry.owner_entity = Some(owner);
                        }
                        if let Some(ready) = ready_prop {
                            knife_entry.ready_to_backstab = ready;
                            knife_entry.last_update_tick = ticknum;

                            let owner_ent = knife_entry.owner_entity.or_else(|| {
                                state.players.iter().find_map(|p| {
                                    if p.active_weapon == Some(entity.entity_index) {
                                        Some(p.entity)
                                    } else {
                                        None
                                    }
                                })
                            });

                            if let Some(owner_entity_id) = owner_ent {
                                if let Some(uid) = state.get_userid_from_entid(owner_entity_id) {
                                    if let Some(sid) = state.get_id64_from_userid(uid) {
                                        self.update_spy_ready_state(sid, ready, ticknum);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Message::TempEntities(msg) => {
                for event in &msg.events {
                    let class = &parser_state.server_classes[usize::from(event.class_id)].name;
                    if matches!(class.as_str(), "CTEFireBullets" | "CTEPlayerAnimEvent") {
                        const BULLETS_PLAYER: SendPropIdentifier =
                            SendPropIdentifier::new("DT_TEFireBullets", "m_iPlayer");
                        const ANIM_PLAYER: SendPropIdentifier =
                            SendPropIdentifier::new("DT_TEPlayerAnimEvent", "m_hPlayer");

                        if let Some(prop) = event
                            .props
                            .iter()
                            .find(|p| matches!(p.identifier, BULLETS_PLAYER | ANIM_PLAYER))
                        {
                            if let Some(id64) = i64::try_from(&prop.value)
                                .ok()
                                .and_then(|id| id.try_into().ok())
                                .map(|id| handle_to_entid(id))
                                .and_then(|id| state.entid_to_userid.get(&id))
                                .and_then(|uid| state.userid_to_id64.get(uid))
                            {
                                let tracker = self.spy_states.entry(*id64).or_default();
                                tracker.record_attack(ticknum);
                            }
                        }
                    }
                }
            }
            Message::GameEvent(event_msg) => match &event_msg.event {
                GameEvent::PlayerSpawn(spawn) => {
                    if let Some(sid) = state.get_id64_from_userid(spawn.user_id.into()) {
                        self.reset_spy_state(sid);
                    }
                }
                GameEvent::PostInventoryApplication(app) => {
                    if let Some(sid) = state.get_id64_from_userid(app.user_id.into()) {
                        self.reset_spy_state(sid);
                    }
                }
                GameEvent::PlayerHurt(hurt) => {
                    let victim_uid = u32::from(hurt.user_id);
                    let attacker_uid = u32::from(hurt.attacker);
                    let is_crit = hurt.crit;
                    let weapon_id = hurt.weapon_id as u16;
                    let damage_amount = hurt.damage_amount;

                    if attacker_uid != victim_uid && damage_amount > 0 {
                        let mut attacker_sid = 0u64;
                        let mut victim_sid = 0u64;

                        for player in &state.players {
                            if let Some(info) = &player.info {
                                if u32::from(info.user_id) == attacker_uid {
                                    attacker_sid = SteamID::from_steam3(&info.steam_id)
                                        .map(u64::from)
                                        .unwrap_or(0);
                                }
                                if u32::from(info.user_id) == victim_uid {
                                    victim_sid = SteamID::from_steam3(&info.steam_id)
                                        .map(u64::from)
                                        .unwrap_or(0);
                                }
                            }
                        }

                        if attacker_sid != 0 && victim_sid != 0 {
                            let attacker_opt =
                                state.players.iter().find(|p| player_id(p) == Some(attacker_sid));
                            let victim_opt =
                                state.players.iter().find(|p| player_id(p) == Some(victim_sid));

                            if let (Some(attacker), Some(victim)) = (attacker_opt, victim_opt) {
                                if attacker.in_pvs && victim.in_pvs && !attacker.is_taunting() {
                                    let is_backstab = is_crit
                                        && weapon_id == 7
                                        && (damage_amount > 400
                                            || damage_amount as f32 >= victim.health as f32 * 5.5);

                                    if is_backstab {
                                        let attacker_pos = attacker.position;
                                        let victim_pos = victim.position;
                                        let dx = victim_pos.x - attacker_pos.x;
                                        let dy = victim_pos.y - attacker_pos.y;
                                        let dz = victim_pos.z - attacker_pos.z;
                                        let dist2d = (dx * dx + dy * dy).sqrt();
                                        let dist3d = (dx * dx + dy * dy + dz * dz).sqrt();

                                        let aim_yaw = dy.atan2(dx).to_degrees();
                                        let aim_fov = ((aim_yaw - attacker.view_angle + 180.0)
                                            .rem_euclid(360.0)
                                            - 180.0)
                                            .abs();

                                        let angle_diff = ((attacker.view_angle - victim.view_angle + 180.0)
                                            .rem_euclid(360.0)
                                            - 180.0)
                                            .abs();

                                        let tracker =
                                            self.spy_states.entry(attacker_sid).or_default();
                                        let t_attack = ticknum;

                                        let (t_ready_opt, window_dur_opt) =
                                            if let Some(t_r) = tracker.ready_start_tick {
                                                let dur = t_attack.saturating_sub(t_r) + 1;
                                                (Some(t_r), Some(dur))
                                            } else if let Some((t_r, t_closed)) =
                                                tracker.last_closed_window
                                            {
                                                if t_attack.saturating_sub(t_closed) <= 2 {
                                                    let dur = t_closed.saturating_sub(t_r) + 1;
                                                    (Some(t_r), Some(dur))
                                                } else {
                                                    (None, None)
                                                }
                                            } else {
                                                (None, None)
                                            };

                                        // Consume closed window once matched
                                        tracker.last_closed_window = None;

                                        let reaction_ticks =
                                            t_ready_opt.map(|t_r| t_attack.saturating_sub(t_r));
                                        let reaction_time_ms =
                                            reaction_ticks.map(|r| r as f32 * 15.0);

                                        let preswing_check_ticks = get_parameter_value::<i32>(
                                            &self.params,
                                            "preswing_check_ticks",
                                        )
                                        .max(0) as u32;

                                        let was_preswinging = if let Some(t_r) = t_ready_opt {
                                            tracker.recent_attacks.iter().any(|&atk_tick| {
                                                atk_tick >= t_r.saturating_sub(preswing_check_ticks)
                                                    && atk_tick < t_r
                                            })
                                        } else {
                                            false
                                        };

                                        let t_ref = t_ready_opt.unwrap_or(t_attack);
                                        let (had_tracking, avg_fov, _last_fov) =
                                            self.check_prior_tracking(
                                                attacker_sid,
                                                victim_sid,
                                                t_ref,
                                            );
                                        let victim_moving =
                                            self.check_victim_movement(victim_sid, t_attack);
                                        let (had_snap, flick_2tick) =
                                            self.detect_aimbot_snap(attacker_sid, t_attack);
                                        let approach_quality = self.calculate_approach_quality(
                                            had_tracking,
                                            victim_moving,
                                            had_snap,
                                            avg_fov,
                                        );

                                        let max_instant = get_parameter_value::<i32>(
                                            &self.params,
                                            "max_instant_trigger_ticks",
                                        )
                                        .max(0) as u32;
                                        let fleeting_max = get_parameter_value::<i32>(
                                            &self.params,
                                            "fleeting_window_max_ticks",
                                        )
                                        .max(0) as u32;
                                        let fleeting_reaction_max = get_parameter_value::<i32>(
                                            &self.params,
                                            "fleeting_reaction_max_ticks",
                                        )
                                        .max(0) as u32;
                                        let teleport_dist_thresh = get_parameter_value::<f32>(
                                            &self.params,
                                            "teleport_distance_threshold",
                                        );
                                        let max_teleport_dist = get_parameter_value::<f32>(
                                            &self.params,
                                            "max_teleport_distance",
                                        );
                                        let imp_angle_thresh = get_parameter_value::<f32>(
                                            &self.params,
                                            "impossible_angle_threshold",
                                        );
                                        let blind_fov_thresh = get_parameter_value::<f32>(
                                            &self.params,
                                            "blind_fov_threshold",
                                        );

                                        // 1. Instant trigger strike: Verified readiness window <= max_instant without prior tracking
                                        let is_instant_trigger = reaction_ticks.is_some_and(|r| {
                                            r <= max_instant
                                                && !was_preswinging
                                                && !had_tracking
                                                && approach_quality < 0.3
                                        });

                                        // 2. Fleeting window exploit: Brief window <= fleeting_max with fast attack and no prior tracking
                                        let is_fleeting_exploit = match (reaction_ticks, window_dur_opt) {
                                            (Some(r), Some(w)) => {
                                                w <= fleeting_max
                                                    && r <= fleeting_reaction_max
                                                    && !was_preswinging
                                                    && !had_tracking
                                                    && approach_quality < 0.3
                                            }
                                            _ => false,
                                        };

                                        // 3. Aimbot snap backstab: Large flick > 40 deg leading into backstab without tracking, or > 45 deg snap to impossible angle
                                        let is_snap_backstab = (had_snap && !had_tracking && approach_quality < 0.35)
                                            || (flick_2tick >= 45.0 && aim_fov <= 35.0 && angle_diff >= 75.0 && !had_tracking && !was_preswinging);

                                        // 4. Impossible angle backstab: Attacker and victim facing each other in real-time (> 115 deg)
                                        let is_impossible_angle = angle_diff > imp_angle_thresh && !was_preswinging;

                                        // 5. Backtrack Tele-stab distance: Attacker landed backstab beyond valid melee reach (180 - 600 HU)
                                        let is_teleport_distance = dist2d > teleport_dist_thresh
                                            && dist2d < max_teleport_dist
                                            && dist3d < max_teleport_dist
                                            && !was_preswinging;

                                        // 6. Blind / High-FOV Triggerbot: Spy landed stab while facing completely away from target (> 85 deg) at an anomalous angle (> 90 deg) without prior tracking
                                        let is_blind_triggerbot = aim_fov > blind_fov_thresh
                                            && angle_diff > 90.0
                                            && !had_tracking
                                            && dist2d < 200.0
                                            && !was_preswinging;

                                        let victim_class_str = format_class_name(victim.class);

                                        let data_json = json!({
                                            "reaction_ticks": reaction_ticks.unwrap_or(0),
                                            "reaction_time_ms": reaction_time_ms.map(|ms| (ms * 10.0).round() / 10.0).unwrap_or(0.0),
                                            "window_duration_ticks": window_dur_opt.unwrap_or(1),
                                            "was_preswinging": was_preswinging,
                                            "had_prior_tracking": had_tracking,
                                            "victim_was_moving": victim_moving,
                                            "had_aimbot_snap": had_snap,
                                            "flick_2tick": (flick_2tick * 10.0).round() / 10.0,
                                            "aim_fov": (aim_fov * 10.0).round() / 10.0,
                                            "angle_diff": (angle_diff * 10.0).round() / 10.0,
                                            "approach_quality": (approach_quality * 100.0).round() / 100.0,
                                            "victim_id": victim_uid,
                                            "victim_class": victim_class_str,
                                            "victim_pos": [
                                                (victim_pos.x * 10.0).round() / 10.0,
                                                (victim_pos.y * 10.0).round() / 10.0,
                                                (victim_pos.z * 10.0).round() / 10.0
                                            ],
                                            "spy_pos": [
                                                (attacker_pos.x * 10.0).round() / 10.0,
                                                (attacker_pos.y * 10.0).round() / 10.0,
                                                (attacker_pos.z * 10.0).round() / 10.0
                                            ],
                                            "dist2d": (dist2d * 10.0).round() / 10.0,
                                            "distance": (dist3d * 10.0).round() / 10.0,
                                            "damage": damage_amount
                                        });

                                        let record = BackstabRecord {
                                            tick: t_attack,
                                            player: attacker_sid,
                                            reaction_ticks,
                                            reaction_time_ms,
                                            window_duration_ticks: window_dur_opt,
                                            was_preswinging,
                                            had_prior_tracking: had_tracking,
                                            victim_was_moving: victim_moving,
                                            approach_quality,
                                            victim_id: victim_uid,
                                            victim_class: victim_class_str,
                                            victim_pos: [victim_pos.x, victim_pos.y, victim_pos.z],
                                            spy_pos: [attacker_pos.x, attacker_pos.y, attacker_pos.z],
                                            dist2d,
                                            dist3d,
                                            angle_diff,
                                            aim_fov,
                                            flick_2tick,
                                            damage: damage_amount,
                                            is_instant_trigger,
                                            is_fleeting_exploit,
                                            is_snap_backstab,
                                            is_impossible_angle,
                                            is_teleport_distance,
                                            is_blind_triggerbot,
                                            data: data_json.clone(),
                                        };

                                        self.backstab_records
                                            .entry(attacker_sid)
                                            .or_default()
                                            .push(record);

                                        if is_instant_trigger
                                            || is_fleeting_exploit
                                            || is_snap_backstab
                                            || is_impossible_angle
                                            || is_teleport_distance
                                            || is_blind_triggerbot
                                        {
                                            if self.logged_detections.insert((attacker_sid, t_attack)) {
                                                let detection = Detection {
                                                    tick: t_attack,
                                                    algorithm: self.algorithm_name().to_string(),
                                                    player: attacker_sid,
                                                    data: data_json,
                                                };
                                                self.detections.push(detection.clone());
                                                new_detections.push(detection);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }

        Ok(new_detections)
    }

    fn on_tick(
        &mut self,
        state: &CheatAnalyserState,
        _: &ParserState,
    ) -> Result<Vec<Detection>, Error> {
        self.jg.on_tick(state);
        self.record_player_snapshots(state);
        let ticknum = u32::from(state.tick);

        for player in &state.players {
            if !player.in_pvs || player.state != PlayerState::Alive || player.class != Class::Spy {
                continue;
            }
            let Some(sid) = player_id(player) else {
                continue;
            };

            if let Some(active_weapon_ent) = player.active_weapon {
                let ready_state = self
                    .knife_entities
                    .get(&active_weapon_ent)
                    .map(|k| k.ready_to_backstab);
                if let Some(ready) = ready_state {
                    if let Some(knife_info) = self.knife_entities.get_mut(&active_weapon_ent) {
                        knife_info.owner_entity = Some(player.entity);
                    }
                    self.update_spy_ready_state(sid, ready, ticknum);
                }
            }

            if self.jg.fired(&sid, ticknum) == 0 {
                let tracker = self.spy_states.entry(sid).or_default();
                tracker.record_attack(ticknum);
            }
        }

        Ok(vec![])
    }

    fn finish(&mut self) -> Result<Vec<Detection>, Error> {
        let min_stabs_aggregate = get_parameter_value::<i32>(&self.params, "min_stabs_aggregate")
            .max(1) as usize;
        let max_mean_reaction =
            get_parameter_value::<f32>(&self.params, "max_mean_reaction_ticks");
        let max_std_dev = get_parameter_value::<f32>(&self.params, "max_std_dev_ticks");

        let mut aggregate_detections = Vec::new();

        for (&player_sid, records) in &self.backstab_records {
            let suspicious_records: Vec<_> = records
                .iter()
                .filter(|r| {
                    !r.was_preswinging
                        && !r.had_prior_tracking
                        && r.approach_quality < 0.35
                        && r.reaction_ticks.is_some()
                })
                .collect();

            let k = suspicious_records.len();

            if k >= min_stabs_aggregate {
                let sum_reaction: f32 = suspicious_records
                    .iter()
                    .map(|r| r.reaction_ticks.unwrap() as f32)
                    .sum();
                let mean_reaction = sum_reaction / k as f32;

                let variance: f32 = if k > 1 {
                    suspicious_records
                        .iter()
                        .map(|r| {
                            let diff = r.reaction_ticks.unwrap() as f32 - mean_reaction;
                            diff * diff
                        })
                        .sum::<f32>()
                        / (k - 1) as f32
                } else {
                    0.0
                };
                let std_dev = variance.sqrt();

                if mean_reaction <= max_mean_reaction && std_dev <= max_std_dev {
                    for record in suspicious_records {
                        if self.logged_detections.insert((player_sid, record.tick)) {
                            let detection = Detection {
                                tick: record.tick,
                                algorithm: self.algorithm_name().to_string(),
                                player: player_sid,
                                data: record.data.clone(),
                            };
                            aggregate_detections.push(detection);
                        }
                    }
                }
            }
        }

        aggregate_detections.sort_by_key(|d| (d.tick, d.player));
        Ok(aggregate_detections)
    }
}

fn player_id(player: &Player) -> Option<u64> {
    let info = player.info.as_ref()?;
    if info.steam_id == "BOT" {
        return None;
    }
    SteamID::from_steam3(&info.steam_id).ok().map(u64::from)
}

fn format_class_name(class: Class) -> &'static str {
    match class {
        Class::Scout => "Scout",
        Class::Sniper => "Sniper",
        Class::Soldier => "Soldier",
        Class::Demoman => "Demoman",
        Class::Medic => "Medic",
        Class::Heavy => "Heavy",
        Class::Pyro => "Pyro",
        Class::Spy => "Spy",
        Class::Engineer => "Engineer",
        Class::Other => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tf_demo_parser::demo::gameevent_gen::PlayerHurtEvent;
    use tf_demo_parser::demo::message::gameevent::GameEventMessage;
    use tf_demo_parser::demo::parser::analyser::{Team, UserId, UserInfo};

    fn create_test_player(
        entity_id: u32,
        user_id: u32,
        steam_id: &str,
        class: Class,
        pos: Vector,
        health: u16,
    ) -> Player {
        Player {
            entity: EntityId::from(entity_id),
            position: pos,
            health,
            max_health: health,
            class,
            team: Team::Blue,
            view_angle: 0.0,
            pitch_angle: 0.0,
            state: PlayerState::Alive,
            info: Some(UserInfo {
                classes: Default::default(),
                name: "TestSpy".to_string(),
                user_id: UserId::from(user_id as u16),
                steam_id: steam_id.to_string(),
                entity_id: EntityId::from(entity_id),
                team: Team::Blue,
            }),
            charge: 0,
            simtime: 0,
            ping: 20,
            in_pvs: true,
            active_weapon: None,
            cond: 0,
            cond_ex: 0,
            cond_ex2: 0,
            invis_change_complete_time: 0.0,
            flags: 1,
        }
    }

    fn create_hurt_message(event: PlayerHurtEvent) -> Message<'static> {
        Message::GameEvent(GameEventMessage {
            event: GameEvent::PlayerHurt(event),
            event_type_id: unsafe { std::mem::zeroed() },
            event_type: tf_demo_parser::demo::gameevent_gen::GameEventType::PlayerHurt,
        })
    }

    #[test]
    fn test_instant_triggerbot_without_tracking() {
        let mut algo = AutoBackstab::new();
        let mut state = CheatAnalyserState::default();

        let spy_steam = "[U:1:12345678]";
        let medic_steam = "[U:1:87654321]";
        let spy_sid = u64::from(SteamID::from_steam3(spy_steam).unwrap());
        let medic_sid = u64::from(SteamID::from_steam3(medic_steam).unwrap());

        let spy = create_test_player(1, 1, spy_steam, Class::Spy, Vector { x: 890.0, y: -320.0, z: 128.0 }, 125);
        let medic = create_test_player(2, 6, medic_steam, Class::Medic, Vector { x: 840.5, y: -310.2, z: 128.0 }, 150);

        state.players = vec![spy, medic];
        state.set_entid_to_userid(EntityId::from(1u32), UserId::from(1u16));
        state.set_entid_to_userid(EntityId::from(2u32), UserId::from(6u16));
        state.set_userid_to_id64(UserId::from(1u16), spy_sid);
        state.set_userid_to_id64(UserId::from(6u16), medic_sid);

        let parser_state = ParserState::new(24, |_| false, false);

        algo.update_spy_ready_state(spy_sid, true, 24529);

        let hurt_event = PlayerHurtEvent {
            user_id: 6,
            attacker: 1,
            health: 0,
            damage_amount: 900,
            crit: true,
            weapon_id: 7,
            bonus_effect: 0,
            custom: 0,
            show_disguised_crit: false,
            mini_crit: false,
            all_see_crit: false,
        };

        let msg = create_hurt_message(hurt_event);

        let detections = algo.on_message(&msg, &state, &parser_state, 24530.into()).unwrap();
        assert_eq!(detections.len(), 1);
        assert_eq!(detections[0].algorithm, "fidoo/auto_backstab");
        assert_eq!(detections[0].player, spy_sid);
        assert_eq!(detections[0].data["reaction_ticks"], 1);
        assert_eq!(detections[0].data["had_prior_tracking"], false);
    }

    #[test]
    fn test_predictable_stalk_approach_no_detection() {
        let mut algo = AutoBackstab::new();
        let mut state = CheatAnalyserState::default();

        let spy_steam = "[U:1:12345678]";
        let pyro_steam = "[U:1:87654321]";
        let spy_sid = u64::from(SteamID::from_steam3(spy_steam).unwrap());
        let pyro_sid = u64::from(SteamID::from_steam3(pyro_steam).unwrap());

        let parser_state = ParserState::new(24, |_| false, false);

        for tick in 19295..=19310 {
            state.tick = tick.into();
            let spy_x = -200.0 + (tick - 19295) as f32 * 10.0;
            let mut spy = create_test_player(1, 1, spy_steam, Class::Spy, Vector { x: spy_x, y: 0.0, z: 0.0 }, 125);
            spy.view_angle = 0.0;
            let mut pyro = create_test_player(2, 6, pyro_steam, Class::Pyro, Vector { x: 0.0, y: 0.0, z: 0.0 }, 175);
            pyro.view_angle = 0.0;

            state.players = vec![spy, pyro];
            state.set_entid_to_userid(EntityId::from(1u32), UserId::from(1u16));
            state.set_entid_to_userid(EntityId::from(2u32), UserId::from(6u16));
            state.set_userid_to_id64(UserId::from(1u16), spy_sid);
            state.set_userid_to_id64(UserId::from(6u16), pyro_sid);

            algo.on_tick(&state, &parser_state).unwrap();
        }

        algo.update_spy_ready_state(spy_sid, true, 19311);

        let hurt_event = PlayerHurtEvent {
            user_id: 6,
            attacker: 1,
            health: 0,
            damage_amount: 1026,
            crit: true,
            weapon_id: 7,
            bonus_effect: 0,
            custom: 0,
            show_disguised_crit: false,
            mini_crit: false,
            all_see_crit: false,
        };

        let msg = create_hurt_message(hurt_event);

        let detections = algo.on_message(&msg, &state, &parser_state, 19311.into()).unwrap();
        assert_eq!(detections.len(), 0);
    }

    #[test]
    fn test_preswing_exclusion() {
        let mut algo = AutoBackstab::new();
        let mut state = CheatAnalyserState::default();

        let spy_steam = "[U:1:12345678]";
        let medic_steam = "[U:1:87654321]";
        let spy_sid = u64::from(SteamID::from_steam3(spy_steam).unwrap());
        let medic_sid = u64::from(SteamID::from_steam3(medic_steam).unwrap());

        let spy = create_test_player(1, 1, spy_steam, Class::Spy, Vector { x: 890.0, y: -320.0, z: 128.0 }, 125);
        let medic = create_test_player(2, 6, medic_steam, Class::Medic, Vector { x: 840.5, y: -310.2, z: 128.0 }, 150);

        state.players = vec![spy, medic];
        state.set_entid_to_userid(EntityId::from(1u32), UserId::from(1u16));
        state.set_entid_to_userid(EntityId::from(2u32), UserId::from(6u16));
        state.set_userid_to_id64(UserId::from(1u16), spy_sid);
        state.set_userid_to_id64(UserId::from(6u16), medic_sid);

        let parser_state = ParserState::new(24, |_| false, false);

        let tracker = algo.spy_states.entry(spy_sid).or_default();
        tracker.record_attack(24528);

        algo.update_spy_ready_state(spy_sid, true, 24529);

        let hurt_event = PlayerHurtEvent {
            user_id: 6,
            attacker: 1,
            health: 0,
            damage_amount: 900,
            crit: true,
            weapon_id: 7,
            bonus_effect: 0,
            custom: 0,
            show_disguised_crit: false,
            mini_crit: false,
            all_see_crit: false,
        };

        let msg = create_hurt_message(hurt_event);

        let detections = algo.on_message(&msg, &state, &parser_state, 24529.into()).unwrap();
        assert_eq!(detections.len(), 0);
    }

    #[test]
    fn test_human_reaction_no_instant_detection() {
        let mut algo = AutoBackstab::new();
        let mut state = CheatAnalyserState::default();

        let spy_steam = "[U:1:12345678]";
        let sniper_steam = "[U:1:87654321]";
        let spy_sid = u64::from(SteamID::from_steam3(spy_steam).unwrap());
        let sniper_sid = u64::from(SteamID::from_steam3(sniper_steam).unwrap());

        let mut spy = create_test_player(1, 1, spy_steam, Class::Spy, Vector { x: 890.0, y: -320.0, z: 128.0 }, 125);
        spy.view_angle = 180.0;
        let mut sniper = create_test_player(2, 6, sniper_steam, Class::Sniper, Vector { x: 840.5, y: -320.0, z: 128.0 }, 125);
        sniper.view_angle = 180.0;

        state.players = vec![spy, sniper];
        state.set_entid_to_userid(EntityId::from(1u32), UserId::from(1u16));
        state.set_entid_to_userid(EntityId::from(2u32), UserId::from(6u16));
        state.set_userid_to_id64(UserId::from(1u16), spy_sid);
        state.set_userid_to_id64(UserId::from(6u16), sniper_sid);

        let parser_state = ParserState::new(24, |_| false, false);

        algo.update_spy_ready_state(spy_sid, true, 24500);

        let hurt_event = PlayerHurtEvent {
            user_id: 6,
            attacker: 1,
            health: 0,
            damage_amount: 750,
            crit: true,
            weapon_id: 7,
            bonus_effect: 0,
            custom: 0,
            show_disguised_crit: false,
            mini_crit: false,
            all_see_crit: false,
        };

        let msg = create_hurt_message(hurt_event);

        let detections = algo.on_message(&msg, &state, &parser_state, 24512.into()).unwrap();
        assert_eq!(detections.len(), 0);

        let finish_detections = algo.finish().unwrap();
        assert_eq!(finish_detections.len(), 0);
    }

    #[test]
    fn test_teleport_distance_detection() {
        let mut algo = AutoBackstab::new();
        let mut state = CheatAnalyserState::default();

        let spy_steam = "[U:1:12345678]";
        let medic_steam = "[U:1:87654321]";
        let spy_sid = u64::from(SteamID::from_steam3(spy_steam).unwrap());
        let medic_sid = u64::from(SteamID::from_steam3(medic_steam).unwrap());

        let spy = create_test_player(1, 1, spy_steam, Class::Spy, Vector { x: 1000.0, y: 0.0, z: 0.0 }, 125);
        let medic = create_test_player(2, 6, medic_steam, Class::Medic, Vector { x: 1220.0, y: 0.0, z: 0.0 }, 150);

        state.players = vec![spy, medic];
        state.set_entid_to_userid(EntityId::from(1u32), UserId::from(1u16));
        state.set_entid_to_userid(EntityId::from(2u32), UserId::from(6u16));
        state.set_userid_to_id64(UserId::from(1u16), spy_sid);
        state.set_userid_to_id64(UserId::from(6u16), medic_sid);

        let parser_state = ParserState::new(24, |_| false, false);

        let hurt_event = PlayerHurtEvent {
            user_id: 6,
            attacker: 1,
            health: 0,
            damage_amount: 900,
            crit: true,
            weapon_id: 7,
            bonus_effect: 0,
            custom: 0,
            show_disguised_crit: false,
            mini_crit: false,
            all_see_crit: false,
        };

        let msg = create_hurt_message(hurt_event);

        let detections = algo.on_message(&msg, &state, &parser_state, 1000.into()).unwrap();
        assert_eq!(detections.len(), 1);
        assert_eq!(detections[0].player, spy_sid);
    }
}
