use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use demo_analysis::lib::parameters::Config;

const PROFILES_DIR: &str = "cdconfigs";

const BUILTIN_PROFILES: &[(&str, &str)] = &[
    ("default", include_str!("../cdconfigs/default.cfg")),
    ("kal", include_str!("../cdconfigs/kal.cfg")),
    ("midnight", include_str!("../cdconfigs/midnight.cfg")),
    ("idke", include_str!("../cdconfigs/idke.cfg")),
];

fn profile_path(name: &str) -> PathBuf {
    PathBuf::from(PROFILES_DIR).join(format!("{name}.cfg"))
}

// Writes any missing bundled profiles to disk without touching ones the user already has/edited.
fn seed_default_profiles() {
    if let Err(e) = fs::create_dir_all(PROFILES_DIR) {
        log::warn!("Couldn't create {PROFILES_DIR} folder, {e}");
        return;
    }
    for (name, contents) in BUILTIN_PROFILES {
        let path = profile_path(name);
        if !path.exists() {
            if let Err(e) = fs::write(&path, contents) {
                log::warn!("Couldn't seed default profile '{name}', {e}");
            }
        }
    }
}

pub fn list_profiles() -> Vec<String> {
    seed_default_profiles();
    let mut names: Vec<String> = fs::read_dir(PROFILES_DIR)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter_map(|e| {
                    let path = e.path();
                    if path.extension().and_then(|ext| ext.to_str()) == Some("cfg") {
                        path.file_stem()
                            .and_then(|s| s.to_str())
                            .map(|s| s.to_owned())
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

pub fn save_profile(name: &str, config: &Config) -> Result<()> {
    let name = sanitize_name(name)?;
    fs::create_dir_all(PROFILES_DIR)?;
    let json = serde_json::to_string_pretty(config)?;
    fs::write(profile_path(&name), json)?;
    Ok(())
}

pub fn load_profile(name: &str) -> Result<Config> {
    let content = fs::read_to_string(profile_path(name))
        .with_context(|| format!("Couldn't read profile '{name}'"))?;
    serde_json::from_str(&content).with_context(|| format!("Couldn't parse profile '{name}'"))
}

pub fn export_text(config: &Config) -> Result<String> {
    let json = serde_json::to_string_pretty(config)?;
    Ok(format!("```\n{json}\n```"))
}

pub fn import_text(text: &str) -> Result<Config> {
    let cleaned: String = text.chars().filter(|c| *c != '`').collect();
    serde_json::from_str(cleaned.trim()).context("Couldn't parse pasted config")
}

fn sanitize_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        anyhow::bail!("Profile name can't be empty");
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        anyhow::bail!("Profile name contains invalid characters");
    }
    Ok(name.to_owned())
}
