use std::collections::HashMap;

use anyhow::Result;
use demo_analysis::lib::algorithm::{
    analyse_multithreaded, apply_config, get_algorithms, Detection,
};
use demo_analysis::lib::parameters::Config;

pub struct AnalysisResult {
    pub detections: Vec<Detection>,
    pub player_names: HashMap<u64, String>,
}

pub fn analyse_demo(
    path: std::path::PathBuf,
    enabled_overrides: HashMap<String, bool>,
    param_overrides: Config,
    threads: usize,
    progress_cb: impl Fn(usize, u32, u32) + Sync,
) -> Result<AnalysisResult> {
    let file = std::fs::read(&path)?;

    let mut algorithms = get_algorithms();
    algorithms.retain(|a| {
        enabled_overrides
            .get(a.algorithm_name())
            .copied()
            .unwrap_or_else(|| a.default())
    });
    apply_config(&mut algorithms, &param_overrides);

    let analyser = analyse_multithreaded(&file, algorithms, threads, progress_cb)?;
    let mut player_names = analyser.state.player_names.clone();
    for (sid, info) in &analyser.state.user_info_history {
        if !info.name.is_empty() {
            player_names
                .entry(*sid)
                .or_insert_with(|| info.name.clone());
        }
    }
    Ok(AnalysisResult {
        detections: analyser.detections,
        player_names,
    })
}
