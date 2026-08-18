// Import algorithm struct here.
pub use crate::algorithms::{
    all_messages::AllMessages,
    viewangles_180degrees::ViewAngles180Degrees,
    viewangles_to_csv::ViewAnglesToCSV,
    write_to_file::WriteToFile,
    angle_history::AngleHistory,
    backtrack::BackTrack,
    double_tap::DoubleTap,
    nocrex:: {
        aimsnap::AimSnap, 
        angle_repeat::AngleRepeat, 
        oob_pitch::OOBPitch,
    }
};

use anyhow::Error;
use crate::{base::cheat_analyser_base::CheatAnalyserState, lib::parameters::{Config, Parameters}};
use bitbuffer::BitRead;
use serde_json::Value;
use serde::{Deserialize, Serialize};

use tf_demo_parser::{demo::{data::DemoTick, header::Header, message::Message, parser::RawPacketStream}, MessageType};

pub use tf_demo_parser::{Demo, DemoParser, Parse, ParseError, ParserState, Stream};

use crate::{base::{cheat_analyser_base::CheatAnalyser, demo_handler_base::CheatDemoHandler}, dev_print};

pub fn get_algorithms() -> Vec<Box<dyn CheatAlgorithm<'static> + Send>> {
    vec![
        Box::new(AllMessages::new()),
        Box::new(ViewAngles180Degrees::new()),
        Box::new(ViewAnglesToCSV::new()),
        Box::new(WriteToFile::new()),
        Box::new(OOBPitch::new()),
        Box::new(AngleRepeat::new()),
        Box::new(AngleHistory::new()),
        Box::new(AimSnap::new()),
        Box::new(BackTrack::new()),
        Box::new(DoubleTap::new()),
    ]
}

// Overrides each algorithm's default parameters with any matching values found in `config`.
// Unknown algorithm/parameter names in `config` (e.g. from a stale save) are ignored, and values
// are reshaped to the kind the algorithm declares so a config written by hand (`20` instead of
// `20.0`) still applies instead of blowing up when the algorithm reads it back.
pub fn apply_config<'a>(algorithms: &mut [Box<dyn CheatAlgorithm<'a> + Send>], config: &Config) {
    for algorithm in algorithms.iter_mut() {
        let name = algorithm.algorithm_name().to_string();
        let Some(overrides) = config.get(&name) else {
            continue;
        };
        let Some(params) = algorithm.params() else {
            continue;
        };
        for (param_name, value) in overrides {
            if let Some(param) = params.get_mut(param_name) {
                if let Some(coerced) = value.coerced_like(param) {
                    *param = coerced;
                }
            }
        }
    }
}

// Every algorithm's built-in parameters, as a complete config.
// This is what the settings UI shows before you touch anything, so it's also what an exported or
// saved profile has to be built from - a profile holding only the values you happened to edit
// looks empty to whoever loads it.
pub fn default_config() -> Config {
    let mut config = Config::new();
    for mut algorithm in get_algorithms() {
        let name = algorithm.algorithm_name().to_string();
        if let Some(params) = algorithm.params() {
            config.insert(name, params.clone());
        }
    }
    config
}

// `defaults` with `overrides` laid on top: the parameter set an analysis run would actually use.
pub fn effective_config(overrides: &Config) -> Config {
    let mut config = default_config();
    for (algorithm, params) in config.iter_mut() {
        let Some(algo_overrides) = overrides.get(algorithm) else {
            continue;
        };
        for (param_name, value) in algo_overrides {
            if let Some(param) = params.get_mut(param_name) {
                if let Some(coerced) = value.coerced_like(param) {
                    *param = coerced;
                }
            }
        }
    }
    config
}

// Checks a config that came from outside (a .cfg file, a pasted blob) against the algorithms that
// actually exist. Returns the config with every value reshaped to its algorithm's declared kind,
// plus a human-readable note for anything that had to be dropped - importing a config that
// silently matches nothing is the same as importing nothing at all.
pub fn normalize_config(config: &Config) -> (Config, Vec<String>) {
    let defaults = default_config();
    let mut normalized = Config::new();
    let mut warnings = Vec::new();

    for (algorithm, params) in config {
        let Some(default_params) = defaults.get(algorithm) else {
            warnings.push(format!("unknown algorithm \"{algorithm}\""));
            continue;
        };
        let mut kept = Parameters::new();
        for (param_name, value) in params {
            let Some(default_value) = default_params.get(param_name) else {
                warnings.push(format!("unknown parameter \"{algorithm}/{param_name}\""));
                continue;
            };
            match value.coerced_like(default_value) {
                Some(coerced) => {
                    kept.insert(param_name.clone(), coerced);
                }
                None => warnings.push(format!(
                    "\"{algorithm}/{param_name}\" has the wrong type for this parameter"
                )),
            }
        }
        if !kept.is_empty() {
            normalized.insert(algorithm.clone(), kept);
        }
    }

    (normalized, warnings)
}

