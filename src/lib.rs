//! Converts Minecraft region files between Anvil (`.mca`), DivineMC's Linear format and the older Linear layouts.

pub mod cli;
pub mod convert;
pub mod diagnostic;
pub mod format;
pub mod info;
pub mod io;
pub mod model;
pub mod writer;

pub fn run() -> std::process::ExitCode {
    cli::run()
}
