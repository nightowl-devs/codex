//! Local implementations of the OpenCode gate tools (bash/edit/glob/grep/
//! read/write), so Codex can execute them itself instead of a proxy having to
//! rewrite tool names and run them out-of-band.
//!
//! The free-tier inference gateway requires these six tools to be present with
//! their exact schemas; because Codex now ships them natively, no cloaking or
//! name restoration is needed on the wire.

use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::gate_tools_spec::create_bash_tool;
use crate::tools::handlers::gate_tools_spec::create_edit_tool;
use crate::tools::handlers::gate_tools_spec::create_glob_tool;
use crate::tools::handlers::gate_tools_spec::create_grep_tool;
use crate::tools::handlers::gate_tools_spec::create_read_tool;
use crate::tools::handlers::gate_tools_spec::create_write_tool;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

const MAX_OUTPUT_BYTES: usize = 128_000;
const DEFAULT_BASH_TIMEOUT_MS: u64 = 120_000;
const MAX_BASH_TIMEOUT_MS: u64 = 600_000;

fn cap(mut text: String) -> String {
    if text.len() > MAX_OUTPUT_BYTES {
        let mut cut = MAX_OUTPUT_BYTES;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n...[truncated]");
    }
    if text.is_empty() {
        return "(no output)".to_string();
    }
    text
}

fn resolve(path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(p)
    }
}

// ------------------------------------------------------------------- bash

#[derive(Debug, Deserialize)]
struct BashArgs {
    command: String,
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    workdir: Option<String>,
}

pub struct BashHandler;

impl ToolExecutor<ToolInvocation> for BashHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("bash")
    }

    fn spec(&self) -> ToolSpec {
        create_bash_tool()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "bash handler received unsupported payload".to_string(),
                ));
            };
            let args: BashArgs = parse_arguments(&arguments)?;
            if args.command.trim().is_empty() {
                return Err(FunctionCallError::RespondToModel(
                    "missing command".to_string(),
                ));
            }
            let timeout = args
                .timeout
                .unwrap_or(DEFAULT_BASH_TIMEOUT_MS)
                .min(MAX_BASH_TIMEOUT_MS);
            let cwd = args
                .workdir
                .as_deref()
                .map(resolve)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

            let mut cmd = tokio::process::Command::new("zsh");
            cmd.arg("-c")
                .arg(&args.command)
                .current_dir(&cwd)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true);

            let child = match cmd.spawn() {
                Ok(child) => child,
                Err(e) => {
                    return Ok(boxed_tool_output(FunctionToolOutput::from_text(
                        format!("error: failed to spawn command: {e}"),
                        Some(false),
                    )));
                }
            };

            match tokio::time::timeout(
                Duration::from_millis(timeout),
                child.wait_with_output(),
            )
            .await
            {
                Err(_) => Ok(boxed_tool_output(FunctionToolOutput::from_text(
                    format!("error: command timed out after {timeout}ms"),
                    Some(false),
                ))),
                Ok(Err(e)) => Ok(boxed_tool_output(FunctionToolOutput::from_text(
                    format!("error: command failed: {e}"),
                    Some(false),
                ))),
                Ok(Ok(o)) => {
                    let success = o.status.success();
                    let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
                    let err = String::from_utf8_lossy(&o.stderr);
                    if !err.is_empty() {
                        if !s.is_empty() {
                            s.push('\n');
                        }
                        s.push_str(&err);
                    }
                    if !success {
                        let code = o.status.code().unwrap_or(-1);
                        s.push_str(&format!("\nexit code {code}"));
                    }
                    Ok(boxed_tool_output(FunctionToolOutput::from_text(
                        cap(s),
                        Some(success),
                    )))
                }
            }
        })
    }
}

impl CoreToolRuntime for BashHandler {}

// ------------------------------------------------------------------- read

