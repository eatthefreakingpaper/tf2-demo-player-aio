use std::collections::HashMap;

use anyhow::Result;
use demo_analysis::lib::algorithm::{analyse, apply_config, get_algorithms, Demo, Detection};
use demo_analysis::lib::parameters::Config;

pub async fn analyse_demo(
    path: std::path::PathBuf,
    enabled_overrides: HashMap<String, bool>,
    param_overrides: Config,
) -> Result<Vec<Detection>> {
    async_std::task::spawn_blocking(move || {
        let file = std::fs::read(&path)?;
        let demo = Demo::new(&file);

        let mut algorithms = get_algorithms();
        algorithms.retain(|a| {
            enabled_overrides
                .get(a.algorithm_name())
                .copied()
                .unwrap_or_else(|| a.default())
        });
        apply_config(&mut algorithms, &param_overrides);

        let analyser = analyse(&demo, algorithms)?;
        Ok(analyser.detections)
    })
    .await
}
