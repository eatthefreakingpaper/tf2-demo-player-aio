use std::collections::HashMap;

use anyhow::Error;
use serde_json::json;
use tf_demo_parser::ParserState;

use crate::base::cheat_analyser_base::CheatAnalyserState;
use crate::lib::algorithm::{CheatAlgorithm, Detection};
use crate::lib::parameters::{get_parameter_value, Parameter, Parameters};
use crate::util::recorder_cmd::{resolve_recorder_sid, RecorderCmd};

pub const ALGORITHM_NAME: &str = "recorder_command_manipulation";

const IN_ATTACK: u32 = 1 << 0;
const IN_ATTACK2: u32 = 1 << 11;
const AMALGAM_CRIT_SEARCH_MAX_DELTA: i64 = 4095;

#[derive(Clone, Copy, Debug)]
struct CommandNumberMismatch {
    tick: u32,
    outer_command_number: u32,
    encoded_command_number: u32,
    buttons: u32,
}

#[derive(Clone, Copy, Debug)]
struct PendingTickRewrite {
    tick: u32,
    command_number: u32,
    previous_tick_count: u32,
    rewritten_tick_count: u32,
    rewrite_step: i64,
    buttons: u32,
}

#[derive(Clone, Copy, Debug)]
struct TickRewrite {
    pending: PendingTickRewrite,
    restored_tick_count: u32,
    recovery_step: i64,
}

pub struct RecorderCommandManipulation {
    recorder_sid: Option<u64>,
    previous_cmd: Option<RecorderCmd>,
    previous_step_was_normal: bool,
    pending_tick_rewrite: Option<PendingTickRewrite>,
    command_number_mismatches: Vec<CommandNumberMismatch>,
    tick_rewrites: Vec<TickRewrite>,
    next_console_cmd: usize,
    full_update_boundaries: u32,
    command_count: u32,
    params: Parameters,
}

impl Default for RecorderCommandManipulation {
    fn default() -> Self {
        Self::new()
    }
}

impl RecorderCommandManipulation {
    pub fn new() -> Self {
        Self {
            recorder_sid: None,
            previous_cmd: None,
            previous_step_was_normal: false,
            pending_tick_rewrite: None,
            command_number_mismatches: Vec::new(),
            tick_rewrites: Vec::new(),
            next_console_cmd: 0,
            full_update_boundaries: 0,
            command_count: 0,
            params: HashMap::from([("min_tick_rewrite_pairs".to_string(), Parameter::Int(3))]),
        }
    }

    fn reset_tick_history(&mut self) {
        self.previous_cmd = None;
        self.previous_step_was_normal = false;
        self.pending_tick_rewrite = None;
    }

    fn observe_command(&mut self, current: RecorderCmd, allow_tick_history: bool) {
        self.command_count = self.command_count.saturating_add(1);

        if current.encoded_command_number != current.command_number {
            self.command_number_mismatches.push(CommandNumberMismatch {
                tick: current.demo_tick,
                outer_command_number: current.command_number,
                encoded_command_number: current.encoded_command_number,
                buttons: current.buttons,
            });
        }

        if !allow_tick_history {
            return;
        }

        let Some(previous) = self.previous_cmd else {
            self.previous_cmd = Some(current);
            self.previous_step_was_normal = false;
            return;
        };

        if !current.is_transport_contiguous_after(previous) {
            self.previous_cmd = Some(current);
            self.previous_step_was_normal = false;
            self.pending_tick_rewrite = None;
            return;
        }

        let step = signed_wrapping_delta(current.tick_count, previous.tick_count);

        if let Some(pending) = self.pending_tick_rewrite.take() {
            // A one-command historical rewrite returns to the unmodified
            // timeline on the next command. If the anomalous step was s, that
            // exact recovery step is 2 - s.
            if pending.command_number == previous.command_number && step == 2 - pending.rewrite_step
            {
                self.tick_rewrites.push(TickRewrite {
                    pending,
                    restored_tick_count: current.tick_count,
                    recovery_step: step,
                });
            }
        }

        if self.previous_step_was_normal && step <= 0 && is_attack(current.buttons) {
            self.pending_tick_rewrite = Some(PendingTickRewrite {
                tick: current.demo_tick,
                command_number: current.command_number,
                previous_tick_count: previous.tick_count,
                rewritten_tick_count: current.tick_count,
                rewrite_step: step,
                buttons: current.buttons,
            });
        }

        self.previous_step_was_normal = step == 1;
        self.previous_cmd = Some(current);
    }

