pub mod args;
pub mod flamegraph;
pub mod hashmap_parser;

use std::convert::AsRef;
use std::ffi::OsString;
use std::fmt::Display;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::str::FromStr;

use colored::{ColoredString, Colorize};
use log::{debug, error, info, trace, warn, Level};
use which::which;

use super::callgrind::args::CallgrindArgs;
use super::meta::Metadata;
use crate::api::ExitWith;
use crate::error::{IaiCallgrindError, Result};
use crate::util::{truncate_str_utf8, write_all_to_stderr, write_all_to_stdout};

#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventType {
    // always on
    Ir,
    // --collect-systime
    sysCount,
    sysTime,
    sysCpuTime,
    // --collect-bus
    Ge,
    // --cache-sim
    Dr,
    Dw,
    I1mr,
    ILmr,
    D1mr,
    DLmr,
    D1mw,
    DLmw,
    // --branch-sim
    Bc,
    Bcm,
    Bi,
    Bim,
    // --simulate-wb
    ILdmr,
    DLdmr,
    DLdmw,
    // --cachuse
    AcCost1,
    AcCost2,
    SpLoss1,
    SpLoss2,
}

impl<T> From<T> for EventType
where
    T: AsRef<str>,
{
    fn from(value: T) -> Self {
        match value.as_ref() {
            "Ir" => Self::Ir,
            "Dr" => Self::Dr,
            "Dw" => Self::Dw,
            "I1mr" => Self::I1mr,
            "ILmr" => Self::ILmr,
            "D1mr" => Self::D1mr,
            "DLmr" => Self::DLmr,
            "D1mw" => Self::D1mw,
            "DLmw" => Self::DLmw,
            "sysCount" => Self::sysCount,
            "sysTime" => Self::sysTime,
            "sysCpuTime" => Self::sysCpuTime,
            "Ge" => Self::Ge,
            "Bc" => Self::Bc,
            "Bcm" => Self::Bcm,
            "Bi" => Self::Bi,
            "Bim" => Self::Bim,
            "ILdmr" => Self::ILdmr,
            "DLdmr" => Self::DLdmr,
            "DLdmw" => Self::DLdmw,
            "AcCost1" => Self::AcCost1,
            "AcCost2" => Self::AcCost2,
            "SpLoss1" => Self::SpLoss1,
            "SpLoss2" => Self::SpLoss2,
            unknown => unreachable!("Unknown event type: {unknown}"),
        }
    }
}

impl Display for EventType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{self:?}"))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Event {
    kind: EventType,
    cost: u64,
}

#[derive(Debug, Clone)]
pub struct Costs(Vec<Event>);

impl Costs {
    pub fn add_iter_str<I, T>(&mut self, iter: T)
    where
        I: AsRef<str>,
        T: IntoIterator<Item = I>,
    {
        // From the documentation of the callgrind format:
        // > If a cost line specifies less event counts than given in the "events" line, the
        // > rest is assumed to be zero.
        for (event, cost) in self.0.iter_mut().zip(iter.into_iter()) {
            event.cost += cost.as_ref().parse::<u64>().unwrap();
        }
    }
    pub fn add(&mut self, other: &Self) {
        for (event, cost) in self
            .0
            .iter_mut()
            .zip(other.0.iter().map(|event| event.cost))
        {
            event.cost += cost;
        }
    }

    pub fn get_by_index(&self, index: usize) -> Option<&Event> {
        self.0.get(index)
    }

    pub fn get_by_type(&self, kind: EventType) -> Option<&Event> {
        self.0.iter().find(|e| e.kind == kind)
    }
}

impl Default for Costs {
    fn default() -> Self {
        Self(vec![Event {
            kind: EventType::Ir,
            cost: 0,
        }])
    }
}

