mod complete;
mod progress;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Args, Parser, Subcommand, ValueEnum, ValueHint};

use crate::convert::{ConvertOptions, InspectOptions, run_with_observer};
use crate::format::{RegionFormat, SourceFormatHint};
use crate::info::inspect;
use progress::ConsoleReporter;

#[derive(Debug, Parser)]
#[command(
    name = "linear-converter",
    version,
    about = "Linear Converter — convert Minecraft region files between Anvil, Linear and the older Linear layouts"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Convert worlds, region folders or single region files to another format.
    Convert(ConvertArgs),
    /// Read saves and report their formats, chunks and damage, without writing anything.
    Info(InfoArgs),
}

#[derive(Debug, Args)]
pub struct ConvertArgs {
    /// World directories, region directories or region files.
    #[arg(required = true, value_hint = ValueHint::AnyPath)]
    pub inputs: Vec<PathBuf>,
    /// Format to write.
    #[arg(long, short, value_enum)]
    pub to: TargetFormatArg,
    /// Directory the converted saves are written to.
    #[arg(long, short, value_hint = ValueHint::DirPath)]
    pub output: PathBuf,
    /// Format to read (default: detected from each file).
    #[arg(long, value_enum, default_value_t = SourceFormatArg::Auto)]
    pub from: SourceFormatArg,
    /// Worker threads (default: all CPU threads).
    #[arg(long, short = 'j', value_name = "N")]
    pub threads: Option<usize>,
    /// Region files read or written at once (default: 4). Raise it on fast SSDs.
    #[arg(long, value_name = "N")]
    pub io_threads: Option<usize>,
    /// Compression level: zlib 0-9 for mca, zstd 1-22 otherwise (default: 6).
    #[arg(long, short, value_name = "LEVEL")]
    pub level: Option<i32>,
    /// Print where the time went once the conversion is done.
    #[arg(long)]
    pub profile: bool,
}

#[derive(Debug, Args)]
pub struct InfoArgs {
    /// World directories, region directories or region files.
    #[arg(required = true, value_hint = ValueHint::AnyPath)]
    pub inputs: Vec<PathBuf>,
    /// Worker threads (default: all CPU threads).
    #[arg(long, short = 'j', value_name = "N")]
    pub threads: Option<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum SourceFormatArg {
    Auto,
    Mca,
    /// Any `.linear` file: format 1 or 2, or a legacy layout, told apart by its header.
    Linear,
    BlinearV2,
    BlinearV3,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum TargetFormatArg {
    /// Minecraft's Anvil format.
    Mca,
    /// DivineMC Linear format 2.
    Linear,
    /// The old Linear layout (linear v3), for servers that do not read the current format.
    LinearLegacy,
    BlinearV2,
    BlinearV3,
}

impl SourceFormatArg {
    pub fn into_source_format_hint(self) -> Option<SourceFormatHint> {
        match self {
            Self::Auto => None,
            Self::Mca => Some(SourceFormatHint::Mca),
            Self::Linear => Some(SourceFormatHint::Linear),
            Self::BlinearV2 => Some(SourceFormatHint::BlinearV2),
            Self::BlinearV3 => Some(SourceFormatHint::BlinearV3),
        }
    }
}

impl TargetFormatArg {
    pub fn into_region_format(self) -> RegionFormat {
        match self {
            Self::Mca => RegionFormat::Mca,
            Self::Linear => RegionFormat::Linear,
            Self::LinearLegacy => RegionFormat::LinearLegacy,
            Self::BlinearV2 => RegionFormat::BlinearV2,
            Self::BlinearV3 => RegionFormat::BlinearV3,
        }
    }
}

impl From<ConvertArgs> for ConvertOptions {
    fn from(args: ConvertArgs) -> Self {
        Self {
            inputs: args.inputs,
            output: args.output,
            source: args.from.into_source_format_hint(),
            target: args.to.into_region_format(),
            threads: args.threads,
            io_threads: args.io_threads,
            compression_level: args.level,
            profile: args.profile,
        }
    }
}

impl From<InfoArgs> for InspectOptions {
    fn from(args: InfoArgs) -> Self {
        Self {
            inputs: args.inputs,
            threads: args.threads,
        }
    }
}

/// Parses the command line and runs it; fails when a region failed or a chunk had to be skipped.
pub fn run() -> ExitCode {
    complete::answer();
    match execute(Cli::parse()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn execute(cli: Cli) -> Result<bool> {
    let mut reporter = ConsoleReporter::new();
    match cli.command {
        Command::Convert(args) => {
            let summary = run_with_observer(args.into(), &mut reporter)?;
            Ok(summary.failed_jobs == 0 && summary.total_warnings == 0)
        }
        Command::Info(args) => {
            let summary = inspect(args.into())?;
            reporter.print_info_summary(&summary)?;
            Ok(summary.failed_regions == 0 && summary.warnings == 0)
        }
    }
}
