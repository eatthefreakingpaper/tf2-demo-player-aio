// Crit hack ("crit manipulation") detection ported from Amalgam/Unibox:
// rolls a per-player, per-weapon window over player_hurt events and flags
// when the crit rate inside a full window crosses a threshold. Crit hacks
// manipulate the shared random seed so random-crit rolls always succeed,
// which shows up as a sustained near-100% crit rate.
//
// Ported adjustments for offline demo analysis:
// - crit-boosted players (kritz, banners, runes, first blood...) have their
//   weapon history reset, exactly like the original;
// - projectile and unclassifiable weapons are skipped, exactly like the
//   original (the crit seed only decides hitscan/melee rolls);
// - headshot (custom 1) and backstab (custom 2) damage is skipped entirely:
//   those crits are guaranteed by aim, not by the crit seed, and counting
//   them would flag every competent sniper or spy.

use std::collections::{HashMap, VecDeque};

use anyhow::Error;
use serde_json::json;
use tf_demo_parser::demo::data::DemoTick;
use tf_demo_parser::demo::gameevent_gen::TeamPlayFlagEventEvent;
use tf_demo_parser::demo::gamevent::GameEvent;
use tf_demo_parser::demo::message::Message;
use tf_demo_parser::demo::parser::analyser::Team;
use tf_demo_parser::{MessageType, ParserState};

use crate::base::cheat_analyser_base::{CheatAnalyserState, Player, PlayerState};
use crate::lib::algorithm::{CheatAlgorithm, Detection};
use crate::lib::parameters::{get_parameter_value, Parameter, Parameters};
use crate::util::helpers::weapon_name_from_id_or_class;

pub const ALGORITHM_NAME: &str = "crit_hack";

const FLAG_EVENT_CAPTURE: u16 = 2;
const CTF_CAPTURE_CRIT_SECONDS: f64 = 10.0;
const DEFAULT_TICKS_PER_SECOND: f64 = 66.0;

// Weapon-name fragments whose damage comes from a projectile; the crit seed
// does not apply to them.
const PROJECTILE_MARKERS: [&str; 12] = [
    "rocket",
    "grenade",
    "sticky",
    "syringe",
    "flare",
    "huntsman",
    "fortified",
    "crossbow",
    "cleaver",
    "guillotine",
    "bison",
    "dragons",
];

#[derive(Default)]
struct WeaponHistory {
    crit_flags: VecDeque<bool>,
    crits: usize,
}

pub struct CritHack {
    // steamid64 -> weapon item def index -> rolling crit window
    history: HashMap<u64, HashMap<u16, WeaponHistory>>,
    // steamid64 -> last tick covered by the team crit bonus from a CTF capture
    ctf_capture_crit_until: HashMap<u64, u32>,
    params: Parameters,
}

impl Default for CritHack {
    fn default() -> Self {
        Self::new()
    }
}

impl CritHack {
    pub fn new() -> Self {
        Self {
            history: HashMap::new(),
            ctf_capture_crit_until: HashMap::new(),
            params: Parameters::from([
                ("window".to_string(), Parameter::Int(12)),
                ("threshold_percent".to_string(), Parameter::Float(85.0)),
            ]),
        }
    }
}

impl<'a> CheatAlgorithm<'a> for CritHack {
    fn default(&self) -> bool {
        true
    }

    fn algorithm_name(&self) -> &str {
        ALGORITHM_NAME
    }

    fn params(&mut self) -> Option<&mut Parameters> {
        Some(&mut self.params)
    }

    fn handled_messages(&self) -> Result<Vec<MessageType>, bool> {
        Ok(vec![MessageType::GameEvent])
    }

    fn on_message(
        &mut self,
        message: &Message,
        state: &CheatAnalyserState,
        _: &ParserState,
        tick: DemoTick,
    ) -> Result<Vec<Detection>, Error> {
        let event = match message {
            Message::GameEvent(msg) => &msg.event,
            _ => return Ok(vec![]),
        };
        match event {
            GameEvent::PlayerHurt(hurt) => Ok(self.process_hurt(hurt, state, tick)),
            GameEvent::TeamPlayFlagEvent(flag) => {
                self.process_flag_event(flag, state, tick);
                Ok(vec![])
            }
            _ => Ok(vec![]),
        }
    }
}

impl CritHack {
    fn player_steam_id(player: &Player) -> Option<u64> {
        player.steam_id()
    }