impl<I> FromIterator<I> for Costs
where
    I: AsRef<str>,
{
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = I>,
    {
        Self(
            iter.into_iter()
                .map(|s| Event {
                    kind: EventType::from(s),
                    cost: 0,
                })
                .collect::<Vec<_>>(),
        )
    }
}

#[derive(Debug, Default, Clone)]
pub struct CallgrindOptions {
    pub env_clear: bool,
    pub current_dir: Option<PathBuf>,
    pub entry_point: Option<String>,
    pub exit_with: Option<ExitWith>,
    pub envs: Vec<(OsString, OsString)>,
}

pub struct CallgrindCommand {
    command: Command,
}

pub trait CallgrindParser {
    fn parse<T>(&mut self, file: T) -> Result<()>
    where
        T: AsRef<CallgrindOutput>,
        Self: std::marker::Sized;
}

impl CallgrindCommand {
    pub fn new(meta: &Metadata) -> Self {
        let command = meta.valgrind_wrapper.as_ref().map_or_else(
            || {
                let meta_cmd = &meta.valgrind;
                let mut cmd = Command::new(&meta_cmd.bin);
                cmd.args(&meta_cmd.args);
                cmd
            },
            |meta_cmd| {
                let mut cmd = Command::new(&meta_cmd.bin);
                cmd.args(&meta_cmd.args);
                cmd
            },
        );
        Self { command }
    }

    fn check_exit(
        executable: &Path,
        output: Output,
        exit_with: Option<&ExitWith>,
    ) -> Result<(Vec<u8>, Vec<u8>)> {
        match (output.status.code().unwrap(), exit_with) {
            (0i32, None | Some(ExitWith::Code(0i32) | ExitWith::Success)) => {
                Ok((output.stdout, output.stderr))
            }
            (0i32, Some(ExitWith::Code(code))) => {
                error!(
                    "Expected benchmark '{}' to exit with '{}' but it succeeded",
                    executable.display(),
                    code
                );
                Err(IaiCallgrindError::BenchmarkLaunchError(output))
            }
            (0i32, Some(ExitWith::Failure)) => {
                error!(
                    "Expected benchmark '{}' to fail but it succeeded",
                    executable.display(),
                );
                Err(IaiCallgrindError::BenchmarkLaunchError(output))
            }
            (_, Some(ExitWith::Failure)) => Ok((output.stdout, output.stderr)),
            (code, Some(ExitWith::Success)) => {
                error!(
                    "Expected benchmark '{}' to succeed but it exited with '{}'",
                    executable.display(),
                    code
                );
                Err(IaiCallgrindError::BenchmarkLaunchError(output))
            }
            (actual_code, Some(ExitWith::Code(expected_code))) if actual_code == *expected_code => {
                Ok((output.stdout, output.stderr))
            }
            (actual_code, Some(ExitWith::Code(expected_code))) => {
                error!(
                    "Expected benchmark '{}' to exit with '{}' but it exited with '{}'",
                    executable.display(),
                    expected_code,
                    actual_code
                );
                Err(IaiCallgrindError::BenchmarkLaunchError(output))
            }
            _ => Err(IaiCallgrindError::BenchmarkLaunchError(output)),
        }
    }

