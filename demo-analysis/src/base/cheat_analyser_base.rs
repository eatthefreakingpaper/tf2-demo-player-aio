// This file is a modified version of GameStateAnalyser.
// https://github.com/demostf/parser/blob/master/src/demo/parser/gamestateanalyser.rs
// TODO: This version will add support for sub analysers that can be used to extend functionality as needed
// without creating an entirely separate analyser.
// Additional functionality that has broad utility can be merged into this base analyser.

use anyhow::Error;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::convert::TryFrom;
use std::str::FromStr;
use steamid_ng::SteamID;
use tf_demo_parser::demo::data::DemoTick;
use tf_demo_parser::demo::gameevent_gen::ObjectDestroyedEvent;
use tf_demo_parser::demo::gamevent::GameEvent;
use tf_demo_parser::demo::header::Header;
use tf_demo_parser::demo::message::gameevent::GameEventMessage;
use tf_demo_parser::demo::message::packetentities::{EntityId, PacketEntity, UpdateType};
use tf_demo_parser::demo::message::Message;
use tf_demo_parser::demo::packet::datatable::{ParseSendTable, ServerClass, ServerClassName};
use tf_demo_parser::demo::packet::message::MessagePacketMeta;
use tf_demo_parser::demo::packet::stringtable::StringTableEntry;
use tf_demo_parser::demo::parser::analyser::UserInfo;
pub use tf_demo_parser::demo::parser::analyser::{Class, Team, UserId};
use tf_demo_parser::demo::parser::handler::BorrowMessageHandler;
use tf_demo_parser::demo::parser::MessageHandler;
use tf_demo_parser::demo::sendprop::{SendProp, SendPropIdentifier, SendPropValue};
use tf_demo_parser::demo::vector::{Vector, VectorXY};
use tf_demo_parser::{MessageType, ParserState, ReadResult, Stream};
use web_time::Instant;

use crate::dev_print;
use crate::lib::algorithm::{CheatAlgorithm, Detection};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
pub enum PlayerState {
    #[default]
    Alive = 0,
    Dying = 1,
    Death = 2,
    Respawnable = 3,
}

