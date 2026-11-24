//! Screen Lighting Control (SLC) — entry point. The decision logic (color, schedule, settings, pipeline
//! split) is shared; each platform has its own shell around it.
#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

#[cfg_attr(not(windows), allow(dead_code))] // not every shared item is used on Linux
mod cities;
#[cfg_attr(not(windows), allow(dead_code))] // not every shared item is used on Linux
mod color;
#[cfg_attr(not(windows), allow(dead_code))] // not every shared item is used on Linux
mod config;
#[cfg_attr(not(windows), allow(dead_code))] // not every shared item is used on Linux
mod engine;
#[cfg_attr(not(windows), allow(dead_code))] // not every shared item is used on Linux
mod log;
#[cfg_attr(not(windows), allow(dead_code))] // not every shared item is used on Linux
mod model;
#[cfg_attr(not(windows), allow(dead_code))] // not every shared item is used on Linux
mod schedule;
#[cfg_attr(not(windows), allow(dead_code))]
mod solar;

#[cfg(windows)]
mod ambient;
#[cfg(windows)]
mod app;
#[cfg(windows)]
mod cli;
#[cfg(windows)]
mod foreground;
#[cfg(windows)]
mod glyph;
#[cfg(windows)]
mod hotkeys;
#[cfg(windows)]
mod icon;
#[cfg(windows)]
mod idle;
#[cfg(windows)]
mod install;
#[cfg(windows)]
mod monitors;
#[cfg(windows)]
mod safety;
#[cfg(windows)]
mod tray;
#[cfg(windows)]
mod ui;
#[cfg(windows)]
mod win;
#[cfg(windows)]
mod windows_main;
#[cfg(windows)]
mod worker;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(windows)]
fn main() {
    windows_main::main();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("slc: {VERSION} has no Linux version yet");
    std::process::exit(1);
}
