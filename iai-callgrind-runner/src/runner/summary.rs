use std::borrow::Cow;
use std::ffi::OsString;
use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use glob::glob;
use indexmap::{indexmap, IndexMap};
use log::debug;
#[cfg(feature = "schema")]
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::callgrind::model::Costs;
use super::tool::{ToolOutputPath, ValgrindTool};
use super::Error;
use crate::api::EventKind;
use crate::util::{factor_diff, make_absolute, percentage_diff};

/// A `Baseline` depending on the [`BaselineKind`] which points to the corresponding path
///
/// This baseline is used for comparisons with the new output of valgrind tools.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct Baseline {
    /// The kind of the `Baseline`, which currently can only be `Old`
    pub kind: BaselineKind,
    /// The path to the file which is used to compare against the new output
    pub path: PathBuf,
}

/// The `BaselineKind` describing the baseline
///
/// Currently, iai-callgrind can only compare callgrind output with `.old` files.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub enum BaselineKind {
    /// Compare new against `*.old` output files
    Old,
}

/// The `BenchmarkKind`, differentiating between library and binary benchmarks
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub enum BenchmarkKind {
    /// A library benchmark
    LibraryBenchmark,
    /// A binary benchmark
    BinaryBenchmark,
}

/// The `BenchmarkSummary` containing all the information of a single benchmark run
///
/// This includes produced files, recorded callgrind events, performance regressions ...
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct BenchmarkSummary {
    /// The version of this format. Only backwards incompatible cause an increase of the version
    pub version: String,
    /// Whether this summary describes a library or binary benchmark
    pub kind: BenchmarkKind,
    /// The destination and kind of the summary file
    pub summary_output: Option<SummaryOutput>,
    /// The project's root directory
    pub project_root: PathBuf,
    /// The directory of the package
    pub package_dir: PathBuf,
    /// The path to the benchmark file
    pub benchmark_file: PathBuf,
    /// The path to the compiled and executable benchmark file
    pub benchmark_exe: PathBuf,
    /// The rust path in the form `bench_file::group::bench`
    pub module_path: String,
    /// The user provided id of this benchmark
    pub id: Option<String>,
    /// More details describing this benchmark run
    pub details: Option<String>,
    /// The summary of the callgrind run
    pub callgrind_summary: Option<CallgrindSummary>,
    /// The summary of other valgrind tool runs
    pub tool_summaries: Vec<ToolSummary>,
}

/// The `CallgrindRegressionSummary` describing a single event based performance regression
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct CallgrindRegressionSummary {
    /// The [`EventKind`] which is affected by a performance regression
    pub event_kind: EventKind,
    /// The value of the new benchmark run
    pub new: u64,
    /// The value of the old benchmark run
    pub old: u64,
    /// The difference between new and old in percent
    pub diff_pct: f64,
    /// The value of the limit which was exceeded to cause a performance regression
    pub limit: f64,
}

/// The `CallgrindRunSummary` containing the recorded events, performance regressions of a single
/// callgrind run
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct CallgrindRunSummary {
    /// The executed command extracted from Valgrind output
    pub command: String,
    /// If present, the `Baseline` used to compare the new with the old output
    pub baseline: Option<Baseline>,
    /// All recorded costs for `EventKinds`
    pub events: CostsSummary,
    /// All detected performance regressions
    pub regressions: Vec<CallgrindRegressionSummary>,
}

/// The `CallgrindSummary` summarizes all callgrind runs
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct CallgrindSummary {
    /// If the regressions were configured to cause the benchmark run to fail immediately or not
    pub regression_fail_fast: bool,
    /// The paths to the `*.log` files
    pub log_paths: Vec<PathBuf>,
    /// The paths to the `*.old` files
    pub out_paths: Vec<PathBuf>,
    /// The summaries of possibly created flamegraphs
    pub flamegraphs: Vec<FlamegraphSummary>,
    /// The summaries of all callgrind runs
    pub summaries: Vec<CallgrindRunSummary>,
}

/// The `CostsDiff` describes the difference between an single optional `new` and `old` cost as
/// percentage and factor.
///
/// There is either a `new` or an `old` value present. Never can both be absent. If both values are
/// present, then there is also a `diff_pct` and `factor` present.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct CostsDiff {
    /// The value of the new cost
    pub new: Option<u64>,
    /// The value of the old cost
    pub old: Option<u64>,
    /// The difference between new and old in percent
    pub diff_pct: Option<f64>,
    /// The difference between new and old expressed as a factor
    pub factor: Option<f64>,
}

/// The `CostsSummary` contains all differences for affected [`EventKind`]s
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct CostsSummary(IndexMap<EventKind, CostsDiff>);