    pub fn run(
        self,
        mut callgrind_args: CallgrindArgs,
        executable: &Path,
        executable_args: &[OsString],
        options: CallgrindOptions,
        output_file: &Path,
    ) -> Result<()> {
        let mut command = self.command;
        debug!(
            "Running callgrind with executable '{}'",
            executable.display()
        );
        let CallgrindOptions {
            env_clear,
            current_dir,
            exit_with,
            entry_point,
            envs,
        } = options;

        if env_clear {
            debug!("Clearing environment variables");
            command.env_clear();
        }
        if let Some(dir) = current_dir {
            debug!("Setting current directory to '{}'", dir.display());
            command.current_dir(dir);
        }

        if let Some(entry_point) = entry_point {
            callgrind_args.collect_atstart = false;
            callgrind_args.insert_toggle_collect(&entry_point);
        } else {
            callgrind_args.collect_atstart = true;
        }
        callgrind_args.set_output_file(output_file);

        let callgrind_args = callgrind_args.to_vec();
        debug!("Callgrind arguments: {}", &callgrind_args.join(" "));

        let executable = if executable.is_absolute() {
            executable.to_owned()
        } else {
            let e = which(executable).map_err(|error| {
                IaiCallgrindError::Other(format!("{}: '{}'", error, executable.display()))
            })?;
            debug!(
                "Found command '{}' in the PATH: '{}'",
                executable.display(),
                e.display()
            );
            e
        };

        let (stdout, stderr) = command
            .arg("--tool=callgrind")
            .args(callgrind_args)
            .arg(&executable)
            .args(executable_args)
            .envs(envs)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|error| IaiCallgrindError::LaunchError(PathBuf::from("valgrind"), error))
            .and_then(|output| Self::check_exit(&executable, output, exit_with.as_ref()))?;

        if !stdout.is_empty() {
            info!("Callgrind output on stdout:");
            if log::log_enabled!(Level::Info) {
                write_all_to_stdout(&stdout);
            }
        }
        if !stderr.is_empty() {
            info!("Callgrind output on stderr:");
            if log::log_enabled!(Level::Info) {
                write_all_to_stderr(&stderr);
            }
        }

        Ok(())
    }
}

// TODO: Rename to needle
#[derive(Debug, Clone)]
pub struct Sentinel(String);

impl Sentinel {
    pub fn new(value: &str) -> Self {
        Self(value.to_owned())
    }

    pub fn from_path(module: &str, function: &str) -> Self {
        Self(format!("{module}::{function}"))
    }

    #[allow(unused)]
    pub fn from_segments<I, T>(segments: T) -> Self
    where
        I: AsRef<str>,
        T: AsRef<[I]>,
    {
        let joined = if let Some((first, suffix)) = segments.as_ref().split_first() {
            suffix.iter().fold(first.as_ref().to_owned(), |mut a, b| {
                a.push_str("::");
                a.push_str(b.as_ref());
                a
            })
        } else {
            String::new()
        };
        Self(joined)
    }

    pub fn to_fn(&self) -> String {
        format!("fn={}", self.0)
    }

    pub fn matches(&self, string: &str) -> bool {
        string.starts_with(self.0.as_str())
    }
}

impl Display for Sentinel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<Sentinel> for Sentinel {
    fn as_ref(&self) -> &Sentinel {
        self
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum PositionsMode {
    Instr,
    Line,
    InstrLine,
}

impl PositionsMode {
    pub fn from_positions_line(line: &str) -> Option<Self> {
        match line.trim().strip_prefix("positions: ") {
            Some("instr line" | "line instr") => Some(Self::InstrLine),
            Some("instr") => Some(Self::Instr),
            Some("line") => Some(Self::Line),
            Some(_) | None => None,
        }
    }
}

impl Default for PositionsMode {
    fn default() -> Self {
        Self::Line
    }
}

impl FromStr for PositionsMode {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let mode = match s.trim() {
            "instr line" | "line instr" => Self::InstrLine,
            "instr" => Self::Instr,
            "line" => Self::Line,
            mode => return Err(format!("Invalid positions mode: '{mode}'")),
        };
        std::result::Result::Ok(mode)
    }
}

#[derive(Debug)]
pub struct CallgrindOutput {
    pub file: PathBuf,
}

impl CallgrindOutput {
    pub fn create(base_dir: &Path, module: &str, name: &str) -> Self {
        let current = base_dir;
        let module_path: PathBuf = module.split("::").collect();
        let sanitized_name = sanitize_filename::sanitize_with_options(
            name,
            sanitize_filename::Options {
                windows: false,
                truncate: false,
                replacement: "_",
            },
        );
        let file_name = PathBuf::from(format!(
            "callgrind.{}.out",
            truncate_str_utf8(&sanitized_name, 237) /* callgrind. + .out.old = 18 with max
                                                     * length 255 */
        ));

        let file = current.join(base_dir).join(module_path).join(file_name);
        let output = Self { file };

        std::fs::create_dir_all(output.file.parent().unwrap()).expect("Failed to create directory");

        if output.file.exists() {
            let old_output = output.old_output();
            // Already run this benchmark once; move last results to .old
            std::fs::copy(&output.file, old_output.file).unwrap();
        }

        output
    }