#[derive(Debug, Deserialize)]
struct ReadArgs {
    #[serde(alias = "file_path")]
    filePath: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

pub struct ReadHandler;

impl ToolExecutor<ToolInvocation> for ReadHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("read")
    }

    fn spec(&self) -> ToolSpec {
        create_read_tool()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "read handler received unsupported payload".to_string(),
                ));
            };
            let args: ReadArgs = parse_arguments(&arguments)?;
            let path = resolve(&args.filePath);

            if path.is_dir() {
                let mut entries: Vec<String> = std::fs::read_dir(&path)
                    .map_err(|e| {
                        FunctionCallError::RespondToModel(format!("error: {e}"))
                    })?
                    .filter_map(|e| e.ok())
                    .map(|e| {
                        let name = e.file_name().to_string_lossy().into_owned();
                        if e.path().is_dir() {
                            format!("{name}/")
                        } else {
                            name
                        }
                    })
                    .collect();
                entries.sort();
                return Ok(boxed_tool_output(FunctionToolOutput::from_text(
                    cap(entries.join("\n")),
                    Some(true),
                )));
            }

            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => {
                    return Ok(boxed_tool_output(FunctionToolOutput::from_text(
                        format!("error: {e}"),
                        Some(false),
                    )));
                }
            };
            let lines: Vec<&str> = text.split('\n').collect();
            let offset = args.offset.unwrap_or(1).max(1);
            let limit = args.limit.unwrap_or(2000).clamp(1, 2000);
            let slice = lines
                .iter()
                .skip(offset - 1)
                .take(limit)
                .enumerate()
                .map(|(i, line)| {
                    let mut l = (*line).to_string();
                    if l.chars().count() > 2000 {
                        l = l.chars().take(2000).collect::<String>();
                    }
                    format!("{}: {}", offset + i, l)
                })
                .collect::<Vec<_>>()
                .join("\n");
            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                cap(slice),
                Some(true),
            )))
        })
    }
}

impl CoreToolRuntime for ReadHandler {}

// ------------------------------------------------------------------ write

#[derive(Debug, Deserialize)]
struct WriteArgs {
    content: String,
    #[serde(alias = "file_path")]
    filePath: String,
}

pub struct WriteHandler;

impl ToolExecutor<ToolInvocation> for WriteHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("write")
    }

    fn spec(&self) -> ToolSpec {
        create_write_tool()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "write handler received unsupported payload".to_string(),
                ));
            };
            let args: WriteArgs = parse_arguments(&arguments)?;
            let path = resolve(&args.filePath);
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::write(&path, args.content.as_bytes()) {
                Ok(()) => Ok(boxed_tool_output(FunctionToolOutput::from_text(
                    format!("wrote {}", path.display()),
                    Some(true),
                ))),
                Err(e) => Ok(boxed_tool_output(FunctionToolOutput::from_text(
                    format!("error: {e}"),
                    Some(false),
                ))),
            }
        })
    }
}

impl CoreToolRuntime for WriteHandler {}

// ------------------------------------------------------------------- edit

#[derive(Debug, Deserialize)]
struct EditArgs {
    #[serde(alias = "file_path")]
    filePath: String,
    #[serde(alias = "old_string")]
    oldString: String,
    #[serde(alias = "new_string")]
    newString: String,
    #[serde(default, alias = "replace_all")]
    replaceAll: bool,
}

pub struct EditHandler;

