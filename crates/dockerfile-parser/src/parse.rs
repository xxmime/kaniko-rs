//! Dockerfile parser implementation.
//!
//! Parses a Dockerfile string into a list of [`Instruction`] values.
//! Supports:
//! - Multi-line continuation (`\` at end of line)
//! - ARG/ENV variable substitution (`$VAR`, `${VAR}`)
//! - All Dockerfile instructions including ONBUILD, STOPSIGNAL, HEALTHCHECK, SHELL

use crate::instruction::*;
use std::collections::{HashMap, HashSet};
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
    /// Whether this stage should be saved as a tarball for later use by other stages.
    /// Analogous to Go: `KanikoStage.SaveStage`.
    pub save_stage: bool,
    /// Index of the base image stage (if the base refers to a previous stage via COPY --from).
    /// -1 if the base image is not a previous stage.
    /// Analogous to Go: `KanikoStage.BaseImageIndex`.
    pub base_image_index: i32,
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
                        result.push_str(&input[i..=start + end]);
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
                    save_stage: false,
                    base_image_index: -1,
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
                let (mounts, network, cmd_raw) = parse_run_flags(rest.trim());
                let cmd = ctx.substitute(&cmd_raw);
                let is_shell = !cmd.starts_with('[');
                let args = if !is_shell {
                    parse_string_list(&cmd)
                } else {
                    vec![]
                };
                add_instruction(&mut current_stage, Instruction::Run(RunInstruction {
                    command: cmd,
                    is_shell_form: is_shell,
                    args,
                    mounts,
                    network,
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
                let (sources, dest, flags) = parse_add(&substituted);
                add_instruction(&mut current_stage, Instruction::Add(AddInstruction {
                    sources,
                    destination: dest,
                    chmod: flags.chmod,
                    chown: flags.chown,
                    link: flags.link,
                }), line_num)?;
            }
            "COPY" => {
                let substituted = ctx.substitute(rest);
                let (sources, dest, flags) = parse_copy(&substituted);
                add_instruction(&mut current_stage, Instruction::Copy(CopyInstruction {
                    sources,
                    destination: dest,
                    from: flags.from,
                    chmod: flags.chmod,
                    chown: flags.chown,
                    link: flags.link,
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

/// Parsed ADD flags (--chown, --chmod, --link).
#[derive(Debug, Clone, Default)]
struct AddFlags {
    chown: Option<String>,
    chmod: Option<String>,
    link: bool,
}

/// Parse ADD instruction, extracting all flags.
fn parse_add(rest: &str) -> (Vec<String>, String, AddFlags) {
    let parts: Vec<&str> = rest.split_whitespace().collect();
    if parts.len() < 2 {
        return (vec![], String::new(), AddFlags::default());
    }
    
    let mut sources = Vec::new();
    let mut dest = String::new();
    let mut flags = AddFlags::default();
    let mut i = 0;
    
    while i < parts.len() {
        let part = parts[i];
        if part.eq_ignore_ascii_case("--chown") && i + 1 < parts.len() {
            flags.chown = Some(parts[i + 1].to_string());
            i += 2;
        } else if part.to_lowercase().starts_with("--chown=") {
            flags.chown = Some(part[8..].to_string());
            i += 1;
        } else if part.eq_ignore_ascii_case("--chmod") && i + 1 < parts.len() {
            flags.chmod = Some(parts[i + 1].to_string());
            i += 2;
        } else if part.to_lowercase().starts_with("--chmod=") {
            flags.chmod = Some(part[8..].to_string());
            i += 1;
        } else if part.eq_ignore_ascii_case("--link") {
            flags.link = true;
            i += 1;
        } else if part.starts_with("--") {
            // Skip unknown flags
            if part.contains('=') {
                i += 1;
            } else if i + 1 < parts.len() {
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
    
    (sources, dest, flags)
}

/// Parsed COPY flags (--from, --chown, --chmod, --link).
#[derive(Debug, Clone, Default)]
struct CopyFlags {
    from: Option<String>,
    chown: Option<String>,
    chmod: Option<String>,
    link: bool,
}

/// Parse COPY instruction, extracting all flags.
fn parse_copy(rest: &str) -> (Vec<String>, String, CopyFlags) {
    let parts: Vec<&str> = rest.split_whitespace().collect();
    if parts.len() < 2 {
        return (vec![], String::new(), CopyFlags::default());
    }
    
    let mut sources = Vec::new();
    let mut dest = String::new();
    let mut flags = CopyFlags::default();
    let mut i = 0;
    
    while i < parts.len() {
        let part = parts[i];
        if part.eq_ignore_ascii_case("--from") && i + 1 < parts.len() {
            flags.from = Some(parts[i + 1].to_string());
            i += 2;
        } else if part.to_lowercase().starts_with("--from=") {
            flags.from = Some(part[7..].to_string());
            i += 1;
        } else if part.eq_ignore_ascii_case("--chown") && i + 1 < parts.len() {
            flags.chown = Some(parts[i + 1].to_string());
            i += 2;
        } else if part.to_lowercase().starts_with("--chown=") {
            flags.chown = Some(part[8..].to_string());
            i += 1;
        } else if part.eq_ignore_ascii_case("--chmod") && i + 1 < parts.len() {
            flags.chmod = Some(parts[i + 1].to_string());
            i += 2;
        } else if part.to_lowercase().starts_with("--chmod=") {
            flags.chmod = Some(part[8..].to_string());
            i += 1;
        } else if part.eq_ignore_ascii_case("--link") {
            flags.link = true;
            i += 1;
        } else if part.starts_with("--") {
            // Skip unknown flags
            if part.contains('=') {
                i += 1;
            } else if i + 1 < parts.len() {
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
    
    (sources, dest, flags)
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
            args: vec![],
            mounts: vec![],
            network: None,
        })),
        "CMD" => Ok(Instruction::Cmd(CmdInstruction {
            command: parse_string_list(inner_rest),
            is_shell_form: false,
        })),
        "COPY" => {
            let (sources, dest, flags) = parse_copy(inner_rest);
            Ok(Instruction::Copy(CopyInstruction {
                sources,
                destination: dest,
                from: flags.from,
                chmod: flags.chmod,
                chown: flags.chown,
                link: flags.link,
            }))
        }
        "ADD" => {
            let (sources, dest, flags) = parse_add(inner_rest);
            Ok(Instruction::Add(AddInstruction {
                sources,
                destination: dest,
                chmod: flags.chmod,
                chown: flags.chown,
                link: flags.link,
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

/// Convert parsed stages into KanikoStages by computing `save_stage` and `base_image_index`.
///
/// `save_stage` is true if any later stage references this stage by alias (COPY --from=alias).
/// `base_image_index` is the index of the stage that provides the base image (if FROM refers
/// to a previous stage), or -1 if the base is an external image.
///
/// Analogous to Go: `dockerfile.MakeKanikoStages()`.
pub fn make_kaniko_stages(stages: &mut [Stage]) {
    let num_stages = stages.len();
    if num_stages == 0 {
        return;
    }

    // Build alias → index map
    let mut alias_to_index: HashMap<String, usize> = HashMap::new();
    for (i, stage) in stages.iter().enumerate() {
        if let Some(alias) = &stage.alias {
            alias_to_index.insert(alias.clone(), i);
        }
    }

    // Compute base_image_index for each stage
    for i in 0..num_stages {
        let image = &stages[i].image;
        // Check if the image name matches an alias of a previous stage
        let mut base_idx: i32 = -1;
        if let Some(&idx) = alias_to_index.get(image) {
            if idx < i {
                base_idx = idx as i32;
            }
        }
        // Also check numeric references (FROM 0, FROM 1)
        if let Ok(idx) = image.parse::<usize>() {
            if idx < i {
                base_idx = idx as i32;
            }
        }
        stages[i].base_image_index = base_idx;
    }

    // Compute save_stage: a stage should be saved if a later stage references it
    // via COPY --from=<alias_or_index>
    let mut save_stage_set: HashSet<usize> = HashSet::new();
    for stage in stages.iter() {
        for instr in &stage.instructions {
            // Look for COPY/ADD instructions with --from flag
            if let Instruction::Copy(copy) = instr {
                if let Some(from) = &copy.from {
                    // Try to resolve --from value to an index
                    if let Ok(idx) = from.parse::<usize>() {
                        save_stage_set.insert(idx);
                    } else if let Some(&idx) = alias_to_index.get(from) {
                        save_stage_set.insert(idx);
                    }
                }
            }
        }
    }

    // Set save_stage for each stage
    for (i, stage) in stages.iter_mut().enumerate() {
        stage.save_stage = save_stage_set.contains(&i);
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

    #[test]
    fn test_copy_with_chown_chmod_link() {
        let dockerfile = r#"
FROM ubuntu:20.04
COPY --chown=1000:1000 --chmod=755 app /app
COPY --chown=appuser --link src/ /src/
"#;
        let stages = parse_dockerfile(dockerfile).unwrap();
        match &stages[0].instructions[0] {
            Instruction::Copy(copy) => {
                assert_eq!(copy.sources, vec!["app"]);
                assert_eq!(copy.destination, "/app");
                assert_eq!(copy.chown, Some("1000:1000".to_string()));
                assert_eq!(copy.chmod, Some("755".to_string()));
                assert!(!copy.link);
            }
            other => panic!("Expected COPY, got {:?}", other),
        }
        match &stages[0].instructions[1] {
            Instruction::Copy(copy) => {
                assert_eq!(copy.chown, Some("appuser".to_string()));
                assert!(copy.chmod.is_none());
                assert!(copy.link);
            }
            other => panic!("Expected COPY, got {:?}", other),
        }
    }

    #[test]
    fn test_copy_with_all_flags() {
        let dockerfile = r#"
FROM ubuntu:20.04
COPY --from=builder --chown=app:app --chmod=644 --link /app/bin /usr/local/bin
"#;
        let stages = parse_dockerfile(dockerfile).unwrap();
        match &stages[0].instructions[0] {
            Instruction::Copy(copy) => {
                assert_eq!(copy.from, Some("builder".to_string()));
                assert_eq!(copy.chown, Some("app:app".to_string()));
                assert_eq!(copy.chmod, Some("644".to_string()));
                assert!(copy.link);
                assert_eq!(copy.sources, vec!["/app/bin"]);
                assert_eq!(copy.destination, "/usr/local/bin");
            }
            other => panic!("Expected COPY, got {:?}", other),
        }
    }

    #[test]
    fn test_add_with_chown_chmod() {
        let dockerfile = r#"
FROM ubuntu:20.04
ADD --chown=root:root --chmod=755 script.sh /usr/bin/
ADD --link archive.tar.gz /opt/
"#;
        let stages = parse_dockerfile(dockerfile).unwrap();
        match &stages[0].instructions[0] {
            Instruction::Add(add) => {
                assert_eq!(add.chown, Some("root:root".to_string()));
                assert_eq!(add.chmod, Some("755".to_string()));
                assert!(!add.link);
            }
            other => panic!("Expected ADD, got {:?}", other),
        }
        match &stages[0].instructions[1] {
            Instruction::Add(add) => {
                assert!(add.chown.is_none());
                assert!(add.chmod.is_none());
                assert!(add.link);
            }
            other => panic!("Expected ADD, got {:?}", other),
        }
    }

    #[test]
    fn test_copy_chmod_equals_form() {
        let dockerfile = r#"
FROM ubuntu:20.04
COPY --chmod=755 app /app
COPY --chown=1000:1000 --chmod=644 file /etc/file
"#;
        let stages = parse_dockerfile(dockerfile).unwrap();
        match &stages[0].instructions[0] {
            Instruction::Copy(copy) => {
                assert_eq!(copy.chmod, Some("755".to_string()));
            }
            other => panic!("Expected COPY, got {:?}", other),
        }
        match &stages[0].instructions[1] {
            Instruction::Copy(copy) => {
                assert_eq!(copy.chown, Some("1000:1000".to_string()));
                assert_eq!(copy.chmod, Some("644".to_string()));
            }
            other => panic!("Expected COPY, got {:?}", other),
        }
    }
}

/// Parse RUN flags (--mount, --network) from a RUN command string.
/// Returns (mounts, network, remaining_command).
fn parse_run_flags(input: &str) -> (Vec<String>, Option<String>, String) {
    let mut mounts = Vec::new();
    let mut network = None;
    let mut remaining = String::new();
    let mut chars = input.chars().peekable();
    let mut current_token = String::new();
    let mut in_flag_value = false;
    let mut flag_name = String::new();

    while let Some(ch) = chars.next() {
        if in_flag_value {
            if ch.is_whitespace() {
                // End of flag value
                if flag_name == "mount" {
                    mounts.push(current_token.trim().to_string());
                } else if flag_name == "network" {
                    network = Some(current_token.trim().to_string());
                }
                current_token.clear();
                flag_name.clear();
                in_flag_value = false;
                continue;
            }
            current_token.push(ch);
            continue;
        }

        if ch == '-' && chars.peek() == Some(&'-') {
            chars.next(); // consume second '-'
            if !current_token.is_empty() {
                if !remaining.is_empty() {
                    remaining.push(' ');
                }
                remaining.push_str(&current_token);
                current_token.clear();
            }
            let mut name = String::new();
            while let Some(&c) = chars.peek() {
                if c == '=' || c.is_whitespace() {
                    break;
                }
                name.push(chars.next().unwrap());
            }
            if chars.peek() == Some(&'=') {
                chars.next();
                flag_name = name;
                in_flag_value = true;
                current_token.clear();
            } else {
                if !remaining.is_empty() {
                    remaining.push(' ');
                }
                remaining.push_str(&format!("--{}", name));
            }
            continue;
        }

        current_token.push(ch);
    }

    if in_flag_value {
        if flag_name == "mount" {
            mounts.push(current_token.trim().to_string());
        } else if flag_name == "network" {
            network = Some(current_token.trim().to_string());
        }
    } else if !current_token.is_empty() {
        if !remaining.is_empty() {
            remaining.push(' ');
        }
        remaining.push_str(&current_token);
    }

    (mounts, network, remaining)
}

#[cfg(test)]
mod tests_run_flags {
    use super::*;

    #[test]
    fn test_parse_run_flags_no_flags() {
        let (mounts, network, cmd) = parse_run_flags("apt-get install -y curl");
        assert!(mounts.is_empty());
        assert!(network.is_none());
        assert_eq!(cmd, "apt-get install -y curl");
    }

    #[test]
    fn test_parse_run_flags_mount() {
        let (mounts, network, cmd) = parse_run_flags("--mount=type=cache,target=/root/.cache,id=npm pip install");
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0], "type=cache,target=/root/.cache,id=npm");
        assert!(network.is_none());
        assert_eq!(cmd, "pip install");
    }

    #[test]
    fn test_parse_run_flags_network() {
        let (mounts, network, cmd) = parse_run_flags("--network=none apt-get update");
        assert!(mounts.is_empty());
        assert_eq!(network, Some("none".to_string()));
        assert_eq!(cmd, "apt-get update");
    }

    #[test]
    fn test_parse_run_flags_both() {
        let (mounts, network, cmd) = parse_run_flags("--mount=type=bind,source=/app,target=/app --network=host make build");
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0], "type=bind,source=/app,target=/app");
        assert_eq!(network, Some("host".to_string()));
        assert_eq!(cmd, "make build");
    }
}

// ============================================================================
// Stage analysis utilities
// ============================================================================

/// Skip unused stages in a multi-stage Dockerfile build.
///
/// Given a list of stages and a target stage, returns only the stages
/// that are actually needed to build the target. This is done by
/// walking backwards from the target and tracking COPY --from dependencies.
///
/// Analogous to Go: `pkg/dockerfile.skipUnusedStages()`.
pub fn skip_unused_stages(
    stages: &[Stage],
    target_index: usize,
) -> Vec<Stage> {
    let mut dependencies = std::collections::HashSet::<String>::new();
    let last_stage_base = stages[target_index].image.to_lowercase();

    // Walk backwards from the target stage
    for i in (0..=target_index).rev() {
        let s = &stages[i];
        let is_dep = (s.alias.as_ref().map(|a| a.to_lowercase()).as_ref() == Some(&last_stage_base))
            || dependencies.contains(&s.alias.as_ref().map(|a| a.to_lowercase()).unwrap_or_default())
            || i == target_index;

        if is_dep {
            // Check COPY --from instructions for dependencies on other stages
            for instruction in &s.instructions {
                if let Instruction::Copy(copy) = instruction {
                    if let Some(ref from) = copy.from {
                        // If it's a numeric index, resolve to the stage name
                        let stage_name = if let Ok(idx) = from.parse::<usize>() {
                            stages.get(idx)
                                .and_then(|st| st.alias.as_ref())
                                .map(|a| a.to_lowercase())
                                .unwrap_or_default()
                        } else {
                            from.to_lowercase()
                        };
                        if !stage_name.is_empty() {
                            dependencies.insert(stage_name);
                        }
                    }
                }
            }

            // The base image of a dependent stage is also a dependency
            if i != target_index {
                dependencies.insert(s.image.to_lowercase());
            }
        }
    }

    // If no dependencies were found, return all stages up to the target
    if dependencies.is_empty() {
        return stages[..=target_index].to_vec();
    }

    // Collect only the stages that are needed
    let mut used_stages = Vec::new();
    for i in 0..target_index {
        let s = &stages[i];
        let alias_lower = s.alias.as_ref().map(|a| a.to_lowercase());
        if alias_lower.as_ref() == Some(&last_stage_base)
            || dependencies.contains(&alias_lower.unwrap_or_default())
        {
            used_stages.push(s.clone());
        }
    }

    // Always include the target stage
    used_stages.push(stages[target_index].clone());
    used_stages
}

/// Build a mapping from stage names to their indices.
/// Analogous to Go: `stageNameToIdx` construction in `MakeKanikoStages()`.
pub fn build_stage_name_to_index(stages: &[Stage]) -> HashMap<String, usize> {
    let mut map = HashMap::new();
    for (i, stage) in stages.iter().enumerate() {
        if let Some(ref alias) = stage.alias {
            map.insert(alias.to_lowercase(), i);
        }
        // Also map numeric index as string
        map.insert(i.to_string(), i);
    }
    map
}

/// Resolve cross-stage COPY --from references from names to indices.
///
/// This function modifies the COPY instructions in-place so that
/// `COPY --from=secondStage` becomes `COPY --from=1` for easier
/// processing later on.
///
/// Analogous to Go: `pkg/dockerfile.ResolveCrossStageCommands()`.
pub fn resolve_cross_stage_commands(
    stages: &mut [Stage],
    stage_name_to_idx: &HashMap<String, usize>,
) {
    for stage in stages.iter_mut() {
        for instruction in stage.instructions.iter_mut() {
            if let Instruction::Copy(copy) = instruction {
                if let Some(ref from) = copy.from {
                    let from_lower = from.to_lowercase();
                    if let Some(&idx) = stage_name_to_idx.get(&from_lower) {
                        copy.from = Some(idx.to_string());
                    }
                }
            }
        }
    }
}

/// Strip enclosing quotes from ARG default values.
///
/// If the quotes are escaped (e.g. `\"value\"`), they are left as-is.
/// Analogous to Go: `pkg/dockerfile.stripEnclosingQuotes()`.
pub fn strip_enclosing_quotes(value: &str) -> Result<String> {
    let backslash = b'\\';
    let bytes = value.as_bytes();

    if bytes.len() < 2 {
        return Ok(value.to_string());
    }

    let mut leader = String::new();
    let mut tail = String::new();

    match bytes[0] {
        b'\'' | b'"' => {
            leader.push(bytes[0] as char);
        }
        b'\\' => {
            if bytes.len() > 1 {
                match bytes[1] {
                    b'\'' | b'"' => {
                        leader.push(bytes[0] as char);
                        leader.push(bytes[1] as char);
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }

    // If the leader is more than one character, it's an escaped character
    if leader.len() < 2 {
        match bytes[bytes.len() - 1] {
            b'\'' | b'"' => {
                tail.push(bytes[bytes.len() - 1] as char);
            }
            _ => {}
        }
    } else {
        let last_two = &value[value.len() - 2..];
        if last_two == "\\'" || last_two == "\\\"" {
            tail = last_two.to_string();
        }
    }

    if leader != tail {
        // Mismatched quotes — but we just log and return the original value
        // (Go version would return an error, but we're more lenient here)
        return Ok(value.to_string());
    }

    if leader.is_empty() {
        return Ok(value.to_string());
    }

    // If escaped, leave as-is
    if leader.len() == 2 {
        return Ok(value.to_string());
    }

    // Strip the enclosing quotes
    Ok(value[1..value.len() - 1].to_string())
}

/// Expand nested ARG references in meta ARGs.
///
/// Tries to resolve each ARG value against previously defined ARGs
/// and runtime build args.
///
/// Analogous to Go: `pkg/dockerfile.expandNestedArgs()`.
pub fn expand_nested_args(
    meta_args: &[(String, Option<String>)],
    build_args: &HashMap<String, String>,
) -> Vec<(String, Option<String>)> {
    let mut prev_args: Vec<String> = Vec::new();
    let mut result = Vec::with_capacity(meta_args.len());

    for (key, value) in meta_args {
        let new_value = if let Some(v) = value {
            // Combine previous args and build args for resolution
            let mut combined: Vec<String> = prev_args.clone();
            for (k, v) in build_args {
                combined.push(format!("{}={}", k, v));
            }
            let resolved = resolve_arg_value(v, &combined);
            prev_args.push(format!("{}={}", key, resolved));
            Some(resolved)
        } else {
            prev_args.push(key.clone());
            None
        };
        result.push((key.clone(), new_value));
    }

    result
}

/// Resolve ARG value references like `$VAR` or `${VAR}` against a list of KEY=VALUE pairs.
fn resolve_arg_value(value: &str, args: &[String]) -> String {
    let mut result = value.to_string();
    for arg in args {
        if let Some((key, val)) = arg.split_once('=') {
            // Replace ${KEY} first (longer match)
            let bracket_ref = format!("${{{}}}", key);
            if result.contains(&bracket_ref) {
                result = result.replace(&bracket_ref, val);
            }
            // Replace $KEY
            let dollar_ref = format!("${}", key);
            // Only replace if not already handled by ${} form
            if result.contains(&dollar_ref) && !result.contains(&bracket_ref) {
                result = result.replace(&dollar_ref, val);
            }
        }
    }
    result
}

#[cfg(test)]
mod tests_stage_analysis {
    use super::*;

    #[test]
    fn test_skip_unused_stages_no_deps() {
        let dockerfile = r#"
FROM ubuntu:20.04 AS builder
RUN echo builder

FROM debian:10 AS unused
RUN echo unused

FROM ubuntu:20.04
COPY --from=builder /app /app
"#;
        let stages = parse_dockerfile(dockerfile).unwrap();
        let used = skip_unused_stages(&stages, 2);
        assert_eq!(used.len(), 2); // builder + final, skip "unused"
        assert_eq!(used[0].alias.as_deref(), Some("builder"));
        assert!(used[1].alias.is_none()); // final stage has no alias
    }

    #[test]
    fn test_skip_unused_stages_all_used() {
        let dockerfile = r#"
FROM ubuntu:20.04 AS stage1
FROM stage1 AS stage2
FROM stage2 AS stage3
"#;
        let stages = parse_dockerfile(dockerfile).unwrap();
        let used = skip_unused_stages(&stages, 2);
        assert_eq!(used.len(), 3); // all stages are used
    }

    #[test]
    fn test_build_stage_name_to_index() {
        let dockerfile = r#"
FROM ubuntu:20.04 AS build
FROM debian:10 AS test
FROM alpine:3 AS final
"#;
        let stages = parse_dockerfile(dockerfile).unwrap();
        let map = build_stage_name_to_index(&stages);
        assert_eq!(map.get("build"), Some(&0));
        assert_eq!(map.get("test"), Some(&1));
        assert_eq!(map.get("final"), Some(&2));
        assert_eq!(map.get("0"), Some(&0));
    }

    #[test]
    fn test_resolve_cross_stage_commands() {
        let dockerfile = r#"
FROM ubuntu:20.04 AS builder
RUN echo builder
FROM alpine:3
COPY --from=builder /app /app
"#;
        let mut stages = parse_dockerfile(dockerfile).unwrap();
        let map = build_stage_name_to_index(&stages);
        resolve_cross_stage_commands(&mut stages, &map);
        // The COPY --from=builder should now be COPY --from=0
        match &stages[1].instructions[0] {
            Instruction::Copy(copy) => {
                assert_eq!(copy.from, Some("0".to_string()));
            }
            other => panic!("Expected COPY, got {:?}", other),
        }
    }

    #[test]
    fn test_strip_enclosing_quotes_double() {
        assert_eq!(strip_enclosing_quotes(r#""hello""#).unwrap(), "hello");
    }

    #[test]
    fn test_strip_enclosing_quotes_single() {
        assert_eq!(strip_enclosing_quotes("'hello'").unwrap(), "hello");
    }

    #[test]
    fn test_strip_enclosing_quotes_escaped() {
        // Escaped quotes should be left as-is
        assert_eq!(strip_enclosing_quotes(r#"\"hello\""#).unwrap(), r#"\"hello\""#);
    }

    #[test]
    fn test_strip_enclosing_quotes_no_quotes() {
        assert_eq!(strip_enclosing_quotes("hello").unwrap(), "hello");
    }

    #[test]
    fn test_strip_enclosing_quotes_empty() {
        assert_eq!(strip_enclosing_quotes("").unwrap(), "");
    }

    #[test]
    fn test_expand_nested_args() {
        let meta_args = vec![
            ("BASE_IMAGE".to_string(), Some("ubuntu:20.04".to_string())),
            ("APP_IMAGE".to_string(), Some("${BASE_IMAGE}".to_string())),
        ];
        let build_args = HashMap::new();
        let result = expand_nested_args(&meta_args, &build_args);
        assert_eq!(result[0].1, Some("ubuntu:20.04".to_string()));
        assert_eq!(result[1].1, Some("ubuntu:20.04".to_string()));
    }

    #[test]
    fn test_expand_nested_args_with_build_args() {
        let meta_args = vec![
            ("VERSION".to_string(), Some("1.0".to_string())),
        ];
        let mut build_args = HashMap::new();
        build_args.insert("VERSION".to_string(), "2.0".to_string());
        let result = expand_nested_args(&meta_args, &build_args);
        // Build args should be used in resolution
        assert_eq!(result[0].1, Some("1.0".to_string())); // meta arg keeps its own value
    }
}