    fn is_ctf_map(state: &CheatAnalyserState) -> bool {
        state.header.as_ref().is_some_and(|header| {
            let map_name = header.map.rsplit(['/', '\\']).next().unwrap_or(&header.map);
            map_name
                .as_bytes()
                .get(..4)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"ctf_"))
        })
    }

    fn capture_bonus_ticks(state: &CheatAnalyserState) -> u32 {
        let ticks_per_second = state
            .header
            .as_ref()
            .filter(|header| {
                header.ticks > 0 && header.duration.is_finite() && header.duration > 0.0
            })
            .map(|header| f64::from(header.ticks) / f64::from(header.duration))
            .filter(|rate| rate.is_finite() && *rate > 0.0)
            .unwrap_or(DEFAULT_TICKS_PER_SECOND);
        (ticks_per_second * CTF_CAPTURE_CRIT_SECONDS).ceil() as u32
    }

    fn process_flag_event(
        &mut self,
        flag: &TeamPlayFlagEventEvent,
        state: &CheatAnalyserState,
        tick: DemoTick,
    ) {
        if flag.event_type != FLAG_EVENT_CAPTURE || !Self::is_ctf_map(state) {
            return;
        }

        let capturing_team = state
            .players
            .iter()
            .find(|player| u32::from(player.entity) == u32::from(flag.player))
            .map(|player| player.team)
            .filter(Team::is_player)
            // teamplay_flag_event reports the captured flag's team, so the
            // capturing team is the opposite side when player state is absent.
            .or_else(|| match Team::new(flag.team) {
                Team::Red => Some(Team::Blue),
                Team::Blue => Some(Team::Red),
                _ => None,
            });
        let Some(capturing_team) = capturing_team else {
            return;
        };

        let now = u32::from(tick);
        let until = now.saturating_add(Self::capture_bonus_ticks(state));
        for player in state
            .players
            .iter()
            .filter(|player| player.team == capturing_team && player.state == PlayerState::Alive)
        {
            let Some(steam_id) = Self::player_steam_id(player) else {
                continue;
            };
            self.ctf_capture_crit_until
                .entry(steam_id)
                .and_modify(|existing| *existing = (*existing).max(until))
                .or_insert(until);
            self.history.remove(&steam_id);
        }
    }

    fn process_hurt(
        &mut self,
        hurt: &tf_demo_parser::demo::gameevent_gen::PlayerHurtEvent,
        state: &CheatAnalyserState,
        tick: DemoTick,
    ) -> Vec<Detection> {
        if hurt.attacker == 0 || hurt.attacker == hurt.user_id {
            return vec![];
        }
        // Guaranteed-by-aim crits: headshots and backstabs.
        if matches!(hurt.custom, 1 | 2) {
            return vec![];
        }

        let Some(attacker_sid) = state.get_id64_from_userid(
            tf_demo_parser::demo::parser::analyser::UserId::from(hurt.attacker),
        ) else {
            return vec![];
        };
        let attacker = state.get_player_by_sid(attacker_sid);

        let now = u32::from(tick);
        self.ctf_capture_crit_until.retain(|_, until| now <= *until);
        if self
            .ctf_capture_crit_until
            .get(&attacker_sid)
            .is_some_and(|until| now <= *until)
        {
            self.history.remove(&attacker_sid);
            return vec![];
        }

        // While crit-boosted the crits are real: wipe that weapon's history.
        if attacker.is_some_and(|p| p.is_crit_boosted()) {
            if let Some(weapons) = self.history.get_mut(&attacker_sid) {
                weapons.remove(&hurt.weapon_id);
            }
            return vec![];
        }

        let weapon_name = weapon_name_from_id_or_class(
            Some(hurt.weapon_id),
            "",
            attacker
                .map(|p| p.class)
                .unwrap_or(tf_demo_parser::demo::parser::analyser::Class::Other),
        );
        let lower = weapon_name.to_lowercase();
        if lower == "unknown"
            || lower.is_empty()
            || PROJECTILE_MARKERS.iter().any(|m| lower.contains(m))
        {
            return vec![];
        }

        let window: usize = get_parameter_value::<i32>(&self.params, "window").max(1) as usize;
        let threshold: f32 = get_parameter_value::<f32>(&self.params, "threshold_percent");

        let entry = self
            .history
            .entry(attacker_sid)
            .or_default()
            .entry(hurt.weapon_id)
            .or_default();
        entry.crit_flags.push_back(hurt.crit);
        if hurt.crit {
            entry.crits += 1;
        }
        while entry.crit_flags.len() > window {
            if entry.crit_flags.pop_front().unwrap() {
                entry.crits -= 1;
            }
        }
        if entry.crit_flags.len() < window {
            return vec![];
        }

        let rate = entry.crits as f32 / entry.crit_flags.len() as f32 * 100.0;
        if rate < threshold {
            return vec![];
        }

        let crits = entry.crits;
        entry.crit_flags.clear();
        entry.crits = 0;

        let (class, weapon) = attacker
            .map(|p| (p.class_name(), weapon_name.clone()))
            .unwrap_or(("unknown", weapon_name.clone()));
        vec![Detection {
            tick: u32::from(tick),
            algorithm: ALGORITHM_NAME.to_string(),
            player: attacker_sid,
            data: json!({
                "weapon": weapon,
                "crits": crits,
                "window": window,
                "crit_rate": (rate * 10.0).round() / 10.0,
                "class": class,
                "note": "crit rate over a full window exceeded the threshold (crit seed manipulation)",
            }),
        }]
    }
}

