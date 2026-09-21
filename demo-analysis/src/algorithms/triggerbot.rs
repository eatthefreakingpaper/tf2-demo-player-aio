// Triggerbot detection over the recorder's console command stream, ported
// from the community Python script that runs demo-dumper's "inputs" mode.
// A human sends one "+attack" per press and one "-attack" per release; some
// triggerbots re-issue "+attack" while it is already held. Commands arrive
// as container-level ConsoleCmd frames with a trailing number, e.g.
// "+attack 45", so only the first whitespace-separated token is compared.
//
// Console commands belong to the player who recorded the demo, so every
// detection is attributed to the recorder (matched by the header nickname).
// The companion firewindow algorithm cross-checks those commands against
// the usercmd button state.

use anyhow::Error;
use serde_json::json;
use tf_demo_parser::ParserState;

use crate::base::cheat_analyser_base::CheatAnalyserState;
use crate::lib::algorithm::{CheatAlgorithm, Detection};

pub const ALGORITHM_NAME: &str = "triggerbot";

pub struct TriggerBot {
    next_cmd: usize,
    attack_depth: i32,
    recorder_sid: Option<u64>,
}

impl Default for TriggerBot {
    fn default() -> Self {
        Self::new()
    }
}

impl TriggerBot {
    pub fn new() -> Self {
        Self {
            next_cmd: 0,
            attack_depth: 0,
            recorder_sid: None,
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

    fn process_new_cmds(
        &mut self,
        state: &CheatAnalyserState,
        cmds: &[(tf_demo_parser::demo::data::DemoTick, String)],
    ) -> Vec<Detection> {
        let mut tick_detections = Vec::new();

        for (tick, command) in cmds {
            let tick = u32::from(*tick);
            let token = command.split_whitespace().next().unwrap_or("");
            match token {
                "+attack" => self.attack_depth += 1,
                "-attack" => self.attack_depth = (self.attack_depth - 1).max(0),
                _ => continue,
            }

            if self.attack_depth >= 2 {
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

                tick_detections.push(Detection {
                    tick,
                    algorithm: ALGORITHM_NAME.to_string(),
                    player,
                    data: json!({
                        "command": command,
                        "held": self.attack_depth,
                        "class": class,
                        "weapon": weapon,
                        "note": "consecutive +attack from the demo recorder (triggerbot)",
                    }),
                });
            }
        }

        tick_detections
    }
}

impl<'a> CheatAlgorithm<'a> for TriggerBot {
    fn default(&self) -> bool {
        true
    }

    fn algorithm_name(&self) -> &str {
        ALGORITHM_NAME
    }

    fn on_tick(
        &mut self,
        state: &CheatAnalyserState,
        _: &ParserState,
    ) -> Result<Vec<Detection>, Error> {
        self.resolve_recorder(state);
        if self.next_cmd >= state.console_cmds.len() {
            return Ok(vec![]);
        }
        let new_cmds: Vec<_> = state.console_cmds[self.next_cmd..].to_vec();
        self.next_cmd = state.console_cmds.len();
        Ok(self.process_new_cmds(state, &new_cmds))
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

    fn run(algo: &mut TriggerBot, state: &CheatAnalyserState) -> Vec<Detection> {
        let parser_state = test_parser_state();
        algo.on_tick(state, &parser_state).unwrap()
    }

    #[test]
    fn paired_attacks_never_flag() {
        let mut algo = TriggerBot::new();
        let mut state = CheatAnalyserState::default();
        recorder_player(&mut state);
        push_cmds(
            &mut state,
            &[
                (100, "+attack 45"),
                (120, "-attack 46"),
                (200, "+attack 47"),
                (220, "-attack 48"),
                (300, "+attack 49"),
                (320, "-attack 50"),
            ],
        );
        assert!(run(&mut algo, &state).is_empty());
    }

    #[test]
    fn double_attack_flags_recorder() {
        let mut algo = TriggerBot::new();
        let mut state = CheatAnalyserState::default();
        recorder_player(&mut state);
        push_cmds(
            &mut state,
            &[(100, "+attack 45"), (101, "+jump 4"), (102, "+attack 46")],
        );
        let detections = run(&mut algo, &state);
        assert_eq!(detections.len(), 1);
        assert_eq!(detections[0].tick, 102);
        assert_eq!(
            detections[0].player,
            u64::from(steamid_ng::SteamID::from_steam3("[U:1:12345678]").unwrap())
        );
        assert_eq!(detections[0].data["held"], 2);
    }

    #[test]
    fn trailing_number_is_not_part_of_the_command() {
        let mut algo = TriggerBot::new();
        let mut state = CheatAnalyserState::default();
        recorder_player(&mut state);
        // "+attack2" (secondary fire) must not count as "+attack".
        push_cmds(
            &mut state,
            &[(100, "+attack2 108"), (110, "-attack2 109")],
        );
        assert!(run(&mut algo, &state).is_empty());
    }

    #[test]
    fn repeated_ticks_do_not_reprocess_commands() {
        let mut algo = TriggerBot::new();
        let mut state = CheatAnalyserState::default();
        recorder_player(&mut state);
        push_cmds(&mut state, &[(100, "+attack 45"), (101, "+attack 46")]);
        assert_eq!(run(&mut algo, &state).len(), 1);
        assert_eq!(run(&mut algo, &state).len(), 0);
        assert!(algo.finish().unwrap().is_empty());
    }
}