/// The `FlamegraphSummary` records all created paths for an [`EventKind`] specific flamegraph
///
/// Either the `regular_path`, `old_path` or the `diff_path` are present. Never can all of them be
/// absent.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct FlamegraphSummary {
    /// The `EventKind` of the flamegraph
    pub event_kind: EventKind,
    /// If present, the path to the file of the regular (non-differential) flamegraph
    pub regular_path: Option<PathBuf>,
    /// If present, the path to the file of the old regular (non-differential) flamegraph
    pub old_path: Option<PathBuf>,
    /// If present, the path to the file of the differential flamegraph
    pub diff_path: Option<PathBuf>,
}

/// The format (json, ...) in which the summary file should be saved or printed
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub enum SummaryFormat {
    /// The format in a space optimal json representation without newlines
    Json,
    /// The format in pretty printed json
    PrettyJson,
}

/// Manage the summary output file with this `SummaryOutput`
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct SummaryOutput {
    /// The [`SummaryFormat`]
    format: SummaryFormat,
    /// The path to the destination file of this summary
    path: PathBuf,
}

/// The `ToolRunSummary` which contains all information about a single tool run process
///
/// There's a separate process and therefore `ToolRunSummary` for the parent process and each child
/// process if `--trace-children=yes` was passed as argument to the `Tool`.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct ToolRunSummary {
    /// The executed command extracted from Valgrind output
    pub command: String,
    /// The pid of the process of the `command`
    pub pid: String,
    /// If present, the baseline used to compare with the new output of this tool
    pub baseline: Option<Baseline>,
    /// The tool specific summary extracted from Valgrind output
    pub summary: IndexMap<String, String>,
}

/// The `ToolSummary` containing all information about a valgrind tool run
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct ToolSummary {
    /// The Valgrind tool like `DHAT`, `Memcheck` etc.
    pub tool: ValgrindTool,
    /// The paths to the `*.log` files. All tools produce at least one log file
    pub log_paths: Vec<PathBuf>,
    /// The paths to the `*.out` files. Not all tools produce an output in addition to the log
    /// files
    pub out_paths: Vec<PathBuf>,
    /// All [`ToolRunSummary`]s
    pub summaries: Vec<ToolRunSummary>,
}

impl BenchmarkSummary {
    /// Create a new `BenchmarkSummary`
    ///
    /// Relative paths are made absolute with the `project_root` as base directory.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        kind: BenchmarkKind,
        project_root: PathBuf,
        package_dir: PathBuf,
        benchmark_file: PathBuf,
        benchmark_exe: PathBuf,
        segments: &[&str],
        id: Option<String>,
        details: Option<String>,
        output: Option<SummaryOutput>,
    ) -> Self {
        Self {
            version: "1".to_owned(),
            kind,
            benchmark_file: make_absolute(&project_root, benchmark_file),
            benchmark_exe: make_absolute(&project_root, benchmark_exe),
            module_path: segments.join("::"),
            id,
            details,
            callgrind_summary: None,
            tool_summaries: vec![],
            summary_output: output,
            project_root,
            package_dir,
        }
    }

    /// If this `BenchmarkSummary` has a value in the option `SummaryOutput` save it in json format
    pub fn save_json(&self, pretty: bool) -> Result<()> {
        if let Some(output) = &self.summary_output {
            let mut file = output.create()?;
            if pretty {
                serde_json::to_writer_pretty(&mut file, self)
                    .with_context(|| "Failed to serialize to json".to_owned())?;
            } else {
                serde_json::to_writer(&mut file, self)
                    .with_context(|| "Failed to serialize to json".to_owned())?;
            }
        }

        Ok(())
    }

    /// If this `BenchmarkSummary` has a value in the option `SummaryOutput` save it
    pub fn save(&self) -> Result<()> {
        if let Some(output) = &self.summary_output {
            debug!(
                "Saving {:?} to {:?}, style: {:?}",
                self.id, output.path, output.format
            );
            match output.format {
                SummaryFormat::Json => self.save_json(false)?,
                SummaryFormat::PrettyJson => self.save_json(true)?,
            }
        } else {
            debug!("No summary output file specified for {:?}", self.id);
        }

        Ok(())
    }

    /// Check if this `BenchmarkSummary` has recorded any performance regressions
    ///
    /// If the regressions are configured to be not `fail_fast` and there is a regressions, then the
    /// `is_regressed` variable is updated to true.
    ///
    /// # Errors
    ///
    /// If the regressions are configured to be `fail_fast` an error is returned
    pub fn check_regression(&self, is_regressed: &mut bool) -> Result<()> {
        if let Some(callgrind_summary) = &self.callgrind_summary {
            let benchmark_is_regressed = callgrind_summary.is_regressed();
            if benchmark_is_regressed && callgrind_summary.regression_fail_fast {
                return Err(Error::RegressionError(true).into());
            }

            *is_regressed |= benchmark_is_regressed;
        }

        Ok(())
    }
}

