use std::path::PathBuf;

use anyhow::{Result, bail};

use crate::convert::discovery::{
    DiscoverySummary, InputDiscovery, Job, RegionSource, discover_jobs_with_summary,
    discover_sources_with_summary,
};
use crate::convert::pipeline::resolve_thread_count;
use crate::format::{RegionFormat, SourceFormatHint};

/// What to convert, where to, and how.
#[derive(Clone, Debug)]
pub struct ConvertOptions {
    pub inputs: Vec<PathBuf>,
    pub output: PathBuf,
    /// Read every input as this format instead of detecting it.
    pub source: Option<SourceFormatHint>,
    pub target: RegionFormat,
    /// Worker threads; all CPU threads when unset.
    pub threads: Option<usize>,
    /// Region files read or written at once; `DEFAULT_IO_THREADS` when unset.
    pub io_threads: Option<usize>,
    /// zlib level for mca (0-9), zstd level otherwise (1-22); the format's default when unset.
    pub compression_level: Option<i32>,
    pub profile: bool,
}

/// What to inspect.
#[derive(Clone, Debug)]
pub struct InspectOptions {
    pub inputs: Vec<PathBuf>,
    pub threads: Option<usize>,
}

pub const DEFAULT_IO_THREADS: usize = 4;

fn thread_count(threads: Option<usize>) -> Result<usize> {
    match threads {
        Some(0) => bail!("thread count must be greater than zero"),
        Some(value) => Ok(value),
        None => Ok(std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1)),
    }
}

impl ConvertOptions {
    pub fn resolved_compression_level(&self) -> Result<i32> {
        let level = self
            .compression_level
            .unwrap_or_else(|| self.target.default_compression_level());
        match self.target {
            RegionFormat::Mca if !(0..=9).contains(&level) => {
                bail!("mca compression level must be in the range 0..=9")
            }
            RegionFormat::Mca => {}
            _ if !(1..=22).contains(&level) => {
                bail!("linear and blinear compression levels must be in the range 1..=22")
            }
            _ => {}
        }
        Ok(level)
    }
}

#[derive(Debug)]
pub struct RunPlan {
    pub input_paths: Vec<PathBuf>,
    pub output_root: PathBuf,
    pub source_format: Option<SourceFormatHint>,
    pub requested_thread_count: usize,
    pub target_format: RegionFormat,
    pub thread_count: usize,
    pub io_thread_count: usize,
    pub compression_level: i32,
    pub profile: bool,
    pub total_jobs: usize,
    pub total_region_directories: usize,
    pub source_breakdown: Vec<FormatBreakdown>,
    pub input_summaries: Vec<InputDiscovery>,
}

#[derive(Debug)]
pub struct FormatBreakdown {
    pub format: SourceFormatHint,
    pub job_count: usize,
}

pub struct ConversionPlan {
    pub run_plan: RunPlan,
    pub jobs: Vec<Job>,
    pub target_format: RegionFormat,
    pub compression_level: i32,
}

pub struct InspectionPlan {
    pub sources: Vec<RegionSource>,
    pub discovery_summary: DiscoverySummary,
    pub thread_count: usize,
}

pub fn plan_conversion(options: &ConvertOptions) -> Result<ConversionPlan> {
    let requested_thread_count = thread_count(options.threads)?;
    let io_thread_count = match options.io_threads {
        Some(0) => bail!("io thread count must be greater than zero"),
        Some(value) => value,
        None => DEFAULT_IO_THREADS,
    };
    let source_format = options.source;
    let target_format = options.target;
    let compression_level = options.resolved_compression_level()?;
    let discovery = discover_jobs_with_summary(
        &options.inputs,
        &options.output,
        source_format,
        target_format,
    )?;
    let thread_count = resolve_thread_count(requested_thread_count, discovery.jobs.len());

    let run_plan = RunPlan {
        input_paths: options.inputs.clone(),
        output_root: options.output.clone(),
        source_format,
        requested_thread_count,
        target_format,
        thread_count,
        io_thread_count: io_thread_count.min(thread_count),
        compression_level,
        profile: options.profile,
        total_jobs: discovery.jobs.len(),
        total_region_directories: discovery.summary.total_region_directories,
        source_breakdown: build_source_breakdown(&discovery.jobs),
        input_summaries: discovery.summary.inputs.clone(),
    };

    Ok(ConversionPlan {
        run_plan,
        jobs: discovery.jobs,
        target_format,
        compression_level,
    })
}

pub fn plan_inspection(options: &InspectOptions) -> Result<InspectionPlan> {
    let requested_thread_count = thread_count(options.threads)?;
    let discovery = discover_sources_with_summary(&options.inputs, None, None)?;
    let thread_count = resolve_thread_count(requested_thread_count, discovery.sources.len());

    Ok(InspectionPlan {
        sources: discovery.sources,
        discovery_summary: discovery.summary,
        thread_count,
    })
}

fn build_source_breakdown(jobs: &[Job]) -> Vec<FormatBreakdown> {
    let formats = [
        SourceFormatHint::Mca,
        SourceFormatHint::Linear,
        SourceFormatHint::BlinearFamily,
        SourceFormatHint::BlinearV2,
        SourceFormatHint::BlinearV3,
    ];
    let mut breakdown = Vec::new();

    for format in formats {
        let job_count = jobs
            .iter()
            .filter(|job| job.source_format == format)
            .count();
        if job_count > 0 {
            breakdown.push(FormatBreakdown { format, job_count });
        }
    }

    breakdown
}
