use anyhow::Error;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use tf_demo_parser::demo::data::DemoTick;
use tf_demo_parser::demo::gameevent_gen::GameEvent;
use tf_demo_parser::demo::message::packetentities::{EntityId, UpdateType};
use tf_demo_parser::demo::message::Message;
use tf_demo_parser::demo::parser::analyser::Class;
use tf_demo_parser::{MessageType, ParserState};

use crate::base::cheat_analyser_base::{CheatAnalyserState, PlayerState};
use crate::lib::algorithm::{CheatAlgorithm, Detection};
use crate::lib::parameters::{get_parameter_value, Parameter, Parameters};
use crate::util::schema_equip_regions::{
    check_cosmetic_conflict, get_cosmetic_info, is_action_item, is_dummy_or_invalid,
    is_weapon_wearable,
};

#[derive(Debug, Clone)]
struct WearableEntityState {
    owner: EntityId,
    item_def_index: u16,
    account_id: u32,
    is_disguise: bool,
    nodraw: bool,
    last_seen_tick: u32,
    spawn_generation: u32,
}

#[derive(Clone, Copy)]
struct InvalidEquipHotParams {
    min_persistence_ticks: u32,
    check_cosmetic_limit: bool,
    ignore_quickswitch: bool,
    valve_servers_only: bool,
    ignore_preset_swap: bool,
}

impl Default for InvalidEquipHotParams {
    fn default() -> Self {
        Self {
            min_persistence_ticks: 600,
            check_cosmetic_limit: true,
            ignore_quickswitch: true,
            valve_servers_only: true,
            ignore_preset_swap: true,
        }
    }
}

impl InvalidEquipHotParams {
    fn from_params(params: &Parameters) -> Self {
        Self {
            min_persistence_ticks: get_parameter_value::<i32>(params, "min_persistence_ticks")
                .max(1) as u32,
            check_cosmetic_limit: get_parameter_value(params, "check_cosmetic_limit"),
            ignore_quickswitch: get_parameter_value(params, "ignore_quickswitch"),
            valve_servers_only: get_parameter_value(params, "valve_servers_only"),
            ignore_preset_swap: get_parameter_value(params, "ignore_preset_swap"),
        }
    }
}

#[derive(Debug, Clone, Default)]
struct CachedLoadout {
    valid: bool,
    wearable_revision: u64,
    generation: u32,
    account_id: u32,
    item_ids: Vec<u16>,
    conflicting_regions: Vec<&'static str>,
    has_conflict: bool,
    exceeds_limit: bool,
    preset_swap_candidate: bool,
}

fn evaluate_loadout(
    wearables: &HashMap<EntityId, WearableEntityState>,
    owner: EntityId,
    generation: u32,
    account_id: u32,
    wearable_revision: u64,
    check_cosmetic_limit: bool,
    ignore_quickswitch: bool,
) -> CachedLoadout {
    let mut item_ids: Vec<u16> = wearables
        .values()
        .filter(|wearable| {
            wearable.owner == owner
                && wearable.spawn_generation == generation
                && !wearable.nodraw
                && !wearable.is_disguise
                && (wearable.account_id == 0 || wearable.account_id == account_id)
                && !is_dummy_or_invalid(wearable.item_def_index)
                && !is_weapon_wearable(wearable.item_def_index)
                && !is_action_item(wearable.item_def_index)
                && get_cosmetic_info(wearable.item_def_index).is_some()
        })
        .map(|wearable| wearable.item_def_index)
        .collect();
    item_ids.sort_unstable();
    item_ids.dedup();

    let mut conflicting_regions = Vec::new();
    let mut has_conflict = false;
    for i in 0..item_ids.len() {
        for j in (i + 1)..item_ids.len() {
            let (Some(info_a), Some(info_b)) = (
                get_cosmetic_info(item_ids[i]),
                get_cosmetic_info(item_ids[j]),
            ) else {
                continue;
            };
            if let Some(conflicts) = check_cosmetic_conflict(info_a, info_b, ignore_quickswitch) {
                has_conflict = true;
                conflicting_regions.extend(conflicts);
            }
        }
    }
    conflicting_regions.sort_unstable();
    conflicting_regions.dedup();

    let hat_cosmetics_count = item_ids
        .iter()
        .filter(|item_id| {
            get_cosmetic_info(**item_id)
                .is_some_and(|info| info.regions.iter().any(|region| region == "hat"))
        })
        .count();
    let exceeds_limit = check_cosmetic_limit && item_ids.len() > 3;
    let preset_swap_candidate = has_conflict
        && item_ids.len() <= 3
        && conflicting_regions.len() == 1
        && conflicting_regions.contains(&"hat")
        && hat_cosmetics_count == 2;

    CachedLoadout {
        valid: true,
        wearable_revision,
        generation,
        account_id,
        item_ids,
        conflicting_regions,
        has_conflict,
        exceeds_limit,
        preset_swap_candidate,
    }
}

