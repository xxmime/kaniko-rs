//! Dockerfile parser implementation.
//!
//! Parses a Dockerfile string into a list of [`Instruction`] values.
//! Supports:
//! - Multi-line continuation (`\` at end of line)
//! - ARG/ENV variable substitution (`$VAR`, `${VAR}`)
//! - All Dockerfile instructions including ONBUILD, STOPSIGNAL, HEALTHCHECK, SHELL

use crate::instruction::*;
use std::collections::HashMap;
use thiserror::Error;

/// Errors that can occur during parsing.
#[derive(Debug, Error)]
pub enum ParseError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse error at line {line}: {message}")]
    Syntax { line: usize, message: String },
}

/// Result type for parsing.
pub type Result<T> = std::result::Result<T, ParseError>;

/// A parsed Dockerfile stage.
#[derive(Debug, Clone)]
pub struct Stage {
    /// Stage index (0-based).
    pub index: usize,
    /// Base image name.
    pub image: String,
    /// Optional alias (FROM ... AS alias).
    pub alias: Option<String>,
    /// Instructions in this stage.
    pub instructions: Vec<Instruction>,
}

/// Variable substitution context - tracks ARG and ENV definitions
/// across the build for `$VAR` / `${VAR}` replacement.
#[derive(Debug, Clone, Default)]
pub struct VarContext {
    /// ARG values (with defaults; can be overridden by --build-arg at runtime).
    args: HashMap<String, String>,
    /// ENV values set so far in the Dockerfile.
    env: HashMap<String, String>,
    /// Runtime build-arg overrides (from --build-arg flag).
    build_args: HashMap<String, String>,
}

impl VarContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_build_args(build_args: HashMap<String, String>) -> Self {
        Self {
            build_args,
            ..Default::default()
        }
    }

    /// Set an ARG with its default value.
    pub fn set_arg(&mut self, name: String, default: Option<String>) {
        // Only set default if not already overridden by --build-arg
        if !self.build_args.contains_key(&name) {
            if let Some(val) = default {
                self.args.insert(name, val);
            } else {
                // ARG without default - register as empty
                self.args.insert(name, String::new());
            }
        }
    }

    /// Set an ENV variable.
    pub fn set_env(&mut self, key: String, value: String) {
        self.env.insert(key, value);
    }

    /// Look up a variable: prefer build_args > env > args.
    pub fn get(&self, name: &str) -> Option<String> {
        if let Some(v) = self.build_args.get(name) {
            return Some(v.clone());
        }
        if let Some(v) = self.env.get(name) {
            return Some(v.clone());
        }
        if let Some(v) = self.args.get(name) {
            return Some(v.clone());
        }
        None
    }

    /// Substitute `$VAR` and `${VAR}` in the input string.
    pub fn substitute(&self, input: &str) -> String {
        substitute_vars(input, self)
    }
}

/// Substitute `$VAR` and `${VAR}` patterns in a string.
///
/// Supports:
/// - `$VAR` - replaced if VAR is defined, otherwise left as-is
/// - `${VAR}` - replaced if VAR is defined, otherwise left as-is
/// - `$$` - escaped literal `$`
/// - `$` at end of string - left as-is
pub fn substitute_vars(input: &str, ctx: &VarContext) -> String {
    let mut result = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        if chars[i] == '$' {
            if i + 1 < len && chars[i + 1] == '$' {
                // Escaped $$
                result.push('$');
                i += 2;
            } else if i + 1 < len && chars[i + 1] == '{' {
                // ${VAR} form
                let start = i + 2;
                if let Some(end) = chars[start..].iter().position(|&c| c == '}') {
                    let var_name: String = chars[start..start + end].iter().collect();
                    if let Some(val) = ctx.get(&var_name) {
                        result.push_str(&val);
                    } else {
                        // Variable not found - leave as-is
                        result.push_str(&input[i..=i + start + end]);
                    }
                    i = start + end + 1;
                } else {
                    // No closing brace - leave as-is
                    result.push('$');
                    i += 1;
                }
            } else if i + 1 < len && (chars[i + 1].is_ascii_alphanumeric() || chars[i + 1] == '_') {
                // $VAR form
                let start = i + 1;
                let mut end = start;
                while end < len && (chars[end].is_ascii_alphanumeric() || chars[end] == '_') {
                    end += 1;
                }
                let var_name: String = chars[start..end].iter().collect();
                if let Some(val) = ctx.get(&var_name) {
                    result.push_str(&val);
                } else {
                    // Variable not found - leave as-is
                    for ch in &chars[i..end] {
                        result.push(*ch);
                    }
                }
                i = end;
            } else {
                // Lone $ - leave as-is
                result.push('$');
                i += 1;
            }
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }

    result
}