    pub fn exists(&self) -> bool {
        self.file.exists()
    }

    pub fn old_output(&self) -> Self {
        CallgrindOutput {
            file: self.file.with_extension("out.old"),
        }
    }

    pub fn parse_summary(&self) -> CallgrindStats {
        trace!(
            "Parsing callgrind output file '{}' for a summary or totals",
            self.file.display(),
        );

        let file = File::open(&self.file).expect("Unable to open callgrind output file");
        let mut iter = BufReader::new(file)
            .lines()
            .map(std::result::Result::unwrap);
        if !iter
            .by_ref()
            .find(|l| !l.trim().is_empty())
            .expect("Found empty file")
            .contains("callgrind format")
        {
            warn!("Missing file format specifier. Assuming callgrind format.");
        };

        // Ir Dr Dw I1mr D1mr D1mw ILmr DLmr DLmw
        let mut counters: [u64; 9] = [0, 0, 0, 0, 0, 0, 0, 0, 0];
        for line in iter {
            if line.starts_with("summary:") {
                trace!("Found line with summary: '{}'", line);
                for (index, counter) in line
                    .strip_prefix("summary:")
                    .unwrap()
                    .trim()
                    .split_ascii_whitespace()
                    .map(|s| s.parse::<u64>().expect("Encountered non ascii digit"))
                    // we're only interested in the counters for instructions and the cache
                    .take(9)
                    .enumerate()
                {
                    counters[index] += counter;
                }
                trace!("Updated counters to '{:?}'", &counters);
                break;
            }
            if line.starts_with("totals:") {
                trace!("Found line with totals: '{}'", line);
                for (index, counter) in line
                    .strip_prefix("totals:")
                    .unwrap()
                    .trim()
                    .split_ascii_whitespace()
                    .map(|s| s.parse::<u64>().expect("Encountered non ascii digit"))
                    // we're only interested in the counters for instructions and the cache
                    .take(9)
                    .enumerate()
                {
                    counters[index] += counter;
                }
                trace!("Updated counters to '{:?}'", &counters);
                break;
            }
        }

        CallgrindStats {
            instructions_executed: counters[0],
            total_data_cache_reads: counters[1],
            total_data_cache_writes: counters[2],
            l1_instructions_cache_read_misses: counters[3],
            l1_data_cache_read_misses: counters[4],
            l1_data_cache_write_misses: counters[5],
            l3_instructions_cache_read_misses: counters[6],
            l3_data_cache_read_misses: counters[7],
            l3_data_cache_write_misses: counters[8],
        }
    }

