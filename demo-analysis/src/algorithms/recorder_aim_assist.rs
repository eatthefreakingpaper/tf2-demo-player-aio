use std::collections::{HashMap, VecDeque};

use anyhow::Error;
use serde_json::json;
use tf_demo_parser::demo::gamevent::GameEvent;
use tf_demo_parser::demo::message::Message;
use tf_demo_parser::demo::vector::Vector;
use tf_demo_parser::{MessageType, ParserState};

use crate::base::cheat_analyser_base::{CheatAnalyserState, Player, PlayerState, UserId};
use crate::lib::algorithm::{CheatAlgorithm, Detection};
use crate::lib::parameters::{get_parameter_value, Parameter, Parameters};
use crate::util::nocrex::jankguard::JankGuard;
use crate::util::recorder_cmd::{
    player_steam_id, recorder_player, resolve_recorder_sid, RecorderCmd,
};

pub const ALGORITHM_NAME: &str = "recorder_aim_assist";

const MAX_HISTORY_TICKS: u32 = 16;
const TELEPORT_DISTANCE_SQ: f32 = 256.0 * 256.0;

#[derive(Clone, Copy, Debug)]
struct TargetSnapshot {
    tick: u32,
    position: Vector,
    ducking: bool,
    user_id: Option<UserId>,
}

#[derive(Clone, Copy, Debug)]
struct TargetAimPoint {
    target_entity: u32,
    target_sid: Option<u64>,
    target_user_id: Option<UserId>,
    target_age_ticks: u32,
    point: &'static str,
    yaw: f32,
    pitch: f32,
    radius_deg: f32,
}

#[derive(Clone, Copy, Debug)]
struct AimHotParams {
    min_movement_deg: f32,
    max_movement_deg: f32,
    min_efficiency: f32,
    max_final_error_deg: f32,
    max_final_target_radii: f32,
    target_history_ticks: u32,
    cluster_gap_ticks: u32,
}

impl Default for AimHotParams {
    fn default() -> Self {
        Self {
            min_movement_deg: 0.05,
            max_movement_deg: 45.0,
            min_efficiency: 0.80,
            max_final_error_deg: 2.0,
            max_final_target_radii: 2.5,
            target_history_ticks: 4,
            cluster_gap_ticks: 8,
        }
    }
}

