use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use demo_analysis::lib::algorithm::{effective_config, normalize_config};
use demo_analysis::lib::parameters::Config;

use crate::util;

const PROFILES_DIR: &str = "cdconfigs";

const BUILTIN_PROFILES: &[(&str, &str)] = &[
    ("default", include_str!("../cdconfigs/default.cfg")),
    ("kal", include_str!("../cdconfigs/kal.cfg")),
    ("midnight", include_str!("../cdconfigs/midnight.cfg")),
    ("idke", include_str!("../cdconfigs/idke.cfg")),
];

fn profiles_dir() -> PathBuf {
    util::app_file(PROFILES_DIR)
}

fn profile_path(name: &str) -> PathBuf {
    profiles_dir().join(format!("{name}.cfg"))
}

// Writes any missing bundled profiles to disk without touching ones the user already has/edited.
fn seed_default_profiles() {
    let dir = profiles_dir();
    if let Err(e) = fs::create_dir_all(&dir) {
        log::warn!("Couldn't create {} folder, {e}", dir.display());
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
    let mut names: Vec<String> = fs::read_dir(profiles_dir())
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

// Profiles are written as the full parameter set, not just the values that differ from the
// built-in defaults. A profile holding only your edits is empty on a fresh install, so saving and
// then loading it appeared to do nothing at all.
pub fn save_profile(name: &str, overrides: &Config) -> Result<()> {
    let name = sanitize_name(name)?;
    let dir = profiles_dir();
    fs::create_dir_all(&dir)
        .with_context(|| format!("Couldn't create {} folder", dir.display()))?;
    let json = serde_json::to_string_pretty(&effective_config(overrides))?;
    fs::write(profile_path(&name), json)
        .with_context(|| format!("Couldn't write profile '{name}'"))?;
    Ok(())
}

// Returns the profile's parameters along with notes about anything in the file that no algorithm
// recognises, so a profile that matches nothing reports that instead of loading as a no-op.
pub fn load_profile(name: &str) -> Result<(Config, Vec<String>)> {
    let content = fs::read_to_string(profile_path(name))
        .with_context(|| format!("Couldn't read profile '{name}'"))?;
    // strip_fences also drops a leading byte order mark, which editors on Windows like to add.
    let config: Config = serde_json::from_str(strip_fences(&content))
        .with_context(|| format!("Couldn't parse profile '{name}'"))?;
    Ok(normalize_config(&config))
}

pub fn export_text(overrides: &Config) -> Result<String> {
    let json = serde_json::to_string_pretty(&effective_config(overrides))?;
    Ok(format!("```\n{json}\n```"))
}

pub fn import_text(text: &str) -> Result<(Config, Vec<String>)> {
    let config: Config = serde_json::from_str(strip_fences(text)).context(
        "Couldn't parse pasted config, it needs to be JSON like {\"nocrex/aimsnap\": {\"noise_max\": 2.5}}",
    )?;
    let (config, warnings) = normalize_config(&config);
    if config.is_empty() {
        anyhow::bail!("Pasted config didn't contain any parameters this build knows about");
    }
    Ok((config, warnings))
}

// Pasted configs usually arrive wrapped in a markdown code fence, sometimes with a language tag
// on the opening line. Drop the fences and anything outside the outermost JSON object.
fn strip_fences(text: &str) -> &str {
    let text = text.trim().trim_matches('`').trim();
    let text = text.strip_prefix("json").unwrap_or(text).trim();
    match (text.find('{'), text.rfind('}')) {
        (Some(start), Some(end)) if end > start => &text[start..=end],
        _ => text,
    }
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