    pub fn parse<T>(&self, bench_file: &Path, sentinel: T) -> CallgrindStats
    where
        T: AsRef<Sentinel>,
    {
        let sentinel = sentinel.as_ref();
        trace!(
            "Parsing callgrind output file '{}' for '{}'",
            self.file.display(),
            sentinel
        );

        trace!(
            "Using sentinel: '{}' for file name ending with: '{}'",
            &sentinel,
            bench_file.display()
        );

        let file = File::open(&self.file).expect("Unable to open callgrind output file");
        let mut iter = BufReader::new(file)
            .lines()
            .map(std::result::Result::unwrap);
        if !iter
            .by_ref()
            .find(|l| !l.trim().is_empty())
            .expect("Found empty file")
            .contains("callgrind format")
        {
            warn!("Missing file format specifier. Assuming callgrind format.");
        };

        let mode = iter
            .find_map(|line| PositionsMode::from_positions_line(&line))
            .expect("Callgrind output line with mode for positions");
        trace!("Using parsing mode: {:?}", mode);

        // Ir Dr Dw I1mr D1mr D1mw ILmr DLmr DLmw
        let mut counters: [u64; 9] = [0, 0, 0, 0, 0, 0, 0, 0, 0];
        let mut start_record = false;
        for line in iter {
            let line = line.trim_start();
            if line.is_empty() {
                start_record = false;
            }
            if !start_record {
                if line.starts_with("fl=") && line.ends_with(bench_file.to_str().unwrap()) {
                    trace!("Found line with benchmark file: '{}'", line);
                } else if line.starts_with(&sentinel.to_fn()) {
                    trace!("Found line with sentinel: '{}'", line);
                    start_record = true;
                } else {
                    // do nothing
                }
                continue;
            }

            // we check if it is a line with counters and summarize them
            if line.starts_with(|c: char| c.is_ascii_digit()) {
                // From the documentation of the callgrind format:
                // > If a cost line specifies less event counts than given in the "events" line, the
                // > rest is assumed to be zero.
                trace!("Found line with counters: '{}'", line);
                for (index, counter) in line
                    .split_ascii_whitespace()
                    // skip the first number which is just the line number or instr number or in
                    // case of `instr line` skip 2
                    .skip(if mode == PositionsMode::InstrLine { 2 } else { 1 })
                    .map(|s| s.parse::<u64>().expect("Encountered non ascii digit"))
                    // we're only interested in the counters for instructions and the cache
                    .take(9)
                    .enumerate()
                {
                    counters[index] += counter;
                }
                trace!("Updated counters to '{:?}'", &counters);
            } else {
                trace!("Skipping line: '{}'", line);
            }
        }

        CallgrindStats {
            instructions_executed: counters[0],
            total_data_cache_reads: counters[1],
            total_data_cache_writes: counters[2],
            l1_instructions_cache_read_misses: counters[3],
            l1_data_cache_read_misses: counters[4],
            l1_data_cache_write_misses: counters[5],
            l3_instructions_cache_read_misses: counters[6],
            l3_data_cache_read_misses: counters[7],
            l3_data_cache_write_misses: counters[8],
        }
    }
}

impl AsRef<CallgrindOutput> for CallgrindOutput {
    fn as_ref(&self) -> &Self {
        self
    }
}

#[derive(Clone, Debug)]
pub struct CallgrindSummary {
    instructions: u64,
    l1_hits: u64,
    l3_hits: u64,
    ram_hits: u64,
    total_memory_rw: u64,
    cycles: u64,
}

#[derive(Clone, Debug)]
pub struct CallgrindStats {
    /// Ir: equals the number of instructions executed
    instructions_executed: u64,
    /// I1mr: I1 cache read misses
    l1_instructions_cache_read_misses: u64,
    /// ILmr: LL cache instruction read misses
    l3_instructions_cache_read_misses: u64,
    /// Dr: Memory reads
    total_data_cache_reads: u64,
    /// D1mr: D1 cache read misses
    l1_data_cache_read_misses: u64,
    /// DLmr: LL cache data read misses
    l3_data_cache_read_misses: u64,
    /// Dw: Memory writes
    total_data_cache_writes: u64,
    /// D1mw: D1 cache write misses
    l1_data_cache_write_misses: u64,
    /// DLmw: LL cache data write misses
    l3_data_cache_write_misses: u64,
}

impl CallgrindStats {
    fn summarize(&self) -> CallgrindSummary {
        let ram_hits = self.l3_instructions_cache_read_misses
            + self.l3_data_cache_read_misses
            + self.l3_data_cache_write_misses;
        let l1_data_accesses = self.l1_data_cache_read_misses + self.l1_data_cache_write_misses;
        let l1_miss = self.l1_instructions_cache_read_misses + l1_data_accesses;
        let l3_accesses = l1_miss;
        let l3_hits = l3_accesses - ram_hits;

        let total_memory_rw =
            self.instructions_executed + self.total_data_cache_reads + self.total_data_cache_writes;
        let l1_hits = total_memory_rw - ram_hits - l3_hits;

        // Uses Itamar Turner-Trauring's formula from https://pythonspeed.com/articles/consistent-benchmarking-in-ci/
        let cycles = l1_hits + (5 * l3_hits) + (35 * ram_hits);

        CallgrindSummary {
            instructions: self.instructions_executed,
            l1_hits,
            l3_hits,
            ram_hits,
            total_memory_rw,
            cycles,
        }
    }