/// Join continuation lines ending with `\` into single logical lines.
fn join_continuation_lines(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let mut result = String::new();
    let mut i = 0;

    while i < lines.len() {
        let mut line = lines[i].to_string();
        while line.ends_with('\\') && i + 1 < lines.len() {
            line.pop(); // Remove the backslash
            i += 1;
            line.push_str(lines[i]);
        }
        result.push_str(&line);
        result.push('\n');
        i += 1;
    }

    result
}

/// Parse a Dockerfile string into stages.
pub fn parse_dockerfile(content: &str) -> Result<Vec<Stage>> {
    parse_dockerfile_with_build_args(content, HashMap::new())
}

/// Parse a Dockerfile string into stages, with runtime --build-arg overrides.
pub fn parse_dockerfile_with_build_args(
    content: &str,
    build_args: HashMap<String, String>,
) -> Result<Vec<Stage>> {
    let joined = join_continuation_lines(content);
    let mut stages: Vec<Stage> = Vec::new();
    let mut current_stage: Option<Stage> = None;
    let mut ctx = VarContext::with_build_args(build_args);
    // ARG instructions before the first FROM - tracked for variable substitution
    // but not added to any stage (per Docker spec).
    let mut pre_from_args: Vec<Instruction> = Vec::new();

    for (line_num, line) in joined.lines().enumerate() {
        let trimmed = line.trim();

        // Skip empty lines and comments
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let (directive, rest) = split_directive(trimmed);

        match directive.to_uppercase().as_str() {
            "FROM" => {
                // Save previous stage
                if let Some(stage) = current_stage.take() {
                    stages.push(stage);
                }

                // Substitute variables in FROM (e.g., FROM $IMAGE:$TAG)
                let substituted = ctx.substitute(rest);
                let (image, alias, _platform) = parse_from(&substituted)?;
                let instructions = std::mem::take(&mut pre_from_args);
                current_stage = Some(Stage {
                    index: stages.len(),
                    image,
                    alias,
                    instructions,
                });
            }
            "ARG" => {
                let (name, default) = parse_arg(rest);
                ctx.set_arg(name.clone(), default.clone());
                let instr = Instruction::Arg(ArgInstruction {
                    name,
                    default_value: default,
                });
                if let Some(stage) = current_stage.as_mut() {
                    stage.instructions.push(instr);
                } else {
                    // ARG before first FROM - save for later
                    pre_from_args.push(instr);
                }
            }
            "ENV" => {
                let (key, value) = parse_env(rest);
                // Substitute variables in the value using current context
                let substituted_value = ctx.substitute(&value);
                ctx.set_env(key.clone(), substituted_value.clone());
                let instr = Instruction::Env(EnvInstruction {
                    key,
                    value: substituted_value,
                });
                add_instruction(&mut current_stage, instr, line_num)?;
            }
            "RUN" => {
                let cmd = ctx.substitute(rest.trim());
                let is_shell = !cmd.starts_with('[');
                add_instruction(&mut current_stage, Instruction::Run(RunInstruction {
                    command: cmd,
                    is_shell_form: is_shell,
                }), line_num)?;
            }
            "CMD" => {
                let substituted = ctx.substitute(rest);
                let cmd = parse_string_list(&substituted);
                add_instruction(&mut current_stage, Instruction::Cmd(CmdInstruction {
                    command: cmd,
                    is_shell_form: false,
                }), line_num)?;
            }
            "LABEL" => {
                let substituted = ctx.substitute(rest);
                let labels = parse_key_value_pairs(&substituted);
                add_instruction(&mut current_stage, Instruction::Label(LabelInstruction { labels }), line_num)?;
            }
            "EXPOSE" => {
                let substituted = ctx.substitute(rest);
                let ports: Vec<String> = substituted.split_whitespace().map(String::from).collect();
                add_instruction(&mut current_stage, Instruction::Expose(ExposeInstruction { ports }), line_num)?;
            }
            "ADD" => {
                let substituted = ctx.substitute(rest);
                let (sources, dest) = parse_add(&substituted);
                add_instruction(&mut current_stage, Instruction::Add(AddInstruction {
                    sources,
                    destination: dest,
                    chmod: None,
                    chown: None,
                    link: false,
                }), line_num)?;
            }
            "COPY" => {
                let substituted = ctx.substitute(rest);
                let (sources, dest, from) = parse_copy(&substituted);
                add_instruction(&mut current_stage, Instruction::Copy(CopyInstruction {
                    sources,
                    destination: dest,
                    from,
                    chmod: None,
                    chown: None,
                    link: false,
                }), line_num)?;
            }
            "ENTRYPOINT" => {
                let substituted = ctx.substitute(rest);
                let cmd = parse_string_list(&substituted);
                add_instruction(&mut current_stage, Instruction::Entrypoint(EntrypointInstruction {
                    command: cmd,
                    is_shell_form: false,
                }), line_num)?;
            }
            "VOLUME" => {
                let substituted = ctx.substitute(rest);
                let paths = parse_string_list(&substituted);
                add_instruction(&mut current_stage, Instruction::Volume(VolumeInstruction { paths }), line_num)?;
            }
            "USER" => {
                let substituted = ctx.substitute(rest).trim().to_string();
                add_instruction(&mut current_stage, Instruction::User(UserInstruction { user: substituted }), line_num)?;
            }
            "WORKDIR" => {
                let substituted = ctx.substitute(rest).trim().to_string();
                add_instruction(&mut current_stage, Instruction::Workdir(WorkdirInstruction { path: substituted }), line_num)?;
            }
            "SHELL" => {
                let substituted = ctx.substitute(rest);
                let shell = parse_string_list(&substituted);
                add_instruction(&mut current_stage, Instruction::Shell(ShellInstruction { shell }), line_num)?;
            }
            "STOPSIGNAL" => {
                let substituted = ctx.substitute(rest).trim().to_string();
                add_instruction(&mut current_stage, Instruction::StopSignal(StopSignalInstruction { signal: substituted }), line_num)?;
            }
            "HEALTHCHECK" => {
                let hc = parse_healthcheck(rest)?;
                add_instruction(&mut current_stage, Instruction::Healthcheck(hc), line_num)?;
            }
            "ONBUILD" => {
                let inner = parse_onbuild(rest)?;
                add_instruction(&mut current_stage, Instruction::Onbuild(OnbuildInstruction {
                    instruction: Box::new(inner),
                }), line_num)?;
            }
            "MAINTAINER" => {
                let name = ctx.substitute(rest).trim().to_string();
                add_instruction(&mut current_stage, Instruction::Maintainer(MaintainerInstruction { name }), line_num)?;
            }
            _ => {
                return Err(ParseError::Syntax {
                    line: line_num,
                    message: format!("Unknown instruction: {}", directive),
                });
            }
        }
    }

    // Save the last stage
    if let Some(stage) = current_stage.take() {
        stages.push(stage);
    }

    Ok(stages)
}

