# Weekly Usage Bar

A compact adaptive weekly quota planner for OpenAI Codex Desktop on Windows.

It attaches to the unused area of the Codex title bar, reads the real Codex rate-limit window from the local `codex app-server`, and turns the weekly quota into seven compact budget cells. No separate dashboard is required.

## Current prototype

```text
5h 83%                     Week 72% · today 8/14% · Sep 17
██████████████             ███│██│█│░│░│░│░
                                      ^ current slot
```

The lower strip is one line split into seven cells. Each cell is one seventh of the actual Codex weekly rate-limit window, so the boundaries stay aligned with the real reset time even when the reset happens in the middle of a calendar day.

## Adaptive budget logic

- On first run, the remaining weekly quota is divided across the current and future slots.
- The current slot's target stays fixed while normal usage progresses.
- If the current slot exceeds its target, future-slot budgets shrink immediately.
- If a slot closes under budget, the unused amount is redistributed across the remaining slots.
- Completed cells show actual usage relative to their target.
- The current cell shows live progress; future cells stay empty.
- If the app was not running across a slot boundary, it does not invent historical usage.
- A new Codex weekly reset starts a fresh seven-slot plan automatically.

Planner state is persisted locally so carry-over survives restarts.

## Refills, reset credits, and manual resets

Weekly Usage Bar treats a quota refill separately from a normal weekly rollover.

- If `resetAt` stays in the same weekly window but used quota drops by at least 5 percentage points, the change is recorded as a `quota_refill` event.
- A refill starts a new budget segment at the exact observation point. Usage from before the refill is preserved in event history but is not charged against the newly allocated budget.
- The newly available quota is redistributed across the current and remaining cells immediately.
- Small backwards movements below 5 percentage points are treated as reporting/rounding jitter and do not create extra budget.
- If `resetAt` moves forward into a new weekly window, the planner records a `cycle_reset` event and starts a new seven-cell plan.
- Reset-time corrections of up to one hour are treated as the same cycle so a minor backend timestamp adjustment does not wipe the plan.
- Recent reset/refill events are retained in `planner.json` (up to 64 events) so previous consumption is not silently erased.

This is intended to cover both automatic/manual quota resets and reset-credit style replenishments without assuming that every increase in remaining quota means a brand-new week.

## Why this exists

Most Codex usage tools answer **"how much is left?"**. Weekly Usage Bar is intended to answer **"how much can I spend now without burning the rest of the week?"** while using almost no screen space.

## Data source and privacy

Like the upstream title-bar meter, this app uses the `codex.exe` bundled with Codex Desktop and launches its local `app-server` in read-only mode. Codex keeps control of its existing login state. Weekly Usage Bar reads quota percentages, window duration, and reset time. It does not require an API key and does not send usage data to a third party.

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

## Build

Rust stable is required.

```powershell
cargo test
cargo build --release
```

## Next

The first milestone is deliberately small: validate the seven-cell bar on a real Codex Desktop window. After that, likely additions are hover details, configurable reserve, weighted high-work days, and a compact projected-reset remainder.

## Attribution

This project is derived in part from [ConfigCrate/codex-titlebar-meter](https://github.com/configcrate/codex-titlebar-meter), which is licensed under the MIT License. See `NOTICE.md` and `LICENSE`.

Weekly Usage Bar is an independent project and is not affiliated with or endorsed by OpenAI.
