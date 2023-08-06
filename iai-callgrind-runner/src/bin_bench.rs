use std::ffi::OsString;
use std::fmt::Display;
use std::io::{stdin, Read};
use std::path::PathBuf;
use std::process::Command;

use colored::Colorize;
use iai_callgrind::{internal, Options};
use log::{debug, info, log_enabled, trace, Level};
use sanitize_filename::Options as SanitizerOptions;
use tempfile::TempDir;

use crate::callgrind::{CallgrindArgs, CallgrindCommand, CallgrindOutput};
use crate::util::{copy_directory, write_all_to_stderr, write_all_to_stdout};
use crate::{get_arch, IaiCallgrindError};

#[derive(Debug)]
struct BinBench {
    id: String,
    orig: String,
    command: PathBuf,
    args: Vec<String>,
    envs: Vec<(String, String)>,
    opts: Options,
}

impl BinBench {
    fn run(&self, config: &Config) -> Result<(), IaiCallgrindError> {
        let command = CallgrindCommand::new(config.allow_aslr, &config.arch);

        let mut callgrind_args = config.callgrind_args.clone();
        if let Some(entry_point) = &self.opts.entry_point {
            callgrind_args.collect_atstart = false;
            callgrind_args.insert_toggle_collect(entry_point);
        } else {
            callgrind_args.collect_atstart = true;
        }

        let output = CallgrindOutput::create(
            &config.package_dir,
            &config.module,
            &format!("{}.{}", self.id, self.sanitized_file_name()),
        );
        callgrind_args.set_output_file(&output.file.display().to_string());

        command.run(
            &callgrind_args,
            &self.command,
            self.args.clone(),
            self.envs.clone(),
            &self.opts,
        )?;

        let new_stats = output.parse_summary();

        let old_output = output.old_output();
        let old_stats = old_output.exists().then(|| old_output.parse_summary());

        println!(
            "{} {}{}{}",
            &config.module.green(),
            &self.id.cyan(),
            ":".cyan(),
            self.to_string().blue().bold()
        );
        new_stats.print(old_stats);
        Ok(())
    }

    fn sanitized_file_name(&self) -> String {
        let mut display_name = self.orig.clone();
        if !self.args.is_empty() {
            display_name.push('.');
            display_name.push_str(&self.args.join(" "));
        }
        sanitize_filename::sanitize_with_options(
            display_name,
            SanitizerOptions {
                windows: true,
                truncate: true,
                replacement: "_",
            },
        )
    }
}

impl Display for BinBench {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&format!("{} {}", self.orig, self.args.join(" ")))
    }
}

#[derive(Debug, Clone)]
enum AssistantKind {
    Setup,
    Teardown,
    Before,
    After,
}

impl AssistantKind {
    fn id(&self) -> String {
        match self {
            AssistantKind::Setup => "setup".to_owned(),
            AssistantKind::Teardown => "teardown".to_owned(),
            AssistantKind::Before => "before".to_owned(),
            AssistantKind::After => "after".to_owned(),
        }
    }
}

#[derive(Debug, Clone)]
struct Assistant {
    name: String,
    kind: AssistantKind,
    bench: bool,
}

impl Assistant {
    fn new(name: String, kind: AssistantKind, bench: bool) -> Self {
        Self { name, kind, bench }
    }

    fn run_bench(&self, config: &Config) -> Result<(), IaiCallgrindError> {
        let command = CallgrindCommand::new(config.allow_aslr, &config.arch);
        let executable_args = vec![
            "--iai-run".to_owned(),
            self.kind.id(),
            format!("{}::{}", &config.module, &self.name),
        ];
        let mut callgrind_args = config.callgrind_args.clone();
        callgrind_args.collect_atstart = false;
        callgrind_args.insert_toggle_collect(&format!("*{}::{}", &config.module, &self.name));

        let output = CallgrindOutput::create(&config.package_dir, &config.module, &self.name);
        callgrind_args.set_output_file(&output.file.display().to_string());
        command.run(
            &callgrind_args,
            &config.bench_bin,
            executable_args,
            vec![],
            &Options::default().env_clear(false),
        )?;

        let new_stats = output.parse(&config.bench_file, &config.module, &self.name);

        let old_output = output.old_output();
        let old_stats = old_output
            .exists()
            .then(|| old_output.parse(&config.bench_file, &config.module, &self.name));

        println!("{}", format!("{}::{}", &config.module, &self.name).green());
        new_stats.print(old_stats);
        Ok(())
    }

