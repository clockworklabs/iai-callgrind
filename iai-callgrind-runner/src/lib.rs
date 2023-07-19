use cfg_if::cfg_if;
use colored::{ColoredString, Colorize};
use log::{debug, info, trace, warn};
use std::{
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

#[derive(Debug)]
struct Config {
    runner_version: String,
    library_version: String,
    bench_file: PathBuf,
    benches: Vec<String>,
    executable: PathBuf,
    module: String,
    callgrind_args: CallgrindArgs,
    allow_aslr: bool,
    arch: String,
}

impl Config {
    fn new() -> Self {
        let mut args_iter = std::env::args().skip(1);

        let library_version = args_iter.next().unwrap();
        let runner_version = env!("CARGO_PKG_VERSION").to_string();

        let bench_file = PathBuf::from(args_iter.next().unwrap());
        let module = args_iter.next().unwrap();
        let mut benches = vec![];
        let executable = loop {
            if let Some(arg) = args_iter.next() {
                match arg.split_once('=') {
                    Some((key, value)) if key == "--iai-bench" => benches.push(value.to_string()),
                    Some(_) | None => break PathBuf::from(arg),
                }
            }
        };

        let mut callgrind_args = args_iter
            .filter(|a| a.starts_with("--"))
            .collect::<Vec<String>>();
        if callgrind_args.last().map_or(false, |a| a == "--bench") {
            callgrind_args.pop();
        }
        let callgrind_args = CallgrindArgs::from_args(callgrind_args);

        let arch = get_arch();
        debug!("Detected architecture: {}", arch);

        let allow_aslr = std::env::var_os("IAI_ALLOW_ASLR").is_some();
        if allow_aslr {
            debug!("Found IAI_ALLOW_ASLR environment variable. Trying to run with ASLR enabled.");
        }

        Self {
            runner_version,
            library_version,
            bench_file,
            benches,
            executable,
            module,
            callgrind_args,
            allow_aslr,
            arch,
        }
    }
}

// TODO: Replace with platform_info or std::env::consts::ARCH??
fn get_arch() -> String {
    let output = Command::new("uname")
        .arg("-m")
        .stdout(Stdio::piped())
        .output()
        .expect("Failed to run `uname` to determine machine architecture.");

    String::from_utf8(output.stdout)
        .expect("`uname -m` returned invalid unicode.")
        .trim()
        .to_owned()
}

fn basic_valgrind() -> Command {
    Command::new("valgrind")
}

// Invoke Valgrind, disabling ASLR if possible because ASLR could noise up the results a bit
cfg_if! {
    if #[cfg(target_os = "linux")] {
        fn valgrind_without_aslr(arch: &str) -> Option<Command> {
            let mut cmd = Command::new("setarch");
            cmd.arg(arch)
                .arg("-R")
                .arg("valgrind");
            Some(cmd)
        }
    } else if #[cfg(target_os = "freebsd")] {
        fn valgrind_without_aslr(_arch: &str) -> Option<Command> {
            let mut cmd = Command::new("proccontrol");
            cmd.arg("-m")
                .arg("aslr")
                .arg("-s")
                .arg("disable");
            Some(cmd)
        }
    } else {
        fn valgrind_without_aslr(_arch: &str) -> Option<Command> {
            // Can't disable ASLR on this platform
            None
        }
    }
}

#[derive(Debug)]
struct CallgrindArgs {
    i1: String,
    d1: String,
    ll: String,
    cache_sim: String,
    collect_atstart: String,
    other: Vec<String>,
    toggle_collect: Option<Vec<String>>,
    compress_strings: String,
    compress_pos: String,
    callgrind_out_file: Option<String>,
}

impl Default for CallgrindArgs {
    fn default() -> Self {
        Self {
            // Set some reasonable cache sizes. The exact sizes matter less than having fixed sizes,
            // since otherwise callgrind would take them from the CPU and make benchmark runs
            // even more incomparable between machines.
            i1: String::from("--I1=32768,8,64"),
            d1: String::from("--D1=32768,8,64"),
            ll: String::from("--LL=8388608,16,64"),
            cache_sim: String::from("--cache-sim=yes"),
            collect_atstart: String::from("--collect-atstart=no"),
            toggle_collect: Default::default(),
            compress_pos: String::from("--compress-pos=no"),
            compress_strings: String::from("--compress-strings=no"),
            callgrind_out_file: Default::default(),
            other: Default::default(),
        }
    }
}

