//! Dockerfile parser for kaniko-rs.
//!
//! Parses Dockerfile syntax into structured instruction types.
//! Supports variable substitution (`$VAR`, `${VAR}`) and multi-line continuation.

pub mod instruction;
pub mod parse;
pub mod heredoc;

pub use instruction::Instruction;
pub use parse::{parse_dockerfile, parse_dockerfile_with_build_args, VarContext, substitute_vars, Stage, ParseError};