impl PlayerState {
    pub fn new(number: i64) -> Self {
        match number {
            1 => PlayerState::Dying,
            2 => PlayerState::Death,
            3 => PlayerState::Respawnable,
            _ => PlayerState::Alive,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Player {
    pub entity: EntityId,
    pub position: Vector,
    pub health: u16,
    pub max_health: u16,
    pub class: Class,
    pub team: Team,
    pub view_angle: f32,
    pub pitch_angle: f32,
    pub state: PlayerState,
    pub info: Option<UserInfo>,
    // Parsed once from userinfo and reused by every detector. Tests and old
    // serialized states can leave this empty; helper methods fall back to info.
    #[serde(default)]
    pub steam_id64: Option<u64>,
    pub charge: u8,
    pub simtime: u16,
    pub ping: u16,
    pub in_pvs: bool,
    pub active_weapon: Option<EntityId>,
    pub cond: u32,
    pub cond_ex: u32,
    pub cond_ex2: u32,
    pub invis_change_complete_time: f32,
    pub flags: u32,
    // pub shot_fired: u32,
}

impl Player {
    pub fn steam_id(&self) -> Option<u64> {
        self.steam_id64.or_else(|| {
            let info = self.info.as_ref()?;
            if info.steam_id == "BOT" {
                return None;
            }
            SteamID::from_steam3(&info.steam_id).map(u64::from).ok()
        })
    }

    pub fn is_on_ground(&self) -> bool {
        (self.flags & 1) != 0
    }

    pub fn is_ducking(&self) -> bool {
        (self.flags & (1 << 1)) != 0
    }

    pub fn is_in_water(&self) -> bool {
        // TF2 networks its water state as m_fFlags bit 10 (0x400): every
        // player-tick below the waterline on water maps carries it, and it is
        // never set anywhere else. The Source SDK's FL_INWATER (bit 9) and
        // FL_SWIM (bit 11) do not appear in demos at all.
        (self.flags & (1 << 10)) != 0
    }

    pub fn class_name(&self) -> &'static str {
        match self.class {
            Class::Scout => "scout",
            Class::Sniper => "sniper",
            Class::Soldier => "soldier",
            Class::Demoman => "demoman",
            Class::Medic => "medic",
            Class::Heavy => "heavy",
            Class::Pyro => "pyro",
            Class::Spy => "spy",
            Class::Engineer => "engineer",
            Class::Other => "unknown",
        }
    }

    pub fn is_cloaked(&self) -> bool {
        (self.cond & (1 << 4)) != 0
    }

    pub fn is_burning(&self) -> bool {
        (self.cond & (1 << 22)) != 0
    }

    pub fn is_jarated(&self) -> bool {
        (self.cond & (1 << 24)) != 0
    }

    pub fn is_bleeding(&self) -> bool {
        (self.cond & (1 << 25)) != 0
    }

    pub fn is_milked(&self) -> bool {
        (self.cond & (1 << 27)) != 0
    }

    pub fn is_flickering(&self) -> bool {
        (self.cond_ex & (1 << (61 - 32))) != 0
    }

    pub fn is_gas_coated(&self) -> bool {
        (self.cond_ex2 & (1 << (73 - 64))) != 0
    }

    pub fn is_stealthed_blink(&self) -> bool {
        (self.cond_ex2 & (1 << (70 - 64))) != 0
    }

    pub fn is_taunting(&self) -> bool {
        (self.cond & (1 << 14)) != 0
    }

    pub fn is_stunned(&self) -> bool {
        (self.cond & (1 << 15)) != 0
    }

    pub fn is_marked_for_death(&self) -> bool {
        (self.cond & (1 << 30)) != 0
    }

    pub fn is_crit_boosted(&self) -> bool {
        // These are the unconditional, all-weapon cases checked by
        // CTFPlayerShared::IsCritBoosted. Condition 105 (the temporary rune
        // boost) is not available because this parser does not retain
        // m_nPlayerCondEx3; condition 44 is handled per active weapon by TF2.
        (self.cond & (1 << 11)) != 0
            || (self.cond_ex
                & ((1 << (33 - 32))
                    | (1 << (34 - 32))
                    | (1 << (35 - 32))
                    | (1 << (37 - 32))
                    | (1 << (38 - 32))
                    | (1 << (39 - 32))
                    | (1 << (40 - 32))
                    | (1 << (56 - 32))))
                != 0
    }

    pub fn has_visible_effect(&self) -> bool {
        self.is_burning()
            || self.is_jarated()
            || self.is_bleeding()
            || self.is_milked()
            || self.is_flickering()
            || self.is_gas_coated()
            || self.is_stealthed_blink()
            || self.is_taunting()
            || self.is_stunned()
            || self.is_marked_for_death()
    }

    pub fn is_completely_invisible(&self) -> bool {
        self.is_cloaked() && !self.has_visible_effect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Sentry {
    pub entity: EntityId,
    pub builder: UserId,
    pub position: Vector,
    pub level: u8,
    pub max_health: u16,
    pub health: u16,
    pub building: bool,
    pub sapped: bool,
    pub team: Team,
    pub angle: f32,
    pub player_controlled: bool,
    pub auto_aim_target: UserId,
    pub shells: u16,
    pub rockets: u16,
    pub is_mini: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Dispenser {
    pub entity: EntityId,
    pub builder: UserId,
    pub position: Vector,
    pub level: u8,
    pub max_health: u16,
    pub health: u16,
    pub building: bool,
    pub sapped: bool,
    pub team: Team,
    pub angle: f32,
    pub healing: Vec<UserId>,
    pub metal: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Teleporter {
    pub entity: EntityId,
    pub builder: UserId,
    pub position: Vector,
    pub level: u8,
    pub max_health: u16,
    pub health: u16,
    pub building: bool,
    pub sapped: bool,
    pub team: Team,
    pub angle: f32,
    pub is_entrance: bool,
    pub other_end: EntityId,
    pub recharge_time: f32,
    pub recharge_duration: f32,
    pub times_used: u16,
    pub yaw_to_exit: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Building {
    Sentry(Sentry),
    Dispenser(Dispenser),
    Teleporter(Teleporter),
}

impl Building {
    pub fn new(entity_id: EntityId, class: BuildingClass) -> Building {
        match class {
            BuildingClass::Sentry => Building::Sentry(Sentry {
                entity: entity_id,
                ..Sentry::default()
            }),
            BuildingClass::Dispenser => Building::Dispenser(Dispenser {
                entity: entity_id,
                ..Dispenser::default()
            }),
            BuildingClass::Teleporter => Building::Teleporter(Teleporter {
                entity: entity_id,
                ..Teleporter::default()
            }),
        }
    }

    pub fn entity_id(&self) -> EntityId {
        match self {
            Building::Sentry(Sentry { entity, .. })
            | Building::Dispenser(Dispenser { entity, .. })
            | Building::Teleporter(Teleporter { entity, .. }) => *entity,
        }
    }

    pub fn level(&self) -> u8 {
        match self {
            Building::Sentry(Sentry { level, .. })
            | Building::Dispenser(Dispenser { level, .. })
            | Building::Teleporter(Teleporter { level, .. }) => *level,
        }
    }

    pub fn position(&self) -> Vector {
        match self {
            Building::Sentry(Sentry { position, .. })
            | Building::Dispenser(Dispenser { position, .. })
            | Building::Teleporter(Teleporter { position, .. }) => *position,
        }
    }

    pub fn builder(&self) -> UserId {
        match self {
            Building::Sentry(Sentry { builder, .. })
            | Building::Dispenser(Dispenser { builder, .. })
            | Building::Teleporter(Teleporter { builder, .. }) => *builder,
        }
    }

    pub fn angle(&self) -> f32 {
        match self {
            Building::Sentry(Sentry { angle, .. })
            | Building::Dispenser(Dispenser { angle, .. })
            | Building::Teleporter(Teleporter { angle, .. }) => *angle,
        }
    }

    pub fn max_health(&self) -> u16 {
        match self {
            Building::Sentry(Sentry { max_health, .. })
            | Building::Dispenser(Dispenser { max_health, .. })
            | Building::Teleporter(Teleporter { max_health, .. }) => *max_health,
        }
    }

    pub fn health(&self) -> u16 {
        match self {
            Building::Sentry(Sentry { health, .. })
            | Building::Dispenser(Dispenser { health, .. })
            | Building::Teleporter(Teleporter { health, .. }) => *health,
        }
    }

    pub fn sapped(&self) -> bool {
        match self {
            Building::Sentry(Sentry { sapped, .. })
            | Building::Dispenser(Dispenser { sapped, .. })
            | Building::Teleporter(Teleporter { sapped, .. }) => *sapped,
        }
    }

    pub fn team(&self) -> Team {
        match self {
            Building::Sentry(Sentry { team, .. })
            | Building::Dispenser(Dispenser { team, .. })
            | Building::Teleporter(Teleporter { team, .. }) => *team,
        }
    }

    pub fn class(&self) -> BuildingClass {
        match self {
            Building::Sentry(_) => BuildingClass::Sentry,
            Building::Dispenser(_) => BuildingClass::Sentry,
            Building::Teleporter(_) => BuildingClass::Teleporter,
        }
    }
}

pub enum BuildingClass {
    Sentry,
    Dispenser,
    Teleporter,
}

#[derive(Default, Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct World {
    pub boundary_min: Vector,
    pub boundary_max: Vector,
}

// #[derive(Default, Debug, Serialize, Deserialize, PartialEq, Clone)]
// pub struct Kill {
//     pub attacker_id: u16,
//     pub assister_id: u16,
//     pub victim_id: u16,
//     pub weapon: String,
//     pub tick: DemoTick,
// }

// impl Kill {
//     fn new(tick: DemoTick, death: &PlayerDeathEvent) -> Self {
//         Kill {
//             attacker_id: death.attacker,
//             assister_id: death.assister,
//             victim_id: death.user_id,
//             weapon: death.weapon.to_string(),
//             tick,
//         }
//     }
// }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct WeaponEntity {
    pub entity: EntityId,
    pub class_name: String,
    pub item_def_index: Option<u16>,
}

impl WeaponEntity {
    pub fn name(&self, player_class: Class) -> String {
        crate::util::helpers::weapon_name_from_id_or_class(
            self.item_def_index,
            &self.class_name,
            player_class,
        )
    }
}

#[derive(Default, Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct CheatAnalyserState {
    pub players: Vec<Player>,
    #[serde(skip)]
    player_index_by_entity: HashMap<EntityId, usize>,
    #[serde(skip)]
    player_index_by_sid: HashMap<u64, usize>,
    // Baseline-expanded Enter properties for the current PacketEntities
    // message. Algorithms share this instead of independently expanding the
    // same entity.
    #[serde(skip)]
    pub resolved_entity_props: HashMap<EntityId, Vec<SendProp>>,
    pub player_names: HashMap<u64, String>,
    pub user_info_history: HashMap<u64, UserInfo>,
    pub entid_to_userid: HashMap<EntityId, UserId>,
    pub userid_to_id64: HashMap<UserId, u64>,
    pub buildings: BTreeMap<EntityId, Building>,
    pub weapons: HashMap<EntityId, WeaponEntity>,
    pub world: Option<World>,
    pub friends_ids: HashMap<EntityId, u32>,
    pub custom_files: HashMap<EntityId, [u32; 4]>,
    // pub kills: Vec<Kill>,
    pub tick: DemoTick,
    pub header: Option<Header>,
    pub user_cmds: Vec<tf_demo_parser::demo::packet::usercmd::UserCmdPacket>,
    // The recorder's literal console commands (container-level ConsoleCmd
    // frames), e.g. "+attack 45". Tick-tagged in file order.
    pub console_cmds: Vec<(DemoTick, String)>,
}

impl CheatAnalyserState {
    pub fn get_player_name(&self, sid: u64) -> Option<&str> {
        if let Some(name) = self.player_names.get(&sid) {
            return Some(name.as_str());
        }
        for player in &self.players {
            if let Some(info) = &player.info {
                if let Ok(steam_id) = SteamID::from_steam3(&info.steam_id) {
                    if u64::from(steam_id) == sid {
                        return Some(info.name.as_str());
                    }
                }
            }
        }
        None
    }

    pub fn get_player_weapon(&self, player: &Player) -> String {
        if let Some(weapon_ent) = player.active_weapon {
            if let Some(weapon) = self.weapons.get(&weapon_ent) {
                return weapon.name(player.class);
            }
        }
        "unknown".to_string()
    }

    pub fn get_player_class_and_weapon_by_sid(&self, sid: u64) -> (&'static str, String) {
        if let Some(player) = self.get_player_by_sid(sid) {
            (player.class_name(), self.get_player_weapon(player))
        } else {
            ("unknown", "unknown".to_string())
        }
    }

    pub fn get_player_by_entity(&self, entity_id: EntityId) -> Option<&Player> {
        self.player_index_by_entity
            .get(&entity_id)
            .and_then(|index| self.players.get(*index))
            .filter(|player| player.entity == entity_id)
            .or_else(|| {
                self.players
                    .iter()
                    .find(|player| player.entity == entity_id)
            })
    }

    pub fn get_player_by_sid(&self, sid: u64) -> Option<&Player> {
        self.player_index_by_sid
            .get(&sid)
            .and_then(|index| self.players.get(*index))
            .filter(|player| player.steam_id64 == Some(sid))
            .or_else(|| {
                self.players.iter().find(|player| {
                    player.steam_id64 == Some(sid)
                        || player.info.as_ref().is_some_and(|info| {
                            SteamID::from_steam3(&info.steam_id).map(u64::from).ok() == Some(sid)
                        })
                })
            })
    }

    fn find_player_index_by_entity(&mut self, entity_id: EntityId) -> Option<usize> {
        if let Some(index) = self
            .player_index_by_entity
            .get(&entity_id)
            .copied()
            .filter(|index| {
                self.players
                    .get(*index)
                    .is_some_and(|player| player.entity == entity_id)
            })
        {
            return Some(index);
        }

        let index = self
            .players
            .iter()
            .position(|player| player.entity == entity_id)?;
        self.player_index_by_entity.insert(entity_id, index);
        if let Some(sid) = self.players[index].steam_id64 {
            self.player_index_by_sid.insert(sid, index);
        }
        Some(index)
    }

    pub fn get_player_by_entity_mut(&mut self, entity_id: EntityId) -> Option<&mut Player> {
        let index = self.find_player_index_by_entity(entity_id)?;
        self.players.get_mut(index)
    }

    pub fn get_or_create_player(&mut self, entity_id: EntityId) -> &mut Player {
        let index = self
            .find_player_index_by_entity(entity_id)
            .unwrap_or_else(|| {
                let index = self.players.len();
                self.players.push(Player {
                    entity: entity_id,
                    ..Player::default()
                });
                self.player_index_by_entity.insert(entity_id, index);
                index
            });
        &mut self.players[index]
    }

    pub fn set_player_steam_id(&mut self, entity_id: EntityId, sid: u64) {
        let player = self.get_or_create_player(entity_id);
        player.steam_id64 = Some(sid);
        let index = self.player_index_by_entity[&entity_id];
        self.player_index_by_sid.insert(sid, index);
    }

    pub fn steam_id_for_entity(&self, entity_id: EntityId) -> Option<u64> {
        self.get_player_by_entity(entity_id)
            .and_then(|player| {
                player.steam_id64.or_else(|| {
                    player
                        .info
                        .as_ref()
                        .and_then(|info| SteamID::from_steam3(&info.steam_id).map(u64::from).ok())
                })
            })
            .or_else(|| {
                self.get_userid_from_entid(entity_id)
                    .and_then(|user_id| self.get_id64_from_userid(user_id))
            })
    }

    pub fn entity_props<'b>(
        &'b self,
        entity: &'b PacketEntity,
        parser_state: &'b ParserState,
    ) -> Cow<'b, [SendProp]> {
        if entity.update_type == UpdateType::Enter {
            if let Some(props) = self.resolved_entity_props.get(&entity.entity_index) {
                return Cow::Borrowed(props);
            }
            Cow::Owned(entity.props(parser_state).collect())
        } else {
            Cow::Borrowed(entity.props.as_slice())
        }
    }
    pub fn get_userid_from_entid(&self, entid: EntityId) -> Option<UserId> {
        self.entid_to_userid.get(&entid).copied()
    }

    pub fn get_id64_from_userid(&self, userid: UserId) -> Option<u64> {
        self.userid_to_id64.get(&userid).copied()
    }

    pub fn set_entid_to_userid(&mut self, entid: EntityId, userid: UserId) {
        self.entid_to_userid.insert(entid, userid);
    }

    pub fn set_userid_to_id64(&mut self, userid: UserId, id64: u64) {
        self.userid_to_id64.insert(userid, id64);
    }

    pub fn get_or_create_building(
        &mut self,
        entity_id: EntityId,
        class: BuildingClass,
    ) -> &mut Building {
        self.buildings
            .entry(entity_id)
            .or_insert_with(|| Building::new(entity_id, class))
    }

    pub fn remove_building(&mut self, entity_id: EntityId) {
        self.buildings.remove(&entity_id);
    }
}

// ParserState requires a static callback. A bit mask keeps this callback lock-free;
// MessageType is repr(u8) and its highest current value is 32.
const fn message_type_mask(message_type: MessageType) -> u64 {
    1_u64 << (message_type as u8)
}

const BASE_MESSAGE_MASK: u64 = message_type_mask(MessageType::PacketEntities)
    | message_type_mask(MessageType::GameEvent)
    | message_type_mask(MessageType::NetTick);

thread_local! {
    static HANDLED_MESSAGE_MASK: Cell<u64> = const { Cell::new(BASE_MESSAGE_MASK) };
}

pub struct CheatAnalyser<'a> {
    pub state: CheatAnalyserState,
    pub algorithms: Vec<Box<dyn CheatAlgorithm<'a> + 'a + Send>>,
    algorithm_message_masks: Vec<u64>,
    pub detections: Vec<Detection>,
    pub header: Option<Header>,
    pub tick: DemoTick,
    pub worker_id: usize,
    last_progress_update_time: Instant,
    progress: Vec<u32>,
    class_names: Vec<ServerClassName>, // indexed by ClassId
}

impl<'a> Default for CheatAnalyser<'a> {
    fn default() -> Self {
        Self {
            state: Default::default(),
            algorithms: Default::default(),
            algorithm_message_masks: Default::default(),
            detections: Default::default(),
            header: Default::default(),
            tick: Default::default(),
            worker_id: 0,
            last_progress_update_time: Instant::now(),
            progress: Default::default(),
            class_names: Default::default(),
        }
    }
}

impl MessageHandler for CheatAnalyser<'_> {
    type Output = CheatAnalyserState;