/// Split a line into directive and the rest.
fn split_directive(line: &str) -> (&str, &str) {
    if let Some(pos) = line.find(char::is_whitespace) {
        (&line[..pos], &line[pos..])
    } else {
        (line, "")
    }
}

/// Parse FROM instruction.
fn parse_from(rest: &str) -> Result<(String, Option<String>, Option<String>)> {
    let parts: Vec<&str> = rest.trim().split_whitespace().collect();
    let mut platform = None;
    let mut alias = None;
    let mut image = String::new();

    let mut i = 0;
    while i < parts.len() {
        if parts[i].starts_with("--platform=") {
            platform = Some(parts[i][11..].to_string());
        } else if parts[i].eq_ignore_ascii_case("AS") && i + 1 < parts.len() {
            alias = Some(parts[i + 1].to_string());
            break;
        } else if image.is_empty() {
            image = parts[i].to_string();
        }
        i += 1;
    }

    Ok((image, alias, platform))
}

/// Parse JSON-style string list ["a", "b"].
fn parse_string_list(rest: &str) -> Vec<String> {
    let rest = rest.trim();
    if rest.starts_with('[') {
        // JSON array format
        let inner = rest.trim_start_matches('[').trim_end_matches(']');
        inner
            .split(',')
            .map(|s| s.trim().trim_matches('"').to_string())
            .collect()
    } else {
        // Shell format: split by whitespace
        rest.split_whitespace().map(String::from).collect()
    }
}

