use std::collections::HashMap;

use anyhow::Result;
use demo_analysis::lib::algorithm::{analyse_multithreaded, apply_config, get_algorithms, Detection};
use demo_analysis::lib::parameters::Config;

pub fn analyse_demo(
    path: std::path::PathBuf,
    enabled_overrides: HashMap<String, bool>,
    param_overrides: Config,
    threads: usize,
    progress_cb: impl Fn(u32, u32) + Sync,
) -> Result<Vec<Detection>> {
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
    Ok(analyser.detections)
}