pub fn analyse<'a>(
    demo: &Demo,
    algorithms: Vec<Box<dyn CheatAlgorithm<'a> + Send>>,
    mut progress_cb: impl FnMut(u32, u32),
) -> anyhow::Result<CheatAnalyser<'a>> {
    let mut stream = demo.get_stream();
    let header: Header = Header::read(&mut stream)?;
    let total_ticks = header.ticks;
    let mut packets = RawPacketStream::new(stream);

    let analyser = CheatAnalyser::new(algorithms);
    let mut handler = CheatDemoHandler::with_analyser(analyser);

    handler.handle_header(&header);
    let _ = handler.analyser.init();
    loop {
        let packet = packets.next(&handler.state_handler);
        let packet = match packet {
            Ok(packet) => match packet {
                Some(packet) => packet,
                None => break,
            },
            Err(e) => {
                dev_print!("ParseError: {}", e);
                continue;
            }
        };
        progress_cb(packet.tick().into(), total_ticks);
        let _ = handler.handle_packet(packet)?;
    }
    let _ = handler.analyser.finish()?;
    Ok(handler.analyser)
}

// Runs independent algorithms concurrently, each re-parsing its own copy of the demo stream.
// State-building is inherently sequential per pass, so parallelism comes from running separate
// algorithm chunks side by side rather than splitting a single pass across threads.
// Takes the raw demo bytes (rather than a `Demo`) because `Demo` wraps a `bitbuffer::Data` enum
// that can hold an `Rc`, making it `!Sync`; a `&[u8]` can be shared across scope threads safely.
pub fn analyse_multithreaded<'a>(
    demo_bytes: &[u8],
    algorithms: Vec<Box<dyn CheatAlgorithm<'a> + Send>>,
    threads: usize,
    progress_cb: impl Fn(usize, u32, u32) + Sync,
) -> anyhow::Result<CheatAnalyser<'a>> {
    let threads = threads.max(1).min(algorithms.len().max(1));
    if threads <= 1 {
        let demo = Demo::new(demo_bytes);
        return analyse(&demo, algorithms, |current, total| progress_cb(0, current, total));
    }

    let mut chunks: Vec<Vec<Box<dyn CheatAlgorithm<'a> + Send>>> =
        (0..threads).map(|_| Vec::new()).collect();
    for (i, algorithm) in algorithms.into_iter().enumerate() {
        chunks[i % threads].push(algorithm);
    }
    chunks.retain(|c| !c.is_empty());

    let results: Vec<anyhow::Result<CheatAnalyser<'a>>> = std::thread::scope(|scope| {
        let handles: Vec<_> = chunks
            .into_iter()
            .enumerate()
            .map(|(i, chunk)| {
                let progress_cb = &progress_cb;
                scope.spawn(move || {
                    let demo = Demo::new(demo_bytes);
                    analyse(&demo, chunk, |current, total| progress_cb(i, current, total))
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join()
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("an analysis thread panicked")))
            })
            .collect()
    });

    let mut merged: Option<CheatAnalyser<'a>> = None;
    for result in results {
        let analyser = result?;
        match &mut merged {
            Some(m) => m.detections.extend(analyser.detections),
            None => merged = Some(analyser),
        }
    }
    Ok(merged.expect("at least one chunk"))
}