/// Parse key=value pairs for LABEL.
fn parse_key_value_pairs(rest: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut quote_char = '"';

    for ch in rest.chars() {
        if in_quotes {
            if ch == quote_char {
                in_quotes = false;
            } else {
                current.push(ch);
            }
        } else if ch == '"' || ch == '\'' {
            in_quotes = true;
            quote_char = ch;
        } else if ch == ' ' || ch == '\t' {
            if !current.is_empty() {
                if let Some(pair) = parse_kv_pair(&current) {
                    pairs.push(pair);
                }
                current.clear();
            }
        } else {
            current.push(ch);
        }
    }

    if !current.is_empty() {
        if let Some(pair) = parse_kv_pair(&current) {
            pairs.push(pair);
        }
    }

    pairs
}

fn parse_kv_pair(s: &str) -> Option<(String, String)> {
    if let Some(eq_pos) = s.find('=') {
        let key = s[..eq_pos].to_string();
        let value = s[eq_pos + 1..].to_string();
        Some((key, value))
    } else {
        None
    }
}

/// Parse ENV instruction (KEY=VALUE or KEY VALUE).
fn parse_env(rest: &str) -> (String, String) {
    let rest = rest.trim();
    if let Some(eq_pos) = rest.find('=') {
        let key = rest[..eq_pos].trim().to_string();
        let value = rest[eq_pos + 1..].trim().to_string();
        (key, value)
    } else {
        // KEY VALUE format
        let parts: Vec<&str> = rest.splitn(2, char::is_whitespace).collect();
        if parts.len() == 2 {
            (parts[0].to_string(), parts[1].to_string())
        } else {
            (parts[0].to_string(), String::new())
        }
    }
}

/// Parse ARG instruction (NAME[=VALUE]).
fn parse_arg(rest: &str) -> (String, Option<String>) {
    let rest = rest.trim();
    if let Some(eq_pos) = rest.find('=') {
        let name = rest[..eq_pos].trim().to_string();
        let value = rest[eq_pos + 1..].trim().to_string();
        (name, Some(value))
    } else {
        (rest.trim().to_string(), None)
    }
}

/// Parse ADD instruction.
fn parse_add(rest: &str) -> (Vec<String>, String) {
    let parts: Vec<&str> = rest.split_whitespace().collect();
    if parts.len() < 2 {
        return (vec![], String::new());
    }
    let dest = parts.last().unwrap().to_string();
    let sources = parts[..parts.len() - 1].iter().map(|s| s.to_string()).collect();
    (sources, dest)
}