    fn does_handle(message_type: MessageType) -> bool {
        HANDLED_MESSAGE_MASK.with(|mask| mask.get() & message_type_mask(message_type) != 0)
    }

    fn handle_header(&mut self, _header: &tf_demo_parser::demo::header::Header) {
        self.header = Some(_header.clone());
        self.state.header = Some(_header.clone());
        if _header.ticks > 0 {
            crate::PROGRESS_TOTAL.store(_header.ticks, std::sync::atomic::Ordering::Relaxed);
        }
        self.print_metadata();
    }

    fn handle_message(&mut self, message: &Message, _tick: DemoTick, parser_state: &ParserState) {
        match message {
            Message::PacketEntities(message) => {
                let mut resolved_entity_props = HashMap::with_capacity(message.entities.len());
                for entity in &message.entities {
                    if entity.update_type == UpdateType::Enter {
                        resolved_entity_props
                            .insert(entity.entity_index, entity.props(parser_state).collect());
                    }
                }
                for entity in &message.entities {
                    self.handle_entity(
                        entity,
                        parser_state,
                        resolved_entity_props
                            .get(&entity.entity_index)
                            .map(Vec::as_slice),
                    );
                }
                self.state.resolved_entity_props = resolved_entity_props;
            }
            Message::NetTick(_) => {
                self.check_progress();
                for algorithm in &mut self.algorithms {
                    match algorithm.on_tick(&self.state, parser_state) {
                        Ok(detections) => self.detections.extend(detections),
                        Err(_) => {}
                    }
                }
                self.state.user_cmds.clear();
            }
            Message::TempEntities(_) => {
                // println!("{}: {:#?}", _tick, message);
            }
            Message::GameEvent(GameEventMessage { event, .. }) => match event {
                // GameEvent::PlayerDeath(death) => {
                //     self.state.kills.push(Kill::new(self.tick, death.as_ref()))
                // }
                // TODO: Wait for https://github.com/demostf/parser/issues/25 to be resolved
                // GameEvent::PlayerShoot(_) => {
                //     println!("player shoot event");
                //     // let player = self.state.players.iter_mut().find(|p|{
                //     //     p.info.as_ref().is_some_and(|info| {
                //     //         println!("{} == {}", info.user_id, user_id);
                //     //         info.user_id == *user_id
                //     //     })
                //     // });
                //     // if let Some(player) = player {
                //     //     player.shot_fired = u32::from(_tick);
                //     // }
                // }
                GameEvent::RoundStart(_) => {
                    self.state.buildings.clear();
                }
                GameEvent::TeamPlayRoundStart(_) => {
                    self.state.buildings.clear();
                }
                GameEvent::ObjectDestroyed(ObjectDestroyedEvent { index, .. }) => {
                    self.state.remove_building((*index as u32).into());
                }
                GameEvent::PlayerConnectClient(event) => {
                    self.state.set_entid_to_userid(
                        EntityId::from(event.index as u32),
                        UserId::from(event.user_id),
                    );
                    if event.network_id != "BOT".into() {
                        let steamid = SteamID::from_steam3(event.network_id.to_string().as_str());
                        let steamid64 = u64::from(steamid.unwrap_or(0.into()));
                        self.state
                            .set_userid_to_id64(event.user_id.into(), steamid64);
                    }
                }
                _ => {}
            },
            _ => {}
        }
        let message_mask = message_type_mask(message.get_message_type());
        for (algorithm, handled_mask) in self
            .algorithms
            .iter_mut()
            .zip(self.algorithm_message_masks.iter().copied())
        {
            if handled_mask & message_mask == 0 {
                continue;
            }
            match algorithm.on_message(message, &self.state, &parser_state, _tick) {
                Ok(detections) => self.detections.extend(detections),
                Err(_) => {}
            }
        }
    }

