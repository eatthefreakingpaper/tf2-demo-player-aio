use demo_analysis::lib::algorithm::Detection;
use itertools::Itertools;
use serde_json::Value;

// Every detection carries an algorithm-specific `data` blob - the pitch that tripped oob_pitch, the
// three angles and their deltas for angle_repeat, the snap deltas for aimsnap. None of it was ever
// shown, so a flagged tick was a bare number you had to take on faith. Everything below turns that
// blob into something readable without the algorithms having to agree on a shape first.

/// Human-readable rendering of one JSON value. Floats are trimmed rather than dumped at full
/// precision, because a screen full of `0.02800000086426735` helps nobody.
pub fn format_value(value: &Value) -> String {
    match value {
        Value::Null => "-".to_string(),
        Value::Bool(true) => "yes".to_string(),
        Value::Bool(false) => "no".to_string(),
        Value::Number(n) => format_number(n),
        Value::String(s) => s.clone(),
        Value::Array(items) => format!("[{}]", items.iter().map(format_value).join(", ")),
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| format!("{}: {}", clean_key(k), format_value(v)))
            .join(", "),
    }
}

fn format_number(n: &serde_json::Number) -> String {
    // Integers go through as-is; routing them via f64 would corrupt large SteamID-sized values.
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    match n.as_f64() {
        Some(f) => format_float(f),
        None => n.to_string(),
    }
}

fn format_float(f: f64) -> String {
    if !f.is_finite() {
        return f.to_string();
    }
    if f == 0.0 {
        return "0".to_string();
    }
    // Angle deltas live in the thousandths and some parameters go smaller still. Rounding those to
    // "0" would hide exactly the number the detection was opened for.
    if f.abs() < 0.0001 {
        return format!("{f:e}");
    }
    let text = format!("{f:.4}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

// backtrack and doubletap pad their field names with a growing run of zero-width spaces so the
// payload's BTreeMap iterates in the order the author wrote them. That trick has to survive - the
// map is walked in its natural order - but the padding itself must not reach the screen or the
// clipboard, where it stays invisible and still gets pasted.
fn clean_key(key: &str) -> String {
    key.replace('\u{200b}', "")
}

/// Field name / value pairs for one detection's payload, in the order the algorithm intended.
pub fn detail_pairs(data: &Value) -> Vec<(String, String)> {
    match data {
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| (clean_key(k), format_value(v)))
            .collect(),
        Value::Null => Vec::new(),
        other => vec![("value".to_string(), format_value(other))],
    }
}

/// The block shown when a detection is expanded: one `key: value` per line.
pub fn detail_block(data: &Value) -> String {
    let pairs = detail_pairs(data);
    if pairs.is_empty() {
        return "This algorithm reported no extra data for the tick.".to_string();
    }
    pairs
        .iter()
        .map(|(key, value)| format!("{key}: {value}"))
        .join("\n")
}

/// One-line gist for the collapsed row. Long payloads (aimsnap's delta list) get cut off here; the
/// expanded block and the copied report still carry the whole thing.
pub fn summary(data: &Value) -> String {
    let pairs = detail_pairs(data);
    if pairs.is_empty() {
        return "no extra data".to_string();
    }
    let mut line = pairs
        .iter()
        .take(3)
        .map(|(key, value)| format!("{key} {value}"))
        .join("  \u{b7}  ");
    if pairs.len() > 3 {
        line.push_str("  \u{b7}  ...");
    }
    truncate_chars(&line, 96)
}

fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    text.chars()
        .take(limit.saturating_sub(1))
        .collect::<String>()
        + "\u{2026}"
}

/// The plain-text block for a single player, as it lands on the clipboard.
pub fn player_report(steamid64: u64, name: Option<&str>, detections: &[Detection]) -> String {
    let header = match name {
        Some(n) if !n.is_empty() => format!("{steamid64} ({n})"),
        _ => steamid64.to_string(),
    };
    let mut out = format!("{header} - {} detection(s)\n", detections.len());
    for detection in detections {
        out.push_str(&format!(
            "  tick {}  {}\n",
            detection.tick, detection.algorithm
        ));
        for (key, value) in detail_pairs(&detection.data) {
            out.push_str(&format!("    {key}: {value}\n"));
        }
    }
    out
}