impl CallgrindArgs {
    fn from_args(args: Vec<String>) -> Self {
        let mut default = Self::default();
        for arg in args.iter() {
            if arg.starts_with("--I1=") {
                default.i1 = arg.to_owned();
            } else if arg.starts_with("--D1=") {
                default.d1 = arg.to_owned();
            } else if arg.starts_with("--LL=") {
                default.ll = arg.to_owned();
            } else if arg.starts_with("--cache-sim=") {
                warn!("Ignoring callgrind argument: '{}'", arg);
            } else if arg.starts_with("--collect-atstart=") {
                default.collect_atstart = arg.to_owned();
            } else if arg.starts_with("--compress-strings=") {
                default.compress_strings = arg.to_owned();
            } else if arg.starts_with("--compress-pos=") {
                default.compress_pos = arg.to_owned();
            } else if arg.starts_with("--toggle-collect=") {
                info!(
                    "The callgrind argument '{}' will be appended to the default setting.",
                    arg
                );
                match default.toggle_collect.as_mut() {
                    Some(toggle_arg) => {
                        toggle_arg.push(arg.to_owned());
                    }
                    None => {
                        default.toggle_collect = Some(vec![arg.to_owned()]);
                    }
                };
            } else if arg.starts_with("--callgrind-out-file=") {
                warn!("Ignoring callgrind argument: '{}'", arg);
            } else {
                default.other.push(arg.to_owned());
            }
        }
        default
    }

    fn parse_with(&self, output_file: &Path, module: &str, function_name: &str) -> Vec<String> {
        let mut args = vec![
            self.i1.clone(),
            self.d1.clone(),
            self.ll.clone(),
            self.cache_sim.clone(),
            self.collect_atstart.clone(),
            self.compress_strings.clone(),
            self.compress_pos.clone(),
        ];

        args.extend_from_slice(self.other.as_slice());

        match &self.callgrind_out_file {
            Some(arg) => args.push(arg.clone()),
            None => args.push(format!("--callgrind-out-file={}", output_file.display())),
        }

        args.push(format!("--toggle-collect=*{}::{}", module, function_name));
        if let Some(arg) = &self.toggle_collect {
            args.extend_from_slice(arg.as_slice())
        }

        args
    }
}

#[inline(never)]
fn run_bench(
    index: usize,
    function_name: &str,
    config: &Config,
) -> Result<(CallgrindStats, Option<CallgrindStats>), IaiCallgrindError> {
    let mut cmd = if config.allow_aslr {
        debug!("Running with ASLR enabled");
        basic_valgrind()
    } else {
        match valgrind_without_aslr(config.arch.as_str()) {
            Some(cmd) => {
                debug!("Running with ASLR disabled");
                cmd
            }
            None => {
                debug!("Running with ASLR enabled");
                basic_valgrind()
            }
        }
    };

    let target = PathBuf::from("target/iai");
    let module_path: PathBuf = config.module.split("::").collect();
    let file_name = PathBuf::from(format!("callgrind.{}.out", function_name));

    let mut output_file = target;
    output_file.push(module_path);
    output_file.push(file_name);

    let old_file = output_file.with_extension("out.old");

    std::fs::create_dir_all(output_file.parent().unwrap()).expect("Failed to create directory");

    if output_file.exists() {
        // Already run this benchmark once; move last results to .old
        std::fs::copy(&output_file, &old_file).unwrap();
    }

    let callgrind_args =
        config
            .callgrind_args
            .parse_with(&output_file, config.module.as_str(), function_name);
    debug!("Callgrind arguments: {}", callgrind_args.join(" "));
    let output = cmd
        .arg("--tool=callgrind")
        .args(callgrind_args)
        .arg(&config.executable)
        .arg("--iai-run")
        .arg(index.to_string())
        // Currently not used in iai-callgrind itself, but in `callgrind_annotate` this name is
        // shown and makes it easier to identify the benchmark under test
        .arg(format!("{}::{}", config.module, function_name))
        // valgrind doesn't output anything on stdout
        // .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(IaiCallgrindError::LaunchError)
        .and_then(|output| {
            if output.status.success() {
                let stderr = String::from_utf8_lossy(output.stderr.as_slice());
                Ok(stderr.trim_end().to_string())
            } else {
                Err(IaiCallgrindError::CallgrindLaunchError(output))
            }
        })?;

    if !output.is_empty() {
        info!("Callgrind output:\n{}", output);
    }

    let new_stats = parse_callgrind_output(
        &output_file,
        &config.bench_file,
        &config.module,
        function_name,
    );
    let old_stats = if old_file.exists() {
        Some(parse_callgrind_output(
            &old_file,
            &config.bench_file,
            &config.module,
            function_name,
        ))
    } else {
        None
    };

    Ok((new_stats, old_stats))
}

