use clap::CommandFactory as _;

use super::Cli;

/// Answers the shell when it asks for completions (`COMPLETE=<shell>`); returns otherwise.
pub fn answer() {
    // A bare name, so the registration finds `linear-converter` on PATH, `.exe` on Windows included.
    clap_complete::CompleteEnv::with_factory(Cli::command)
        .bin("linear-converter")
        .completer("linear-converter")
        .complete();
}