/// Everything the window found, for the "Copy all" button.
pub fn full_report(demo_name: &str, players: &[(u64, Option<String>, Vec<Detection>)]) -> String {
    let mut out = format!("Cheater detection - {demo_name}\n");
    out.push_str(&format!("{} player(s) flagged\n", players.len()));
    for (steamid64, name, detections) in players {
        out.push('\n');
        out.push_str(&player_report(*steamid64, name.as_deref(), detections));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn detection(tick: u32, algorithm: &str, data: Value) -> Detection {
        Detection {
            tick,
            algorithm: algorithm.to_string(),
            player: 76561198000000000,
            data,
        }
    }

    // The two payloads the user actually asked about: oob_pitch's angle and angle_repeat's deltas.
    #[test]
    fn oob_pitch_shows_its_angle() {
        let block = detail_block(&json!({ "pitch": 90.0, "valve_server": true }));
        assert_eq!(block, "pitch: 90
valve_server: yes");
    }

    #[test]
    fn angle_repeat_shows_every_delta() {
        let block = detail_block(&json!({
            "angle_1": 12.5,
            "angle_2": 12.528,
            "angle_3": 12.5,
            "1_2_delta": 0.028,
            "1_3_delta": 0.0,
            "ratio": 3.4,
        }));
        for needed in ["1_2_delta: 0.028", "1_3_delta: 0", "ratio: 3.4", "angle_2: 12.528"] {
            assert!(block.contains(needed), "{needed:?} missing from:
{block}");
        }
    }

    // Full f32 precision leaks into the JSON; showing it raw makes the panel unreadable.
    #[test]
    fn floats_are_trimmed_but_small_ones_survive() {
        assert_eq!(format_value(&json!(0.028000000864267349)), "0.028");
        assert_eq!(format_value(&json!(0.0)), "0");
        assert_eq!(format_value(&json!(20)), "20");
        // Below the 4-decimal cutoff, exponent form rather than a misleading "0".
        assert_eq!(format_value(&json!(0.00001)), "1e-5");
    }

    // aimsnap reports a list, angle_history reports pairs. Neither should render as JSON debris.
    #[test]
    fn arrays_and_pairs_stay_readable() {
        assert_eq!(format_value(&json!([1.5, 2.25])), "[1.5, 2.25]");
        assert_eq!(format_value(&json!([[1.0, 2.0], [3.0, 4.0]])), "[[1, 2], [3, 4]]");
    }

    #[test]
    fn a_payload_with_no_fields_says_so_instead_of_looking_broken() {
        assert!(detail_block(&Value::Null).contains("no extra data"));
        assert_eq!(summary(&Value::Null), "no extra data");
    }

    // The whole point of the copy button: the detail has to come with it.
    #[test]
    fn the_copied_report_carries_the_numbers_not_just_the_tick() {
        let report = player_report(
            76561198000000000,
            Some("someone"),
            &[
                detection(1234, "nocrex_oob_pitch", json!({ "pitch": 90.0 })),
                detection(1240, "nocrex_angle_repeat", json!({ "1_2_delta": 0.028 })),
            ],
        );
        assert!(report.contains("76561198000000000 (someone) - 2 detection(s)"));
        assert!(report.contains("tick 1234  nocrex_oob_pitch"));
        assert!(report.contains("    pitch: 90"));
        assert!(report.contains("tick 1240  nocrex_angle_repeat"));
        assert!(report.contains("    1_2_delta: 0.028"));
    }

    #[test]
    fn copy_all_covers_every_flagged_player() {
        let players = vec![
            (1u64, Some("a".to_string()), vec![detection(10, "x", json!({ "v": 1 }))]),
            (2u64, None, vec![detection(20, "y", json!({ "v": 2 }))]),
        ];
        let report = full_report("demo.dem", &players);
        assert!(report.starts_with("Cheater detection - demo.dem
2 player(s) flagged"));
        assert!(report.contains("1 (a) - 1 detection(s)"));
        assert!(report.contains("
2 - 1 detection(s)"));
        assert!(report.contains("v: 1") && report.contains("v: 2"));
    }

    // A long delta list must not blow the collapsed row width out.
    #[test]
    fn the_collapsed_summary_stays_short() {
        let data = json!({ "deltas": (0..200).map(|i| i as f64 / 7.0).collect::<Vec<_>>() });
        let line = summary(&data);
        assert!(line.chars().count() <= 96, "summary was {} chars", line.chars().count());
        // ...while the expanded block keeps everything.
        assert!(detail_block(&data).len() > line.len());
    }

    // backtrack/doubletap pad keys with zero-width spaces to control ordering. Invisible on screen,
    // but they would ride along into anything pasted out of the copy buttons.
    #[test]
    fn ordering_padding_is_stripped_but_ordering_is_kept() {
        let data = json!({
            "\u{200b}\u{200b}angle_diff": 24.28,
            "\u{200b}angle_victim": 154.83,
            "angle_attacker": 179.12,
        });
        let pairs = detail_pairs(&data);
        let keys: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["angle_attacker", "angle_victim", "angle_diff"]);

        let block = detail_block(&data);
        assert!(!block.contains('\u{200b}'), "zero-width space survived into the output");
        assert!(block.contains("angle_diff: 24.28"));
    }
}