pub trait CheatAlgorithm<'a> {
    fn default(&self) -> bool {
        panic!("default() not set for {}", std::any::type_name::<Self>());
    }

    fn algorithm_name(&self) -> &str {
        panic!("algorithm_name() not implemented for {}", std::any::type_name::<Self>());
    }

    fn params(&mut self) -> Option<&mut Parameters>{
        None
    }

    fn does_handle(&self, message_type: MessageType) -> bool {
        match self.handled_messages() {
            Ok(types) => types.contains(&message_type),
            Err(parse_all) => parse_all,
        }
    }

    // Called before any other events
    // Use this instead of ::new() when performing any non-ephemeral actions e.g. modifying files
    fn init(&mut self) -> Result<(), Error> {
        Ok(())
    }

    // Called for each tick. Passes the basic game state for the tick
    // Try the write_to_file algorithm to see what those states look like (there is one state per line)
    // cargo run -- -i demo.dem -a write_to_file
    fn on_tick(&mut self, _state: &CheatAnalyserState, _parser_state: &ParserState) -> Result<Vec<Detection>, Error> {
        Ok(vec![])
    }

    // If your algorithm needs to handle additional message types, return those types in a Vec.
    // You can return Err(true) to accept all messages, or Err(false) to reject all messages.
    fn handled_messages(&self) -> Result<Vec<MessageType>, bool> {
        Err(false)
    }

    // Called for each message received by the parser.
    // Only called for types specified in handled_messages.
    fn on_message(&mut self, _message: &Message, _state: &CheatAnalyserState, _parser_state: &ParserState, _tick: DemoTick) -> Result<Vec<Detection>, Error> {
        Ok(vec![])
    }

    // Called after all other events
    // Use for cleaning up or for aggregate analysis
    fn finish(&mut self) -> Result<Vec<Detection>, Error> {
        Ok(vec![])
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Detection {
    pub tick: u32,
    pub algorithm: String,
    pub player: u64,
    pub data: Value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lib::parameters::{get_parameter_value, Parameter};

    fn config_of(algorithm: &str, param: &str, value: Parameter) -> Config {
        let mut params = Parameters::new();
        params.insert(param.to_string(), value);
        let mut config = Config::new();
        config.insert(algorithm.to_string(), params);
        config
    }

    // Every algorithm has to be able to read every one of its own parameters back. This is the
    // panic that used to take a whole scan down: get_parameter_value unwraps the type conversion.
    #[test]
    fn defaults_round_trip_through_every_algorithm() {
        let defaults = default_config();
        assert!(!defaults.is_empty(), "no algorithm exposed any parameters");
        for mut algorithm in get_algorithms() {
            let name = algorithm.algorithm_name().to_string();
            let Some(params) = algorithm.params() else {
                continue;
            };
            for (param_name, value) in params.clone() {
                match value {
                    Parameter::Float(_) => {
                        get_parameter_value::<f32>(params, &param_name);
                    }
                    Parameter::Int(_) => {
                        get_parameter_value::<i32>(params, &param_name);
                    }
                    Parameter::Bool(_) => {
                        get_parameter_value::<bool>(params, &param_name);
                    }
                }
            }
            assert!(defaults.contains_key(&name), "{name} missing from defaults");
        }
    }

    // The original bug: a float parameter written as `20` landed in the algorithm as an Int, and
    // the algorithm's own read of it panicked mid-analysis.
    #[test]
    fn int_written_for_a_float_parameter_stays_readable() {
        let mut algorithms = get_algorithms();
        let target = algorithms
            .iter_mut()
            .find_map(|a| {
                let name = a.algorithm_name().to_string();
                let params = a.params()?;
                let param = params
                    .iter()
                    .find(|(_, v)| matches!(v, Parameter::Float(_)))?
                    .0
                    .clone();
                Some((name, param))
            })
            .expect("no algorithm has a float parameter to test with");
        let (algorithm_name, param_name) = target;

        let config = config_of(&algorithm_name, &param_name, Parameter::Int(20));
        apply_config(&mut algorithms, &config);

        let applied = algorithms
            .iter_mut()
            .find(|a| a.algorithm_name() == algorithm_name)
            .and_then(|a| a.params())
            .unwrap();
        assert_eq!(applied[&param_name], Parameter::Float(20.0));
        // Would have panicked before the fix.
        assert_eq!(get_parameter_value::<f32>(applied, &param_name), 20.0);
    }

    #[test]
    fn normalize_reports_what_it_dropped() {
        let mut config = config_of("nope/not_an_algorithm", "x", Parameter::Int(1));
        let real = default_config();
        let (algorithm_name, params) = real.iter().next().unwrap();
        let param_name = params.keys().next().unwrap();
        config.insert(algorithm_name.clone(), {
            let mut p = Parameters::new();
            p.insert(param_name.clone(), params[param_name].clone());
            p.insert("not_a_parameter".to_string(), Parameter::Int(1));
            p
        });

        let (normalized, warnings) = normalize_config(&config);
        assert!(!normalized.contains_key("nope/not_an_algorithm"));
        assert!(normalized[algorithm_name].contains_key(param_name));
        assert!(!normalized[algorithm_name].contains_key("not_a_parameter"));
        assert_eq!(warnings.len(), 2, "warnings were {warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("not_an_algorithm")));
        assert!(warnings.iter().any(|w| w.contains("not_a_parameter")));
    }

    // A profile saved from an untouched install used to hold nothing, so loading it did nothing.
    #[test]
    fn effective_config_is_complete_even_with_no_overrides() {
        let effective = effective_config(&Config::new());
        assert_eq!(effective, default_config());
        assert!(effective.values().any(|p| !p.is_empty()));
    }

    #[test]
    fn effective_config_lays_overrides_on_top() {
        let defaults = default_config();
        let (algorithm_name, params) = defaults
            .iter()
            .find(|(_, p)| p.values().any(|v| matches!(v, Parameter::Float(_))))
            .expect("no float parameter anywhere");
        let param_name = params
            .iter()
            .find(|(_, v)| matches!(v, Parameter::Float(_)))
            .unwrap()
            .0;

        let config = config_of(algorithm_name, param_name, Parameter::Float(123.5));
        let effective = effective_config(&config);
        assert_eq!(effective[algorithm_name][param_name], Parameter::Float(123.5));
        // Untouched parameters survive.
        assert_eq!(effective.len(), defaults.len());
        for (name, params) in &defaults {
            assert_eq!(effective[name].len(), params.len());
        }
    }
}
