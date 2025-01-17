pub mod args;
pub mod format;
pub mod logfile_parser;

use std::ffi::OsString;
use std::fmt::Display;
use std::fs::File;
use std::io::{stdout, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use anyhow::{anyhow, Context, Result};
use glob::glob;
use indexmap::IndexMap;
use log::{debug, error, log_enabled, Level};
use regex::Regex;
#[cfg(feature = "schema")]
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use self::args::ToolArgs;
use super::callgrind::parser::Parser;
use super::dhat::logfile_parser::LogfileParser as DhatLogfileParser;
use super::meta::Metadata;
use super::summary::ToolSummary;
use crate::api::ExitWith;
use crate::error::Error;
use crate::runner::print::tool_summary_header;
use crate::runner::summary::ToolRunSummary;
use crate::runner::tool::format::LogfileSummaryFormatter;
use crate::runner::tool::logfile_parser::LogfileParser;
use crate::util::{resolve_binary_path, truncate_str_utf8};
use crate::{api, util};

#[derive(Debug, Default, Clone)]
pub struct RunOptions {
    pub env_clear: bool,
    pub current_dir: Option<PathBuf>,
    pub entry_point: Option<String>,
    pub exit_with: Option<ExitWith>,
    pub envs: Vec<(OsString, OsString)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolConfig {
    pub tool: ValgrindTool,
    pub is_enabled: bool,
    pub args: ToolArgs,
    pub outfile_modifier: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutputPath {
    pub tool: ValgrindTool,
    pub dir: PathBuf,
    pub extension: String,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub enum ValgrindTool {
    Callgrind,
    Memcheck,
    Helgrind,
    DRD,
    Massif,
    DHAT,
    BBV,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolConfigs(pub Vec<ToolConfig>);

pub struct ToolCommand {
    tool: ValgrindTool,
    command: Command,
}

pub struct ToolOutput {
    pub tool: ValgrindTool,
    pub output: Output,
}

impl ToolCommand {
    pub fn new(tool: ValgrindTool, meta: &Metadata) -> Self {
        Self {
            tool,
            command: meta.into(),
        }
    }

    pub fn env_clear(&mut self) -> &mut Self {
        debug!("{}: Clearing environment variables", self.tool.id());
        for (key, _) in std::env::vars() {
            match (key.as_str(), self.tool) {
                (key @ ("DEBUGINFOD_URLS" | "PATH" | "HOME"), ValgrindTool::Memcheck) => {
                    debug!(
                        "{}: Clearing environment variables: Skipping {key}",
                        self.tool.id()
                    );
                }
                (key @ ("LD_PRELOAD" | "LD_LIBRARY_PATH"), _) => {
                    debug!(
                        "{}: Clearing environment variables: Skipping {key}",
                        self.tool.id()
                    );
                }
                _ => {
                    self.command.env_remove(key);
                }
            }
        }
        self
    }

    pub fn run(
        mut self,
        config: ToolConfig,
        executable: &Path,
        executable_args: &[OsString],
        options: RunOptions,
        output_path: &ToolOutputPath,
    ) -> Result<ToolOutput> {
        debug!(
            "{}: Running with executable '{}'",
            self.tool.id(),
            executable.display()
        );

        let RunOptions {
            env_clear,
            current_dir,
            exit_with,
            envs,
            ..
        } = options;

        if env_clear {
            self.env_clear();
        }
        if let Some(dir) = current_dir {
            debug!(
                "{}: Setting current directory to '{}'",
                self.tool.id(),
                dir.display()
            );
            self.command.current_dir(dir);
        }

        let mut tool_args = config.args;
        tool_args.set_output_arg(output_path, config.outfile_modifier.as_ref());
        tool_args.set_log_arg(output_path, config.outfile_modifier.as_ref());

        let executable = resolve_binary_path(executable)?;

        let output = self
            .command
            .args(tool_args.to_vec())
            .arg(&executable)
            .args(executable_args)
            .envs(envs)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|error| -> anyhow::Error {
                Error::LaunchError(PathBuf::from("valgrind"), error.to_string()).into()
            })
            .and_then(|output| {
                check_exit(
                    self.tool,
                    &executable,
                    output,
                    &output_path.to_log_output(),
                    exit_with.as_ref(),
                )
            })?;

        Ok(ToolOutput {
            tool: self.tool,
            output,
        })
    }
}

impl From<api::Tool> for ToolConfig {
    fn from(value: api::Tool) -> Self {
        let tool = value.kind.into();
        Self {
            tool,
            is_enabled: value.enable.unwrap_or(true),
            args: ToolArgs::from_raw_args(tool, value.raw_args),
            outfile_modifier: value.outfile_modifier,
        }
    }
}

impl ToolConfigs {
    pub fn has_tools_enabled(&self) -> bool {
        self.0.iter().any(|t| t.is_enabled)
    }

    pub fn run(
        &self,
        meta: &Metadata,
        executable: &Path,
        executable_args: &[OsString],
        options: &RunOptions,
        output_path: &ToolOutputPath,
    ) -> Result<Vec<ToolSummary>> {
        let mut tool_summaries = vec![];
        for tool_config in self.0.iter().filter(|t| t.is_enabled) {
            let tool = tool_config.tool;

            let mut tool_summary = ToolSummary {
                tool,
                log_paths: vec![],
                out_paths: vec![],
                summaries: vec![],
            };

            let command = ToolCommand::new(tool, meta);

            let output_path = output_path.to_tool_output(tool);
            output_path.init();

            let log_path = output_path.to_log_output();
            log_path.init();

            println!("{}", tool_summary_header(tool));

            let output = command.run(
                tool_config.clone(),
                executable,
                executable_args,
                options.clone(),
                &output_path,
            )?;

            if let ValgrindTool::DHAT = tool {
                let parser = DhatLogfileParser {
                    root_dir: meta.project_root.clone(),
                };
                let logfile_summaries = parser.parse(&log_path)?;
                for logfile_summary in logfile_summaries {
                    LogfileSummaryFormatter::print(&logfile_summary);

                    tool_summary.summaries.push(ToolRunSummary {
                        command: logfile_summary.command.to_string_lossy().to_string(),
                        pid: logfile_summary.pid.to_string(),
                        baseline: None,
                        summary: logfile_summary.fields.iter().cloned().collect(),
                    });
                }

                tool_summary.log_paths = log_path.real_paths();
                tool_summary.out_paths = output_path.real_paths();
            } else {
                let parser = LogfileParser {
                    root_dir: meta.project_root.clone(),
                };
                let logfile_summaries = parser.parse(&log_path)?;
                for logfile_summary in logfile_summaries {
                    LogfileSummaryFormatter::print(&logfile_summary);
                    let mut summary: IndexMap<String, String> =
                        logfile_summary.fields.iter().cloned().collect();
                    if !logfile_summary.body.is_empty() {
                        summary.insert("Summary".to_owned(), logfile_summary.body.join("\n"));
                    }
                    if let Some(error_summary) = logfile_summary.error_summary {
                        summary.insert("Error Summary".to_owned(), error_summary);
                    }
                    tool_summary.summaries.push(ToolRunSummary {
                        command: logfile_summary.command.to_string_lossy().to_string(),
                        pid: logfile_summary.pid.to_string(),
                        baseline: None,
                        summary,
                    });
                }
                if tool.has_output_file() {
                    tool_summary.out_paths = output_path.real_paths();
                }
                tool_summary.log_paths = log_path.real_paths();
            }

            output.dump_log(log::Level::Info);
            log_path.dump_log(log::Level::Info, &mut stdout())?;

            tool_summaries.push(tool_summary);
        }

        Ok(tool_summaries)
    }
}

impl ToolOutput {
    pub fn dump_log(&self, log_level: Level) {
        if log::log_enabled!(log_level) {
            let (stdout, stderr) = (&self.output.stdout, &self.output.stderr);
            if !stdout.is_empty() {
                log::log!(log_level, "{} output on stdout:", self.tool.id());
                util::write_all_to_stdout(stdout);
            }
            if !stderr.is_empty() {
                log::log!(log_level, "{} output on stderr:", self.tool.id());
                util::write_all_to_stderr(stderr);
            }
        }
    }
}

impl ToolOutputPath {
    pub fn new(tool: ValgrindTool, base_dir: &Path, module: &str, name: &str) -> Self {
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
        let sanitized_name = truncate_str_utf8(&sanitized_name, 200);
        Self {
            tool,
            dir: current
                .join(base_dir)
                .join(module_path)
                .join(sanitized_name),
            extension: "out".to_owned(),
            name: sanitized_name.to_owned(),
        }
    }

    pub fn from_existing<T>(path: T) -> Result<Self>
    where
        T: Into<PathBuf>,
    {
        let path: PathBuf = path.into();
        if !path.is_file() {
            return Err(anyhow!(
                "The output file '{}' did not exist or is not a valid file",
                path.display()
            ));
        }
        let file_name = path.file_name().unwrap().to_string_lossy();
        let re = Regex::new(r"^(?<tool>.*?)[.](?<name>.*)[.](?<extension>out(\..*)?)$")
            .expect("Regex should compile");
        let caps = re
            .captures(&file_name)
            .ok_or_else(|| anyhow!("Illegal file name: {file_name}"))?;

        Ok(Self {
            tool: caps
                .name("tool")
                .ok_or_else(|| anyhow!("Illegal file name: {file_name}"))?
                .as_str()
                .try_into()
                .unwrap(),
            dir: path.parent().unwrap().to_owned(),
            extension: caps
                .name("extension")
                .ok_or_else(|| anyhow!("Illegal file name: {file_name}"))?
                .as_str()
                .to_owned(),
            name: caps
                .name("name")
                .ok_or_else(|| anyhow!("Illegal file name: {file_name}"))?
                .as_str()
                .to_owned(),
        })
    }

    /// Initialize and create the output directory and organize files
    ///
    /// This method moves the old output to `$TOOL_ID.*.out.old`
    /// TODO: RETURN Result
    pub fn with_init(tool: ValgrindTool, base_dir: &Path, module: &str, name: &str) -> Self {
        let output = Self::new(tool, base_dir, module, name);
        output.init();
        output
    }

    // TODO: RETURN Result
    pub fn init(&self) {
        std::fs::create_dir_all(&self.dir).expect("Failed to create directory");
        self.move_old();
    }

    pub fn move_old(&self) {
        let path = self.to_path();

        // Cleanup old files
        for entry in glob(&format!("{}*.old", path.display()))
            .expect("Reading glob patterns should succeed")
            .map(Result::unwrap)
        {
            std::fs::remove_file(entry).unwrap();
        }

        // Move existing files to *.old
        for entry in glob(&format!("{}*", path.display()))
            .expect("Reading glob patterns should succeed")
            .map(Result::unwrap)
        {
            let mut extension = entry.extension().unwrap().to_owned();
            extension.push(".old");
            std::fs::rename(&entry, entry.with_extension(extension)).unwrap();
        }
    }

    pub fn exists(&self) -> bool {
        self.to_path().exists()
    }

    pub fn to_old_output(&self) -> Self {
        let mut extension = self.extension.clone();
        if !std::path::Path::new(&extension)
            .extension()
            .map_or(false, |ext| ext.eq_ignore_ascii_case("old"))
        {
            extension.push_str(".old");
        }
        Self {
            tool: self.tool,
            name: self.name.clone(),
            extension,
            dir: self.dir.clone(),
        }
    }

    pub fn to_tool_output(&self, tool: ValgrindTool) -> Self {
        Self {
            tool,
            name: self.name.clone(),
            extension: self.extension.clone(),
            dir: self.dir.clone(),
        }
    }

    pub fn to_log_output(&self) -> Self {
        Self {
            tool: self.tool,
            name: self.name.clone(),
            extension: "log".to_owned(),
            dir: self.dir.clone(),
        }
    }

    pub fn open(&self) -> Result<File> {
        let path = self.to_path();
        File::open(&path).with_context(|| {
            format!(
                "Error opening {} output file '{}'",
                self.tool.id(),
                path.display()
            )
        })
    }

    pub fn lines(&self) -> Result<impl Iterator<Item = String>> {
        let file = self.open()?;
        Ok(BufReader::new(file)
            .lines()
            .map(std::result::Result::unwrap))
    }

    pub fn dump_log(&self, log_level: log::Level, writer: &mut impl Write) -> Result<()> {
        if log_enabled!(log_level) {
            for path in self.real_paths() {
                log::log!(
                    log_level,
                    "{} log output '{}':",
                    self.tool.id(),
                    path.display()
                );

                let file = File::open(&path).with_context(|| {
                    format!(
                        "Error opening {} output file '{}'",
                        self.tool.id(),
                        path.display()
                    )
                })?;

                let mut reader = BufReader::new(file);
                std::io::copy(&mut reader, writer)?;
            }
        }
        Ok(())
    }

    pub fn to_path(&self) -> PathBuf {
        self.dir.join(format!(
            "{}.{}.{}",
            self.tool.id(),
            self.name,
            self.extension,
        ))
    }

    pub fn real_paths(&self) -> Vec<PathBuf> {
        glob(&format!("{}*", self.to_path().display()))
            .expect("Reading glob patterns should succeed")
            .map(Result::unwrap)
            .filter(|e| {
                e.extension()
                    .map_or(false, |e| !e.eq_ignore_ascii_case("old"))
            })
            .collect()
    }
}

impl Display for ToolOutputPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{}", self.to_path().display()))
    }
}

impl ValgrindTool {
    /// Return the id used by the `valgrind --tool` option
    pub fn id(&self) -> String {
        match self {
            ValgrindTool::DHAT => "dhat".to_owned(),
            ValgrindTool::Callgrind => "callgrind".to_owned(),
            ValgrindTool::Memcheck => "memcheck".to_owned(),
            ValgrindTool::Helgrind => "helgrind".to_owned(),
            ValgrindTool::DRD => "drd".to_owned(),
            ValgrindTool::Massif => "massif".to_owned(),
            ValgrindTool::BBV => "exp-bbv".to_owned(),
        }
    }

    pub fn has_output_file(&self) -> bool {
        matches!(
            self,
            ValgrindTool::Callgrind | ValgrindTool::DHAT | ValgrindTool::BBV | ValgrindTool::Massif
        )
    }
}

impl From<api::ValgrindTool> for ValgrindTool {
    fn from(value: api::ValgrindTool) -> Self {
        match value {
            api::ValgrindTool::Memcheck => ValgrindTool::Memcheck,
            api::ValgrindTool::Helgrind => ValgrindTool::Helgrind,
            api::ValgrindTool::DRD => ValgrindTool::DRD,
            api::ValgrindTool::Massif => ValgrindTool::Massif,
            api::ValgrindTool::DHAT => ValgrindTool::DHAT,
            api::ValgrindTool::BBV => ValgrindTool::BBV,
        }
    }
}

impl TryFrom<&str> for ValgrindTool {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> std::result::Result<Self, Self::Error> {
        match value {
            "dhat" => Ok(ValgrindTool::DHAT),
            "callgrind" => Ok(ValgrindTool::Callgrind),
            "memcheck" => Ok(ValgrindTool::Memcheck),
            "helgrind" => Ok(ValgrindTool::Helgrind),
            "drd" => Ok(ValgrindTool::DRD),
            "massif" => Ok(ValgrindTool::Massif),
            "exp-bbv" => Ok(ValgrindTool::BBV),
            v => Err(anyhow!("Unknown tool '{}'", v)),
        }
    }
}

pub fn check_exit(
    tool: ValgrindTool,
    executable: &Path,
    output: Output,
    output_path: &ToolOutputPath,
    exit_with: Option<&ExitWith>,
) -> Result<Output> {
    let status_code = if let Some(code) = output.status.code() {
        code
    } else {
        return Err(Error::ProcessError((tool.id(), output, Some(output_path.clone()))).into());
    };

    match (status_code, exit_with) {
        (0i32, None | Some(ExitWith::Code(0i32) | ExitWith::Success)) => Ok(output),
        (0i32, Some(ExitWith::Code(code))) => {
            error!(
                "{}: Expected '{}' to exit with '{}' but it succeeded",
                tool.id(),
                executable.display(),
                code
            );
            Err(Error::ProcessError((tool.id(), output, Some(output_path.clone()))).into())
        }
        (0i32, Some(ExitWith::Failure)) => {
            error!(
                "{}: Expected '{}' to fail but it succeeded",
                tool.id(),
                executable.display(),
            );
            Err(Error::ProcessError((tool.id(), output, Some(output_path.clone()))).into())
        }
        (_, Some(ExitWith::Failure)) => Ok(output),
        (code, Some(ExitWith::Success)) => {
            error!(
                "{}: Expected '{}' to succeed but it terminated with '{}'",
                tool.id(),
                executable.display(),
                code
            );
            Err(Error::ProcessError((tool.id(), output, Some(output_path.clone()))).into())
        }
        (actual_code, Some(ExitWith::Code(expected_code))) if actual_code == *expected_code => {
            Ok(output)
        }
        (actual_code, Some(ExitWith::Code(expected_code))) => {
            error!(
                "{}: Expected '{}' to exit with '{}' but it terminated with '{}'",
                tool.id(),
                executable.display(),
                expected_code,
                actual_code
            );
            Err(Error::ProcessError((tool.id(), output, Some(output_path.clone()))).into())
        }
        _ => Err(Error::ProcessError((tool.id(), output, Some(output_path.clone()))).into()),
    }
}