impl CallgrindSummary {
    /// Create a new `CallgrindSummary`
    pub fn new(
        fail_fast: bool,
        log_paths: Vec<PathBuf>,
        out_paths: Vec<PathBuf>,
    ) -> CallgrindSummary {
        Self {
            regression_fail_fast: fail_fast,
            log_paths,
            out_paths,
            flamegraphs: Vec::default(),
            summaries: Vec::default(),
        }
    }

    /// Return true if there are any recorded regressions in this `CallgrindSummary`
    pub fn is_regressed(&self) -> bool {
        self.summaries.iter().any(|r| !r.regressions.is_empty())
    }

    /// Create and add a [`CallgrindRunSummary`] to this `CallgrindSummary`
    pub fn add_summary(
        &mut self,
        bench_bin: &Path,
        bench_args: &[OsString],
        old_output: &ToolOutputPath,
        events: CostsSummary,
        regressions: Vec<CallgrindRegressionSummary>,
    ) {
        self.summaries.push(CallgrindRunSummary {
            command: format!(
                "{} {}",
                bench_bin.display(),
                shlex::join(
                    bench_args
                        .iter()
                        .map(|s| s.to_string_lossy().to_string())
                        .collect::<Vec<String>>()
                        .as_slice()
                        .iter()
                        .map(std::string::String::as_str)
                )
            ),
            baseline: old_output.exists().then(|| Baseline {
                kind: BaselineKind::Old,
                path: old_output.to_path(),
            }),
            events,
            regressions,
        });
    }
}

impl CostsSummary {
    /// Create a new `CostsSummary` calculating the differences between new and old (if any)
    /// [`Costs`]
    pub fn new(new_costs: &Costs, old_costs: Option<&Costs>) -> Self {
        let mut new_costs = Cow::Borrowed(new_costs);
        if !new_costs.is_summarized() {
            _ = new_costs.to_mut().make_summary();
        }

        if let Some(old_costs) = old_costs {
            let mut old_costs = Cow::Borrowed(old_costs);
            if !old_costs.is_summarized() {
                _ = old_costs.to_mut().make_summary();
            }
            let mut map = indexmap! {};
            for event_kind in new_costs.event_kinds_union(old_costs.as_ref()) {
                let diff = match (
                    new_costs.cost_by_kind(&event_kind),
                    old_costs.cost_by_kind(&event_kind),
                ) {
                    (None, Some(cost)) => CostsDiff {
                        new: None,
                        old: Some(cost),
                        diff_pct: None,
                        factor: None,
                    },
                    (Some(cost), None) => CostsDiff {
                        new: Some(cost),
                        old: None,
                        diff_pct: None,
                        factor: None,
                    },
                    (Some(new), Some(old)) => CostsDiff {
                        new: Some(new),
                        old: Some(old),
                        diff_pct: Some(percentage_diff(new, old)),
                        factor: Some(factor_diff(new, old)),
                    },
                    (None, None) => unreachable!(),
                };
                map.insert(event_kind, diff);
            }
            Self(map)
        } else {
            CostsSummary(
                new_costs
                    .iter()
                    .map(|(event_kind, cost)| {
                        (
                            *event_kind,
                            CostsDiff {
                                new: Some(*cost),
                                old: None,
                                diff_pct: None,
                                factor: None,
                            },
                        )
                    })
                    .collect::<IndexMap<_, _>>(),
            )
        }
    }

    /// Try to return a [`CostsDiff`] for the specified [`crate::api::EventKind`]
    pub fn diff_by_kind(&self, event_kind: &EventKind) -> Option<&CostsDiff> {
        self.0.get(event_kind)
    }
}

impl FlamegraphSummary {
    /// Create a new `FlamegraphSummary`
    pub fn new(event_kind: EventKind) -> Self {
        Self {
            event_kind,
            regular_path: Option::default(),
            old_path: Option::default(),
            diff_path: Option::default(),
        }
    }
}

impl SummaryOutput {
    /// Create a new `SummaryOutput` with `dir` as base dir and an extension fitting the
    /// [`SummaryFormat`]
    pub fn new(format: SummaryFormat, dir: &Path) -> Self {
        Self {
            format,
            path: dir.join("summary.json"),
        }
    }

    /// Initialize this `SummaryOutput` removing old summary files
    pub fn init(&self) {
        for entry in glob(self.path.with_extension("*").to_string_lossy().as_ref())
            .expect("Glob pattern should be valid")
        {
            std::fs::remove_file(entry.unwrap().as_path())
                .expect("Path from matched glob pattern should be present");
        }
    }

    /// Try to create an empty summary file returning the [`File`] object
    pub fn create(&self) -> Result<File> {
        File::create(&self.path).with_context(|| "Failed to create json summary file")
    }
}
