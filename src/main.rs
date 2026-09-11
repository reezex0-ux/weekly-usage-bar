#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(windows))]
compile_error!("Weekly Usage Bar currently supports Windows only.");

mod codex;
mod locale;
mod model;
mod native;
mod planner;
mod settings;

use anyhow::Result;

fn main() -> Result<()> {
    native::run()
}
