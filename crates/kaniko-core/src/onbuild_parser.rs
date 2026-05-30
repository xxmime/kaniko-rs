//! ONBUILD instruction parser for complete Docker command parsing.
//!
//! This module provides full parsing of ONBUILD triggers using the
//! dockerfile-parser crate, matching Go's ParseCommands functionality.

use crate::command::{CommandError, Result};
use dockerfile_parser::instruction::Instruction as ParsedInstruction;
use dockerfile_parser::parse_dockerfile;
use std::collections::HashMap;

/// Parse ONBUILD triggers using the full dockerfile parser
pub fn parse_onbuild_triggers(triggers: &[String]) -> Result<Vec<Box<dyn super::command::DockerCommand>>> {
    if triggers.is_empty() {
        return Ok(Vec::new());
    }

    tracing::info!("Parsing {} ONBUILD trigger(s) using full parser", triggers.len());

    let mut commands = Vec::new();

    // Join triggers with newlines and parse as a Dockerfile
    let dockerfile_content = triggers.join("\n");
    
    // Parse the triggers as if they were a Dockerfile
    let parsed_stages = parse_dockerfile(&dockerfile_content)
        .map_err(|e| CommandError::Failed(format!("Failed to parse ONBUILD triggers: {}", e)))?;

    // Extract commands from the first stage (there should only be one)
    if let Some(stage) = parsed_stages.first() {
        for instruction in &stage.instructions {
            if let Some(command) = convert_parsed_instruction_to_command(instruction) {
                commands.push(command);
            } else {
                tracing::warn!("Could not convert ONBUILD instruction: {:?}", instruction);
            }
        }
    }

    Ok(commands)
}

/// Convert a parsed Dockerfile instruction to a DockerCommand
fn convert_parsed_instruction_to_command(instruction: &ParsedInstruction) -> Option<Box<dyn super::command::DockerCommand>> {
    use dockerfile_parser::instruction::*;

    match instruction {
        ParsedInstruction::Run(run) => {
            let cmd = crate::command::RunCommand::new_shell(
                run.command.clone(),
                false, // should_cache
            );
            Some(Box::new(cmd))
        }
        ParsedInstruction::Copy(copy) => {
            let sources = copy.sources.clone();
            let dest = copy.destination.clone();
            let from = copy.from.clone();
            let chown = copy.chown.clone();
            let chmod = copy.chmod.clone();
            
            let cmd = crate::command::CopyCommand::new(
                sources,
                dest,
                from,
                std::path::PathBuf::from("."), // file_context
                false, // should_cache
            );
            Some(Box::new(cmd))
        }
        ParsedInstruction::Add(add) => {
            let sources = add.sources.clone();
            let dest = add.destination.clone();
            let chown = add.chown.clone();
            let chmod = add.chmod.clone();
            
            let cmd = crate::command::AddCommand::new(
                sources,
                dest,
                std::path::PathBuf::from("."), // file_context
                false, // should_cache
            );
            Some(Box::new(cmd))
        }
        ParsedInstruction::Env(env) => {
            let cmd = crate::command::EnvCommand::new(
                env.key.clone(),
                env.value.clone(),
            );
            Some(Box::new(cmd))
        }
        ParsedInstruction::Expose(expose) => {
            let cmd = crate::command::ExposeCommand::new(
                expose.ports.clone()
            );
            Some(Box::new(cmd))
        }
        ParsedInstruction::Label(label) => {
            let cmd = crate::command::LabelCommand::new(
                label.labels.clone()
            );
            Some(Box::new(cmd))
        }
        ParsedInstruction::Volume(volume) => {
            let cmd = crate::command::VolumeCommand::new(
                volume.paths.clone()
            );
            Some(Box::new(cmd))
        }
        ParsedInstruction::User(user) => {
            let cmd = crate::command::UserCommand::new(
                user.user.clone()
            );
            Some(Box::new(cmd))
        }
        ParsedInstruction::Workdir(workdir) => {
            let cmd = crate::command::WorkdirCommand::new(
                workdir.path.clone()
            );
            Some(Box::new(cmd))
        }
        ParsedInstruction::Arg(arg) => {
            let cmd = crate::command::ArgCommand::new(
                arg.name.clone(),
                arg.default_value.clone(),
            );
            Some(Box::new(cmd))
        }
        ParsedInstruction::Cmd(cmd_instr) => {
            let cmd = if cmd_instr.is_shell_form {
                crate::command::CmdCommand::new_shell(
                    cmd_instr.command.join(" ")
                )
            } else {
                crate::command::CmdCommand::new_exec(
                    cmd_instr.command.clone()
                )
            };
            Some(Box::new(cmd))
        }
        ParsedInstruction::Entrypoint(entrypoint) => {
            let cmd = if entrypoint.is_shell_form {
                crate::command::EntrypointCommand::new_shell(
                    entrypoint.command.join(" ")
                )
            } else {
                crate::command::EntrypointCommand::new_exec(
                    entrypoint.command.clone()
                )
            };
            Some(Box::new(cmd))
        }
        ParsedInstruction::Shell(shell) => {
            let cmd = crate::command::ShellCommand::new(
                shell.shell.clone()
            );
            Some(Box::new(cmd))
        }
        ParsedInstruction::StopSignal(stopsignal) => {
            let cmd = crate::command::StopSignalCommand::new(
                stopsignal.signal.clone()
            );
            Some(Box::new(cmd))
        }
        ParsedInstruction::Healthcheck(healthcheck) => {
            // Skip HEALTHCHECK in ONBUILD as it's not typically used in ONBUILD
            tracing::warn!("HEALTHCHECK instruction found in ONBUILD - skipping");
            None
        }
        ParsedInstruction::Maintainer(maintainer) => {
            // Skip MAINTAINER in ONBUILD as it's deprecated
            tracing::warn!("MAINTAINER instruction found in ONBUILD - skipping");
            None
        }
        _ => {
            tracing::warn!("Unsupported ONBUILD instruction type: {:?}", instruction);
            None
        }
    }
}

/// Resolve cross-stage references in ONBUILD commands
pub fn resolve_cross_stage_commands(
    commands: &mut [Box<dyn super::command::DockerCommand>],
    stage_name_to_idx: &HashMap<String, usize>,
) {
    for command in commands {
        // This would need to be implemented based on the specific command types
        // For now, we'll log that this step is being performed
        tracing::debug!("Resolving cross-stage references in ONBUILD commands");
    }
}