#[derive(Debug, Clone, Default)]
struct PlayerGenerationState {
    generation: u32,
    current_class: Class,
    conflict_ticks: u32,
    post_reset_conflict_ticks: u32,
    witnessed_in_pvs_reset: bool,
    reported_loadouts: HashSet<Vec<u16>>,
    cached_loadout: CachedLoadout,
}

pub struct InvalidEquipRegion {
    pub params: Parameters,
    wearables: HashMap<EntityId, WearableEntityState>,
    players: HashMap<EntityId, PlayerGenerationState>,
    current_tick: u32,
    pub server_name: String,
    wearables_revision: u64,
    hot_params: InvalidEquipHotParams,
}

impl Default for InvalidEquipRegion {
    fn default() -> Self {
        Self::new()
    }
}

impl InvalidEquipRegion {
    pub fn new() -> Self {
        Self {
            params: HashMap::from([
                ("min_persistence_ticks".to_string(), Parameter::Int(600)),
                ("check_cosmetic_limit".to_string(), Parameter::Bool(true)),
                ("ignore_quickswitch".to_string(), Parameter::Bool(true)),
                ("valve_servers_only".to_string(), Parameter::Bool(true)),
                ("ignore_preset_swap".to_string(), Parameter::Bool(true)),
            ]),
            wearables: HashMap::new(),
            players: HashMap::new(),
            current_tick: 0,
            server_name: String::new(),
            wearables_revision: 0,
            hot_params: InvalidEquipHotParams::default(),
        }
    }

    fn advance_generation(&mut self, player_ent: EntityId, in_pvs_reset: bool) {
        let p = self.players.entry(player_ent).or_default();
        p.generation += 1;
        p.conflict_ticks = 0;
        p.post_reset_conflict_ticks = 0;
        p.witnessed_in_pvs_reset = in_pvs_reset;
    }
}

impl<'a> CheatAlgorithm<'a> for InvalidEquipRegion {
    fn default(&self) -> bool {
        true
    }

    fn init(&mut self) -> Result<(), Error> {
        self.hot_params = InvalidEquipHotParams::from_params(&self.params);
        Ok(())
    }

    fn algorithm_name(&self) -> &str {
        "fidoo/invalid_equip_region"
    }

    fn params(&mut self) -> Option<&mut Parameters> {
        Some(&mut self.params)
    }

    fn handled_messages(&self) -> Result<Vec<MessageType>, bool> {
        Ok(vec![
            MessageType::PacketEntities,
            MessageType::NetTick,
            MessageType::GameEvent,
            MessageType::ServerInfo,
            MessageType::SetConVar,
        ])
    }