    fn run_plain(&self, config: &Config) -> Result<(), IaiCallgrindError> {
        let id = self.kind.id();
        let mut command = Command::new(&config.bench_bin);
        command.arg("--iai-run");
        command.arg(&id);

        let (stdout, stderr) = command
            .output()
            .map_err(|error| IaiCallgrindError::LaunchError(config.bench_bin.clone(), error))
            .and_then(|output| {
                if output.status.success() {
                    Ok((output.stdout, output.stderr))
                } else {
                    Err(IaiCallgrindError::BenchmarkLaunchError(output))
                }
            })?;

        if !stdout.is_empty() {
            info!("{} function '{}': stdout:", id, self.name);
            if log_enabled!(Level::Info) {
                write_all_to_stdout(&stdout);
            }
        }
        if !stderr.is_empty() {
            info!("{} function '{}': stderr:", id, self.name);
            if log_enabled!(Level::Info) {
                write_all_to_stderr(&stderr);
            }
        }
        Ok(())
    }

    fn run(&mut self, config: &Config) -> Result<(), IaiCallgrindError> {
        if self.bench {
            match self.kind {
                AssistantKind::Setup | AssistantKind::Teardown => self.bench = false,
                _ => {}
            }
            self.run_bench(config)
        } else {
            self.run_plain(config)
        }
    }
}

#[derive(Debug, Clone)]
struct BenchmarkAssistants {
    before: Option<Assistant>,
    after: Option<Assistant>,
    setup: Option<Assistant>,
    teardown: Option<Assistant>,
}

impl Default for BenchmarkAssistants {
    fn default() -> Self {
        Self::new()
    }
}

impl BenchmarkAssistants {
    fn new() -> Self {
        Self {
            before: Option::default(),
            after: Option::default(),
            setup: Option::default(),
            teardown: Option::default(),
        }
    }
}

#[derive(Debug, Clone)]
struct Fixtures {
    path: PathBuf,
    follow_symlinks: bool,
}

#[derive(Debug)]
pub(crate) struct Config {
    package_dir: PathBuf,
    bench_file: PathBuf,
    module: String,
    bench_bin: PathBuf,
    sandbox: bool,
    fixtures: Option<Fixtures>,
    benches: Vec<BinBench>,
    bench_assists: BenchmarkAssistants,
    callgrind_args: CallgrindArgs,
    allow_aslr: bool,
    arch: String,
}

impl Config {
    fn receive_benchmark(bytes: usize) -> Result<internal::BinaryBenchmark, IaiCallgrindError> {
        let mut encoded = vec![];
        let mut stdin = stdin();
        stdin.read_to_end(&mut encoded).map_err(|error| {
            IaiCallgrindError::Other(format!("Failed to read encoded configuration: {error}"))
        })?;
        assert!(
            encoded.len() == bytes,
            "Bytes mismatch when decoding configuration: Expected {bytes} bytes but received: {} \
             bytes",
            encoded.len()
        );

        let benchmark: internal::BinaryBenchmark =
            bincode::deserialize(&encoded).map_err(|error| {
                IaiCallgrindError::Other(format!("Failed to decode configuration: {error}"))
            })?;

        Ok(benchmark)
    }

    fn parse_fixtures(fixtures: Option<internal::Fixtures>) -> Option<Fixtures> {
        fixtures.map(|f| Fixtures {
            path: PathBuf::from(f.path),
            follow_symlinks: f.follow_symlinks,
        })
    }

    fn parse_runs(runs: Vec<internal::Run>) -> Vec<BinBench> {
        let mut benches = vec![];
        let mut counter: usize = 0;
        for run in runs {
            let orig = run.orig;
            let command = PathBuf::from(run.cmd);
            let opts = run.opts;
            let envs: Option<Vec<(String, String)>> = run.envs.map(|envs| {
                envs.iter()
                    .filter_map(|e| match e.split_once('=') {
                        Some((key, value)) => Some((key.to_owned(), value.to_owned())),
                        None => std::env::var(e).ok().map(|v| (e.clone(), v)),
                    })
                    .collect()
            });
            for args in run.args {
                let id = if let Some(id) = args.id {
                    id
                } else {
                    let id = counter.to_string();
                    counter += 1;
                    id
                };
                benches.push(BinBench {
                    id,
                    orig: orig.clone(),
                    command: command.clone(),
                    args: args.args,
                    envs: envs
                        .as_ref()
                        .map_or_else(std::vec::Vec::new, std::clone::Clone::clone),
                    opts: opts
                        .as_ref()
                        .map_or_else(Options::default, std::clone::Clone::clone),
                });
            }
        }
        benches
    }