    fn handle_string_entry(
        &mut self,
        table: &str,
        index: usize,
        entry: &StringTableEntry,
        _parser_state: &ParserState,
    ) {
        if table == "userinfo" {
            let _ = self.parse_user_info(
                index,
                entry.text.as_ref().map(|s| s.as_ref()),
                entry.extra_data.as_ref().map(|data| data.data.clone()),
            );
        }
    }

    fn handle_data_tables(
        &mut self,
        _parse_tables: &[ParseSendTable],
        server_classes: &[ServerClass],
        _parser_state: &ParserState,
    ) {
        self.class_names = server_classes
            .iter()
            .map(|class| &class.name)
            .cloned()
            .collect();
    }

    fn handle_packet_meta(
        &mut self,
        tick: DemoTick,
        _meta: &MessagePacketMeta,
        _parser_state: &ParserState,
    ) {
        self.state.tick = tick;
        self.tick = tick;
    }

    fn into_output(self, _state: &ParserState) -> Self::Output {
        self.state
    }
}

impl BorrowMessageHandler for CheatAnalyser<'_> {
    fn borrow_output(&self, _state: &ParserState) -> &Self::Output {
        &self.state
    }
}

impl crate::base::demo_handler_base::DemoHandlerAnalyser for CheatAnalyser<'_> {
    fn handle_user_cmd(&mut self, packet: tf_demo_parser::demo::packet::usercmd::UserCmdPacket) {
        self.state.user_cmds.push(packet);
    }

    fn handle_console_cmd(
        &mut self,
        packet: tf_demo_parser::demo::packet::consolecmd::ConsoleCmdPacket,
    ) {
        self.state.console_cmds.push((packet.tick, packet.command));
    }
}

impl<'a> CheatAnalyser<'a> {
    pub fn new(algorithms: Vec<Box<dyn CheatAlgorithm<'a> + 'a + Send>>) -> Self {
        let mut parser_message_mask = BASE_MESSAGE_MASK;
        let algorithm_message_masks: Vec<u64> = algorithms
            .iter()
            .map(|algorithm| {
                let mask = match algorithm.handled_messages() {
                    Ok(types) => types.into_iter().fold(0, |mask, message_type| {
                        mask | message_type_mask(message_type)
                    }),
                    Err(true) => u64::MAX,
                    Err(false) => 0,
                };
                parser_message_mask |= mask;
                mask
            })
            .collect();

        // Parsing and the static callback run on the same worker thread. A
        // thread-local mask gives concurrent demos their exact message set and
        // avoids a previously enabled parse-all algorithm permanently widening
        // every later analysis.
        HANDLED_MESSAGE_MASK.with(|mask| mask.set(parser_message_mask));

        Self {
            state: Default::default(),
            algorithms,
            algorithm_message_masks,
            detections: Vec::new(),
            header: None,
            tick: DemoTick::default(),
            worker_id: 0,
            last_progress_update_time: Instant::now(),
            progress: vec![],
            class_names: Vec::new(),
        }
    }

    pub fn handle_user_cmd(
        &mut self,
        packet: tf_demo_parser::demo::packet::usercmd::UserCmdPacket,
    ) {
        self.state.user_cmds.push(packet);
    }

    pub fn init(&mut self) -> Result<(), Error> {
        for algorithm in &mut self.algorithms {
            match algorithm.init() {
                Ok(_) => {}
                Err(_) => continue,
            }
        }
        Ok(())
    }

