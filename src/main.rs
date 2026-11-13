// Hide the console window for the GUI binary. Use the `comic-crate-cli` companion
// for console/automation use.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod reader;
mod worker;

fn main() {
    if let Err(error) = app::run() {
        eprintln!("comiccrate: {error}");
        std::process::exit(1);
    }
}
