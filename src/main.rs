#![windows_subsystem = "windows"]

mod app;
mod chrome;
mod errors;
mod http;
mod icon_discovery;
mod icons;
mod metadata;
mod operations;
mod packages;
mod platform;
mod portable_lock;
mod scoop;
mod scoop_catalog;
mod smoke;
mod storage;
mod winget;

slint::include_modules!();

fn main() {
    if let Err(error) = app::run() {
        platform::message(&format!("SlimStore could not start.\n\n{error}"));
    }
}
