use std::{
    fs,
    path::Path,
    sync::{Mutex, OnceLock},
};

use chrono::Local;
use serde::{Deserialize, Serialize};

use crate::model::UsageSnapshot;

pub const SLOT_COUNT: usize = 7;
const STATE_VERSION: u8 = 2;
const ATTRIBUTION_GAP_SECS: i64 = 5 * 60;
const RESET_TIME_TOLERANCE_SECS: i64 = 60 * 60;
const REFILL_THRESHOLD_PERCENT: f64 = 5.0;
const MAX_EVENTS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlannerEventKind {
    CycleReset,
    QuotaRefill,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlannerEvent {
    pub kind: PlannerEventKind,
    pub at_unix: i64,
    pub slot: usize,
    pub amount_percent: f64,
    pub remaining_before_percent: f64,
    pub remaining_after_percent: f64,
    pub segment_used_before_percent: f64,
    pub reset_at_before_unix: i64,
    pub reset_at_after_unix: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlannerState {
    version: u8,
    reset_at_unix: i64,
    duration_minutes: u64,
    active_slot: usize,
    active_start_used_percent: f64,
    last_used_percent: f64,
    last_sample_unix: i64,
    plans: [Option<f64>; SLOT_COUNT],
    actuals: [Option<f64>; SLOT_COUNT],
    #[serde(default)]
    events: Vec<PlannerEvent>,
}

#[derive(Clone, Debug)]
pub struct PlanView {
    pub active_slot: usize,
    pub fill_ratios: [Option<f64>; SLOT_COUNT],
    pub reset_markers: [bool; SLOT_COUNT],
    pub today_budget: f64,
    pub today_used: f64,
}

#[derive(Default)]
struct Runtime {
    state: Option<PlannerState>,
    view: Option<PlanView>,
}

static RUNTIME: OnceLock<Mutex<Runtime>> = OnceLock::new();

fn runtime() -> &'static Mutex<Runtime> {
    RUNTIME.get_or_init(|| {
        Mutex::new(Runtime {
            state: PlannerState::load(&crate::settings::planner_state_path()),
            view: None,
        })
    })
}

pub fn update_from_snapshot(snapshot: &UsageSnapshot) {
    let Some(weekly) = snapshot.weekly.as_ref() else {
        return;
    };
    let Some(reset) = weekly.resets_at.as_ref() else {
        return;
    };

    let now = Local::now().timestamp();
    let remaining = weekly.remaining_percent as f64;
    let Ok(mut runtime) = runtime().lock() else {
        return;
    };
    let (state, view) = PlannerState::advance(
        runtime.state.take(),
        now,
        reset.timestamp(),
        weekly.duration_minutes,
        remaining,
    );
    state.save(&crate::settings::planner_state_path());
    runtime.state = Some(state);
    runtime.view = Some(view);
}

pub fn current_view() -> Option<PlanView> {
    runtime().lock().ok()?.view.clone()
}

impl PlannerState {
    fn load(path: &Path) -> Option<Self> {
        let bytes = fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    fn save(&self, path: &Path) {
        let Some(directory) = path.parent() else {
            return;
        };
        if fs::create_dir_all(directory).is_err() {
            return;
        }
        let Ok(bytes) = serde_json::to_vec_pretty(self) else {
            return;
        };
        let temporary = path.with_extension("json.tmp");
        if fs::write(&temporary, bytes).is_ok() {
            let _ = fs::remove_file(path);
            let _ = fs::rename(temporary, path);
        }
    }

    pub fn advance(
        previous: Option<Self>,
        now_unix: i64,
        reset_at_unix: i64,
        duration_minutes: u64,
        weekly_remaining_percent: f64,
    ) -> (Self, PlanView) {
        let remaining = weekly_remaining_percent.clamp(0.0, 100.0);
        let raw_used = 100.0 - remaining;
        let computed_slot = slot_index(now_unix, reset_at_unix, duration_minutes);

        let mut state = match previous {
            Some(mut state)
                if (1..=STATE_VERSION).contains(&state.version)
                    && state.duration_minutes == duration_minutes
                    && reset_at_unix.abs_diff(state.reset_at_unix)
                        <= RESET_TIME_TOLERANCE_SECS as u64 =>
            {
                state.version = STATE_VERSION;
                state.reset_at_unix = reset_at_unix;
                state
            }
            Some(previous_state) => {
                let previous_reset = previous_state.reset_at_unix;
                let previous_remaining = (100.0 - previous_state.last_used_percent).clamp(0.0, 100.0);
                let previous_segment_used = (previous_state.last_used_percent
                    - previous_state.active_start_used_percent)
                    .max(0.0);
                let mut state = Self::fresh(
                    now_unix,
                    reset_at_unix,
                    duration_minutes,
                    computed_slot,
                    raw_used,
                    remaining,
                );
                state.events = previous_state.events;
                if reset_at_unix > previous_reset + RESET_TIME_TOLERANCE_SECS {
                    state.push_event(PlannerEvent {
                        kind: PlannerEventKind::CycleReset,
                        at_unix: now_unix,
                        slot: computed_slot,
                        amount_percent: (remaining - previous_remaining).max(0.0),
                        remaining_before_percent: previous_remaining,
                        remaining_after_percent: remaining,
                        segment_used_before_percent: previous_segment_used,
                        reset_at_before_unix: previous_reset,
                        reset_at_after_unix: reset_at_unix,
                    });
                }
                let view = state.view(0.0, state.plans[computed_slot].unwrap_or(0.0));
                return (state, view);
            }
            None => {
                let state = Self::fresh(
                    now_unix,
                    reset_at_unix,
                    duration_minutes,
                    computed_slot,
                    raw_used,
                    remaining,
                );
                let view = state.view(0.0, state.plans[computed_slot].unwrap_or(0.0));
                return (state, view);
            }
        };

        // A small reset timestamp correction should never make the planner move backwards.
        let active_slot = computed_slot.max(state.active_slot).min(SLOT_COUNT - 1);
        let used_drop = state.last_used_percent - raw_used;
        let is_refill = used_drop >= REFILL_THRESHOLD_PERCENT;

        // Small backwards movements are treated as quota-reporting jitter. They must not
        // create free budget or make the displayed usage run backwards.
        let effective_used = if !is_refill && raw_used < state.last_used_percent {
            state.last_used_percent
        } else {
            raw_used
        };
        let effective_remaining = 100.0 - effective_used;

        let previous_slot = state.active_slot;
        let previous_segment_used =
            (state.last_used_percent - state.active_start_used_percent).max(0.0);
        let previous_plan = state.plans[previous_slot].unwrap_or(0.0);

        if active_slot > previous_slot {
            let close_at_used = if !is_refill
                && active_slot == previous_slot + 1
                && now_unix.saturating_sub(state.last_sample_unix) <= ATTRIBUTION_GAP_SECS
            {
                effective_used
            } else {
                state.last_used_percent
            };
            state.actuals[previous_slot] = Some(
                (close_at_used - state.active_start_used_percent)
                    .max(0.0)
                    .min(100.0),
            );
            state.active_slot = active_slot;
            state.active_start_used_percent = effective_used;
            redistribute_from(&mut state.plans, active_slot, effective_remaining);
        }

        if is_refill {
            let remaining_before = (100.0 - state.last_used_percent).clamp(0.0, 100.0);
            state.push_event(PlannerEvent {
                kind: PlannerEventKind::QuotaRefill,
                at_unix: now_unix,
                slot: active_slot,
                amount_percent: used_drop,
                remaining_before_percent: remaining_before,
                remaining_after_percent: remaining,
                segment_used_before_percent: if active_slot == previous_slot {
                    previous_segment_used
                } else {
                    0.0
                },
                reset_at_before_unix: state.reset_at_unix,
                reset_at_after_unix: reset_at_unix,
            });

            // A refill starts a new budget segment inside the same weekly window. Usage
            // before this point stays in event history and must not reduce the new budget.
            state.active_slot = active_slot;
            state.active_start_used_percent = raw_used;
            redistribute_from(&mut state.plans, active_slot, remaining);
            state.last_used_percent = raw_used;
            state.last_sample_unix = now_unix;

            let today_budget = state.plans[active_slot].unwrap_or(0.0).max(0.0);
            let view = state.view(0.0, today_budget);
            return (state, view);
        }

        let today_used = (effective_used - state.active_start_used_percent).max(0.0);
        let today_budget = state.plans[state.active_slot].unwrap_or(0.0).max(0.0);
        let keep_for_today = (today_budget - today_used)
            .max(0.0)
            .min(effective_remaining);
        let future_count = SLOT_COUNT.saturating_sub(state.active_slot + 1);

        if future_count > 0 {
            let future_pool = (effective_remaining - keep_for_today).max(0.0);
            let each = future_pool / future_count as f64;
            for slot in (state.active_slot + 1)..SLOT_COUNT {
                state.plans[slot] = Some(each);
            }
        }

        // Keep the previous plan referenced so refill events can report the pre-refill
        // segment budget without changing the compact PlanView API later.
        let _ = previous_plan;
        state.last_used_percent = effective_used;
        state.last_sample_unix = now_unix;
        let view = state.view(today_used, today_budget);
        (state, view)
    }

    fn fresh(
        now_unix: i64,
        reset_at_unix: i64,
        duration_minutes: u64,
        active_slot: usize,
        used: f64,
        remaining: f64,
    ) -> Self {
        let mut plans = [None; SLOT_COUNT];
        redistribute_from(&mut plans, active_slot, remaining);
        Self {
            version: STATE_VERSION,
            reset_at_unix,
            duration_minutes,
            active_slot,
            active_start_used_percent: used,
            last_used_percent: used,
            last_sample_unix: now_unix,
            plans,
            actuals: [None; SLOT_COUNT],
            events: Vec::new(),
        }
    }

    fn push_event(&mut self, event: PlannerEvent) {
        self.events.push(event);
        if self.events.len() > MAX_EVENTS {
            let extra = self.events.len() - MAX_EVENTS;
            self.events.drain(0..extra);
        }
    }

    fn view(&self, today_used: f64, today_budget: f64) -> PlanView {
        let mut fill_ratios = [None; SLOT_COUNT];
        for slot in 0..self.active_slot {
            if let (Some(actual), Some(plan)) = (self.actuals[slot], self.plans[slot]) {
                fill_ratios[slot] = Some(ratio(actual, plan));
            }
        }
        fill_ratios[self.active_slot] = Some(ratio(today_used, today_budget));
        for slot in (self.active_slot + 1)..SLOT_COUNT {
            fill_ratios[slot] = Some(0.0);
        }

        let mut reset_markers = [false; SLOT_COUNT];
        for event in &self.events {
            if event.reset_at_after_unix == self.reset_at_unix && event.slot < SLOT_COUNT {
                reset_markers[event.slot] = true;
            }
        }

        PlanView {
            active_slot: self.active_slot,
            fill_ratios,
            reset_markers,
            today_budget,
            today_used,
        }
    }
}

fn redistribute_from(plans: &mut [Option<f64>; SLOT_COUNT], first_slot: usize, remaining: f64) {
    let count = SLOT_COUNT.saturating_sub(first_slot).max(1);
    let each = remaining.max(0.0) / count as f64;
    for slot in first_slot..SLOT_COUNT {
        plans[slot] = Some(each);
    }
}

fn ratio(actual: f64, plan: f64) -> f64 {
    if plan <= f64::EPSILON {
        if actual > 0.0 { 2.0 } else { 0.0 }
    } else {
        (actual / plan).max(0.0)
    }
}

fn slot_index(now_unix: i64, reset_at_unix: i64, duration_minutes: u64) -> usize {
    let duration_seconds = (duration_minutes as i64).saturating_mul(60);
    if duration_seconds <= 0 {
        return 0;
    }
    let start = reset_at_unix.saturating_sub(duration_seconds);
    if now_unix <= start {
        return 0;
    }
    let elapsed = now_unix
        .saturating_sub(start)
        .min(duration_seconds.saturating_sub(1));
    ((elapsed as i128 * SLOT_COUNT as i128) / duration_seconds as i128) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400;
    const WEEK_MINUTES: u64 = 10_080;

    #[test]
    fn first_run_splits_remaining_quota_across_remaining_slots() {
        let reset = 7 * DAY;
        let now = 2 * DAY + 60;
        let (_, view) = PlannerState::advance(None, now, reset, WEEK_MINUTES, 70.0);
        assert_eq!(view.active_slot, 2);
        assert!((view.today_budget - 14.0).abs() < 0.001);
        assert_eq!(view.today_used, 0.0);
    }

    #[test]
    fn normal_usage_does_not_steal_from_future_days() {
        let reset = 7 * DAY;
        let (state, _) = PlannerState::advance(None, 60, reset, WEEK_MINUTES, 100.0);
        let (_, view) = PlannerState::advance(Some(state), 3_600, reset, WEEK_MINUTES, 95.0);
        assert!((view.today_budget - (100.0 / 7.0)).abs() < 0.001);
    }

    #[test]
    fn overspend_reduces_future_budget_immediately() {
        let reset = 7 * DAY;
        let (state, first) = PlannerState::advance(None, 60, reset, WEEK_MINUTES, 100.0);
        assert!((first.today_budget - (100.0 / 7.0)).abs() < 0.001);
        let (state, view) = PlannerState::advance(Some(state), 3_600, reset, WEEK_MINUTES, 80.0);
        assert!(view.today_used > view.today_budget);
        assert!(state.plans[1].unwrap() < first.today_budget);
    }

    #[test]
    fn unused_budget_is_carried_forward_at_next_slot() {
        let reset = 7 * DAY;
        let (state, _) = PlannerState::advance(None, 60, reset, WEEK_MINUTES, 100.0);
        let (_, view) = PlannerState::advance(Some(state), DAY + 60, reset, WEEK_MINUTES, 90.0);
        assert_eq!(view.active_slot, 1);
        assert!((view.today_budget - 15.0).abs() < 0.001);
    }

    #[test]
    fn same_window_refill_reallocates_without_erasing_history() {
        let reset = 7 * DAY;
        let now = 3 * DAY + 60;
        let (state, _) = PlannerState::advance(None, now, reset, WEEK_MINUTES, 50.0);
        let (state, view) = PlannerState::advance(Some(state), now + 60, reset, WEEK_MINUTES, 80.0);

        assert_eq!(view.active_slot, 3);
        assert_eq!(view.today_used, 0.0);
        assert!((view.today_budget - 20.0).abs() < 0.001);
        assert!(view.reset_markers[3]);
        assert_eq!(state.events.len(), 1);
        assert_eq!(state.events[0].kind, PlannerEventKind::QuotaRefill);
        assert!((state.events[0].amount_percent - 30.0).abs() < 0.001);
        assert!((state.events[0].remaining_before_percent - 50.0).abs() < 0.001);
        assert!((state.events[0].remaining_after_percent - 80.0).abs() < 0.001);
    }

    #[test]
    fn refill_does_not_charge_pre_refill_usage_to_the_new_budget() {
        let reset = 7 * DAY;
        let (state, _) = PlannerState::advance(None, 60, reset, WEEK_MINUTES, 100.0);
        let (state, before) = PlannerState::advance(Some(state), 3_600, reset, WEEK_MINUTES, 90.0);
        assert!((before.today_used - 10.0).abs() < 0.001);

        let (state, refill) = PlannerState::advance(Some(state), 3_660, reset, WEEK_MINUTES, 100.0);
        assert_eq!(refill.today_used, 0.0);
        assert!((refill.today_budget - (100.0 / 7.0)).abs() < 0.001);
        assert!((state.events[0].segment_used_before_percent - 10.0).abs() < 0.001);

        let (_, after) = PlannerState::advance(Some(state), 3_720, reset, WEEK_MINUTES, 95.0);
        assert!((after.today_used - 5.0).abs() < 0.001);
    }

    #[test]
    fn small_usage_regression_is_jitter_not_a_refill() {
        let reset = 7 * DAY;
        let (state, _) = PlannerState::advance(None, 60, reset, WEEK_MINUTES, 100.0);
        let (state, before) = PlannerState::advance(Some(state), 3_600, reset, WEEK_MINUTES, 60.0);
        assert!((before.today_used - 40.0).abs() < 0.001);

        let (state, after) = PlannerState::advance(Some(state), 3_660, reset, WEEK_MINUTES, 61.0);
        assert!((after.today_used - 40.0).abs() < 0.001);
        assert!(state.events.is_empty());
    }

    #[test]
    fn reset_change_starts_a_new_plan_and_records_cycle_event() {
        let reset = 7 * DAY;
        let (state, _) = PlannerState::advance(None, 60, reset, WEEK_MINUTES, 50.0);
        let (state, view) = PlannerState::advance(
            Some(state),
            reset + 60,
            reset + 7 * DAY,
            WEEK_MINUTES,
            100.0,
        );
        assert_eq!(view.active_slot, 0);
        assert!((view.today_budget - (100.0 / 7.0)).abs() < 0.001);
        assert!(view.reset_markers[0]);
        assert_eq!(state.events.last().unwrap().kind, PlannerEventKind::CycleReset);
    }

    #[test]
    fn small_reset_time_correction_preserves_the_current_cycle() {
        let reset = 7 * DAY;
        let (state, _) = PlannerState::advance(None, DAY + 60, reset, WEEK_MINUTES, 80.0);
        let (state, view) = PlannerState::advance(
            Some(state),
            DAY + 120,
            reset + 120,
            WEEK_MINUTES,
            79.0,
        );
        assert_eq!(view.active_slot, 1);
        assert!(state.events.is_empty());
    }
}