/// Parse COPY instruction.
fn parse_copy(rest: &str) -> (Vec<String>, String, Option<String>) {
    let parts: Vec<&str> = rest.split_whitespace().collect();
    if parts.len() < 2 {
        return (vec![], String::new(), None);
    }
    
    let mut sources = Vec::new();
    let mut dest = String::new();
    let mut from = None;
    let mut i = 0;
    
    while i < parts.len() {
        let part = parts[i];
        if part.eq_ignore_ascii_case("--from") && i + 1 < parts.len() {
            // --from builder (space-separated)
            from = Some(parts[i + 1].to_string());
            i += 2;
        } else if part.to_lowercase().starts_with("--from=") {
            // --from=builder (equals-separated)
            from = Some(part[7..].to_string());
            i += 1;
        } else if part.starts_with("--") {
            // Skip other flags like --chown, --chmod, --link
            // For --chown=value and --chmod=value (equals form), just skip
            if part.contains('=') {
                i += 1;
            } else if i + 1 < parts.len() {
                // Skip flag and its value
                i += 2;
            } else {
                i += 1;
            }
        } else if i == parts.len() - 1 {
            dest = parts[i].to_string();
            break;
        } else {
            sources.push(parts[i].to_string());
            i += 1;
        }
    }
    
    (sources, dest, from)
}

/// Parse HEALTHCHECK instruction.
fn parse_healthcheck(rest: &str) -> Result<HealthcheckInstruction> {
    let rest = rest.trim();
    if rest.eq_ignore_ascii_case("NONE") {
        return Ok(HealthcheckInstruction { is_none: true, ..Default::default() });
    }
    
    // Parse HEALTHCHECK [OPTIONS] CMD command
    let mut interval = None;
    let mut timeout = None;
    let mut start_period = None;
    let mut retries = None;
    let mut cmd = None;
    
    let parts: Vec<&str> = rest.split_whitespace().collect();
    let mut i = 0;
    
    while i < parts.len() {
        if parts[i].starts_with("--interval=") {
            interval = Some(parts[i][11..].to_string());
            i += 1;
        } else if parts[i].starts_with("--timeout=") {
            timeout = Some(parts[i][10..].to_string());
            i += 1;
        } else if parts[i].starts_with("--start-period=") {
            start_period = Some(parts[i][15..].to_string());
            i += 1;
        } else if parts[i].starts_with("--retries=") {
            retries = Some(parts[i][10..].parse().unwrap_or(0));
            i += 1;
        } else if parts[i].eq_ignore_ascii_case("CMD") && i + 1 < parts.len() {
            cmd = Some(parts[i + 1..].join(" "));
            break;
        } else {
            i += 1;
        }
    }
    
    Ok(HealthcheckInstruction {
        is_none: false,
        interval,
        timeout,
        start_period,
        retries,
        cmd,
    })
}

/// Parse ONBUILD instruction.
fn parse_onbuild(rest: &str) -> Result<Instruction> {
    let rest = rest.trim();
    let parts: Vec<&str> = rest.splitn(2, char::is_whitespace).collect();
    if parts.is_empty() {
        return Err(ParseError::Syntax {
            line: 0,
            message: "ONBUILD requires an instruction".to_string(),
        });
    }
    
    let inner_directive = parts[0];
    let inner_rest = if parts.len() > 1 { parts[1] } else { "" };
    
    match inner_directive.to_uppercase().as_str() {
        "RUN" => Ok(Instruction::Run(RunInstruction {
            command: inner_rest.trim().to_string(),
            is_shell_form: true,
        })),
        "CMD" => Ok(Instruction::Cmd(CmdInstruction {
            command: parse_string_list(inner_rest),
            is_shell_form: false,
        })),
        "COPY" => {
            let (sources, dest, from) = parse_copy(inner_rest);
            Ok(Instruction::Copy(CopyInstruction {
                sources,
                destination: dest,
                from,
                chmod: None,
                chown: None,
                link: false,
            }))
        }
        "ADD" => {
            let (sources, dest) = parse_add(inner_rest);
            Ok(Instruction::Add(AddInstruction {
                sources,
                destination: dest,
                chmod: None,
                chown: None,
                link: false,
            }))
        }
        _ => Err(ParseError::Syntax {
            line: 0,
            message: format!("Unsupported ONBUILD instruction: {}", inner_directive),
        }),
    }
}