    pub fn finish(&mut self) -> Result<(), Error> {
        for algorithm in &mut self.algorithms {
            match algorithm.finish() {
                Ok(detections) => self.detections.extend(detections),
                Err(_) => continue,
            }
        }
        self.detections.sort_by(|a, b| {
            a.tick
                .cmp(&b.tick)
                .then_with(|| a.player.cmp(&b.player))
                .then_with(|| a.algorithm.cmp(&b.algorithm))
        });
        Ok(())
    }

    pub fn print_metadata(&self) {
        if self.header.is_none() {
            return;
        }
        let header = self.header.as_ref().unwrap();
        let ticks = self.get_tick_count_u32();

        dev_print!("Map: {}", header.map);
        let hours = (header.duration / 3600.0).floor();
        let minutes = ((header.duration % 3600.0) / 60.0).floor();
        let seconds = (header.duration % 60.0).floor();
        let milliseconds = ((header.duration % 1.0) * 100.0).floor();
        dev_print!(
            "Duration: {:02}:{:02}:{:02}.{:03} ({} ticks)",
            hours,
            minutes,
            seconds,
            milliseconds,
            ticks
        );
        dev_print!("User: {}", header.nick);
        dev_print!("Server: {}", header.server);
    }

    pub fn print_detection_json(&self, pretty: bool) {
        let analysis = serde_json::json!({
            "server_ip": self.header.as_ref().map_or("unknown".to_string(), |h| h.server.clone()),
            "duration": self.tick,
            "author": self.header.as_ref().map_or("unknown".to_string(), |h| h.nick.clone()),
            "map": self.header.as_ref().map_or("unknown".to_string(), |h| h.map.clone()),
            "detections": self.detections
        });
        let json = if pretty {
            serde_json::to_string_pretty(&analysis).unwrap()
        } else {
            serde_json::to_string(&analysis).unwrap()
        };
        println!("{}", json);
    }

    pub fn print_detection_summary(&self) {
        let mut algorithm_counts: HashMap<String, HashMap<u64, usize>> = HashMap::new();
        for detection in &self.detections {
            let algorithm = detection.algorithm.clone();
            let steamid = detection.player;
            *algorithm_counts
                .entry(algorithm)
                .or_insert(HashMap::new())
                .entry(steamid)
                .or_insert(0) += 1;
        }

        dev_print!("Total detections: {}", self.detections.len());
        if self.detections.is_empty() {
            return;
        }
        dev_print!("Detections by Algorithm:");
        for (algorithm, steamid_counts) in algorithm_counts {
            dev_print!(
                "  {}: {} players, {} detections",
                algorithm,
                steamid_counts.len(),
                steamid_counts.values().sum::<usize>()
            );
            let mut steamid_counts_vec: Vec<_> = steamid_counts.into_iter().collect();
            steamid_counts_vec.sort_by(|a, b| b.1.cmp(&a.1));
            for (steamid, count) in steamid_counts_vec {
                dev_print!("    {}: {}", steamid, count);
            }
        }
    }
    // This code doesn't include the very first interval in any averages.
    // I didn't intend for that but it makes sense to exclude the intitial interval since
    // there tends to be a lot of boiler plate stuff which throws off the average anyway.
    fn check_progress(&mut self) {
        const PROGRESS_UPDATE_INTERVAL_MS: u128 = 1000;
        const TPS_ROLLING_AVERAGE_WINDOW: u32 = 10;
        let tick: u32 = self.tick.into();
        let total_ticks = self.get_header_tick_count_u32();

        if self.worker_id < crate::MAX_WORKERS {
            crate::WORKER_TICKS[self.worker_id].store(tick, std::sync::atomic::Ordering::Relaxed);
        }
        if total_ticks > 0 {
            crate::PROGRESS_TOTAL.store(total_ticks, std::sync::atomic::Ordering::Relaxed);
        }
        crate::PROGRESS_CURRENT.store(tick, std::sync::atomic::Ordering::Relaxed);

        if self.last_progress_update_time.elapsed().as_millis() < PROGRESS_UPDATE_INTERVAL_MS {
            return;
        }

        self.last_progress_update_time = Instant::now();
        self.progress.push(tick);
        while self.progress.len() > TPS_ROLLING_AVERAGE_WINDOW.try_into().unwrap() {
            self.progress.remove(0);
        }

        let tps = if self.progress.len() >= 2 {
            let tps = (self.progress.last().unwrap() - self.progress.first().unwrap()) as f64
                / (self.progress.len() as f64 - 1.0);
            tps * PROGRESS_UPDATE_INTERVAL_MS as f64 / 1000.0
        } else {
            tick.into()
        };

        if self.worker_id == 0 {
            dev_print!(
                "Processing tick {} ({} remaining, {:.0} tps)",
                tick,
                if total_ticks > tick {
                    total_ticks - tick
                } else {
                    0
                },
                tps
            );
        }
    }

    pub fn get_header_tick_count_u32(&self) -> u32 {
        self.header.as_ref().map(|h| h.ticks).unwrap_or(0)
    }

    pub fn get_tick_count_u32(&self) -> u32 {
        if self.header.is_none() {
            return self.tick.into();
        }
        let header = self.header.as_ref().unwrap();
        if self.tick > header.ticks {
            self.tick.into()
        } else {
            header.ticks
        }
    }