    fn consume_console_boundaries(&mut self, state: &CheatAnalyserState) -> bool {
        let mut saw_full_update = false;
        while self.next_console_cmd < state.console_cmds.len() {
            let (_, command) = &state.console_cmds[self.next_console_cmd];
            if is_full_update_command(command) {
                self.full_update_boundaries = self.full_update_boundaries.saturating_add(1);
                saw_full_update = true;
            }
            self.next_console_cmd += 1;
        }
        if saw_full_update {
            self.reset_tick_history();
        }
        saw_full_update
    }
}

impl<'a> CheatAlgorithm<'a> for RecorderCommandManipulation {
    fn default(&self) -> bool {
        true
    }

    fn algorithm_name(&self) -> &str {
        ALGORITHM_NAME
    }

    fn params(&mut self) -> Option<&mut Parameters> {
        Some(&mut self.params)
    }

    fn on_tick(
        &mut self,
        state: &CheatAnalyserState,
        _: &ParserState,
    ) -> Result<Vec<Detection>, Error> {
        if self.recorder_sid.is_none() {
            self.recorder_sid = resolve_recorder_sid(state);
        }

        // cl_fullupdate is a legitimate command and never evidence. The packet
        // and UserCmd queues do not expose their relative order inside this
        // callback, so discard tick-history inference for the entire batch.
        let full_update_batch = self.consume_console_boundaries(state);
        for packet in &state.user_cmds {
            self.observe_command(RecorderCmd::from_packet(packet), !full_update_batch);
        }
        if full_update_batch {
            self.reset_tick_history();
        }

        Ok(vec![])
    }

    fn finish(&mut self) -> Result<Vec<Detection>, Error> {
        let Some(recorder_sid) = self.recorder_sid else {
            return Ok(vec![]);
        };

        let mismatch_count = self.command_number_mismatches.len();
        let rewrite_count = self.tick_rewrites.len();
        let min_rewrite_pairs =
            get_parameter_value::<i32>(&self.params, "min_tick_rewrite_pairs").max(1) as usize;
        let mut detections = Vec::new();

        for mismatch in &self.command_number_mismatches {
            let forward_delta = signed_wrapping_delta(
                mismatch.encoded_command_number,
                mismatch.outer_command_number,
            );
            let attacking = is_attack(mismatch.buttons);
            let matches_amalgam_crit_search =
                attacking && (1..=AMALGAM_CRIT_SEARCH_MAX_DELTA).contains(&forward_delta);
            detections.push(Detection {
                tick: mismatch.tick,
                algorithm: ALGORITHM_NAME.to_string(),
                player: recorder_sid,
                data: json!({
                    "signature": "command_number_override",
                    "outer_command_number": mismatch.outer_command_number,
                    "encoded_command_number": mismatch.encoded_command_number,
                    "forward_command_delta": forward_delta,
                    "buttons": mismatch.buttons,
                    "attacking": attacking,
                    "matches_amalgam_crit_search": matches_amalgam_crit_search,
                    "amalgam_crit_search_max_delta": AMALGAM_CRIT_SEARCH_MAX_DELTA,
                    "recording_command_count": self.command_count,
                    "recording_signature_count": mismatch_count,
                    "full_update_boundaries_ignored": self.full_update_boundaries,
                    "evidence": "encoded CUserCmd command_number differs from the dem_usercmd sequence written by TF2",
                    "confidence": if matches_amalgam_crit_search { 0.995 } else { 0.97 },
                }),
            });
        }

        if rewrite_count >= min_rewrite_pairs {
            let confidence = (0.91_f64 + 0.01 * rewrite_count.min(7) as f64).clamp(0.0, 0.98);
            for rewrite in &self.tick_rewrites {
                detections.push(Detection {
                    tick: rewrite.pending.tick,
                    algorithm: ALGORITHM_NAME.to_string(),
                    player: recorder_sid,
                    data: json!({
                        "signature": "historical_tick_rewrite",
                        "command_number": rewrite.pending.command_number,
                        "previous_tick_count": rewrite.pending.previous_tick_count,
                        "rewritten_tick_count": rewrite.pending.rewritten_tick_count,
                        "restored_tick_count": rewrite.restored_tick_count,
                        "rewrite_step": rewrite.pending.rewrite_step,
                        "recovery_step": rewrite.recovery_step,
                        "ticks_behind_expected": 1 - rewrite.pending.rewrite_step,
                        "buttons": rewrite.pending.buttons,
                        "recording_command_count": self.command_count,
                        "recording_signature_count": rewrite_count,
                        "minimum_signature_count": min_rewrite_pairs,
                        "full_update_boundaries_ignored": self.full_update_boundaries,
                        "evidence": "attacking UserCmd rewinds or holds tick_count for one command, then exactly restores the original timeline",
                        "confidence": confidence,
                    }),
                });
            }
        }

        Ok(detections)
    }
}

