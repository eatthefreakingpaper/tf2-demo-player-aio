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
// Unknown algorithm/parameter names in `config` (e.g. from a stale save) are ignored.
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
                *param = value.clone();
            }
        }
    }
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
            .map(|h| h.join().expect("analysis thread panicked"))
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
