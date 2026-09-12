# Weekly Usage Bar

A compact adaptive weekly quota planner for OpenAI Codex Desktop on Windows.

It attaches to the unused area of the Codex title bar, reads the real Codex rate-limit window from the local `codex app-server`, and answers two questions at a glance: how much of the weekly quota is left, and how much of today's allocated budget is left.

## Current display

```text
9/12 [   63%   ] 9/19   R17:31 [   75%   ] +24h
start   weekly   reset    anchor    daily    span
```

- The **left bar** is the real remaining weekly quota. Its left date is the weekly-window start and its right date is the actual reset date.
- The **right bar** treats the current 24-hour allocation as 100%. Its day does **not** start at midnight: `R17:31` means the daily budget is anchored to the weekly reset clock time, and `+24h` means the slot ends 24 hours later. This avoids making the boundary label look like the current clock. If that allocation is 16% of the weekly quota and 4 percentage points have been used since the reset-anchored day began, the right bar shows `75%` because 12/16 remains.
- All compact title-bar text uses **Segoe UI Variable Display Semibold** styling for a stronger, cleaner Windows 11 look.
- Both bars are intentionally thick, with their percentages drawn inside them. There is no seven-cell strip in the UI.
- The bars use a restrained dark faux-glass treatment: rounded capsule edges, a subtle bright rim, top glint, lower shadow, and a dimmed accent fill. It does not depend on desktop blur or Acrylic composition.

The planner divides the actual weekly rate-limit window into seven equal 24-hour slots anchored to the weekly reset time. For example, a weekly cycle that begins at 17:31 uses 17:31→17:31 as each daily boundary, not 00:00→00:00. The slots remain calculation state rather than seven separate visible bars.

## Adaptive budget logic

- On first run, the remaining weekly quota is divided across the current and future internal slots.
- The current slot's target stays fixed while normal usage progresses.
- If the current slot exceeds its target, future-slot budgets shrink immediately.
- If a slot closes under budget, the unused amount is redistributed across the remaining slots.
- If the app was not running across a slot boundary, it does not invent historical usage.
- A new Codex weekly reset starts a fresh plan automatically.
- The daily bar always normalizes the current reset-anchored 24-hour slot's allocation to 100%, so it answers "how much of this reset-day budget is left?"

Planner state is persisted locally so carry-over survives restarts.

## Refills, reset credits, and manual resets

Weekly Usage Bar treats a quota refill separately from a normal weekly rollover.

- If `resetAt` stays in the same weekly window and used quota drops by at least 5 percentage points, the change is recorded as a `quota_refill` event.
- Returning all the way to 100% remaining is also treated as a full reset even when less than 5 percentage points were used. This catches cases such as 99% remaining -> 100% remaining.
- The read-only rate-limit response also exposes the number of available reset credits. If that count drops and the quota improves within the next 10 minutes, even a small 1-point refill is confirmed and recorded as `reset_credit_used`.
- A refill starts a new budget segment at the exact observation point. Usage from before the refill is preserved in event history but is not charged against the newly allocated budget.
- The newly available quota is redistributed across the current and remaining internal slots immediately.
- Small backwards movements below 5 percentage points that are not a full reset and are not confirmed by a reset-credit count drop are treated as reporting/rounding jitter and do not create extra budget.
- If `resetAt` moves forward into a new weekly window, the planner records a `cycle_reset` event and starts a new plan.
- Reset-time corrections of up to one hour are treated as the same cycle so a minor backend timestamp adjustment does not wipe the plan.
- Recent reset/refill events are retained in `planner.json` (up to 64 events) so previous consumption is not silently erased.

This covers both automatic/manual quota resets and reset-credit style replenishments without assuming that every increase in remaining quota means a brand-new week.

## Why this exists

Most Codex usage tools answer **"how much is left?"**. Weekly Usage Bar is intended to answer **"how much can I spend today without burning the rest of the week?"** while using almost no screen space.

## Data source and privacy

Like the upstream title-bar meter, this app uses the `codex.exe` bundled with Codex Desktop and launches its local `app-server` in read-only mode. Codex keeps control of its existing login state. Weekly Usage Bar reads quota percentages, window duration, reset time, and reset-credit availability. It never consumes a reset credit. It does not require an API key and does not send usage data to a third party.

Codex plan variants do not always place the same time window in the same `primary`/`secondary` field. Weekly Usage Bar classifies the returned windows by duration, so a seven-day window works whether it arrives as `primary` or `secondary`.

Local data is stored under:

```text
%LOCALAPPDATA%\WeeklyUsageBar\
```

## Scope

- Windows 10/11 x64
- Microsoft Store build of OpenAI Codex Desktop
- ChatGPT-managed Codex rate-limit windows
- Korean, Chinese, and English status text
- One Codex window

## Interaction

- Left-click and drag the overlay area to drag the Codex window.
- Double-click the overlay area to forward the title-bar double-click behavior.
- Right-click the overlay to cycle the accent palette.
- Window movement is tracked with Windows `EVENT_OBJECT_LOCATIONCHANGE` events for immediate attachment; a 1.5-second poll remains only as recovery fallback.

## Build

Rust stable is required.

```powershell
cargo test
cargo build --release
```

## Next

The quota reader, adaptive planner, reset-credit handling, and compact title-bar attachment are implemented. Likely next additions are hover details, configurable reserve, and weighted high-work days.

## Attribution

This project is derived in part from [ConfigCrate/codex-titlebar-meter](https://github.com/configcrate/codex-titlebar-meter), which is licensed under the MIT License. See `NOTICE.md` and `LICENSE`.

Weekly Usage Bar is an independent project and is not affiliated with or endorsed by OpenAI.