impl AimHotParams {
    fn from_params(params: &Parameters) -> Self {
        Self {
            min_movement_deg: get_parameter_value::<f32>(params, "min_movement_deg"),
            max_movement_deg: get_parameter_value::<f32>(params, "max_movement_deg"),
            min_efficiency: get_parameter_value::<f32>(params, "min_efficiency"),
            max_final_error_deg: get_parameter_value::<f32>(params, "max_final_error_deg"),
            max_final_target_radii: get_parameter_value::<f32>(params, "max_final_target_radii"),
            target_history_ticks: get_parameter_value::<i32>(params, "target_history_ticks")
                .clamp(0, 16) as u32,
            cluster_gap_ticks: get_parameter_value::<i32>(params, "cluster_gap_ticks").max(0)
                as u32,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct AimSolution {
    target_entity: u32,
    target_sid: Option<u64>,
    target_user_id: Option<UserId>,
    target_age_ticks: u32,
    point: &'static str,
    before_error_deg: f32,
    after_error_deg: f32,
    after_error_radii: f32,
    movement_deg: f32,
    efficiency: f32,
    delta_yaw: f32,
    delta_pitch: f32,
}

#[derive(Clone, Copy, Debug)]
struct InputStep {
    delta_yaw: f32,
    delta_pitch: f32,
    mouse_dx: i16,
    mouse_dy: i16,
}

#[derive(Clone, Copy, Debug)]
struct SimulationEvidence {
    yaw_offset_deg: f32,
    pitch_offset_deg: f32,
    offset_deg: f32,
    offset_ratio: f32,
    mouse_dx: i16,
    mouse_dy: i16,
    eligible_step: bool,
    mismatch_steps: u32,
    movement_deg: f32,
}

#[derive(Clone, Debug)]
struct AimEvent {
    start_tick: u32,
    end_tick: u32,
    best_tick: u32,
    target_entity: u32,
    target_sid: Option<u64>,
    target_user_id: Option<UserId>,
    class_name: &'static str,
    weapon_name: String,
    correction_commands: u32,
    attack_commands: u32,
    best: AimSolution,
    input_steps: Vec<InputStep>,
}

#[derive(Clone, Copy, Debug)]
struct HurtEvidence {
    tick: u32,
    victim_sid: Option<u64>,
    victim_user_id: UserId,
}

#[derive(Default)]
struct HurtIndex {
    by_sid: HashMap<u64, Vec<u32>>,
    by_user_id: HashMap<u32, Vec<u32>>,
}

impl HurtIndex {
    fn new(hurts: &[HurtEvidence]) -> Self {
        let mut index = Self::default();
        for hurt in hurts {
            if let Some(sid) = hurt.victim_sid {
                index.by_sid.entry(sid).or_default().push(hurt.tick);
            }
            index
                .by_user_id
                .entry(u32::from(hurt.victim_user_id))
                .or_default()
                .push(hurt.tick);
        }
        index
    }

    fn confirmed(&self, event: &AimEvent) -> bool {
        let start = event.start_tick.saturating_sub(2);
        let end = event.end_tick.saturating_add(6);
        let ticks = event
            .target_sid
            .and_then(|sid| self.by_sid.get(&sid))
            .or_else(|| {
                event
                    .target_user_id
                    .and_then(|user_id| self.by_user_id.get(&u32::from(user_id)))
            });
        ticks.is_some_and(|ticks| {
            let first = ticks.partition_point(|tick| *tick < start);
            ticks.get(first).is_some_and(|tick| *tick <= end)
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct AxisSample {
    count: f32,
    delta_deg: f32,
}

pub struct RecorderAimAssist {
    recorder_sid: Option<u64>,
    previous_cmd: Option<RecorderCmd>,
    previous_weapon: Option<String>,
    jank_guard: JankGuard,
    target_history: HashMap<u32, VecDeque<TargetSnapshot>>,
    target_points: Vec<TargetAimPoint>,
    hot_params: AimHotParams,
    active_event: Option<AimEvent>,
    events: Vec<AimEvent>,
    hurt_events: Vec<HurtEvidence>,
    eligible_commands: u32,
    candidate_commands: u32,
    yaw_samples: Vec<AxisSample>,
    pitch_samples: Vec<AxisSample>,
    params: Parameters,
}

impl Default for RecorderAimAssist {
    fn default() -> Self {
        Self::new()
    }
}

impl RecorderAimAssist {
    pub fn new() -> Self {
        Self {
            recorder_sid: None,
            previous_cmd: None,
            previous_weapon: None,
            jank_guard: JankGuard::default(),
            target_history: HashMap::new(),
            target_points: Vec::new(),
            hot_params: AimHotParams::default(),
            active_event: None,
            events: Vec::new(),
            hurt_events: Vec::new(),
            eligible_commands: 0,
            candidate_commands: 0,
            yaw_samples: Vec::new(),
            pitch_samples: Vec::new(),
            params: HashMap::from([
                ("min_movement_deg".to_string(), Parameter::Float(0.05)),
                ("max_movement_deg".to_string(), Parameter::Float(45.0)),
                ("min_efficiency".to_string(), Parameter::Float(0.80)),
                ("max_final_error_deg".to_string(), Parameter::Float(2.0)),
                ("max_final_target_radii".to_string(), Parameter::Float(2.5)),
                ("target_history_ticks".to_string(), Parameter::Int(4)),
                ("cluster_gap_ticks".to_string(), Parameter::Int(8)),
                ("min_eligible_commands".to_string(), Parameter::Int(300)),
                ("min_candidate_rate".to_string(), Parameter::Float(0.008)),
                (
                    "min_simulated_offset_deg".to_string(),
                    Parameter::Float(0.05),
                ),
                (
                    "min_simulated_offset_ratio".to_string(),
                    Parameter::Float(2.5),
                ),
                (
                    "require_zero_mouse_counts".to_string(),
                    Parameter::Bool(true),
                ),
                ("min_input_mismatch_events".to_string(), Parameter::Int(12)),
                (
                    "min_input_mismatch_event_rate".to_string(),
                    Parameter::Float(0.001),
                ),
                (
                    "min_input_mismatch_event_fraction".to_string(),
                    Parameter::Float(0.25),
                ),
                ("burst_window_ticks".to_string(), Parameter::Int(64)),
                ("min_burst_mismatch_steps".to_string(), Parameter::Int(16)),
                ("min_burst_events".to_string(), Parameter::Int(2)),
                ("tracking_window_ticks".to_string(), Parameter::Int(128)),
                ("min_tracking_events".to_string(), Parameter::Int(5)),
                (
                    "max_micro_correction_deg".to_string(),
                    Parameter::Float(0.20),
                ),
                (
                    "min_micro_correction_event_fraction".to_string(),
                    Parameter::Float(0.75),
                ),
            ]),
        }
    }

    fn reset_motion(&mut self) {
        self.flush_active_event();
        self.previous_cmd = None;
        self.previous_weapon = None;
    }

    fn flush_active_event(&mut self) {
        if let Some(event) = self.active_event.take() {
            self.events.push(event);
        }
    }

    fn update_target_history(&mut self, state: &CheatAnalyserState, now: u32) {
        for player in state
            .players
            .iter()
            .filter(|player| player.in_pvs && player.state == PlayerState::Alive)
        {
            let entity = u32::from(player.entity);
            let snapshot = TargetSnapshot {
                tick: now,
                position: player.position,
                ducking: player.is_ducking(),
                user_id: player.info.as_ref().map(|info| info.user_id),
            };
            let history = self.target_history.entry(entity).or_default();

            if let Some(previous) = history.front() {
                if previous.user_id != snapshot.user_id
                    || distance_sq(previous.position, snapshot.position) > TELEPORT_DISTANCE_SQ
                {
                    history.clear();
                }
            }
            if history.front().is_some_and(|previous| previous.tick == now) {
                history.pop_front();
            }
            history.push_front(snapshot);
            while history
                .back()
                .is_some_and(|oldest| now.saturating_sub(oldest.tick) > MAX_HISTORY_TICKS)
            {
                history.pop_back();
            }
        }

        self.target_history.retain(|_, history| {
            history
                .front()
                .is_some_and(|snapshot| now.saturating_sub(snapshot.tick) <= MAX_HISTORY_TICKS)
        });
    }

    fn current_targets<'a>(
        state: &'a CheatAnalyserState,
        recorder: &'a Player,
    ) -> impl Iterator<Item = &'a Player> {
        state.players.iter().filter(move |target| {
            target.entity != recorder.entity
                && target.in_pvs
                && target.state == PlayerState::Alive
                && target.team.is_player()
                && recorder.team.is_player()
                && target.team != recorder.team
                && !target.is_cloaked()
        })
    }

    fn rebuild_target_points(
        &mut self,
        state: &CheatAnalyserState,
        recorder: &Player,
        history_ticks: u32,
    ) {
        self.target_points.clear();

        let mut eye = recorder.position;
        eye.z += if recorder.is_ducking() { 45.0 } else { 64.0 };
        let now = u32::from(state.tick);

        for target in Self::current_targets(state, recorder) {
            let entity = u32::from(target.entity);
            let Some(history) = self.target_history.get(&entity) else {
                continue;
            };
            let target_sid = player_steam_id(target);
            let target_user_id = target.info.as_ref().map(|info| info.user_id);

            for snapshot in history
                .iter()
                .filter(|snapshot| now.saturating_sub(snapshot.tick) <= history_ticks)
            {
                for (height, radius, point) in target_points(snapshot.ducking) {
                    let mut point_position = snapshot.position;
                    point_position.z += height;
                    let Some((yaw, pitch, distance)) = aim_angles(eye, point_position) else {
                        continue;
                    };
                    self.target_points.push(TargetAimPoint {
                        target_entity: entity,
                        target_sid,
                        target_user_id,
                        target_age_ticks: now.saturating_sub(snapshot.tick),
                        point,
                        yaw,
                        pitch,
                        radius_deg: radius.atan2(distance).to_degrees().max(0.15),
                    });
                }
            }
        }
    }

    fn best_solution(&self, previous: RecorderCmd, current: RecorderCmd) -> Option<AimSolution> {
        let delta_yaw = signed_angle_delta(current.yaw, previous.yaw);
        let delta_pitch = current.pitch - previous.pitch;
        let movement_deg = delta_yaw.hypot(delta_pitch);
        let mut best: Option<AimSolution> = None;

        for target in &self.target_points {
            let before_error_deg =
                angular_error(previous.yaw, previous.pitch, target.yaw, target.pitch);
            let after_error_deg =
                angular_error(current.yaw, current.pitch, target.yaw, target.pitch);
            let after_error_radii = after_error_deg / target.radius_deg;
            let efficiency = if movement_deg > f32::EPSILON {
                (before_error_deg - after_error_deg) / movement_deg
            } else {
                0.0
            };

            let solution = AimSolution {
                target_entity: target.target_entity,
                target_sid: target.target_sid,
                target_user_id: target.target_user_id,
                target_age_ticks: target.target_age_ticks,
                point: target.point,
                before_error_deg,
                after_error_deg,
                after_error_radii,
                movement_deg,
                efficiency,
                delta_yaw,
                delta_pitch,
            };

            if !self.solution_is_candidate(solution) {
                continue;
            }
            let replace = best.as_ref().is_none_or(|old| {
                solution.after_error_radii < old.after_error_radii
                    || (solution.after_error_radii == old.after_error_radii
                        && solution.after_error_deg < old.after_error_deg)
            });
            if replace {
                best = Some(solution);
            }
        }

        best
    }

    fn solution_is_candidate(&self, solution: AimSolution) -> bool {
        solution.movement_deg >= self.hot_params.min_movement_deg
            && solution.movement_deg <= self.hot_params.max_movement_deg
            && solution.efficiency >= self.hot_params.min_efficiency
            && solution.after_error_deg <= self.hot_params.max_final_error_deg
            && solution.after_error_radii <= self.hot_params.max_final_target_radii
            && solution.before_error_deg > solution.after_error_deg + 0.01
    }

    fn collect_mouse_model(&mut self, current: RecorderCmd, delta_yaw: f32, delta_pitch: f32) {
        let mouse_dx = i32::from(current.mouse_dx);
        if mouse_dx.abs() >= 8 && delta_yaw.abs() >= 0.01 && delta_yaw.abs() <= 45.0 {
            let ratio = delta_yaw / mouse_dx as f32;
            if ratio.is_finite() && (0.001..=1.0).contains(&ratio.abs()) {
                self.yaw_samples.push(AxisSample {
                    count: mouse_dx as f32,
                    delta_deg: delta_yaw,
                });
            }
        }

        let mouse_dy = i32::from(current.mouse_dy);
        if mouse_dy.abs() >= 8 && delta_pitch.abs() >= 0.01 && delta_pitch.abs() <= 45.0 {
            let ratio = delta_pitch / mouse_dy as f32;
            if ratio.is_finite() && (0.001..=1.0).contains(&ratio.abs()) {
                self.pitch_samples.push(AxisSample {
                    count: mouse_dy as f32,
                    delta_deg: delta_pitch,
                });
            }
        }
    }
    fn record_candidate(
        &mut self,
        solution: AimSolution,
        current: RecorderCmd,
        recorder: &Player,
        weapon_name: String,
    ) {
        self.candidate_commands += 1;
        let input_step = InputStep {
            delta_yaw: solution.delta_yaw,
            delta_pitch: solution.delta_pitch,
            mouse_dx: current.mouse_dx,
            mouse_dy: current.mouse_dy,
        };
        let cluster_gap = self.hot_params.cluster_gap_ticks;

        let merge = self.active_event.as_ref().is_some_and(|event| {
            event.target_entity == solution.target_entity
                && event.target_user_id == solution.target_user_id
                && event.target_sid == solution.target_sid
                && current.demo_tick.saturating_sub(event.end_tick) <= cluster_gap
                && event.weapon_name == weapon_name
        });

        if merge {
            let event = self.active_event.as_mut().expect("checked above");
            event.end_tick = current.demo_tick;
            event.correction_commands += 1;
            event.attack_commands += u32::from(current.attacking());
            event.input_steps.push(input_step);

            if better_solution(solution, event.best) {
                event.best_tick = current.demo_tick;
                event.best = solution;
            }
            return;
        }

        self.flush_active_event();
        self.active_event = Some(AimEvent {
            start_tick: current.demo_tick,
            end_tick: current.demo_tick,
            best_tick: current.demo_tick,
            target_entity: solution.target_entity,
            target_sid: solution.target_sid,
            target_user_id: solution.target_user_id,
            class_name: recorder.class_name(),
            weapon_name,
            correction_commands: 1,
            attack_commands: u32::from(current.attacking()),
            best: solution,
            input_steps: vec![input_step],
        });
    }

    fn process_command(&mut self, recorder: &Player, weapon_name: &str, current: RecorderCmd) {
        let Some(previous) = self.previous_cmd else {
            self.previous_cmd = Some(current);
            return;
        };

        if !current.is_contiguous_after(previous) {
            self.flush_active_event();
            self.previous_cmd = Some(current);
            return;
        }

        let delta_yaw = signed_angle_delta(current.yaw, previous.yaw);
        let delta_pitch = current.pitch - previous.pitch;
        if previous.pitch.abs() >= 88.0
            || current.pitch.abs() >= 88.0
            || delta_yaw.abs() > 45.0
            || delta_pitch.abs() > 45.0
        {
            self.flush_active_event();
            self.previous_cmd = Some(current);
            return;
        }

        if self.target_points.is_empty() {
            self.collect_mouse_model(current, delta_yaw, delta_pitch);
            self.previous_cmd = Some(current);
            return;
        }

        self.eligible_commands += 1;
        let mut recorded_candidate = false;

        if let Some(solution) = self.best_solution(previous, current) {
            self.record_candidate(solution, current, recorder, weapon_name.to_string());
            recorded_candidate = true;
        } else if self.active_event.as_ref().is_some_and(|event| {
            current.demo_tick.saturating_sub(event.end_tick) > self.hot_params.cluster_gap_ticks
        }) {
            self.flush_active_event();
        }

        // Candidate corrections are excluded so an aim assist cannot train the
        // mouse model toward its own injected view-angle changes.
        if !recorded_candidate {
            self.collect_mouse_model(current, delta_yaw, delta_pitch);
        }
        self.previous_cmd = Some(current);
    }
}

impl<'a> CheatAlgorithm<'a> for RecorderAimAssist {
    fn default(&self) -> bool {
        false
    }

    fn init(&mut self) -> Result<(), Error> {
        self.hot_params = AimHotParams::from_params(&self.params);
        Ok(())
    }

    fn algorithm_name(&self) -> &str {
        ALGORITHM_NAME
    }

    fn params(&mut self) -> Option<&mut Parameters> {
        Some(&mut self.params)
    }

    fn handled_messages(&self) -> Result<Vec<MessageType>, bool> {
        self.jank_guard.handled_messages()
    }

    fn on_message(
        &mut self,
        message: &Message,
        state: &CheatAnalyserState,
        parser_state: &ParserState,
        tick: tf_demo_parser::demo::data::DemoTick,
    ) -> Result<Vec<Detection>, Error> {
        self.jank_guard
            .on_message(message, state, parser_state, tick);
        if self.recorder_sid.is_none() {
            self.recorder_sid = resolve_recorder_sid(state);
        }
        let Some(recorder_sid) = self.recorder_sid else {
            return Ok(vec![]);
        };

        if let Message::GameEvent(event_message) = message {
            if let GameEvent::PlayerHurt(hurt) = &event_message.event {
                if state.get_id64_from_userid(hurt.attacker.into()) == Some(recorder_sid) {
                    self.hurt_events.push(HurtEvidence {
                        tick: u32::from(tick),
                        victim_sid: state.get_id64_from_userid(hurt.user_id.into()),
                        victim_user_id: hurt.user_id.into(),
                    });
                }
            }
        }
        Ok(vec![])
    }

    fn on_tick(
        &mut self,
        state: &CheatAnalyserState,
        _: &ParserState,
    ) -> Result<Vec<Detection>, Error> {
        self.jank_guard.on_tick(state);
        if self.recorder_sid.is_none() {
            self.recorder_sid = resolve_recorder_sid(state);
        }
        let Some(recorder_sid) = self.recorder_sid else {
            self.reset_motion();
            return Ok(vec![]);
        };
        let Some(recorder) = recorder_player(state, recorder_sid) else {
            self.reset_motion();
            return Ok(vec![]);
        };

        let now = u32::from(state.tick);
        self.update_target_history(state, now);

        let ticks_since_reset = self
            .jank_guard
            .spawned(&recorder_sid, now)
            .min(self.jank_guard.teleported(&recorder_sid, now));
        if ticks_since_reset < 60
            || recorder.state != PlayerState::Alive
            || !recorder.team.is_player()
            || has_forced_view_angles(recorder)
        {
            self.reset_motion();
            return Ok(vec![]);
        }

        let weapon_name = state.get_player_weapon(recorder);
        if !is_supported_hitscan(&weapon_name) {
            self.reset_motion();
            return Ok(vec![]);
        }
        if self.previous_weapon.as_deref() != Some(weapon_name.as_str()) {
            self.reset_motion();
            self.previous_weapon = Some(weapon_name.clone());
        }

        if !state.user_cmds.is_empty() {
            self.rebuild_target_points(state, recorder, self.hot_params.target_history_ticks);
        }

        for packet in &state.user_cmds {
            let command = RecorderCmd::from_packet(packet);
            if !command.yaw.is_finite() || !command.pitch.is_finite() {
                self.reset_motion();
                continue;
            }
            self.process_command(recorder, &weapon_name, command);
        }

        if self.active_event.as_ref().is_some_and(|event| {
            now.saturating_sub(event.end_tick) > self.hot_params.cluster_gap_ticks
        }) {
            self.flush_active_event();
        }

        Ok(vec![])
    }

    fn finish(&mut self) -> Result<Vec<Detection>, Error> {
        self.flush_active_event();

        let min_eligible =
            get_parameter_value::<i32>(&self.params, "min_eligible_commands").max(1) as u32;
        let min_candidate_rate =
            get_parameter_value::<f32>(&self.params, "min_candidate_rate").max(0.0);
        let min_simulated_offset =
            get_parameter_value::<f32>(&self.params, "min_simulated_offset_deg").max(0.0);
        let min_simulated_offset_ratio =
            get_parameter_value::<f32>(&self.params, "min_simulated_offset_ratio").max(0.0);
        let require_zero_mouse_counts =
            get_parameter_value::<bool>(&self.params, "require_zero_mouse_counts");
        let min_input_mismatch_events =
            get_parameter_value::<i32>(&self.params, "min_input_mismatch_events").max(0) as usize;
        let min_mismatch_rate =
            get_parameter_value::<f32>(&self.params, "min_input_mismatch_event_rate").max(0.0);
        let min_mismatch_fraction =
            get_parameter_value::<f32>(&self.params, "min_input_mismatch_event_fraction")
                .clamp(0.0, 1.0);
        let burst_window_ticks =
            get_parameter_value::<i32>(&self.params, "burst_window_ticks").max(0) as u32;
        let min_burst_mismatch_steps =
            get_parameter_value::<i32>(&self.params, "min_burst_mismatch_steps").max(0) as u32;
        let min_burst_events =
            get_parameter_value::<i32>(&self.params, "min_burst_events").max(0) as usize;
        let tracking_window_ticks =
            get_parameter_value::<i32>(&self.params, "tracking_window_ticks").max(0) as u32;
        let min_tracking_events =
            get_parameter_value::<i32>(&self.params, "min_tracking_events").max(0) as usize;
        let max_micro_correction =
            get_parameter_value::<f32>(&self.params, "max_micro_correction_deg").max(0.0);
        let min_micro_correction_fraction =
            get_parameter_value::<f32>(&self.params, "min_micro_correction_event_fraction")
                .clamp(0.0, 1.0);
        let candidate_rate = if self.eligible_commands == 0 {
            0.0
        } else {
            self.candidate_commands as f32 / self.eligible_commands as f32
        };

        let yaw_per_count = fit_axis_scale(&self.yaw_samples);
        let pitch_per_count = fit_axis_scale(&self.pitch_samples);
        if yaw_per_count.is_none() && pitch_per_count.is_none() {
            return Ok(vec![]);
        }

        let event_evidence: Vec<(&AimEvent, SimulationEvidence)> = self
            .events
            .iter()
            .map(|event| {
                (
                    event,
                    simulate_event(
                        event,
                        yaw_per_count,
                        pitch_per_count,
                        require_zero_mouse_counts,
                        min_simulated_offset,
                        min_simulated_offset_ratio,
                    ),
                )
            })
            .collect();
        let mismatch_events: Vec<(&AimEvent, SimulationEvidence)> = event_evidence
            .iter()
            .copied()
            .filter(|(_, evidence)| {
                evidence.offset_deg >= min_simulated_offset
                    && evidence.offset_ratio >= min_simulated_offset_ratio
            })
            .collect();
        let mismatch_event_rate = if self.eligible_commands == 0 {
            0.0
        } else {
            mismatch_events.len() as f32 / self.eligible_commands as f32
        };
        let mismatch_event_fraction = if self.events.is_empty() {
            0.0
        } else {
            mismatch_events.len() as f32 / self.events.len() as f32
        };

        // A few legitimate mouse-independent view updates can happen to converge on a
        // target. Require the recording-wide mismatches to form a recognizable assist
        // pattern instead of accepting unrelated coincidences spread across the demo.
        let micro_correction_events = mismatch_events
            .iter()
            .filter(|(_, evidence)| evidence.movement_deg <= max_micro_correction)
            .count();
        let micro_correction_fraction =
            evidence_fraction(micro_correction_events, mismatch_events.len());
        let (_, max_burst_mismatch_steps, max_burst_events) =
            max_target_window_evidence(&mismatch_events, burst_window_ticks);
        let (max_tracking_events, _, _) =
            max_target_window_evidence(&mismatch_events, tracking_window_ticks);

        let calibration_mode = min_simulated_offset == 0.0
            && min_simulated_offset_ratio == 0.0
            && min_input_mismatch_events == 0
            && min_mismatch_rate == 0.0
            && min_mismatch_fraction == 0.0;
        let assistance_profile = if calibration_mode {
            Some("calibration")
        } else {
            assistance_profile(
                micro_correction_events,
                mismatch_events.len(),
                min_micro_correction_fraction,
                max_burst_mismatch_steps,
                min_burst_mismatch_steps,
                max_burst_events,
                min_burst_events,
                max_tracking_events,
                min_tracking_events,
            )
        };
        if !aggregate_evidence_passes(
            self.eligible_commands,
            candidate_rate,
            mismatch_events.len(),
            mismatch_event_rate,
            mismatch_event_fraction,
            min_eligible,
            min_candidate_rate,
            min_input_mismatch_events,
            min_mismatch_rate,
            min_mismatch_fraction,
            calibration_mode,
        ) || assistance_profile.is_none()
        {
            return Ok(vec![]);
        }
        let assistance_profile = assistance_profile.expect("profile checked above");
        let profile_summary = json!({
            "name": assistance_profile,
            "micro_correction_event_count": micro_correction_events,
            "micro_correction_event_fraction": micro_correction_fraction,
            "max_burst_mismatch_steps": max_burst_mismatch_steps,
            "max_burst_events": max_burst_events,
            "max_tracking_events": max_tracking_events,
        });

        let Some(recorder_sid) = self.recorder_sid else {
            return Ok(vec![]);
        };
        let event_count = self.events.len();
        let mismatch_event_count = mismatch_events.len();
        let output_events = if calibration_mode {
            event_evidence
        } else {
            mismatch_events
        };
        let hurt_index = HurtIndex::new(&self.hurt_events);

        Ok(output_events
            .into_iter()
            .map(|(event, simulation)| {
                let confirmed_hit = hurt_index.confirmed(event);
                let confidence = (0.40 * event.best.efficiency.clamp(0.0, 1.0)
                    + 0.25 * (1.0 - event.best.after_error_radii / 2.5).clamp(0.0, 1.0)
                    + 0.15 * f32::from(event.attack_commands > 0)
                    + 0.10 * f32::from(confirmed_hit)
                    + 0.10 * (simulation.offset_ratio / 4.0).clamp(0.0, 1.0))
                .clamp(0.0, 1.0);

                Detection {
                    tick: event.best_tick,
                    algorithm: ALGORITHM_NAME.to_string(),
                    player: recorder_sid,
                    data: json!({
                        "target_steam_id": event.target_sid,
                        "target_user_id": event.target_user_id,
                        "target_entity": event.target_entity,
                        "class": event.class_name,
                        "weapon": event.weapon_name,
                        "event_start_tick": event.start_tick,
                        "event_end_tick": event.end_tick,
                        "correction_commands": event.correction_commands,
                        "attack_commands": event.attack_commands,
                        "confirmed_hit": confirmed_hit,
                        "target_point": event.best.point,
                        "target_history_age_ticks": event.best.target_age_ticks,
                        "initial_error_degrees": event.best.before_error_deg,
                        "final_error_degrees": event.best.after_error_deg,
                        "final_error_target_radii": event.best.after_error_radii,
                        "movement_degrees": event.best.movement_deg,
                        "path_efficiency": event.best.efficiency,
                        "simulated_yaw_offset_degrees": simulation.yaw_offset_deg,
                        "simulated_pitch_offset_degrees": simulation.pitch_offset_deg,
                        "simulated_viewangle_offset_degrees": simulation.offset_deg,
                        "simulated_offset_ratio": simulation.offset_ratio,
                        "simulated_mouse_dx": simulation.mouse_dx,
                        "simulated_mouse_dy": simulation.mouse_dy,
                        "simulated_has_eligible_step": simulation.eligible_step,
                        "simulated_mismatch_steps": simulation.mismatch_steps,
                        "recording_yaw_degrees_per_mouse_count": yaw_per_count,
                        "recording_pitch_degrees_per_mouse_count": pitch_per_count,
                        "recording_candidate_commands": self.candidate_commands,
                        "recording_eligible_commands": self.eligible_commands,
                        "recording_candidate_rate": candidate_rate,
                        "recording_event_count": event_count,
                        "recording_mismatch_event_count": mismatch_event_count,
                        "recording_mismatch_event_rate": mismatch_event_rate,
                        "recording_mismatch_event_fraction": mismatch_event_fraction,
                        "recording_assistance_profile": profile_summary.clone(),
                        "recording_requires_zero_mouse_counts": require_zero_mouse_counts,
                        "input_evidence": "target correction diverges from UserCmd mouse simulation",
                        "confidence": confidence,
                    }),
                }
            })
            .collect())
    }
}
fn evidence_fraction(count: usize, total: usize) -> f32 {
    if total == 0 {
        0.0
    } else {
        count as f32 / total as f32
    }
}

fn max_target_window_evidence(
    events: &[(&AimEvent, SimulationEvidence)],
    window_ticks: u32,
) -> (usize, u32, usize) {
    type TargetKey = (u32, Option<u64>, Option<u32>);

    let mut grouped: HashMap<TargetKey, Vec<(u32, u32)>> = HashMap::new();
    for (event, evidence) in events {
        grouped
            .entry((
                event.target_entity,
                event.target_sid,
                event.target_user_id.map(u32::from),
            ))
            .or_default()
            .push((event.best_tick, evidence.mismatch_steps));
    }

    let mut max_events = 0;
    let mut max_mismatch_steps = 0;
    let mut events_at_max_mismatch_steps = 0;

    for target_events in grouped.values_mut() {
        target_events.sort_unstable_by_key(|(tick, _)| *tick);
        let mut left = 0;
        let mut mismatch_steps = 0_u32;

        for right in 0..target_events.len() {
            mismatch_steps = mismatch_steps.saturating_add(target_events[right].1);
            while target_events[right].0.saturating_sub(target_events[left].0) > window_ticks {
                mismatch_steps = mismatch_steps.saturating_sub(target_events[left].1);
                left += 1;
            }

            let event_count = right - left + 1;
            max_events = max_events.max(event_count);
            if mismatch_steps > max_mismatch_steps
                || (mismatch_steps == max_mismatch_steps
                    && event_count > events_at_max_mismatch_steps)
            {
                max_mismatch_steps = mismatch_steps;
                events_at_max_mismatch_steps = event_count;
            }
        }
    }

    (max_events, max_mismatch_steps, events_at_max_mismatch_steps)
}

fn assistance_profile(
    micro_correction_events: usize,
    mismatch_events: usize,
    min_micro_correction_fraction: f32,
    max_burst_mismatch_steps: u32,
    min_burst_mismatch_steps: u32,
    max_burst_events: usize,
    min_burst_events: usize,
    max_tracking_events: usize,
    min_tracking_events: usize,
) -> Option<&'static str> {
    let micro_correction_fraction = evidence_fraction(micro_correction_events, mismatch_events);

    if micro_correction_fraction >= min_micro_correction_fraction {
        Some("micro_corrections")
    } else if max_burst_mismatch_steps >= min_burst_mismatch_steps
        && max_burst_events >= min_burst_events
    {
        Some("burst_corrections")
    } else if max_tracking_events >= min_tracking_events {
        Some("rapid_target_tracking")
    } else {
        None
    }
}

fn aggregate_evidence_passes(
    eligible_commands: u32,
    candidate_rate: f32,
    mismatch_events: usize,
    mismatch_event_rate: f32,
    mismatch_event_fraction: f32,
    min_eligible_commands: u32,
    min_candidate_rate: f32,
    min_input_mismatch_events: usize,
    min_input_mismatch_event_rate: f32,
    min_input_mismatch_event_fraction: f32,
    calibration_mode: bool,
) -> bool {
    eligible_commands >= min_eligible_commands
        && candidate_rate >= min_candidate_rate
        && (calibration_mode
            || (mismatch_events >= min_input_mismatch_events
                && mismatch_event_rate >= min_input_mismatch_event_rate
                && mismatch_event_fraction >= min_input_mismatch_event_fraction))
}

fn fit_axis_scale(samples: &[AxisSample]) -> Option<f32> {
    if samples.len() < 24 {
        return None;
    }

    let mut ratios: Vec<f32> = samples
        .iter()
        .map(|sample| sample.delta_deg / sample.count)
        .filter(|ratio| ratio.is_finite() && (0.001..=1.0).contains(&ratio.abs()))
        .collect();
    let initial = median(&mut ratios)?;
    let residual_limit = (initial.abs() * 2.0).max(0.04);

    let mut numerator = 0.0;
    let mut denominator = 0.0;
    let mut inliers = 0;
    for sample in samples {
        if (sample.delta_deg - initial * sample.count).abs() <= residual_limit {
            numerator += sample.count * sample.delta_deg;
            denominator += sample.count * sample.count;
            inliers += 1;
        }
    }
    if inliers < 16 || denominator <= f32::EPSILON {
        return None;
    }

    let scale = numerator / denominator;
    (scale.is_finite() && (0.001..=1.0).contains(&scale.abs())).then_some(scale)
}

fn unexplained_axis_delta(delta_deg: f32, count: i16, scale: f32) -> f32 {
    let count = f32::from(count);
    let (count_low, count_high) = if count > 0.0 {
        (count, count + 1.0)
    } else if count < 0.0 {
        (count - 1.0, count)
    } else {
        (-1.0, 1.0)
    };
    let endpoint_a = scale * count_low;
    let endpoint_b = scale * count_high;
    let lower = endpoint_a.min(endpoint_b);
    let upper = endpoint_a.max(endpoint_b);

    if delta_deg < lower {
        delta_deg - lower
    } else if delta_deg > upper {
        delta_deg - upper
    } else {
        0.0
    }
}

fn simulate_event(
    event: &AimEvent,
    yaw_per_count: Option<f32>,
    pitch_per_count: Option<f32>,
    require_zero_mouse_counts: bool,
    min_offset_deg: f32,
    min_offset_ratio: f32,
) -> SimulationEvidence {
    let per_command_quantization = yaw_per_count
        .map(f32::abs)
        .unwrap_or(0.0)
        .hypot(pitch_per_count.map(f32::abs).unwrap_or(0.0));

    let mut mismatch_steps = 0;
    let best = event
        .input_steps
        .iter()
        .filter(|step| !require_zero_mouse_counts || (step.mouse_dx == 0 && step.mouse_dy == 0))
        .map(|step| {
            let yaw_offset_deg = yaw_per_count
                .map(|scale| unexplained_axis_delta(step.delta_yaw, step.mouse_dx, scale))
                .unwrap_or(0.0);
            let pitch_offset_deg = pitch_per_count
                .map(|scale| unexplained_axis_delta(step.delta_pitch, step.mouse_dy, scale))
                .unwrap_or(0.0);
            let offset_deg = yaw_offset_deg.hypot(pitch_offset_deg);
            let offset_ratio = offset_deg / per_command_quantization.max(0.01);
            if offset_deg >= min_offset_deg && offset_ratio >= min_offset_ratio {
                mismatch_steps += 1;
            }

            SimulationEvidence {
                yaw_offset_deg,
                pitch_offset_deg,
                offset_deg,
                offset_ratio,
                mouse_dx: step.mouse_dx,
                mouse_dy: step.mouse_dy,
                eligible_step: true,
                mismatch_steps: 0,
                movement_deg: step.delta_yaw.hypot(step.delta_pitch),
            }
        })
        .max_by(|a, b| a.offset_ratio.total_cmp(&b.offset_ratio))
        .unwrap_or(SimulationEvidence {
            yaw_offset_deg: 0.0,
            pitch_offset_deg: 0.0,
            offset_deg: 0.0,
            offset_ratio: 0.0,
            mouse_dx: 0,
            mouse_dy: 0,
            eligible_step: false,
            mismatch_steps: 0,
            movement_deg: 0.0,
        });

    SimulationEvidence {
        mismatch_steps,
        ..best
    }
}
fn better_solution(candidate: AimSolution, current: AimSolution) -> bool {
    candidate.after_error_radii < current.after_error_radii
        || (candidate.after_error_radii == current.after_error_radii
            && candidate.efficiency > current.efficiency)
}

fn target_points(ducking: bool) -> [(f32, f32, &'static str); 3] {
    if ducking {
        [
            (27.0, 17.0, "lower_body"),
            (45.0, 16.0, "upper_body"),
            (57.0, 9.0, "head"),
        ]
    } else {
        [
            (38.0, 18.0, "lower_body"),
            (60.0, 17.0, "upper_body"),
            (75.0, 9.0, "head"),
        ]
    }
}

fn aim_angles(from: Vector, to: Vector) -> Option<(f32, f32, f32)> {
    let dx = to.x - from.x;
    let dy = to.y - from.y;
    let dz = to.z - from.z;
    let horizontal = dx.hypot(dy);
    let distance = horizontal.hypot(dz);
    if !distance.is_finite() || distance < 1.0 {
        return None;
    }

    let yaw = dy.atan2(dx).to_degrees();
    let pitch = -dz.atan2(horizontal).to_degrees();
    Some((yaw, pitch, distance))
}

fn signed_angle_delta(current: f32, previous: f32) -> f32 {
    let delta = (current - previous).rem_euclid(360.0);
    if delta > 180.0 {
        delta - 360.0
    } else {
        delta
    }
}

fn angular_error(yaw: f32, pitch: f32, target_yaw: f32, target_pitch: f32) -> f32 {
    signed_angle_delta(yaw, target_yaw).hypot(pitch - target_pitch)
}

fn distance_sq(a: Vector, b: Vector) -> f32 {
    let dx = a.x - b.x;
    let dy = a.y - b.y;
    let dz = a.z - b.z;
    dx * dx + dy * dy + dz * dz
}

fn median(values: &mut [f32]) -> Option<f32> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.total_cmp(b));
    let middle = values.len() / 2;
    if values.len() % 2 == 0 {
        Some((values[middle - 1] + values[middle]) * 0.5)
    } else {
        Some(values[middle])
    }
}

fn has_forced_view_angles(player: &Player) -> bool {
    const TAUNTING: u32 = 1 << 7;
    const SHIELD_CHARGE: u32 = 1 << 17;
    const HALLOWEEN_THRILLER: u32 = 1 << (54 - 32);
    const HALLOWEEN_KART: u32 = 1 << (82 - 64);
    const FREEZE_INPUT: u32 = 1 << (87 - 64);

    player.cond & (TAUNTING | SHIELD_CHARGE) != 0
        || player.cond_ex & HALLOWEEN_THRILLER != 0
        || player.cond_ex2 & (HALLOWEEN_KART | FREEZE_INPUT) != 0
}
fn is_supported_hitscan(weapon_name: &str) -> bool {
    let name = weapon_name.to_ascii_lowercase();
    [
        "scattergun",
        "force-a-nature",
        "shortstop",
        "soda popper",
        "baby face",
        "back scatter",
        "pistol",
        "winger",
        "pretty boy",
        "shotgun",
        "reserve shooter",
        "panic attack",
        "frontier justice",
        "widowmaker",
        "family business",
        "sniper rifle",
        "sydney sleeper",
        "bazaar bargain",
        "machina",
        "hitman's heatmaker",
        "awper hand",
        "classic",
        "smg",
        "cleaner's carbine",
        "revolver",
        "ambassador",
        "l'etranger",
        "enforcer",
        "diamondback",
    ]
    .iter()
    .any(|needle| name.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn angle_delta_wraps_at_180() {
        assert!((signed_angle_delta(-179.0, 179.0) - 2.0).abs() < 0.001);
        assert!((signed_angle_delta(179.0, -179.0) + 2.0).abs() < 0.001);
    }

    #[test]
    fn aim_angles_follow_source_pitch_sign() {
        let origin = Vector::default();
        let mut ahead = Vector::default();
        ahead.x = 100.0;
        assert_eq!(aim_angles(origin, ahead), Some((0.0, -0.0, 100.0)));

        let mut above = ahead;
        above.z = 100.0;
        let (_, pitch, _) = aim_angles(origin, above).unwrap();
        assert!((pitch + 45.0).abs() < 0.001);
    }

    #[test]
    fn target_size_is_distance_aware() {
        let close = 18.0_f32.atan2(100.0).to_degrees();
        let far = 18.0_f32.atan2(1000.0).to_degrees();
        assert!(close > far);
    }

    #[test]
    fn weapon_filter_keeps_discrete_hitscan_only() {
        assert!(is_supported_hitscan("Sniper Rifle"));
        assert!(is_supported_hitscan("Scattergun"));
        assert!(is_supported_hitscan("The Ambassador"));
        assert!(!is_supported_hitscan("Rocket Launcher"));
        assert!(!is_supported_hitscan("Flame Thrower"));
        assert!(!is_supported_hitscan("Minigun"));
        assert!(!is_supported_hitscan("Kukri"));
        assert!(!is_supported_hitscan("unknown"));
    }

    #[test]
    fn median_handles_odd_and_even_samples() {
        assert_eq!(median(&mut [3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&mut [4.0, 1.0, 3.0, 2.0]), Some(2.5));
        assert_eq!(median(&mut []), None);
    }

    fn event_with_steps(input_steps: Vec<InputStep>) -> AimEvent {
        let correction_commands = input_steps.len() as u32;
        let best = AimSolution {
            target_entity: 2,
            target_sid: None,
            target_user_id: None,
            target_age_ticks: 0,
            point: "head",
            before_error_deg: 2.0,
            after_error_deg: 0.1,
            after_error_radii: 0.1,
            movement_deg: 1.0,
            efficiency: 0.95,
            delta_yaw: 1.0,
            delta_pitch: 0.0,
        };
        AimEvent {
            start_tick: 10,
            end_tick: 10 + correction_commands.saturating_sub(1),
            best_tick: 10,
            target_entity: 2,
            target_sid: None,
            target_user_id: None,
            class_name: "sniper",
            weapon_name: "Sniper Rifle".to_string(),
            correction_commands,
            attack_commands: 0,
            best,
            input_steps,
        }
    }

    fn evidence_with_mismatch_steps(mismatch_steps: u32) -> SimulationEvidence {
        SimulationEvidence {
            yaw_offset_deg: 1.0,
            pitch_offset_deg: 0.0,
            offset_deg: 1.0,
            offset_ratio: 10.0,
            mouse_dx: 0,
            mouse_dy: 0,
            eligible_step: true,
            mismatch_steps,
            movement_deg: 1.0,
        }
    }

    #[test]
    fn target_window_evidence_respects_target_and_time() {
        let mut first = event_with_steps(vec![]);
        first.best_tick = 100;
        let mut second = first.clone();
        second.best_tick = 150;
        let mut third = first.clone();
        third.best_tick = 220;
        let mut other = first.clone();
        other.target_entity = 3;
        other.best_tick = 110;
        let mut other_later = other.clone();
        other_later.best_tick = 180;

        let events = vec![
            (&first, evidence_with_mismatch_steps(3)),
            (&second, evidence_with_mismatch_steps(4)),
            (&third, evidence_with_mismatch_steps(2)),
            (&other, evidence_with_mismatch_steps(20)),
            (&other_later, evidence_with_mismatch_steps(1)),
        ];

        assert_eq!(max_target_window_evidence(&events, 64), (2, 20, 1));
        assert_eq!(max_target_window_evidence(&events, 80), (2, 21, 2));
    }

    #[test]
    fn axis_scale_fit_needs_support_and_rejects_outliers() {
        let mut samples: Vec<AxisSample> = (1..=23)
            .map(|i| {
                let count = (i * 10) as f32;
                AxisSample {
                    count,
                    delta_deg: count * 0.022,
                }
            })
            .collect();
        assert!(fit_axis_scale(&samples).is_none());
        samples.push(AxisSample {
            count: 240.0,
            delta_deg: 240.0 * 0.022,
        });
        samples.extend((1..=8).map(|i| {
            let count = (i * 15) as f32;
            AxisSample {
                count,
                delta_deg: count * 0.10,
            }
        }));
        let fitted = fit_axis_scale(&samples).expect("24 coherent samples should fit");
        assert!((fitted - 0.022).abs() < 1e-5, "fitted={fitted}");
    }

    #[test]
    fn unexplained_axis_delta_respects_signed_count_quantization() {
        let scale = 0.02;
        assert_eq!(unexplained_axis_delta(0.21, 10, scale), 0.0);
        assert!((unexplained_axis_delta(0.18, 10, scale) + 0.02).abs() < 1e-6);
        assert!((unexplained_axis_delta(0.25, 10, scale) - 0.03).abs() < 1e-6);
        assert_eq!(unexplained_axis_delta(-0.21, -10, scale), 0.0);
        assert!((unexplained_axis_delta(-0.25, -10, scale) + 0.03).abs() < 1e-6);
        assert_eq!(unexplained_axis_delta(0.01, 0, scale), 0.0);
        assert!((unexplained_axis_delta(0.05, 0, scale) - 0.03).abs() < 1e-6);
        assert_eq!(unexplained_axis_delta(-0.21, 10, -scale), 0.0);
        assert!((unexplained_axis_delta(-0.25, 10, -scale) + 0.03).abs() < 1e-6);
    }

    #[test]
    fn event_simulation_uses_available_axes_independently() {
        let event = event_with_steps(vec![InputStep {
            delta_yaw: 5.0,
            delta_pitch: 2.0,
            mouse_dx: 0,
            mouse_dy: 100,
        }]);
        let yaw_only = simulate_event(&event, Some(0.02), None, false, 0.05, 2.5);
        assert!((yaw_only.yaw_offset_deg - 4.98).abs() < 1e-5);
        assert_eq!(yaw_only.pitch_offset_deg, 0.0);
        let pitch_only = simulate_event(&event, None, Some(0.02), false, 0.05, 2.5);
        assert_eq!(pitch_only.yaw_offset_deg, 0.0);
        assert_eq!(pitch_only.pitch_offset_deg, 0.0);
        assert_eq!(pitch_only.offset_deg, 0.0);
    }

    #[test]
    fn event_simulation_does_not_cancel_opposite_unexplained_steps() {
        let event = event_with_steps(vec![
            InputStep {
                delta_yaw: 1.0,
                delta_pitch: 0.0,
                mouse_dx: 0,
                mouse_dy: 0,
            },
            InputStep {
                delta_yaw: -1.0,
                delta_pitch: 0.0,
                mouse_dx: 0,
                mouse_dy: 0,
            },
        ]);
        assert_eq!(
            event
                .input_steps
                .iter()
                .map(|step| step.delta_yaw)
                .sum::<f32>(),
            0.0
        );
        let evidence = simulate_event(&event, Some(0.02), None, true, 0.05, 2.5);
        assert!((evidence.offset_deg - 0.98).abs() < 1e-5);
        assert!((evidence.yaw_offset_deg.abs() - 0.98).abs() < 1e-5);
        assert!(evidence.offset_ratio > 40.0);
    }
    #[test]
    fn assistance_profiles_reject_diffuse_legitimate_mismatches() {
        let profile = |micro, total, burst_steps, burst_events, tracking_events| {
            assistance_profile(
                micro,
                total,
                0.75,
                burst_steps,
                16,
                burst_events,
                2,
                tracking_events,
                5,
            )
        };

        // The strongest negative windows reach 12 unexplained steps in 64 ticks
        // and four same-target events in 128 ticks, both below the assist profiles.
        assert_eq!(profile(0, 19, 12, 2, 4), None);
        assert_eq!(profile(0, 207, 12, 1, 4), None);

        assert_eq!(profile(22, 24, 19, 2, 3), Some("micro_corrections"));
        assert_eq!(profile(3, 35, 24, 2, 3), Some("burst_corrections"));
        assert_eq!(profile(3, 18, 9, 3, 6), Some("rapid_target_tracking"));
        assert_eq!(profile(0, 0, 0, 0, 0), None);
    }

    #[test]
    fn aggregate_gate_separates_current_clean_and_assisted_corpora() {
        let passes = |eligible, candidate_rate, unexplained, unexplained_rate, fraction| {
            aggregate_evidence_passes(
                eligible,
                candidate_rate,
                unexplained,
                unexplained_rate,
                fraction,
                300,
                0.008,
                12,
                0.001,
                0.25,
                false,
            )
        };

        assert!(!passes(12_468, 0.013234, 1, 0.000080, 0.017));
        assert!(!passes(2_214, 0.009485, 1, 0.000452, 0.091));
        assert!(!passes(21_414, 0.038946, 31, 0.001448, 0.169));
        assert!(!passes(2_505, 0.061477, 4, 0.001597, 0.111));

        assert!(passes(12_782, 0.020576, 24, 0.001878, 0.407));
        assert!(passes(5_125, 0.045268, 35, 0.006829, 0.745));
        assert!(passes(2_209, 0.044364, 18, 0.008148, 0.563));
    }
}