    fn on_message(
        &mut self,
        message: &Message,
        state: &CheatAnalyserState,
        pstate: &ParserState,
        tick: DemoTick,
    ) -> Result<Vec<Detection>, Error> {
        self.current_tick = u32::from(tick);

        match message {
            Message::ServerInfo(si) => {
                if self.server_name.is_empty() {
                    self.server_name = si.server_name.trim().to_string();
                }
            }
            Message::SetConVar(cvar) => {
                for cv in &cvar.vars {
                    if cv.key == "hostname" && self.server_name.is_empty() {
                        self.server_name = cv.value.trim().to_string();
                    }
                }
            }
            Message::GameEvent(event_msg) => match &event_msg.event {
                GameEvent::RoundStart(_) | GameEvent::TeamPlayRoundStart(_) => {
                    self.wearables.clear();
                    self.players.clear();
                    self.wearables_revision = self.wearables_revision.wrapping_add(1);
                }
                GameEvent::PlayerDeath(death) => {
                    let victim_uid = u32::from(death.user_id);
                    if let Some(player) = state.players.iter().find(|p| {
                        p.info
                            .as_ref()
                            .is_some_and(|info| u32::from(info.user_id) == victim_uid)
                    }) {
                        self.advance_generation(player.entity, false);
                    }
                }
                GameEvent::PlayerSpawn(spawn) => {
                    let spawn_uid = u32::from(spawn.user_id);
                    if let Some(player) = state.players.iter().find(|p| {
                        p.info
                            .as_ref()
                            .is_some_and(|info| u32::from(info.user_id) == spawn_uid)
                    }) {
                        let in_pvs = player.in_pvs;
                        self.advance_generation(player.entity, in_pvs);
                    }
                }
                GameEvent::PostInventoryApplication(inv) => {
                    let inv_uid = u32::from(inv.user_id);
                    if let Some(player) = state.players.iter().find(|p| {
                        p.info
                            .as_ref()
                            .is_some_and(|info| u32::from(info.user_id) == inv_uid)
                    }) {
                        let in_pvs = player.in_pvs;
                        let p = self.players.entry(player.entity).or_default();
                        p.conflict_ticks = 0;
                        p.post_reset_conflict_ticks = 0;
                        p.witnessed_in_pvs_reset = in_pvs;
                    }
                }
                _ => {}
            },
            Message::PacketEntities(msg) => {
                for removed in &msg.removed_entities {
                    if self.wearables.remove(removed).is_some() {
                        self.wearables_revision = self.wearables_revision.wrapping_add(1);
                    }
                }

                for entity in &msg.entities {
                    let class_name = pstate
                        .server_classes
                        .get(usize::from(entity.server_class))
                        .map(|c| c.name.as_str())
                        .unwrap_or("");

                    if entity.update_type == UpdateType::Delete {
                        if self.wearables.remove(&entity.entity_index).is_some() {
                            self.wearables_revision = self.wearables_revision.wrapping_add(1);
                        }
                        continue;
                    }

                    if class_name.contains("Wearable") {
                        let mut owner_ent = None;
                        let mut item_def = None;
                        let mut is_disguise_prop = None;
                        let mut nodraw_prop = None;
                        let mut account_id_prop = None;

                        for prop in state.entity_props(entity, pstate).iter() {
                            if let Some((_table, name)) = prop.identifier.names() {
                                match name.as_str() {
                                    "m_hOwnerEntity" | "m_hOwner" => {
                                        if let Ok(val) = i64::try_from(&prop.value) {
                                            let handle = val as u32;
                                            let ent = EntityId::from(handle & 0x7FF);
                                            if u32::from(ent) != 0x7FF && u32::from(ent) != 0 {
                                                owner_ent = Some(ent);
                                            } else {
                                                owner_ent = Some(EntityId::from(0u32));
                                            }
                                        }
                                    }
                                    "m_iItemDefinitionIndex" => {
                                        if let Ok(val) = i64::try_from(&prop.value) {
                                            item_def = Some(val as u16);
                                        }
                                    }
                                    "m_iAccountID" => {
                                        if let Ok(val) = i64::try_from(&prop.value) {
                                            account_id_prop = Some(val as u32);
                                        }
                                    }
                                    "m_bDisguiseWearable" => {
                                        if let Ok(val) = i64::try_from(&prop.value) {
                                            is_disguise_prop = Some(val > 0);
                                        }
                                    }
                                    "m_fEffects" => {
                                        if let Ok(val) = i64::try_from(&prop.value) {
                                            nodraw_prop = Some((val & 32) != 0);
                                            // EF_NODRAW
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }

                        let tick_u32 = self.current_tick;
                        let w = self
                            .wearables
                            .entry(entity.entity_index)
                            .or_insert_with(|| WearableEntityState {
                                owner: EntityId::from(0u32),
                                item_def_index: 0,
                                account_id: 0,
                                is_disguise: false,
                                nodraw: false,
                                last_seen_tick: tick_u32,
                                spawn_generation: 0,
                            });

                        if let Some(owner) = owner_ent {
                            if u32::from(owner) == 0 {
                                w.owner = EntityId::from(0u32);
                            } else {
                                let p_state = self.players.entry(owner).or_default();
                                if owner != w.owner || entity.update_type == UpdateType::Enter {
                                    w.owner = owner;
                                    w.spawn_generation = p_state.generation;
                                }
                            }
                        }
                        if let Some(def) = item_def {
                            w.item_def_index = def;
                        }
                        if let Some(acc) = account_id_prop {
                            w.account_id = acc;
                        }
                        if let Some(disguise) = is_disguise_prop {
                            w.is_disguise = disguise;
                        }
                        if let Some(nd) = nodraw_prop {
                            w.nodraw = nd;
                        }
                        w.last_seen_tick = tick_u32;
                        self.wearables_revision = self.wearables_revision.wrapping_add(1);
                    }
                }
            }
            Message::NetTick(_) => {
                let params = self.hot_params;
                let is_valve_server = self.server_name.contains("Valve Matchmaking Server");

                if params.valve_servers_only && !is_valve_server {
                    return Ok(vec![]);
                }

                let mut detections = Vec::new();

                for player in &state.players {
                    if player.state != PlayerState::Alive {
                        continue;
                    }

                    let Some(steam_id64) = player.steam_id() else {
                        continue;
                    };

                    let player_account_id = (steam_id64 & 0xFFFFFFFF) as u32;
                    let p_state = self.players.entry(player.entity).or_default();

                    // A class transition starts a fresh wearable generation.
                    if p_state.current_class != Class::Other
                        && p_state.current_class != player.class
                    {
                        p_state.generation += 1;
                        p_state.conflict_ticks = 0;
                        p_state.post_reset_conflict_ticks = 0;
                        p_state.witnessed_in_pvs_reset = player.in_pvs;
                    }
                    p_state.current_class = player.class;
                    let target_generation = p_state.generation;

                    let cache_stale = !p_state.cached_loadout.valid
                        || p_state.cached_loadout.wearable_revision != self.wearables_revision
                        || p_state.cached_loadout.generation != target_generation
                        || p_state.cached_loadout.account_id != player_account_id;
                    if cache_stale {
                        p_state.cached_loadout = evaluate_loadout(
                            &self.wearables,
                            player.entity,
                            target_generation,
                            player_account_id,
                            self.wearables_revision,
                            params.check_cosmetic_limit,
                            params.ignore_quickswitch,
                        );
                    }

                    let has_conflict = p_state.cached_loadout.has_conflict;
                    let exceeds_limit = p_state.cached_loadout.exceeds_limit;
                    let is_violating = has_conflict || exceeds_limit;

                    if is_violating {
                        p_state.conflict_ticks += 1;
                        if player.in_pvs && p_state.witnessed_in_pvs_reset {
                            p_state.post_reset_conflict_ticks += 1;
                        }

                        let preset_swap_candidate = p_state.cached_loadout.preset_swap_candidate;
                        let can_flag = if params.ignore_preset_swap && preset_swap_candidate {
                            p_state.witnessed_in_pvs_reset
                                && p_state.post_reset_conflict_ticks >= params.min_persistence_ticks
                        } else {
                            p_state.conflict_ticks >= params.min_persistence_ticks
                        };

                        if can_flag
                            && !p_state
                                .reported_loadouts
                                .contains(&p_state.cached_loadout.item_ids)
                        {
                            let loadout_key = p_state.cached_loadout.item_ids.clone();
                            let regions = p_state.cached_loadout.conflicting_regions.clone();
                            p_state.reported_loadouts.insert(loadout_key.clone());

                            let duration_ticks =
                                if params.ignore_preset_swap && preset_swap_candidate {
                                    p_state.post_reset_conflict_ticks
                                } else {
                                    p_state.conflict_ticks
                                };

                            let violation_type = if has_conflict && exceeds_limit {
                                "equip_region_conflict_and_excess_cosmetics"
                            } else if has_conflict {
                                "equip_region_conflict"
                            } else {
                                "excess_cosmetic_count"
                            };

                            let items_data: Vec<_> = loadout_key
                                .iter()
                                .filter_map(|item_id| {
                                    get_cosmetic_info(*item_id).map(|info| {
                                        json!({
                                            "id": item_id,
                                            "name": info.name,
                                            "regions": info.regions,
                                            "region_mask": format!("0x{:X}", info.region_mask),
                                        })
                                    })
                                })
                                .collect();

                            detections.push(Detection {
                                tick: self.current_tick,
                                algorithm: "fidoo/invalid_equip_region".to_string(),
                                player: steam_id64,
                                data: json!({
                                    "class": player.class_name(),
                                    "violation_type": violation_type,
                                    "conflicting_regions": regions,
                                    "total_cosmetics": loadout_key.len(),
                                    "cosmetics": items_data,
                                    "duration_ticks": duration_ticks,
                                    "valve_server": is_valve_server,
                                    "server_name": self.server_name,
                                }),
                            });
                        }
                    } else {
                        p_state.conflict_ticks = 0;
                        if player.in_pvs {
                            p_state.post_reset_conflict_ticks = 0;
                            p_state.witnessed_in_pvs_reset = false;
                        }
                    }
                }

                return Ok(detections);
            }
            _ => {}
        }

        Ok(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::base::cheat_analyser_base::Player;
    use tf_demo_parser::demo::message::NetTickMessage;

    #[test]
    fn test_valve_servers_only_filter_enabled_community() {
        let mut algo = InvalidEquipRegion::new();
        algo.server_name = "UGC.TF | Trade #11 | FREE ITEMS!".to_string();
        algo.params
            .insert("valve_servers_only".to_string(), Parameter::Bool(true));
        algo.init().unwrap();

        let state = CheatAnalyserState::default();
        let pstate = ParserState::new(24, |_| false, false);
        let msg = Message::NetTick(NetTickMessage {
            tick: 100.into(),
            frame_time: 15,
            std_dev: 0,
        });

        let res = algo
            .on_message(&msg, &state, &pstate, DemoTick::from(100))
            .unwrap();
        assert!(
            res.is_empty(),
            "Community server should be skipped when valve_servers_only is true"
        );
    }

    #[test]
    fn test_valve_servers_only_filter_disabled_community() {
        let mut algo = InvalidEquipRegion::new();
        algo.server_name = "UGC.TF | Trade #11 | FREE ITEMS!".to_string();
        algo.params
            .insert("valve_servers_only".to_string(), Parameter::Bool(false));
        algo.params
            .insert("min_persistence_ticks".to_string(), Parameter::Int(1));
        algo.params
            .insert("ignore_preset_swap".to_string(), Parameter::Bool(false));
        algo.init().unwrap();

        let player_ent = EntityId::from(10u32);
        let mut state = CheatAnalyserState::default();
        let mut p = Player::default();
        p.entity = player_ent;
        p.state = PlayerState::Alive;
        p.class = Class::Heavy;
        let mut uinfo = tf_demo_parser::demo::data::UserInfo::default();
        uinfo.player_info.steam_id = "[U:1:1]".to_string();
        p.info = Some(uinfo.into());
        state.players.push(p);

        algo.wearables.insert(
            EntityId::from(101u32),
            WearableEntityState {
                owner: player_ent,
                item_def_index: 378, // Team Captain (hat)
                account_id: 1,
                is_disguise: false,
                nodraw: false,
                last_seen_tick: 1,
                spawn_generation: 0,
            },
        );
        algo.wearables.insert(
            EntityId::from(102u32),
            WearableEntityState {
                owner: player_ent,
                item_def_index: 538, // Killer Exclusive (hat)
                account_id: 1,
                is_disguise: false,
                nodraw: false,
                last_seen_tick: 1,
                spawn_generation: 0,
            },
        );

        let pstate = ParserState::new(24, |_| false, false);
        let msg = Message::NetTick(NetTickMessage {
            tick: 1.into(),
            frame_time: 15,
            std_dev: 0,
        });

        let res = algo
            .on_message(&msg, &state, &pstate, DemoTick::from(1))
            .unwrap();
        assert_eq!(
            res.len(),
            1,
            "Community server should flag when valve_servers_only is false"
        );
        assert_eq!(res[0].data["valve_server"], false);
        assert_eq!(
            res[0].data["server_name"],
            "UGC.TF | Trade #11 | FREE ITEMS!"
        );
    }

    #[test]
    fn test_valve_servers_only_valve_server() {
        let mut algo = InvalidEquipRegion::new();
        algo.server_name = "Valve Matchmaking Server (Stockholm srcds2015-sto1 #69)".to_string();
        algo.params
            .insert("valve_servers_only".to_string(), Parameter::Bool(true));
        algo.params
            .insert("min_persistence_ticks".to_string(), Parameter::Int(1));
        algo.params
            .insert("ignore_preset_swap".to_string(), Parameter::Bool(false));
        algo.init().unwrap();

        let player_ent = EntityId::from(10u32);
        let mut state = CheatAnalyserState::default();
        let mut p = Player::default();
        p.entity = player_ent;
        p.state = PlayerState::Alive;
        p.class = Class::Heavy;
        let mut uinfo = tf_demo_parser::demo::data::UserInfo::default();
        uinfo.player_info.steam_id = "[U:1:1]".to_string();
        p.info = Some(uinfo.into());
        state.players.push(p);

        algo.wearables.insert(
            EntityId::from(101u32),
            WearableEntityState {
                owner: player_ent,
                item_def_index: 378, // Team Captain (hat)
                account_id: 1,
                is_disguise: false,
                nodraw: false,
                last_seen_tick: 1,
                spawn_generation: 0,
            },
        );
        algo.wearables.insert(
            EntityId::from(102u32),
            WearableEntityState {
                owner: player_ent,
                item_def_index: 538, // Killer Exclusive (hat)
                account_id: 1,
                is_disguise: false,
                nodraw: false,
                last_seen_tick: 1,
                spawn_generation: 0,
            },
        );

        let pstate = ParserState::new(24, |_| false, false);
        let msg = Message::NetTick(NetTickMessage {
            tick: 1.into(),
            frame_time: 15,
            std_dev: 0,
        });

        let res = algo
            .on_message(&msg, &state, &pstate, DemoTick::from(1))
            .unwrap();
        assert_eq!(
            res.len(),
            1,
            "Valve server should flag when valve_servers_only is true"
        );
        assert_eq!(res[0].data["valve_server"], true);
        assert_eq!(
            res[0].data["server_name"],
            "Valve Matchmaking Server (Stockholm srcds2015-sto1 #69)"
        );
    }

    #[test]
    fn test_account_id_filtering() {
        let mut algo = InvalidEquipRegion::new();
        algo.server_name = "Valve Matchmaking Server (test)".to_string();
        algo.params
            .insert("min_persistence_ticks".to_string(), Parameter::Int(1));
        algo.init().unwrap();

        let player_ent = EntityId::from(13u32);
        let steam_id64 = 76561199232172054u64; // account_id = 1271906326
        let player_account_id = (steam_id64 & 0xFFFFFFFF) as u32;

        let mut state = CheatAnalyserState::default();
        let mut p = Player::default();
        p.entity = player_ent;
        p.state = PlayerState::Alive;
        p.class = Class::Pyro;
        let mut uinfo = tf_demo_parser::demo::data::UserInfo::default();
        uinfo.player_info.steam_id = "[U:1:1271906326]".to_string();
        p.info = Some(uinfo.into());
        state.players.push(p);

        // Wearable 1: owned by player, def 940 (Ghostly Gibus, hat)
        algo.wearables.insert(
            EntityId::from(101u32),
            WearableEntityState {
                owner: player_ent,
                item_def_index: 940,
                account_id: player_account_id,
                is_disguise: false,
                nodraw: false,
                last_seen_tick: 1,
                spawn_generation: 0,
            },
        );

        // Wearable 2: orphaned entity with different account_id (e.g. 345727541 from disconnected player), def 471 (hat)
        algo.wearables.insert(
            EntityId::from(102u32),
            WearableEntityState {
                owner: player_ent,
                item_def_index: 471,
                account_id: 345727541, // Mismatch!
                is_disguise: false,
                nodraw: false,
                last_seen_tick: 1,
                spawn_generation: 0,
            },
        );

        let pstate = ParserState::new(24, |_| false, false);
        let msg = Message::NetTick(NetTickMessage {
            tick: 1.into(),
            frame_time: 15,
            std_dev: 0,
        });

        let res = algo
            .on_message(&msg, &state, &pstate, DemoTick::from(1))
            .unwrap();
        assert!(
            res.is_empty(),
            "Orphaned wearable with mismatching account_id must be ignored and not cause conflict"
        );
    }
}