    fn parse_assists(assists: Vec<internal::Assistant>) -> BenchmarkAssistants {
        let mut bench_assists = BenchmarkAssistants::default();
        for assist in assists {
            match assist.id.as_str() {
                "before" => {
                    bench_assists.before = Some(Assistant::new(
                        assist.name,
                        AssistantKind::Before,
                        assist.bench,
                    ));
                }
                "after" => {
                    bench_assists.after = Some(Assistant::new(
                        assist.name,
                        AssistantKind::After,
                        assist.bench,
                    ));
                }
                "setup" => {
                    bench_assists.setup = Some(Assistant::new(
                        assist.name,
                        AssistantKind::Setup,
                        assist.bench,
                    ));
                }
                "teardown" => {
                    bench_assists.teardown = Some(Assistant::new(
                        assist.name,
                        AssistantKind::Teardown,
                        assist.bench,
                    ));
                }
                name => panic!("Unknown assistant function: {name}"),
            }
        }
        bench_assists
    }

    fn parse_callgrind_args(options: &[String]) -> CallgrindArgs {
        let mut callgrind_args: Vec<OsString> = options.iter().map(OsString::from).collect();

        // The last argument is sometimes --bench. This argument comes from cargo and does not
        // belong to the arguments passed from the main macro. So, we're removing it if it is there.
        if callgrind_args.last().map_or(false, |a| a == "--bench") {
            callgrind_args.pop();
        }

        CallgrindArgs::from_args(&callgrind_args)
    }

    fn generate(
        mut env_args_iter: impl Iterator<Item = OsString> + std::fmt::Debug,
    ) -> Result<Self, IaiCallgrindError> {
        // The following unwraps are safe because these arguments are assuredly submitted by the
        // iai_callgrind::main macro
        let package_dir = PathBuf::from(env_args_iter.next().unwrap());
        let bench_file = PathBuf::from(env_args_iter.next().unwrap());
        let module = env_args_iter.next().unwrap().to_str().unwrap().to_owned();
        let bench_bin = PathBuf::from(env_args_iter.next().unwrap());
        let bytes = env_args_iter
            .next()
            .unwrap()
            .to_string_lossy()
            .parse::<usize>()
            .unwrap();

        let benchmark = Self::receive_benchmark(bytes)?;

        let sandbox = benchmark.sandbox;
        let fixtures = Self::parse_fixtures(benchmark.fixtures);
        let benches = Self::parse_runs(benchmark.runs);
        let bench_assists = Self::parse_assists(benchmark.assists);
        let callgrind_args = Self::parse_callgrind_args(&benchmark.options);

        let arch = get_arch();
        debug!("Detected architecture: {}", arch);

        let allow_aslr = std::env::var_os("IAI_ALLOW_ASLR").is_some();
        if allow_aslr {
            debug!("Found IAI_ALLOW_ASLR environment variable. Trying to run with ASLR enabled.");
        }

        Ok(Self {
            package_dir,
            bench_file,
            module,
            bench_bin,
            sandbox,
            fixtures,
            benches,
            bench_assists,
            callgrind_args,
            allow_aslr,
            arch,
        })
    }
}

fn setup_sandbox(config: &Config) -> Result<TempDir, IaiCallgrindError> {
    debug!("Creating temporary workspace directory");
    let temp_dir = tempfile::tempdir().expect("Create temporary directory");
    if let Some(fixtures) = &config.fixtures {
        debug!(
            "Copying fixtures from '{}' to '{}'",
            &fixtures.path.display(),
            temp_dir.path().display()
        );
        copy_directory(&fixtures.path, temp_dir.path(), fixtures.follow_symlinks)?;
    }
    trace!(
        "Changing current directory to temporary directory: '{}'",
        temp_dir.path().display()
    );
    std::env::set_current_dir(temp_dir.path())
        .expect("Set current directory to temporary workspace directory");
    Ok(temp_dir)
}

pub(crate) fn run(
    env_args_iter: impl Iterator<Item = OsString> + std::fmt::Debug,
) -> Result<(), IaiCallgrindError> {
    let config = Config::generate(env_args_iter)?;

    // We need the TempDir to exist within this function or else it's getting dropped and deleted
    // too early.
    let temp_dir = if config.sandbox {
        debug!("Setting up sandbox");
        Some(setup_sandbox(&config)?)
    } else {
        debug!(
            "Sandbox switched off: Running benchmarks in the current directory: '{}'",
            std::env::current_dir().unwrap().display()
        );
        None
    };

    let mut assists = config.bench_assists.clone();

    if let Some(before) = assists.before.as_mut() {
        before.run(&config)?;
    }
    for bench in &config.benches {
        if let Some(setup) = assists.setup.as_mut() {
            setup.run(&config)?;
        }

        bench.run(&config)?;

        if let Some(teardown) = assists.teardown.as_mut() {
            teardown.run(&config)?;
        }
    }
    if let Some(after) = assists.after.as_mut() {
        after.run(&config)?;
    }

    // Drop temp_dir and it's getting deleted
    drop(temp_dir);
    Ok(())
}