// A curated sample output which this function must be able to parse to CallgrindStats.
// For more details see the format specification https://valgrind.org/docs/manual/cl-format.html
//
// # callgrind format
// # ... a lot of lines which we're not interested in
// fn=test_file::test_function
// 0 4 1 2 1 1 0 1
// cfn=some::library::function
// calls=1 0
// 0 3 1 0 1 0 0 1
// 0 12 3 6 1 0 0 1
// cfn=some::other::library::function
// calls=1 0
// 0 6789 593 463 72 37 18 72 37 18
// 0 4 2 0 1 0 0 1
//
// # the empty line above or the end of file ends the parsing
fn parse_callgrind_output(
    file: &Path,
    bench_file: &Path,
    module: &str,
    function_name: &str,
) -> CallgrindStats {
    trace!(
        "Parsing callgrind output file '{}' for '{}::{}'",
        file.display(),
        module,
        function_name
    );

    let sentinel = format!("fn={}", [module, function_name].join("::"));
    trace!(
        "Using sentinel: '{}' for file name ending with: '{}'",
        &sentinel,
        bench_file.display()
    );

    let file_in = File::open(file).expect("Unable to open callgrind output file");
    let mut iter = BufReader::new(file_in).lines().map(|l| l.unwrap());
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
    let mut start_record = false;
    let mut maybe_counting = false;
    let mut start_counting = false;
    for line in iter {
        let line = line.trim_start();
        if line.is_empty() {
            start_record = false;
            maybe_counting = false;
            start_counting = false;
        }
        if !start_record {
            if line.starts_with("fl=") && line.ends_with(bench_file.to_str().unwrap()) {
                trace!("Found line with benchmark file: '{}'", line);
            } else if line.starts_with(&sentinel) {
                trace!("Found line with sentinel: '{}'", line);
                start_record = true;
            }
            continue;
        }
        // We're only interested in the counters for event counters within the benchmark function
        // and ignore counters for the benchmark function itself.
        if !maybe_counting {
            if line.starts_with("cfn=") {
                trace!("Found line with a calling function: '{}'", line);
                maybe_counting = true;
            }
            continue;
        }
        if !start_counting {
            if line.starts_with("calls") {
                trace!("Found line with calls: '{}'. Starting the counting", line);
                start_counting = true;
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
                // skip the first number which is just the line number
                .skip(1)
                .map(|s| s.parse::<u64>().expect("Encountered non ascii digit"))
                // we're only interested in the counters for instructions and the cache
                .take(9)
                .enumerate()
            {
                counters[index] += counter;
            }
            trace!("Updated counters to '{:?}'", &counters);
        } else if line.starts_with("cfn=") {
            trace!("Found line with a calling function: '{}'", line);
            start_counting = false;
        } else {
            trace!("Pausing counting. End of a cfn record");
            maybe_counting = false;
            start_counting = false;
        }
    }

    CallgrindStats {
        l1_instructions_cache_reads: counters[0],
        total_data_cache_reads: counters[1],
        total_data_cache_writes: counters[2],
        l1_instructions_cache_read_misses: counters[3],
        l1_data_cache_read_misses: counters[4],
        l1_data_cache_write_misses: counters[5],
        l3_instructions_cache_misses: counters[6],
        l3_data_cache_read_misses: counters[7],
        l3_data_cache_write_misses: counters[8],
    }
}

#[derive(Clone, Debug)]
struct CallgrindStats {
    /// Ir: equals the number of instructions executed
    l1_instructions_cache_reads: u64,
    /// I1mr: I1 cache read misses
    l1_instructions_cache_read_misses: u64,
    /// ILmr: LL cache instruction read misses
    l3_instructions_cache_misses: u64,
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
        let ram_hits = self.l3_instructions_cache_misses
            + self.l3_data_cache_read_misses
            + self.l3_data_cache_write_misses;
        let l1_data_accesses = self.l1_data_cache_read_misses + self.l1_data_cache_write_misses;
        let l1_miss = self.l1_instructions_cache_read_misses + l1_data_accesses;
        let l3_accesses = l1_miss;
        let l3_hits = l3_accesses - ram_hits;

        let total_memory_rw = self.l1_instructions_cache_reads
            + self.total_data_cache_reads
            + self.total_data_cache_writes;
        let l1_data_hits =
            total_memory_rw - self.l1_instructions_cache_reads - (ram_hits + l3_hits);
        assert!(
            total_memory_rw == l1_data_hits + self.l1_instructions_cache_reads + l3_hits + ram_hits
        );

        // Uses Itamar Turner-Trauring's formula from https://pythonspeed.com/articles/consistent-benchmarking-in-ci/
        let cycles =
            self.l1_instructions_cache_reads + l1_data_hits + (5 * l3_hits) + (35 * ram_hits);

        CallgrindSummary {
            l1_instructions: self.l1_instructions_cache_reads,
            l1_data_hits,
            l3_hits,
            ram_hits,
            total_memory_rw,
            cycles,
        }
    }

    fn signed_short(n: f64) -> String {
        let n_abs = n.abs();

        if n_abs < 10.0 {
            format!("{:+.6}", n)
        } else if n_abs < 100.0 {
            format!("{:+.5}", n)
        } else if n_abs < 1000.0 {
            format!("{:+.4}", n)
        } else if n_abs < 10000.0 {
            format!("{:+.3}", n)
        } else if n_abs < 100000.0 {
            format!("{:+.2}", n)
        } else if n_abs < 1000000.0 {
            format!("{:+.1}", n)
        } else {
            format!("{:+.0}", n)
        }
    }

    fn percentage_diff(new: u64, old: u64) -> ColoredString {
        fn format(string: ColoredString) -> ColoredString {
            ColoredString::from(format!(" ({})", string).as_str())
        }

        if new == old {
            return format("No Change".bright_black());
        }

        let new = new as f64;
        let old = old as f64;

        let diff = (new - old) / old;
        let pct = diff * 100.0;

        if pct.is_sign_positive() {
            format(
                format!("{:>+6}%", Self::signed_short(pct))
                    .bright_red()
                    .bold(),
            )
        } else {
            format(
                format!("{:>+6}%", Self::signed_short(pct))
                    .bright_green()
                    .bold(),
            )
        }
    }

    fn print(&self, old: Option<CallgrindStats>) {
        let summary = self.summarize();
        let old_summary = old.map(|stat| stat.summarize());
        println!(
            "  Instructions:     {:>15}{}",
            summary.l1_instructions.to_string().bold(),
            match &old_summary {
                Some(old) => Self::percentage_diff(summary.l1_instructions, old.l1_instructions),
                None => String::new().normal(),
            }
        );
        println!(
            "  L1 Data Hits:     {:>15}{}",
            summary.l1_data_hits.to_string().bold(),
            match &old_summary {
                Some(old) => Self::percentage_diff(summary.l1_data_hits, old.l1_data_hits),
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

#[derive(Clone, Debug)]
struct CallgrindSummary {
    l1_instructions: u64,
    l1_data_hits: u64,
    l3_hits: u64,
    ram_hits: u64,
    total_memory_rw: u64,
    cycles: u64,
}

pub enum IaiCallgrindError {
    VersionMismatch(version_compare::Cmp, String, String),
    LaunchError(std::io::Error),
    CallgrindLaunchError(Output),
}

pub fn run() -> Result<(), IaiCallgrindError> {
    let config = Config::new();

    match version_compare::compare(&config.runner_version, &config.library_version) {
        Ok(cmp) => match cmp {
            version_compare::Cmp::Lt | version_compare::Cmp::Gt => {
                return Err(IaiCallgrindError::VersionMismatch(
                    cmp,
                    config.runner_version,
                    config.library_version,
                ));
            }
            // version_compare::compare only returns Cmp::Lt, Cmp::Gt and Cmp::Eq so the versions
            // are equal here
            _ => {}
        },
        // iai-callgrind versions before 0.3.0 don't submit the version
        Err(_) => {
            return Err(IaiCallgrindError::VersionMismatch(
                version_compare::Cmp::Ne,
                config.runner_version,
                config.library_version,
            ));
        }
    }

    for (index, name) in config.benches.iter().enumerate() {
        let (stats, old_stats) = run_bench(index, name, &config)?;

        println!("{}", format!("{}::{}", config.module, name).green());
        stats.print(old_stats);
    }

    Ok(())
}