impl ToolExecutor<ToolInvocation> for EditHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("edit")
    }

    fn spec(&self) -> ToolSpec {
        create_edit_tool()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "edit handler received unsupported payload".to_string(),
                ));
            };
            let args: EditArgs = parse_arguments(&arguments)?;
            let path = resolve(&args.filePath);
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => {
                    return Ok(boxed_tool_output(FunctionToolOutput::from_text(
                        format!("error: {e}"),
                        Some(false),
                    )));
                }
            };
            let matches = text.matches(&args.oldString).count();
            if matches == 0 {
                return Ok(boxed_tool_output(FunctionToolOutput::from_text(
                    format!("error: oldString not found in {}", path.display()),
                    Some(false),
                )));
            }
            if matches > 1 && !args.replaceAll {
                return Ok(boxed_tool_output(FunctionToolOutput::from_text(
                    format!(
                        "error: Found {matches} matches for oldString in {}. Provide more surrounding lines or set replaceAll.",
                        path.display()
                    ),
                    Some(false),
                )));
            }
            let next = if args.replaceAll {
                text.replace(&args.oldString, &args.newString)
            } else {
                text.replacen(&args.oldString, &args.newString, 1)
            };
            match std::fs::write(&path, next.as_bytes()) {
                Ok(()) => Ok(boxed_tool_output(FunctionToolOutput::from_text(
                    format!("edited {}", path.display()),
                    Some(true),
                ))),
                Err(e) => Ok(boxed_tool_output(FunctionToolOutput::from_text(
                    format!("error: {e}"),
                    Some(false),
                ))),
            }
        })
    }
}

impl CoreToolRuntime for EditHandler {}

// ------------------------------------------------------------ glob / grep

#[derive(Debug, Deserialize)]
struct GlobArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GrepArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    include: Option<String>,
}

fn run_rg(argv: Vec<String>, cwd: PathBuf) -> (bool, String) {
    match std::process::Command::new("rg")
        .args(&argv)
        .current_dir(&cwd)
        .output()
    {
        Ok(o) if o.status.success() => (true, String::from_utf8_lossy(&o.stdout).into_owned()),
        Ok(o) if o.status.code() == Some(1) => (true, String::new()),
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr).into_owned();
            (false, err)
        }
        Err(e) => (false, format!("error: {e}")),
    }
}

pub struct GlobHandler;

impl ToolExecutor<ToolInvocation> for GlobHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("glob")
    }

    fn spec(&self) -> ToolSpec {
        create_glob_tool()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "glob handler received unsupported payload".to_string(),
                ));
            };
            let args: GlobArgs = parse_arguments(&arguments)?;
            let dir = args
                .path
                .as_deref()
                .filter(|p| !p.is_empty())
                .map(resolve)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            let dir_str = dir.to_string_lossy().into_owned();
            // Always pass an explicit path operand so rg never reads stdin.
            let argv = vec![
                "--files".to_string(),
                "-g".to_string(),
                args.pattern.clone(),
                dir_str,
            ];
            let (ok, out) = tokio::task::spawn_blocking(move || run_rg(argv, dir.clone()))
                .await
                .map_err(|e| FunctionCallError::Fatal(format!("glob join error: {e}")))?;
            let text = if out.trim().is_empty() && ok {
                "(no matches)".to_string()
            } else {
                cap(out)
            };
            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                text,
                Some(ok),
            )))
        })
    }
}

impl CoreToolRuntime for GlobHandler {}

pub struct GrepHandler;

impl ToolExecutor<ToolInvocation> for GrepHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("grep")
    }

    fn spec(&self) -> ToolSpec {
        create_grep_tool()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "grep handler received unsupported payload".to_string(),
                ));
            };
            let args: GrepArgs = parse_arguments(&arguments)?;
            let dir = args
                .path
                .as_deref()
                .filter(|p| !p.is_empty())
                .map(resolve)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            let dir_str = dir.to_string_lossy().into_owned();
            let mut argv = vec![
                "-n".to_string(),
                "--no-heading".to_string(),
                args.pattern.clone(),
                dir_str,
            ];
            if let Some(inc) = args.include.as_deref().filter(|s| !s.is_empty()) {
                argv.push("-g".to_string());
                argv.push(inc.to_string());
            }
            let (ok, out) = tokio::task::spawn_blocking(move || run_rg(argv, dir.clone()))
                .await
                .map_err(|e| FunctionCallError::Fatal(format!("grep join error: {e}")))?;
            let text = if out.trim().is_empty() && ok {
                "(no matches)".to_string()
            } else {
                cap(out)
            };
            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                text,
                Some(ok),
            )))
        })
    }
}

impl CoreToolRuntime for GrepHandler {}
