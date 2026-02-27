use std::process::Command;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct StatsPayload {
    #[allow(dead_code)]
    pub status: String,
    pub data: Option<ActiveData>,
}

#[derive(Debug, Deserialize)]
pub struct ActiveData {
    pub five_hour: Option<UsageWindow>,
    pub seven_day: Option<UsageWindow>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct UsageWindow {
    pub utilization: f64,
    pub resets_in_minutes: Option<f64>,
    pub usage_level: String,
    pub pace: Option<PaceInfo>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PaceInfo {
    pub delta_percent: f64,
    pub expected_percent: f64,
    pub will_last_to_reset: bool,
    pub eta_minutes: Option<f64>,
}

/// Compute pace locally when the API doesn't provide it.
/// Mirrors the logic in claude-code-stats/src/types.rs.
fn compute_pace(utilization: f64, resets_in_minutes: f64, window_minutes: f64) -> Option<PaceInfo> {
    if window_minutes <= 0.0 || resets_in_minutes <= 0.0 || resets_in_minutes > window_minutes {
        return None;
    }

    let elapsed = (window_minutes - resets_in_minutes) * 60.0;
    let duration = window_minutes * 60.0;
    let time_left = resets_in_minutes * 60.0;

    let actual = utilization.clamp(0.0, 100.0);
    let expected = ((elapsed / duration) * 100.0).clamp(0.0, 100.0);

    if (elapsed == 0.0 && actual > 0.0) || expected < 3.0 {
        return None;
    }

    let delta = actual - expected;

    let (will_last_to_reset, eta_minutes) = if elapsed > 0.0 && actual > 0.0 {
        let rate = actual / elapsed;
        if rate > 0.0 {
            let remaining = (100.0 - actual).max(0.0);
            let candidate = remaining / rate;
            if candidate >= time_left {
                (true, None)
            } else {
                (false, Some(candidate / 60.0))
            }
        } else {
            (true, None)
        }
    } else if elapsed > 0.0 {
        (true, None)
    } else {
        return None;
    };

    Some(PaceInfo {
        delta_percent: delta,
        expected_percent: expected,
        will_last_to_reset,
        eta_minutes,
    })
}

/// Fill in pace data for windows that don't have it.
fn ensure_pace(window: &mut UsageWindow, window_minutes: f64) {
    if window.pace.is_some() {
        return;
    }
    if let Some(resets_in) = window.resets_in_minutes {
        window.pace = compute_pace(window.utilization, resets_in, window_minutes);
    }
}

// ── ccusage deserialization structs ──

#[derive(Deserialize)]
struct CcusageOutput {
    blocks: Vec<CcusageBlock>,
}

#[derive(Deserialize)]
struct CcusageBlock {
    #[serde(rename = "isActive")]
    is_active: bool,
    #[serde(rename = "isGap")]
    is_gap: Option<bool>,
    #[serde(rename = "totalTokens")]
    total_tokens: u64,
    #[serde(rename = "actualEndTime")]
    actual_end_time: Option<String>,
    projection: Option<CcusageProjection>,
}

#[derive(Deserialize)]
struct CcusageProjection {
    #[serde(rename = "remainingMinutes")]
    remaining_minutes: f64,
}

pub fn fetch_stats_ccusage() -> Result<ActiveData> {
    let output = Command::new("ccusage")
        .args(["blocks", "--json", "--offline"])
        .output()
        .context("failed to run ccusage")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: CcusageOutput =
        serde_json::from_str(&stdout).context("failed to parse ccusage output")?;

    // Historical max: highest totalTokens among completed (non-gap) blocks
    let max_past_tokens = parsed
        .blocks
        .iter()
        .filter(|b| !b.is_active && b.is_gap != Some(true) && b.total_tokens > 0)
        .map(|b| b.total_tokens)
        .max()
        .unwrap_or(0);

    // Find active non-gap block
    let active = parsed
        .blocks
        .iter()
        .find(|b| b.is_active && b.is_gap != Some(true))
        .context("no active block found in ccusage output")?;

    // Calibrate against historical max; fallback to active tokens on first session
    let reference = if max_past_tokens > 0 {
        max_past_tokens
    } else {
        active.total_tokens
    };

    let utilization = if reference > 0 {
        (active.total_tokens as f64 / reference as f64 * 100.0).min(100.0)
    } else {
        0.0
    };

    let resets_in_minutes = active.projection.as_ref().map(|p| p.remaining_minutes);

    let usage_level = if utilization > 90.0 {
        "danger"
    } else if utilization > 70.0 {
        "warn"
    } else {
        "normal"
    }
    .to_string();

    let mut data = ActiveData {
        five_hour: Some(UsageWindow {
            utilization,
            resets_in_minutes,
            usage_level,
            pace: None,
        }),
        seven_day: None,
        updated_at: active.actual_end_time.clone(),
    };

    if let Some(w) = &mut data.five_hour {
        ensure_pace(w, 300.0);
    }

    Ok(data)
}

pub fn fetch_stats() -> Result<ActiveData> {
    let payload_json = claude_code_stats::collect_widget_payload_json();
    let payload: StatsPayload =
        serde_json::from_str(&payload_json).context("failed to parse claude-code-stats payload")?;

    let mut data = payload
        .data
        .context("claude-code-stats returned non-active status")?;

    // Compute pace locally if not provided
    if let Some(w) = &mut data.five_hour {
        ensure_pace(w, 300.0); // 5 hours
    }
    if let Some(w) = &mut data.seven_day {
        ensure_pace(w, 10080.0); // 7 days
    }

    Ok(data)
}
