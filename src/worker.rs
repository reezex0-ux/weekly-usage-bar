use std::{
    fs,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Deserialize;

const ACTIVE_TTL_MS: u64 = 180_000;
const FINISHED_TTL_MS: u64 = 60_000;

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct WorkerStatus {
    #[serde(default)]
    pub schema: u32,
    #[serde(default)]
    pub run_id: String,
    #[serde(default)]
    pub worker: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub model_actual: Option<String>,
    #[serde(default)]
    pub fallback_from: Option<String>,
    #[serde(default)]
    pub fallback_to: Option<String>,
    #[serde(default)]
    pub phase: String,
    #[serde(default)]
    pub started_at_ms: u64,
    #[serde(default)]
    pub updated_at_ms: u64,
    #[serde(default)]
    pub tool_calls: u32,
    #[serde(default)]
    pub changed_files: u32,
    #[serde(default)]
    pub current_tool: Option<String>,
    #[serde(default)]
    pub current_command: Option<String>,
    #[serde(default)]
    pub patch_ready: bool,
    #[serde(default)]
    pub ok: Option<bool>,
}

impl WorkerStatus {
    fn is_finished(&self) -> bool {
        matches!(self.phase.as_str(), "DONE" | "FAILED")
    }

    fn visible_at(&self, now_ms: u64) -> bool {
        if self.schema != 1
            || self.run_id.is_empty()
            || self.phase.is_empty()
            || self.updated_at_ms == 0
        {
            return false;
        }
        let age = now_ms.saturating_sub(self.updated_at_ms);
        age <= if self.is_finished() {
            FINISHED_TTL_MS
        } else {
            ACTIVE_TTL_MS
        }
    }

    pub fn compact_label(&self, now_ms: u64) -> String {
        let elapsed_s = now_ms.saturating_sub(self.started_at_ms) / 1_000;
        let model = route_label(self);
        let phase = self.phase.to_ascii_uppercase();
        let mut label = format!(
            "DSH {model} {phase} {elapsed_s}s · {}f · {}t",
            self.changed_files, self.tool_calls
        );
        if let Some(command) = self
            .current_command
            .as_deref()
            .map(compact_text)
            .filter(|s| !s.is_empty())
        {
            label.push_str(" · ");
            label.push_str(&command);
        } else if let Some(tool) = self
            .current_tool
            .as_deref()
            .map(compact_text)
            .filter(|s| !s.is_empty())
        {
            label.push_str(" · ");
            label.push_str(&tool);
        }
        label
    }
}

pub fn load_visible() -> Option<WorkerStatus> {
    let bytes = fs::read(crate::settings::worker_status_path()).ok()?;
    let status: WorkerStatus = serde_json::from_slice(&bytes).ok()?;
    status.visible_at(now_millis()).then_some(status)
}

pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

fn route_label(status: &WorkerStatus) -> String {
    if let Some(to) = status.fallback_to.as_deref() {
        if let Some(from) = status.fallback_from.as_deref() {
            return format!("{}→{}", model_label(from), model_label(to));
        }
        return model_label(to);
    }
    status
        .model_actual
        .as_deref()
        .map(model_label)
        .or_else(|| status.model.as_deref().map(model_label))
        .unwrap_or_else(|| "AUTO".to_string())
}

fn model_label(value: &str) -> String {
    let value = value.trim().to_ascii_lowercase();
    match value.as_str() {
        "cloud-flash" => "AUTO".to_string(),
        "glm-5.3-flash" => "GLM5.3F".to_string(),
        "deepseek-v4.1-flash" => "DSV4.1F".to_string(),
        "deepseek-v4-flash" | "deepseek-v4-flash:0731" => "DSV4F".to_string(),
        "nemotron-3-nano:30b" => "NEMO-N".to_string(),
        _ => value.to_ascii_uppercase(),
    }
}

fn compact_text(value: &str) -> String {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    const MAX_CHARS: usize = 44;
    if compact.chars().count() <= MAX_CHARS {
        return compact;
    }
    let shortened: String = compact.chars().take(MAX_CHARS.saturating_sub(1)).collect();
    format!("{shortened}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> WorkerStatus {
        WorkerStatus {
            schema: 1,
            run_id: "run-1".to_string(),
            worker: "ollama-a".to_string(),
            model: Some("cloud-flash".to_string()),
            phase: "TEST".to_string(),
            started_at_ms: 1_000,
            updated_at_ms: 10_000,
            tool_calls: 4,
            changed_files: 3,
            current_command: Some("cargo test --all-targets".to_string()),
            ..WorkerStatus::default()
        }
    }

    #[test]
    fn active_status_expires_if_writer_disappears() {
        let status = sample();
        assert!(status.visible_at(10_000 + ACTIVE_TTL_MS));
        assert!(!status.visible_at(10_001 + ACTIVE_TTL_MS));
    }

    #[test]
    fn finished_status_has_shorter_visibility_window() {
        let mut status = sample();
        status.phase = "DONE".to_string();
        assert!(status.visible_at(10_000 + FINISHED_TTL_MS));
        assert!(!status.visible_at(10_001 + FINISHED_TTL_MS));
    }

    #[test]
    fn compact_label_uses_actual_model_and_real_metrics() {
        let mut status = sample();
        status.model_actual = Some("glm-5.3-flash".to_string());
        assert_eq!(
            status.compact_label(19_000),
            "DSH GLM5.3F TEST 18s · 3f · 4t · cargo test --all-targets"
        );
    }

    #[test]
    fn fallback_route_is_explicit() {
        let mut status = sample();
        status.model_actual = Some("deepseek-v4-flash".to_string());
        status.fallback_from = Some("glm-5.3-flash".to_string());
        status.fallback_to = Some("deepseek-v4-flash".to_string());
        assert!(status.compact_label(19_000).contains("GLM5.3F→DSV4F"));
    }
}