fn is_attack(buttons: u32) -> bool {
    buttons & (IN_ATTACK | IN_ATTACK2) != 0
}

fn signed_wrapping_delta(current: u32, previous: u32) -> i64 {
    i64::from(current.wrapping_sub(previous) as i32)
}

fn is_full_update_command(command: &str) -> bool {
    command
        .split(|character: char| character.is_whitespace() || character == ';')
        .any(|token| token.eq_ignore_ascii_case("cl_fullupdate"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(sequence: u32, tick_count: u32, buttons: u32) -> RecorderCmd {
        RecorderCmd {
            demo_tick: sequence,
            command_number: sequence,
            encoded_command_number: sequence,
            tick_count,
            yaw: 0.0,
            pitch: 0.0,
            buttons,
            mouse_dx: 0,
            mouse_dy: 0,
        }
    }

    fn add_rewrite_pair(
        detector: &mut RecorderCommandManipulation,
        sequence: &mut u32,
        tick_count: &mut u32,
    ) {
        *sequence += 1;
        *tick_count += 1;
        detector.observe_command(command(*sequence, *tick_count, 0), true);

        *sequence += 1;
        *tick_count -= 1;
        detector.observe_command(command(*sequence, *tick_count, IN_ATTACK), true);

        *sequence += 1;
        *tick_count += 3;
        detector.observe_command(command(*sequence, *tick_count, IN_ATTACK), true);
    }

    #[test]
    fn preserves_direct_command_number_override() {
        let mut detector = RecorderCommandManipulation::new();
        detector.recorder_sid = Some(42);
        let mut rewritten = command(100, 500, IN_ATTACK);
        rewritten.encoded_command_number = 321;
        detector.observe_command(rewritten, true);

        let detections = detector.finish().unwrap();
        assert_eq!(detections.len(), 1);
        assert_eq!(detections[0].data["signature"], "command_number_override");
        assert_eq!(detections[0].data["matches_amalgam_crit_search"], true);
    }

    #[test]
    fn repeated_attack_tick_rewrites_pass_but_one_does_not() {
        let mut detector = RecorderCommandManipulation::new();
        detector.recorder_sid = Some(42);
        let mut sequence = 1;
        let mut tick_count = 100;
        detector.observe_command(command(sequence, tick_count, 0), true);

        add_rewrite_pair(&mut detector, &mut sequence, &mut tick_count);
        assert_eq!(detector.tick_rewrites.len(), 1);
        assert!(detector.finish().unwrap().is_empty());

        let mut detector = RecorderCommandManipulation::new();
        detector.recorder_sid = Some(42);
        sequence = 1;
        tick_count = 100;
        detector.observe_command(command(sequence, tick_count, 0), true);
        for _ in 0..3 {
            add_rewrite_pair(&mut detector, &mut sequence, &mut tick_count);
        }

        let detections = detector.finish().unwrap();
        assert_eq!(detections.len(), 3);
        assert!(detections
            .iter()
            .all(|detection| detection.data["signature"] == "historical_tick_rewrite"));
    }

    #[test]
    fn non_attack_and_unpaired_tick_changes_are_ignored() {
        let mut detector = RecorderCommandManipulation::new();
        detector.observe_command(command(1, 100, 0), true);
        detector.observe_command(command(2, 101, 0), true);
        detector.observe_command(command(3, 100, 0), true);
        detector.observe_command(command(4, 103, 0), true);
        detector.observe_command(command(5, 104, 0), true);
        detector.observe_command(command(6, 103, IN_ATTACK), true);
        detector.observe_command(command(7, 105, IN_ATTACK), true);
        assert!(detector.tick_rewrites.is_empty());
    }

    #[test]
    fn full_update_is_only_a_history_boundary() {
        assert!(is_full_update_command("cl_fullupdate"));
        assert!(is_full_update_command(
            "echo before; CL_FULLUPDATE; echo after"
        ));
        assert!(!is_full_update_command("echo cl_fullupdate_test"));

        let mut detector = RecorderCommandManipulation::new();
        detector.observe_command(command(1, 100, 0), true);
        detector.observe_command(command(2, 101, 0), true);
        detector.reset_tick_history();
        detector.observe_command(command(3, 100, IN_ATTACK), true);
        detector.observe_command(command(4, 103, IN_ATTACK), true);
        assert!(detector.tick_rewrites.is_empty());
    }
}