    fn signed_short(n: f64) -> String {
        let n_abs = n.abs();

        if n_abs < 10.0f64 {
            format!("{n:+.6}")
        } else if n_abs < 100.0f64 {
            format!("{n:+.5}")
        } else if n_abs < 1000.0f64 {
            format!("{n:+.4}")
        } else if n_abs < 10000.0f64 {
            format!("{n:+.3}")
        } else if n_abs < 100_000.0_f64 {
            format!("{n:+.2}")
        } else if n_abs < 1_000_000.0_f64 {
            format!("{n:+.1}")
        } else {
            format!("{n:+.0}")
        }
    }

    fn percentage_diff(new: u64, old: u64) -> ColoredString {
        fn format(string: &ColoredString) -> ColoredString {
            ColoredString::from(format!(" ({string})").as_str())
        }

        if new == old {
            return format(&"No Change".bright_black());
        }

        #[allow(clippy::cast_precision_loss)]
        let new = new as f64;
        #[allow(clippy::cast_precision_loss)]
        let old = old as f64;

        let diff = (new - old) / old;
        let pct = diff * 100.0f64;

        if pct.is_sign_positive() {
            format(
                &format!("{:>+6}%", Self::signed_short(pct))
                    .bright_red()
                    .bold(),
            )
        } else {
            format(
                &format!("{:>+6}%", Self::signed_short(pct))
                    .bright_green()
                    .bold(),
            )
        }
    }

    pub fn print(&self, old: Option<CallgrindStats>) {
        let summary = self.summarize();
        let old_summary = old.map(|stat| stat.summarize());
        println!(
            "  Instructions:     {:>15}{}",
            summary.instructions.to_string().bold(),
            match &old_summary {
                Some(old) => Self::percentage_diff(summary.instructions, old.instructions),
                None => String::new().normal(),
            }
        );
        println!(
            "  L1 Hits:          {:>15}{}",
            summary.l1_hits.to_string().bold(),
            match &old_summary {
                Some(old) => Self::percentage_diff(summary.l1_hits, old.l1_hits),
                None => String::new().normal(),
            }
        );
        println!(
            "  L2 Hits:          {:>15}{}",
            summary.l3_hits.to_string().bold(),
            match &old_summary {
                Some(old) => Self::percentage_diff(summary.l3_hits, old.l3_hits),
                None => String::new().normal(),
            }
        );
        println!(
            "  RAM Hits:         {:>15}{}",
            summary.ram_hits.to_string().bold(),
            match &old_summary {
                Some(old) => Self::percentage_diff(summary.ram_hits, old.ram_hits),
                None => String::new().normal(),
            }
        );
        println!(
            "  Total read+write: {:>15}{}",
            summary.total_memory_rw.to_string().bold(),
            match &old_summary {
                Some(old) => Self::percentage_diff(summary.total_memory_rw, old.total_memory_rw),
                None => String::new().normal(),
            }
        );
        println!(
            "  Estimated Cycles: {:>15}{}",
            summary.cycles.to_string().bold(),
            match &old_summary {
                Some(old) => Self::percentage_diff(summary.cycles, old.cycles),
                None => String::new().normal(),
            }
        );
    }
}
