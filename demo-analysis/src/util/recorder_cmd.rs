use std::collections::BTreeSet;

use steamid_ng::SteamID;
use tf_demo_parser::demo::packet::usercmd::UserCmdPacket;

use crate::base::cheat_analyser_base::{CheatAnalyserState, Player};

/// A demo recorder command reconstructed exactly as TF2 writes dem_usercmd.
///
/// TF2 encodes every recorded command against a zeroed CUserCmd. Most absent
/// optional fields therefore mean zero; command_number and tick_count are the
/// Source delta codec's special default-plus-one fields and mean one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RecorderCmd {
    pub demo_tick: u32,
    /// Sequence number written in the outer `dem_usercmd` packet header.
    pub command_number: u32,
    /// `CUserCmd::command_number` encoded inside the packet payload.
    ///
    /// Vanilla TF2 assigns this from the same sequence passed to
    /// `RecordUserInput`. Keeping both values exposes clients which rewrite the
    /// command number after `CreateMove`, such as Amalgam's crit seed search.
    pub encoded_command_number: u32,
    pub tick_count: u32,
    pub yaw: f32,
    pub pitch: f32,
    pub buttons: u32,
    pub mouse_dx: i16,
    pub mouse_dy: i16,
}

impl RecorderCmd {
    pub fn from_packet(packet: &UserCmdPacket) -> Self {
        Self {
            demo_tick: u32::from(packet.tick),
            command_number: packet.sequence_out,
            encoded_command_number: packet.cmd.command_number.unwrap_or(1),
            tick_count: packet.cmd.tick_count.unwrap_or(1),
            yaw: packet.cmd.view_angles[1].unwrap_or_default(),
            pitch: packet.cmd.view_angles[0].unwrap_or_default(),
            buttons: packet.cmd.buttons.unwrap_or_default(),
            mouse_dx: packet.cmd.mouse_dx.unwrap_or_default() as i16,
            mouse_dy: packet.cmd.mouse_dy.unwrap_or_default() as i16,
        }
    }

    pub fn attacking(self) -> bool {
        self.buttons & 1 != 0
    }

    pub fn is_contiguous_after(self, previous: Self) -> bool {
        self.is_transport_contiguous_after(previous) && self.has_expected_tick_after(previous)
    }

    /// Whether two commands are adjacent in the recorded command stream.
    ///
    /// This deliberately does not inspect `tick_count`: a cheat may rewrite
    /// that field while the demo packet sequence remains perfectly adjacent.
    pub fn is_transport_contiguous_after(self, previous: Self) -> bool {
        self.demo_tick >= previous.demo_tick
            && self.demo_tick - previous.demo_tick <= 16
            && self.command_number == previous.command_number.wrapping_add(1)
    }

    pub fn has_expected_tick_after(self, previous: Self) -> bool {
        self.tick_count == previous.tick_count.wrapping_add(1)
    }
}

pub fn player_steam_id(player: &Player) -> Option<u64> {
    player.steam_id64.or_else(|| {
        let info = player.info.as_ref()?;
        SteamID::from_steam3(&info.steam_id).map(u64::from).ok()
    })
}

pub fn recorder_player(state: &CheatAnalyserState, recorder_sid: u64) -> Option<&Player> {
    state.get_player_by_sid(recorder_sid)
}