/// Add an instruction to the current stage, or return error if no stage is active.
fn add_instruction(
    current_stage: &mut Option<Stage>,
    instr: Instruction,
    line_num: usize,
) -> Result<()> {
    if let Some(stage) = current_stage.as_mut() {
        stage.instructions.push(instr);
        Ok(())
    } else {
        Err(ParseError::Syntax {
            line: line_num,
            message: format!("Instruction {:?} must appear after FROM", instr),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_dockerfile() {
        let dockerfile = r#"
FROM ubuntu:20.04
RUN apt-get update
ENV FOO=bar
COPY . /app
"#;
        let stages = parse_dockerfile(dockerfile).unwrap();
        assert_eq!(stages.len(), 1);
        assert_eq!(stages[0].image, "ubuntu:20.04");
        assert_eq!(stages[0].instructions.len(), 3);
    }

    #[test]
    fn test_parse_multistage_dockerfile() {
        let dockerfile = r#"
FROM golang:1.24 AS builder
COPY . /src
RUN go build -o /app

FROM alpine:3.18
COPY --from=builder /app /app
"#;
        let stages = parse_dockerfile(dockerfile).unwrap();
        assert_eq!(stages.len(), 2);
        assert_eq!(stages[0].alias, Some("builder".to_string()));
    }

    #[test]
    fn test_variable_substitution() {
        let ctx = VarContext::new();
        // No variables set
        assert_eq!(substitute_vars("hello world", &ctx), "hello world");
        assert_eq!(substitute_vars("$FOO", &ctx), "$FOO");
        assert_eq!(substitute_vars("${FOO}", &ctx), "${FOO}");
        assert_eq!(substitute_vars("$$", &ctx), "$");
        assert_eq!(substitute_vars("$$FOO", &ctx), "$FOO");
    }

    #[test]
    fn test_substitute_vars_with_context() {
        let mut ctx = VarContext::new();
        ctx.set_arg("FOO".to_string(), Some("bar".to_string()));
        ctx.set_env("BAZ".to_string(), "qux".to_string());
        
        assert_eq!(substitute_vars("$FOO", &ctx), "bar");
        assert_eq!(substitute_vars("${FOO}", &ctx), "bar");
        assert_eq!(substitute_vars("$BAZ", &ctx), "qux");
        assert_eq!(substitute_vars("hello $FOO", &ctx), "hello bar");
        assert_eq!(substitute_vars("$FOO$BAZ", &ctx), "barqux");
        assert_eq!(substitute_vars("${FOO}_${BAZ}", &ctx), "bar_qux");
    }

    #[test]
    fn test_continuation_lines() {
        let dockerfile = r#"
FROM ubuntu:20.04
RUN apt-get update && \
    apt-get install -y \
    curl \
    wget
"#;
        let stages = parse_dockerfile(dockerfile).unwrap();
        assert_eq!(stages[0].instructions.len(), 1);
        match &stages[0].instructions[0] {
            Instruction::Run(run) => {
                assert!(run.command.contains("apt-get update"));
                assert!(run.command.contains("curl"));
                assert!(run.command.contains("wget"));
            }
            other => panic!("Expected RUN instruction, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_onbuild() {
        let dockerfile = r#"
FROM ubuntu:20.04
ONBUILD RUN echo hello
ONBUILD COPY . /app
"#;
        let stages = parse_dockerfile(dockerfile).unwrap();
        assert_eq!(stages[0].instructions.len(), 2);
        match &stages[0].instructions[0] {
            Instruction::Onbuild(ob) => match ob.instruction.as_ref() {
                Instruction::Run(run) => assert_eq!(run.command, "echo hello"),
                other => panic!("Expected RUN, got {:?}", other),
            },
            other => panic!("Expected ONBUILD, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_healthcheck() {
        let dockerfile = r#"
FROM ubuntu:20.04
HEALTHCHECK --interval=30s --timeout=10s --retries=3 CMD curl -f http://localhost/
HEALTHCHECK NONE
"#;
        let stages = parse_dockerfile(dockerfile).unwrap();
        assert_eq!(stages[0].instructions.len(), 2);
        match &stages[0].instructions[0] {
            Instruction::Healthcheck(hc) => {
                assert!(!hc.is_none);
                assert_eq!(hc.interval, Some("30s".to_string()));
                assert_eq!(hc.timeout, Some("10s".to_string()));
                assert_eq!(hc.retries, Some(3));
                assert_eq!(hc.cmd, Some("curl -f http://localhost/".to_string()));
            }
            other => panic!("Expected HEALTHCHECK, got {:?}", other),
        }
        match &stages[0].instructions[1] {
            Instruction::Healthcheck(hc) => assert!(hc.is_none),
            other => panic!("Expected HEALTHCHECK, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_shell() {
        let dockerfile = r#"
FROM ubuntu:20.04
SHELL ["/bin/bash", "-c"]
"#;
        let stages = parse_dockerfile(dockerfile).unwrap();
        match &stages[0].instructions[0] {
            Instruction::Shell(shell) => {
                assert_eq!(shell.shell, vec!["/bin/bash", "-c"]);
            }
            other => panic!("Expected SHELL, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_stopsignal() {
        let dockerfile = r#"
FROM ubuntu:20.04
STOPSIGNAL SIGTERM
"#;
        let stages = parse_dockerfile(dockerfile).unwrap();
        match &stages[0].instructions[0] {
            Instruction::StopSignal(ss) => assert_eq!(ss.signal, "SIGTERM"),
            other => panic!("Expected STOPSIGNAL, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_maintainer() {
        let dockerfile = r#"
FROM ubuntu:20.04
MAINTAINER test@example.com
"#;
        let stages = parse_dockerfile(dockerfile).unwrap();
        match &stages[0].instructions[0] {
            Instruction::Maintainer(m) => assert_eq!(m.name, "test@example.com"),
            other => panic!("Expected MAINTAINER, got {:?}", other),
        }
    }

    #[test]
    fn test_variable_substitution_with_build_args() {
        let mut build_args = HashMap::new();
        build_args.insert("IMAGE".to_string(), "alpine:latest".to_string());
        
        let dockerfile = r#"
FROM $IMAGE
ARG FOO=default
ENV BAR=$FOO
"#;
        let stages = parse_dockerfile_with_build_args(dockerfile, build_args).unwrap();
        assert_eq!(stages[0].image, "alpine:latest");
        match &stages[0].instructions[1] {
            Instruction::Env(env) => assert_eq!(env.value, "default"),
            other => panic!("Expected ENV, got {:?}", other),
        }
    }

    #[test]
    fn test_variable_substitution_with_arg() {
        let dockerfile = r#"
FROM ubuntu:20.04
ARG FOO=default_value
ENV BAR=$FOO
"#;
        let stages = parse_dockerfile(dockerfile).unwrap();
        match &stages[0].instructions[1] {
            Instruction::Env(env) => assert_eq!(env.value, "default_value"),
            other => panic!("Expected ENV, got {:?}", other),
        }
    }

    #[test]
    fn test_variable_substitution_with_env() {
        let dockerfile = r#"
FROM ubuntu:20.04
ENV FOO=env_value
ENV BAR=$FOO
"#;
        let stages = parse_dockerfile(dockerfile).unwrap();
        match &stages[0].instructions[1] {
            Instruction::Env(env) => assert_eq!(env.value, "env_value"),
            other => panic!("Expected ENV, got {:?}", other),
        }
    }

    #[test]
    fn test_workdir_variable_substitution() {
        let dockerfile = r#"
FROM ubuntu:20.04
ARG WORK_DIR=/app
WORKDIR $WORK_DIR
"#;
        let stages = parse_dockerfile(dockerfile).unwrap();
        match &stages[0].instructions[1] {
            Instruction::Workdir(wd) => assert_eq!(wd.path, "/app"),
            other => panic!("Expected WORKDIR, got {:?}", other),
        }
    }

    #[test]
    fn test_user_variable_substitution() {
        let dockerfile = r#"
FROM ubuntu:20.04
ARG USER_NAME=appuser
USER $USER_NAME
"#;
        let stages = parse_dockerfile(dockerfile).unwrap();
        match &stages[0].instructions[1] {
            Instruction::User(u) => assert_eq!(u.user, "appuser"),
            other => panic!("Expected USER, got {:?}", other),
        }
    }
}