#[cfg(test)]
use steamid_ng::SteamID;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::base::cheat_analyser_base::{Player, PlayerState};
    use tf_demo_parser::demo::gameevent_gen::PlayerHurtEvent;
    use tf_demo_parser::demo::header::Header;
    use tf_demo_parser::demo::message::packetentities::EntityId;
    use tf_demo_parser::demo::parser::analyser::{Class, Team, UserId, UserInfo};

    fn attacker_sid() -> u64 {
        u64::from(SteamID::from_steam3("[U:1:12345678]").unwrap())
    }

    fn test_state() -> CheatAnalyserState {
        let mut state = CheatAnalyserState::default();
        state.players = vec![Player {
            steam_id64: None,
            entity: EntityId::from(2u32),
            position: Default::default(),
            health: 125,
            max_health: 125,
            class: Class::Scout,
            team: Team::Red,
            view_angle: 0.0,
            pitch_angle: 0.0,
            state: PlayerState::Alive,
            info: Some(UserInfo {
                classes: Default::default(),
                name: "ScoutMain".to_string(),
                user_id: UserId::from(3u16),
                steam_id: "[U:1:12345678]".to_string(),
                entity_id: EntityId::from(2u32),
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
        state.set_userid_to_id64(UserId::from(3u16), attacker_sid());
        state
    }

    fn set_map(state: &mut CheatAnalyserState, map: &str) {
        state.header = Some(Header {
            demo_type: "HL2DEMO".to_string(),
            version: 4,
            protocol: 24,
            server: String::new(),
            nick: String::new(),
            map: map.to_string(),
            game: "tf".to_string(),
            duration: 100.0,
            ticks: 6600,
            frames: 6600,
            signon: 0,
        });
    }

    fn flag_capture(player: u16, flag_team: Team) -> TeamPlayFlagEventEvent {
        TeamPlayFlagEventEvent {
            player,
            carrier: 0,
            event_type: FLAG_EVENT_CAPTURE,
            home: 0,
            team: flag_team as u8,
        }
    }

    fn hurt(crit: bool, weapon_id: u16, custom: u16) -> PlayerHurtEvent {
        PlayerHurtEvent {
            user_id: 7,
            health: 100,
            attacker: 3,
            damage_amount: 20,
            custom,
            show_disguised_crit: false,
            crit,
            mini_crit: false,
            all_see_crit: false,
            weapon_id,
            bonus_effect: 0,
        }
    }

    fn run_at(
        algo: &mut CritHack,
        hurt: &PlayerHurtEvent,
        state: &CheatAnalyserState,
        tick: u32,
    ) -> Vec<Detection> {
        algo.process_hurt(hurt, state, DemoTick::from(tick))
    }

    fn run(
        algo: &mut CritHack,
        hurt: &PlayerHurtEvent,
        state: &CheatAnalyserState,
    ) -> Vec<Detection> {
        run_at(algo, hurt, state, 500)
    }

    #[test]
    fn mixed_crits_stay_silent() {
        let mut algo = CritHack::new();
        let state = test_state();
        for crit in [
            false, true, true, false, true, false, false, true, false, false, true, false,
        ] {
            assert!(run(&mut algo, &hurt(crit, 13, 0), &state).is_empty());
        }
    }

    #[test]
    fn all_crit_window_flags() {
        let mut algo = CritHack::new();
        let state = test_state();
        let mut detections = Vec::new();
        for _ in 0..12 {
            detections.extend(run(&mut algo, &hurt(true, 13, 0), &state));
        }
        assert_eq!(detections.len(), 1);
        assert_eq!(detections[0].player, attacker_sid());
        assert_eq!(detections[0].data["weapon"], "Scattergun");
        assert_eq!(detections[0].data["crits"], 12);
    }

    #[test]
    fn crit_boost_resets_history() {
        let mut algo = CritHack::new();
        let mut state = test_state();
        for _ in 0..8 {
            run(&mut algo, &hurt(true, 13, 0), &state);
        }
        // Kritz'd (TF_COND_CRITBOOSTED): history wiped, further crits ignored.
        state.players[0].cond = 1 << 11;
        for _ in 0..10 {
            assert!(run(&mut algo, &hurt(true, 13, 0), &state).is_empty());
        }
        // Boost gone: the window restarts from scratch.
        state.players[0].cond = 0;
        for _ in 0..11 {
            assert!(run(&mut algo, &hurt(true, 13, 0), &state).is_empty());
        }
        assert_eq!(run(&mut algo, &hurt(true, 13, 0), &state).len(), 1);
    }

    #[test]
    fn ctf_capture_condition_resets_history() {
        let mut algo = CritHack::new();
        let mut state = test_state();
        for _ in 0..8 {
            run(&mut algo, &hurt(true, 13, 0), &state);
        }

        state.players[0].cond_ex = 1 << (39 - 32);
        for _ in 0..10 {
            assert!(run(&mut algo, &hurt(true, 13, 0), &state).is_empty());
        }

        state.players[0].cond_ex = 0;
        for _ in 0..11 {
            assert!(run(&mut algo, &hurt(true, 13, 0), &state).is_empty());
        }
        assert_eq!(run(&mut algo, &hurt(true, 13, 0), &state).len(), 1);
    }

    #[test]
    fn ctf_capture_event_suppresses_bonus_window() {
        let mut algo = CritHack::new();
        let mut state = test_state();
        set_map(&mut state, "ctf_2fort");

        for _ in 0..8 {
            run_at(&mut algo, &hurt(true, 13, 0), &state, 100);
        }
        algo.process_flag_event(
            &flag_capture(u32::from(state.players[0].entity) as u16, Team::Blue),
            &state,
            DemoTick::from(200u32),
        );

        for _ in 0..20 {
            assert!(run_at(&mut algo, &hurt(true, 13, 0), &state, 800).is_empty());
        }

        // 66 ticks/s * 10 seconds: after tick 860 the history starts clean.
        for _ in 0..11 {
            assert!(run_at(&mut algo, &hurt(true, 13, 0), &state, 861).is_empty());
        }
        assert_eq!(run_at(&mut algo, &hurt(true, 13, 0), &state, 861).len(), 1);
    }

    #[test]
    fn flag_capture_event_is_ignored_outside_ctf_maps() {
        let mut algo = CritHack::new();
        let mut state = test_state();
        set_map(&mut state, "pl_upward");

        for _ in 0..8 {
            run_at(&mut algo, &hurt(true, 13, 0), &state, 100);
        }
        algo.process_flag_event(
            &flag_capture(u32::from(state.players[0].entity) as u16, Team::Blue),
            &state,
            DemoTick::from(200u32),
        );

        for _ in 0..3 {
            assert!(run_at(&mut algo, &hurt(true, 13, 0), &state, 201).is_empty());
        }
        assert_eq!(run_at(&mut algo, &hurt(true, 13, 0), &state, 201).len(), 1);
    }

    #[test]
    fn headshots_and_backstabs_do_not_count() {
        let mut algo = CritHack::new();
        let state = test_state();
        for _ in 0..20 {
            assert!(run(&mut algo, &hurt(true, 14, 1), &state).is_empty()); // headshot
            assert!(run(&mut algo, &hurt(true, 4, 2), &state).is_empty()); // backstab
        }
    }

    #[test]
    fn projectiles_and_unknown_weapons_skip() {
        let mut algo = CritHack::new();
        let state = test_state();
        for id in [18, 20, 39, 56, 812, 12345] {
            for _ in 0..20 {
                assert!(run(&mut algo, &hurt(true, id, 0), &state).is_empty());
            }
        }
    }
}