/// Resolves the recorder only when the header nickname maps to one SteamID.
///
/// Names are checked in both the current name table and the complete user-info
/// history because the recorder may not be in PVS when an algorithm first runs.
/// Ambiguous names deliberately return None.
pub fn resolve_recorder_sid(state: &CheatAnalyserState) -> Option<u64> {
    let nickname = state.header.as_ref()?.nick.trim_end_matches('\0').trim();
    if nickname.is_empty() {
        return None;
    }

    let mut candidates = BTreeSet::new();
    for (sid, name) in &state.player_names {
        if name.trim_end_matches('\0').trim() == nickname {
            candidates.insert(*sid);
        }
    }
    for (sid, info) in &state.user_info_history {
        if info.name.trim_end_matches('\0').trim() == nickname {
            candidates.insert(*sid);
        }
    }

    if candidates.len() == 1 {
        candidates.into_iter().next()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tf_demo_parser::demo::data::DemoTick;
    use tf_demo_parser::demo::header::Header;
    use tf_demo_parser::demo::packet::usercmd::{UserCmd, UserCmdPacket};

    fn packet() -> UserCmdPacket {
        UserCmdPacket {
            tick: DemoTick::from(25),
            sequence_out: 7,
            cmd: UserCmd {
                command_number: Some(10),
                tick_count: Some(20),
                view_angles: [Some(-12.5), Some(91.25), None],
                movement: [None, None, None],
                buttons: Some(1),
                impulse: None,
                weapon_select: None,
                mouse_dx: Some((-12_i16) as u16),
                mouse_dy: Some(8),
            },
        }
    }

    #[test]
    fn decodes_null_based_fields_and_signed_mouse_counts() {
        let decoded = RecorderCmd::from_packet(&packet());
        assert_eq!(decoded.demo_tick, 25);
        assert_eq!(decoded.command_number, 7);
        assert_eq!(decoded.encoded_command_number, 10);
        assert_eq!(decoded.tick_count, 20);
        assert_eq!(decoded.yaw, 91.25);
        assert_eq!(decoded.pitch, -12.5);
        assert_eq!(decoded.buttons, 1);
        assert_eq!(decoded.mouse_dx, -12);
        assert_eq!(decoded.mouse_dy, 8);
        assert!(decoded.attacking());

        let mut empty = packet();
        empty.cmd.command_number = None;
        empty.cmd.tick_count = None;
        empty.cmd.view_angles = [None, None, None];
        empty.cmd.buttons = None;
        empty.cmd.mouse_dx = None;
        empty.cmd.mouse_dy = None;
        let decoded = RecorderCmd::from_packet(&empty);
        assert_eq!(
            (
                decoded.command_number,
                decoded.encoded_command_number,
                decoded.tick_count
            ),
            (7, 1, 1)
        );
        assert_eq!((decoded.yaw, decoded.pitch), (0.0, 0.0));
        assert_eq!(decoded.buttons, 0);
        assert_eq!((decoded.mouse_dx, decoded.mouse_dy), (0, 0));
    }

    #[test]
    fn detects_command_gaps() {
        let first = RecorderCmd::from_packet(&packet());
        let mut second_packet = packet();
        second_packet.tick = DemoTick::from(26);
        second_packet.sequence_out = 8;
        second_packet.cmd.command_number = Some(11);
        second_packet.cmd.tick_count = Some(21);
        let second = RecorderCmd::from_packet(&second_packet);
        assert!(second.is_transport_contiguous_after(first));
        assert!(second.has_expected_tick_after(first));
        assert!(second.is_contiguous_after(first));

        second_packet.sequence_out = 10;
        second_packet.cmd.command_number = Some(13);
        assert!(!RecorderCmd::from_packet(&second_packet).is_contiguous_after(first));

        second_packet.sequence_out = 8;
        second_packet.cmd.command_number = Some(11);
        second_packet.cmd.tick_count = Some(18);
        let rewritten = RecorderCmd::from_packet(&second_packet);
        assert!(rewritten.is_transport_contiguous_after(first));
        assert!(!rewritten.has_expected_tick_after(first));
        assert!(!rewritten.is_contiguous_after(first));

        second_packet.cmd.tick_count = Some(21);
        second_packet.tick = DemoTick::from(24);
        assert!(!RecorderCmd::from_packet(&second_packet).is_contiguous_after(first));
    }

    #[test]
    fn ambiguous_recorder_names_are_rejected() {
        let mut state = CheatAnalyserState::default();
        state.header = Some(Header {
            demo_type: "HL2DEMO".to_string(),
            version: 3,
            protocol: 4,
            server: String::new(),
            nick: "same name".to_string(),
            map: String::new(),
            game: "tf".to_string(),
            duration: 1.0,
            ticks: 1,
            frames: 1,
            signon: 0,
        });
        state.player_names.insert(1, "same name".to_string());
        state.player_names.insert(2, "same name".to_string());
        assert_eq!(resolve_recorder_sid(&state), None);

        state.player_names.remove(&2);
        assert_eq!(resolve_recorder_sid(&state), Some(1));
    }
}
