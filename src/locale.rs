use std::{env, fs, path::PathBuf};

use chrono::{Datelike, Timelike};
use windows_sys::Win32::Globalization::GetUserDefaultLocaleName;

use crate::model::{LimitWindow, UsageStatus};

const LOCALE_NAME_CAPACITY: usize = 85;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppLocale {
    Korean,
    Chinese,
    English,
}

impl AppLocale {
    pub fn detect() -> Self {
        codex_locale_override()
            .as_deref()
            .map(Self::from_language_tag)
            .unwrap_or_else(system_locale)
    }

    pub fn metric_text(self, window: &LimitWindow) -> String {
        let duration = self.duration_label(window.duration_minutes);
        let reset = self.reset_label(window);
        match self {
            Self::Korean => format!("{duration} {}% · {reset} 리셋", window.remaining_percent),
            Self::Chinese => format!("{duration}额度 {}% · {reset}重置", window.remaining_percent),
            Self::English => format!(
                "{duration} quota {}% · resets {reset}",
                window.remaining_percent
            ),
        }
    }

    pub fn weekly_plan_text(
        self,
        window: &LimitWindow,
        today_used: f64,
        today_budget: f64,
    ) -> String {
        let reset = self.reset_label(window);
        match self {
            Self::Korean => format!(
                "주간 {}% · 오늘 {:.0}/{:.0}% · {reset}",
                window.remaining_percent, today_used, today_budget
            ),
            Self::Chinese => format!(
                "本周 {}% · 今日 {:.0}/{:.0}% · {reset}",
                window.remaining_percent, today_used, today_budget
            ),
            Self::English => format!(
                "Week {}% · today {:.0}/{:.0}% · {reset}",
                window.remaining_percent, today_used, today_budget
            ),
        }
    }

    pub fn status_text(self, status: UsageStatus) -> &'static str {
        match (self, status) {
            (Self::Korean, UsageStatus::Connecting) => "Codex 사용량 읽는 중…",
            (Self::Korean, UsageStatus::Retrying) => "사용량을 읽지 못했습니다. 재시도 중…",
            (Self::Chinese, UsageStatus::Connecting) => "正在读取 Codex 用量…",
            (Self::Chinese, UsageStatus::Retrying) => "暂时无法读取用量，正在重试…",
            (Self::English, UsageStatus::Connecting) => "Reading Codex usage…",
            (Self::English, UsageStatus::Retrying) => "Usage unavailable. Retrying…",
        }
    }

    fn from_language_tag(tag: &str) -> Self {
        let tag = tag.trim().to_ascii_lowercase();
        if tag.starts_with("ko") {
            Self::Korean
        } else if tag.starts_with("zh") {
            Self::Chinese
        } else {
            Self::English
        }
    }

    fn duration_label(self, minutes: u64) -> String {
        match self {
            Self::Korean if minutes == 10_080 => "주간".to_string(),
            Self::Chinese if minutes == 10_080 => "1周".to_string(),
            Self::English if minutes == 10_080 => "1 week".to_string(),
            Self::Korean if minutes >= 1_440 && minutes.is_multiple_of(1_440) => {
                format!("{}일", minutes / 1_440)
            }
            Self::Chinese if minutes >= 1_440 && minutes.is_multiple_of(1_440) => {
                format!("{}天", minutes / 1_440)
            }
            Self::English if minutes >= 1_440 && minutes.is_multiple_of(1_440) => {
                plural(minutes / 1_440, "day")
            }
            Self::Korean if minutes >= 60 && minutes.is_multiple_of(60) => {
                format!("{}시간", minutes / 60)
            }
            Self::Chinese if minutes >= 60 && minutes.is_multiple_of(60) => {
                format!("{}小时", minutes / 60)
            }
            Self::English if minutes >= 60 && minutes.is_multiple_of(60) => {
                plural(minutes / 60, "hour")
            }
            Self::Korean => "현재".to_string(),
            Self::Chinese => "当前".to_string(),
            Self::English => "Current".to_string(),
        }
    }

    fn reset_label(self, window: &LimitWindow) -> String {
        match (self, window.resets_at.as_ref()) {
            (_, None) => "--".to_string(),
            (Self::Korean, Some(reset)) if window.duration_minutes >= 1_440 => {
                format!("{}/{}", reset.month(), reset.day())
            }
            (Self::Chinese, Some(reset)) if window.duration_minutes >= 1_440 => {
                format!("{}月{}日", reset.month(), reset.day())
            }
            (Self::English, Some(reset)) if window.duration_minutes >= 1_440 => {
                reset.format("%b %-d").to_string()
            }
            (_, Some(reset)) => format!("{:02}:{:02}", reset.hour(), reset.minute()),
        }
    }
}

fn plural(value: u64, unit: &str) -> String {
    if value == 1 {
        format!("{value} {unit}")
    } else {
        format!("{value} {unit}s")
    }
}

fn codex_locale_override() -> Option<String> {
    let path = codex_config_path()?;
    let contents = fs::read_to_string(path).ok()?;
    contents.lines().find_map(parse_locale_override)
}

fn codex_config_path() -> Option<PathBuf> {
    if let Some(home) = env::var_os("CODEX_HOME") {
        return Some(PathBuf::from(home).join("config.toml"));
    }
    env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join(".codex/config.toml"))
}

fn parse_locale_override(line: &str) -> Option<String> {
    let line = line.trim();
    if line.starts_with('#') {
        return None;
    }
    let (key, value) = line.split_once('=')?;
    if key.trim() != "localeOverride" {
        return None;
    }
    let value = value.trim();
    let quote = value.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let remainder = &value[quote.len_utf8()..];
    let end = remainder.find(quote)?;
    let locale = remainder[..end].trim();
    (!locale.is_empty()).then(|| locale.to_string())
}

fn system_locale() -> AppLocale {
    let mut buffer = [0_u16; LOCALE_NAME_CAPACITY];
    let length = unsafe { GetUserDefaultLocaleName(buffer.as_mut_ptr(), buffer.len() as i32) };
    if length > 1 {
        let tag = String::from_utf16_lossy(&buffer[..length as usize - 1]);
        AppLocale::from_language_tag(&tag)
    } else {
        AppLocale::English
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Local, TimeZone};

    use super::*;

    fn window(minutes: u64) -> LimitWindow {
        LimitWindow {
            duration_minutes: minutes,
            remaining_percent: 94,
            resets_at: Local.timestamp_opt(1_800_500_000, 0).single(),
        }
    }

    #[test]
    fn parses_codex_locale_override() {
        assert_eq!(
            parse_locale_override("localeOverride = \"ko-KR\""),
            Some("ko-KR".to_string())
        );
        assert_eq!(parse_locale_override("# localeOverride = \"en\""), None);
        assert_eq!(parse_locale_override("theme = \"dark\""), None);
    }

    #[test]
    fn detects_supported_languages() {
        assert_eq!(AppLocale::from_language_tag("ko-KR"), AppLocale::Korean);
        assert_eq!(AppLocale::from_language_tag("zh-CN"), AppLocale::Chinese);
        assert_eq!(AppLocale::from_language_tag("en-US"), AppLocale::English);
    }

    #[test]
    fn formats_compact_weekly_plan() {
        let weekly = window(10_080);
        assert!(
            AppLocale::Korean
                .weekly_plan_text(&weekly, 8.0, 14.0)
                .starts_with("주간 94% · 오늘 8/14% · ")
        );
    }
}