    pub fn handle_entity<'b>(
        &mut self,
        entity: &'b PacketEntity,
        parser_state: &'b ParserState,
        resolved_enter_props: Option<&'b [SendProp]>,
    ) {
        let class_name: &str = self
            .class_names
            .get(usize::from(entity.server_class))
            .map(|class_name| class_name.as_str())
            .unwrap_or("");

        let needs_state_props = matches!(
            class_name,
            "CTFPlayer"
                | "CTFPlayerResource"
                | "CWorld"
                | "CObjectSentrygun"
                | "CObjectDispenser"
                | "CObjectTeleporter"
        );
        // Delta updates already own exactly the changed properties, so borrow them directly.
        // Enter updates also need their class baseline merged in; materialize that merged view
        // once and share it between weapon tracking and the specialized state handler.
        let state_props: Option<Cow<'_, [SendProp]>> = needs_state_props.then(|| {
            if entity.update_type == UpdateType::Enter {
                resolved_enter_props.map_or_else(
                    || Cow::Owned(entity.props(parser_state).collect()),
                    Cow::Borrowed,
                )
            } else {
                Cow::Borrowed(entity.props.as_slice())
            }
        });

        if entity.update_type == UpdateType::Delete {
            self.state.weapons.remove(&entity.entity_index);
        } else {
            const ITEM_DEFINITION_INDEX: SendPropIdentifier =
                SendPropIdentifier::new("DT_ScriptCreatedItem", "m_iItemDefinitionIndex");

            let read_item_definition = |prop: &SendProp| {
                (prop.identifier == ITEM_DEFINITION_INDEX)
                    .then(|| i64::try_from(&prop.value).ok().map(|value| value as u16))
                    .flatten()
            };
            let item_def = match state_props.as_deref() {
                Some(props) => props.iter().find_map(read_item_definition),
                None if entity.update_type == UpdateType::Enter => resolved_enter_props
                    .and_then(|props| props.iter().find_map(read_item_definition))
                    .or_else(|| {
                        entity
                            .props(parser_state)
                            .find_map(|prop| read_item_definition(&prop))
                    }),
                None => entity.props.iter().find_map(read_item_definition),
            };

            if item_def.is_some()
                || class_name.starts_with("CTFWeapon")
                || class_name.starts_with("CTFShotgun")
                || class_name.starts_with("CTFRocketLauncher")
                || class_name.starts_with("CTFPipebombLauncher")
                || class_name.starts_with("CTFGrenadeLauncher")
                || class_name.starts_with("CTFCompoundBow")
                || class_name.starts_with("CTFMinigun")
                || class_name.starts_with("CTFScatterGun")
                || class_name.starts_with("CTFKnife")
                || class_name.starts_with("CTFRevolver")
                || class_name.starts_with("CTFMedigun")
                || class_name.starts_with("CTFBonesaw")
                || class_name.starts_with("CTFClub")
                || class_name.starts_with("CTFSword")
                || class_name.starts_with("CTFWrench")
                || class_name.starts_with("CTFFlameThrower")
                || class_name.starts_with("CTFPistol")
                || class_name.starts_with("CTFJar")
                || class_name.starts_with("CTFBuffItem")
                || class_name.starts_with("CTFLunchBox")
                || class_name.starts_with("CTFCrossbow")
                || class_name.starts_with("CTFStickBomb")
                || class_name.starts_with("CTFParticleCannon")
                || class_name.starts_with("CTFRaygun")
                || class_name.starts_with("CTFDRG_Pomson")
                || class_name.starts_with("CTFSniperRifle")
                || class_name.starts_with("CWeapon")
                || (class_name.starts_with("CTF") && class_name.contains("Weapon"))
            {
                let entry = self.state.weapons.entry(entity.entity_index).or_default();
                entry.entity = entity.entity_index;
                entry.class_name = class_name.to_string();
                if item_def.is_some() {
                    entry.item_def_index = item_def;
                }
            }
        }

        let props = state_props.as_deref().unwrap_or_default();
        match class_name {
            "CTFPlayer" => self.handle_player_entity(entity, props),
            "CTFPlayerResource" => self.handle_player_resource(props),
            "CWorld" => self.handle_world_entity(props),
            "CObjectSentrygun" => self.handle_sentry_entity(entity, props),
            "CObjectDispenser" => self.handle_dispenser_entity(entity, props),
            "CObjectTeleporter" => self.handle_teleporter_entity(entity, props),
            _ => {}
        }
    }

    pub fn handle_player_resource(&mut self, props: &[SendProp]) {
        for prop in props {
            if let Some((table_name, prop_name)) = prop.identifier.names() {
                if let Ok(player_id) = u32::from_str(prop_name.as_str()) {
                    let entity_id = EntityId::from(player_id);
                    let mut mapping = None;
                    if let Some(player) = self.state.get_player_by_entity_mut(entity_id) {
                        if let Some(info) = &player.info {
                            mapping = Some((entity_id, info.user_id));
                        }
                        match table_name.as_str() {
                            "m_iTeam" => {
                                player.team =
                                    Team::new(i64::try_from(&prop.value).unwrap_or_default())
                            }
                            "m_iMaxHealth" => {
                                player.max_health =
                                    i64::try_from(&prop.value).unwrap_or_default() as u16
                            }
                            "m_iPlayerClass" => {
                                player.class =
                                    Class::new(i64::try_from(&prop.value).unwrap_or_default())
                            }
                            "m_iChargeLevel" => {
                                player.charge = i64::try_from(&prop.value).unwrap_or_default() as u8
                            }
                            "m_iPing" => {
                                player.ping = i64::try_from(&prop.value).unwrap_or_default() as u16
                            }
                            _ => {}
                        }
                    }
                    if let Some((entity_id, user_id)) = mapping {
                        self.state.set_entid_to_userid(entity_id, user_id);
                    }
                }
            }
        }
    }

    pub fn handle_player_entity(&mut self, entity: &PacketEntity, props: &[SendProp]) {
        let player = self.state.get_or_create_player(entity.entity_index);

        const HEALTH_PROP: SendPropIdentifier =
            SendPropIdentifier::new("DT_BasePlayer", "m_iHealth");
        const MAX_HEALTH_PROP: SendPropIdentifier =
            SendPropIdentifier::new("DT_BasePlayer", "m_iMaxHealth");
        const LIFE_STATE_PROP: SendPropIdentifier =
            SendPropIdentifier::new("DT_BasePlayer", "m_lifeState");

        const LOCAL_ORIGIN: SendPropIdentifier =
            SendPropIdentifier::new("DT_TFLocalPlayerExclusive", "m_vecOrigin");
        const NON_LOCAL_ORIGIN: SendPropIdentifier =
            SendPropIdentifier::new("DT_TFNonLocalPlayerExclusive", "m_vecOrigin");
        const LOCAL_ORIGIN_Z: SendPropIdentifier =
            SendPropIdentifier::new("DT_TFLocalPlayerExclusive", "m_vecOrigin[2]");
        const NON_LOCAL_ORIGIN_Z: SendPropIdentifier =
            SendPropIdentifier::new("DT_TFNonLocalPlayerExclusive", "m_vecOrigin[2]");
        const LOCAL_EYE_ANGLES: SendPropIdentifier =
            SendPropIdentifier::new("DT_TFLocalPlayerExclusive", "m_angEyeAngles[1]");
        const NON_LOCAL_EYE_ANGLES: SendPropIdentifier =
            SendPropIdentifier::new("DT_TFNonLocalPlayerExclusive", "m_angEyeAngles[1]");
        const LOCAL_PITCH_ANGLES: SendPropIdentifier =
            SendPropIdentifier::new("DT_TFLocalPlayerExclusive", "m_angEyeAngles[0]");
        const NON_LOCAL_PITCH_ANGLES: SendPropIdentifier =
            SendPropIdentifier::new("DT_TFNonLocalPlayerExclusive", "m_angEyeAngles[0]");

        const SIMTIME_PROP: SendPropIdentifier =
            SendPropIdentifier::new("DT_BaseEntity", "m_flSimulationTime");
        const ACTIVE_WEAPON_PROP: SendPropIdentifier =
            SendPropIdentifier::new("DT_BaseCombatCharacter", "m_hActiveWeapon");
        const CONDITION_BITS_PROP: SendPropIdentifier =
            SendPropIdentifier::new("DT_TFPlayerConditionListExclusive", "_condition_bits");
        const CONDITION_PROP: SendPropIdentifier =
            SendPropIdentifier::new("DT_TFPlayerShared", "m_nPlayerCond");
        const CONDITION_EX_PROP: SendPropIdentifier =
            SendPropIdentifier::new("DT_TFPlayerShared", "m_nPlayerCondEx");
        const CONDITION_EX2_PROP: SendPropIdentifier =
            SendPropIdentifier::new("DT_TFPlayerShared", "m_nPlayerCondEx2");
        const INVIS_CHANGE_COMPLETE_TIME_PROP: SendPropIdentifier =
            SendPropIdentifier::new("DT_TFPlayerShared", "m_flInvisChangeCompleteTime");
        const FLAGS_PROP: SendPropIdentifier = SendPropIdentifier::new("DT_BasePlayer", "m_fFlags");

        player.in_pvs = entity.in_pvs;

        for prop in props {
            match prop.identifier {
                ACTIVE_WEAPON_PROP => {
                    if let Ok(val) = i64::try_from(&prop.value) {
                        let handle = val as u32;
                        let ent_id = crate::util::helpers::handle_to_entid(handle);
                        player.active_weapon =
                            if u32::from(ent_id) != 0x7FF && u32::from(ent_id) != 0 {
                                Some(ent_id)
                            } else {
                                None
                            };
                    }
                }
                CONDITION_PROP | CONDITION_BITS_PROP => {
                    if let Ok(val) = i64::try_from(&prop.value) {
                        player.cond = val as u32;
                    }
                }
                CONDITION_EX_PROP => {
                    if let Ok(val) = i64::try_from(&prop.value) {
                        player.cond_ex = val as u32;
                    }
                }
                CONDITION_EX2_PROP => {
                    if let Ok(val) = i64::try_from(&prop.value) {
                        player.cond_ex2 = val as u32;
                    }
                }
                INVIS_CHANGE_COMPLETE_TIME_PROP => {
                    if let Ok(val) = f32::try_from(&prop.value) {
                        player.invis_change_complete_time = val;
                    }
                }
                FLAGS_PROP => {
                    if let Ok(val) = i64::try_from(&prop.value) {
                        player.flags = val as u32;
                    }
                }
                HEALTH_PROP => {
                    player.health = i64::try_from(&prop.value).unwrap_or_default() as u16
                }
                MAX_HEALTH_PROP => {
                    player.max_health = i64::try_from(&prop.value).unwrap_or_default() as u16
                }
                LIFE_STATE_PROP => {
                    player.state = PlayerState::new(i64::try_from(&prop.value).unwrap_or_default())
                }
                LOCAL_ORIGIN | NON_LOCAL_ORIGIN => {
                    let pos_xy = VectorXY::try_from(&prop.value).unwrap_or_default();
                    player.position.x = pos_xy.x;
                    player.position.y = pos_xy.y;
                }
                LOCAL_ORIGIN_Z | NON_LOCAL_ORIGIN_Z => {
                    player.position.z = f32::try_from(&prop.value).unwrap_or_default()
                }
                LOCAL_EYE_ANGLES | NON_LOCAL_EYE_ANGLES => {
                    player.view_angle = f32::try_from(&prop.value).unwrap_or_default()
                }
                LOCAL_PITCH_ANGLES | NON_LOCAL_PITCH_ANGLES => {
                    player.pitch_angle = f32::try_from(&prop.value).unwrap_or_default()
                }
                SIMTIME_PROP => {
                    player.simtime = i64::try_from(&prop.value).unwrap_or_default() as u16
                }
                _ => {}
            }
        }
    }

    pub fn handle_world_entity(&mut self, props: &[SendProp]) {
        const BOUNDARY_MIN: SendPropIdentifier = SendPropIdentifier::new("DT_WORLD", "m_WorldMins");
        const BOUNDARY_MAX: SendPropIdentifier = SendPropIdentifier::new("DT_WORLD", "m_WorldMaxs");

        let vector_prop = |identifier| {
            props
                .iter()
                .find(|prop| prop.identifier == identifier)
                .and_then(|prop| match &prop.value {
                    SendPropValue::Vector(vector) => Some(*vector),
                    _ => None,
                })
        };
        if let (Some(boundary_min), Some(boundary_max)) =
            (vector_prop(BOUNDARY_MIN), vector_prop(BOUNDARY_MAX))
        {
            self.state.world = Some(World {
                boundary_min,
                boundary_max,
            });
        }
    }

    pub fn handle_sentry_entity(&mut self, entity: &PacketEntity, props: &[SendProp]) {
        const ANGLE: SendPropIdentifier =
            SendPropIdentifier::new("DT_TFNonLocalPlayerExclusive", "m_angEyeAngles[1]");
        const MINI: SendPropIdentifier =
            SendPropIdentifier::new("DT_BaseObject", "m_bMiniBuilding");
        const CONTROLLED: SendPropIdentifier =
            SendPropIdentifier::new("DT_ObjectSentrygun", "m_bPlayerControlled");
        const TARGET: SendPropIdentifier =
            SendPropIdentifier::new("DT_ObjectSentrygun", "m_hAutoAimTarget");
        const SHELLS: SendPropIdentifier =
            SendPropIdentifier::new("DT_ObjectSentrygun", "m_iAmmoShells");
        const ROCKETS: SendPropIdentifier =
            SendPropIdentifier::new("DT_ObjectSentrygun", "m_iAmmoRockets");

        if entity.update_type == UpdateType::Delete {
            self.state.remove_building(entity.entity_index);
            return;
        }

        self.handle_building(entity, props, BuildingClass::Sentry);

        let building = self
            .state
            .get_or_create_building(entity.entity_index, BuildingClass::Sentry);

        if let Building::Sentry(sentry) = building {
            for prop in props {
                match prop.identifier {
                    ANGLE => sentry.angle = f32::try_from(&prop.value).unwrap_or_default(),
                    MINI => sentry.is_mini = i64::try_from(&prop.value).unwrap_or_default() > 0,
                    CONTROLLED => {
                        sentry.player_controlled =
                            i64::try_from(&prop.value).unwrap_or_default() > 0
                    }
                    TARGET => {
                        sentry.auto_aim_target =
                            UserId::from(i64::try_from(&prop.value).unwrap_or_default() as u16)
                    }
                    SHELLS => sentry.shells = i64::try_from(&prop.value).unwrap_or_default() as u16,
                    ROCKETS => {
                        sentry.rockets = i64::try_from(&prop.value).unwrap_or_default() as u16
                    }
                    _ => {}
                }
            }
        }
    }

    pub fn handle_teleporter_entity(&mut self, entity: &PacketEntity, props: &[SendProp]) {
        const RECHARGE_TIME: SendPropIdentifier =
            SendPropIdentifier::new("DT_ObjectTeleporter", "m_flRechargeTime");
        const RECHARGE_DURATION: SendPropIdentifier =
            SendPropIdentifier::new("DT_ObjectTeleporter", "m_flCurrentRechargeDuration");
        const TIMES_USED: SendPropIdentifier =
            SendPropIdentifier::new("DT_ObjectTeleporter", "m_iTimesUsed");
        const OTHER_END: SendPropIdentifier =
            SendPropIdentifier::new("DT_ObjectTeleporter", "m_bMatchBuilding");
        const YAW_TO_EXIT: SendPropIdentifier =
            SendPropIdentifier::new("DT_ObjectTeleporter", "m_flYawToExit");
        const IS_ENTRANCE: SendPropIdentifier =
            SendPropIdentifier::new("DT_BaseObject", "m_iObjectMode");

        if entity.update_type == UpdateType::Delete {
            self.state.remove_building(entity.entity_index);
            return;
        }

        self.handle_building(entity, props, BuildingClass::Teleporter);

        let building = self
            .state
            .get_or_create_building(entity.entity_index, BuildingClass::Teleporter);

        if let Building::Teleporter(teleporter) = building {
            for prop in props {
                match prop.identifier {
                    RECHARGE_TIME => {
                        teleporter.recharge_time = f32::try_from(&prop.value).unwrap_or_default()
                    }
                    RECHARGE_DURATION => {
                        teleporter.recharge_duration =
                            f32::try_from(&prop.value).unwrap_or_default()
                    }
                    TIMES_USED => {
                        teleporter.times_used =
                            i64::try_from(&prop.value).unwrap_or_default() as u16
                    }
                    OTHER_END => {
                        teleporter.other_end =
                            EntityId::from(i64::try_from(&prop.value).unwrap_or_default() as u32)
                    }
                    YAW_TO_EXIT => {
                        teleporter.yaw_to_exit = f32::try_from(&prop.value).unwrap_or_default()
                    }
                    IS_ENTRANCE => {
                        teleporter.is_entrance = i64::try_from(&prop.value).unwrap_or_default() == 0
                    }
                    _ => {}
                }
            }
        }
    }

    pub fn handle_dispenser_entity(&mut self, entity: &PacketEntity, props: &[SendProp]) {
        const AMMO: SendPropIdentifier =
            SendPropIdentifier::new("DT_ObjectDispenser", "m_iAmmoMetal");
        const HEALING: SendPropIdentifier =
            SendPropIdentifier::new("DT_ObjectDispenser", "healing_array");

        if entity.update_type == UpdateType::Delete {
            self.state.remove_building(entity.entity_index);
            return;
        }

        self.handle_building(entity, props, BuildingClass::Dispenser);

        let building = self
            .state
            .get_or_create_building(entity.entity_index, BuildingClass::Dispenser);

        if let Building::Dispenser(dispenser) = building {
            for prop in props {
                match prop.identifier {
                    AMMO => dispenser.metal = i64::try_from(&prop.value).unwrap_or_default() as u16,
                    HEALING => {
                        let values = match &prop.value {
                            SendPropValue::Array(vec) => vec.as_slice(),
                            _ => Default::default(),
                        };

                        dispenser.healing = values
                            .iter()
                            .map(|val| UserId::from(i64::try_from(val).unwrap_or_default() as u16))
                            .collect()
                    }
                    _ => {}
                }
            }
        }
    }

    fn handle_building(&mut self, entity: &PacketEntity, props: &[SendProp], class: BuildingClass) {
        let building = self
            .state
            .get_or_create_building(entity.entity_index, class);

        const LOCAL_ORIGIN: SendPropIdentifier =
            SendPropIdentifier::new("DT_BaseEntity", "m_vecOrigin");
        const TEAM: SendPropIdentifier = SendPropIdentifier::new("DT_BaseEntity", "m_iTeamNum");
        const ANGLE: SendPropIdentifier = SendPropIdentifier::new("DT_BaseEntity", "m_angRotation");
        const SAPPED: SendPropIdentifier = SendPropIdentifier::new("DT_BaseObject", "m_bHasSapper");
        const BUILDING: SendPropIdentifier =
            SendPropIdentifier::new("DT_BaseObject", "m_bBuilding");
        const LEVEL: SendPropIdentifier =
            SendPropIdentifier::new("DT_BaseObject", "m_iUpgradeLevel");
        const BUILDER: SendPropIdentifier = SendPropIdentifier::new("DT_BaseObject", "m_hBuilder");
        const MAX_HEALTH: SendPropIdentifier =
            SendPropIdentifier::new("DT_BaseObject", "m_iMaxHealth");
        const HEALTH: SendPropIdentifier = SendPropIdentifier::new("DT_BaseObject", "m_iHealth");

        match building {
            Building::Sentry(Sentry {
                position,
                team,
                angle,
                sapped,
                builder,
                level,
                building,
                max_health,
                health,
                ..
            })
            | Building::Dispenser(Dispenser {
                position,
                team,
                angle,
                sapped,
                builder,
                level,
                building,
                max_health,
                health,
                ..
            })
            | Building::Teleporter(Teleporter {
                position,
                team,
                angle,
                sapped,
                builder,
                level,
                building,
                max_health,
                health,
                ..
            }) => {
                for prop in props {
                    match prop.identifier {
                        LOCAL_ORIGIN => {
                            *position = Vector::try_from(&prop.value).unwrap_or_default()
                        }
                        TEAM => *team = Team::new(i64::try_from(&prop.value).unwrap_or_default()),
                        ANGLE => *angle = f32::try_from(&prop.value).unwrap_or_default(),
                        SAPPED => *sapped = i64::try_from(&prop.value).unwrap_or_default() > 0,
                        BUILDING => *building = i64::try_from(&prop.value).unwrap_or_default() > 0,
                        LEVEL => *level = i64::try_from(&prop.value).unwrap_or_default() as u8,
                        BUILDER => {
                            *builder =
                                UserId::from(i64::try_from(&prop.value).unwrap_or_default() as u16)
                        }
                        MAX_HEALTH => {
                            *max_health = i64::try_from(&prop.value).unwrap_or_default() as u16
                        }
                        HEALTH => *health = i64::try_from(&prop.value).unwrap_or_default() as u16,
                        _ => {}
                    }
                }
            }
        }
    }

    fn parse_user_info(
        &mut self,
        index: usize,
        text: Option<&str>,
        data: Option<Stream>,
    ) -> ReadResult<()> {
        if let Some(user_info) =
            tf_demo_parser::demo::data::UserInfo::parse_from_string_table(index as u16, text, data)?
        {
            let ent_id = user_info.entity_id;
            self.state
                .friends_ids
                .insert(ent_id, user_info.player_info.friends_id);
            self.state
                .custom_files
                .insert(ent_id, user_info.player_info.custom_file);
            self.state
                .set_entid_to_userid(ent_id, user_info.player_info.user_id.clone());
            if let Ok(steam_id) = SteamID::from_steam3(&user_info.player_info.steam_id) {
                let id64: u64 = steam_id.into();
                self.state
                    .set_userid_to_id64(user_info.player_info.user_id.clone(), id64);
                if !user_info.player_info.name.is_empty() {
                    self.state
                        .player_names
                        .insert(id64, user_info.player_info.name.clone());
                }
                self.state
                    .user_info_history
                    .insert(id64, user_info.clone().into());
            }
            let player_info: UserInfo = user_info.into();
            let sid = SteamID::from_steam3(&player_info.steam_id)
                .map(u64::from)
                .ok();
            self.state.get_or_create_player(ent_id).info = Some(player_info);
            if let Some(sid) = sid {
                self.state.set_player_steam_id(ent_id, sid);
            }
        }

        Ok(())
    }
}
