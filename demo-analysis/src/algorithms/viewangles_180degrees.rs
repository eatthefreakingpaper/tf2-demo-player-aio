use std::collections::HashMap;

use anyhow::Error;
use serde_json::json;
#[cfg(test)]
use steamid_ng::SteamID;
use tf_demo_parser::ParserState;

use crate::base::cheat_analyser_base::{CheatAnalyserState, Player, PlayerState};
use crate::lib::algorithm::{CheatAlgorithm, Detection};
use crate::util::helpers::viewangle_delta;

#[derive(Clone, Copy)]
struct PreviousAngles {
    view: f32,
    pitch: f32,
}

pub struct ViewAngles180Degrees {
    previous: HashMap<u64, PreviousAngles>,
    current: HashMap<u64, PreviousAngles>,
    previous_tick: u32,
    has_previous_tick: bool,
}

impl ViewAngles180Degrees {
    pub fn new() -> Self {
        Self {
            previous: HashMap::new(),
            current: HashMap::new(),
            previous_tick: 0,
            has_previous_tick: false,
        }
    }

    fn player_id(_state: &CheatAnalyserState, player: &Player) -> Option<u64> {
        player.steam_id()
    }
}

impl CheatAlgorithm<'_> for ViewAngles180Degrees {
    fn default(&self) -> bool {
        true
    }

    fn algorithm_name(&self) -> &str {
        "viewangles_180degrees"
    }

    fn on_tick(
        &mut self,
        state: &CheatAnalyserState,
        _: &ParserState,
    ) -> Result<Vec<Detection>, Error> {
        let tick = u32::from(state.tick);
        let tick_delta = if tick == 0 || !self.has_previous_tick {
            0
        } else {
            tick.saturating_sub(self.previous_tick)
        };
        let mut detections = Vec::new();
        self.current.clear();
        self.current.reserve(state.players.len());

        for player in state.players.iter().filter(|player| {
            player.in_pvs
                && player.state == PlayerState::Alive
                && player
                    .info
                    .as_ref()
                    .is_some_and(|info| info.steam_id != "BOT")
        }) {
            let Some(player_id) = Self::player_id(state, player) else {
                continue;
            };

            if let Some(previous) = self.previous.get(&player_id) {
                let (view_delta, pitch_delta) = viewangle_delta(
                    player.view_angle,
                    player.pitch_angle,
                    previous.view,
                    previous.pitch,
                    tick_delta,
                );
                if view_delta.abs() >= 180.0 || pitch_delta.abs() >= 180.0 {
                    detections.push(Detection {
                        tick,
                        algorithm: self.algorithm_name().to_string(),
                        player: player_id,
                        data: json!({
                            "class": player.class_name(),
                            "weapon": state.get_player_weapon(player),
                            "va_delta": view_delta,
                            "pa_delta": pitch_delta,
                        }),
                    });
                }
            }

            self.current.insert(
                player_id,
                PreviousAngles {
                    view: player.view_angle,
                    pitch: player.pitch_angle,
                },
            );
        }

        std::mem::swap(&mut self.previous, &mut self.current);
        self.previous_tick = tick;
        self.has_previous_tick = true;
        Ok(detections)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tf_demo_parser::demo::data::DemoTick;
    use tf_demo_parser::demo::message::packetentities::EntityId;
    use tf_demo_parser::demo::parser::analyser::{Class, Team, UserId, UserInfo};

    #[test]
    fn detects_a_half_turn_between_consecutive_ticks() {
        let entity = EntityId::from(2u32);
        let user_id = UserId::from(3u16);
        let steam_id = u64::from(SteamID::from_steam3("[U:1:12345678]").unwrap());
        let mut state = CheatAnalyserState::default();
        state.players.push(Player {
            entity,
            class: Class::Scout,
            team: Team::Red,
            state: PlayerState::Alive,
            in_pvs: true,
            info: Some(UserInfo {
                classes: Default::default(),
                name: "ScoutMain".to_string(),
                user_id,
                steam_id: "[U:1:12345678]".to_string(),
                entity_id: entity,
                team: Team::Red,
            }),
            ..Player::default()
        });
        state.set_entid_to_userid(entity, user_id);
        state.set_userid_to_id64(user_id, steam_id);

        let parser_state = ParserState::new(24, |_| true, false);
        let mut algorithm = ViewAngles180Degrees::new();

        state.tick = DemoTick::from(100u32);
        assert!(algorithm.on_tick(&state, &parser_state).unwrap().is_empty());

        state.tick = DemoTick::from(101u32);
        state.players[0].view_angle = 180.0;
        let detections = algorithm.on_tick(&state, &parser_state).unwrap();

        assert_eq!(detections.len(), 1);
        assert_eq!(detections[0].player, steam_id);
        assert_eq!(detections[0].tick, 101);
    }
}
