//! Dockerfile instruction types.
//!
//! Defines all supported Dockerfile instructions as Rust types.


/// A Dockerfile instruction.
#[derive(Debug, Clone, PartialEq)]
pub enum Instruction {
    /// FROM — base image specification
    From(FromInstruction),
    /// RUN — execute command
    Run(RunInstruction),
    /// CMD — default command
    Cmd(CmdInstruction),
    /// LABEL — metadata labels
    Label(LabelInstruction),
    /// EXPOSE — expose ports
    Expose(ExposeInstruction),
    /// ENV — environment variables
    Env(EnvInstruction),
    /// ADD — add files (with URL/tar support)
    Add(AddInstruction),
    /// COPY — copy files
    Copy(CopyInstruction),
    /// ENTRYPOINT — entry point
    Entrypoint(EntrypointInstruction),
    /// VOLUME — volume mount points
    Volume(VolumeInstruction),
    /// USER — set user
    User(UserInstruction),
    /// WORKDIR — working directory
    Workdir(WorkdirInstruction),
    /// ARG — build argument
    Arg(ArgInstruction),
    /// ONBUILD — trigger instruction
    Onbuild(OnbuildInstruction),
    /// STOPSIGNAL — stop signal
    StopSignal(StopSignalInstruction),
    /// HEALTHCHECK — health check
    Healthcheck(HealthcheckInstruction),
    /// SHELL — default shell
    Shell(ShellInstruction),
    /// MAINTAINER — maintainer (deprecated)
    Maintainer(MaintainerInstruction),
    /// COMMENT — a comment line
    Comment(String),
}

/// FROM instruction.
#[derive(Debug, Clone, PartialEq)]
pub struct FromInstruction {
    pub image: String,
    pub alias: Option<String>,
    pub platform: Option<String>,
}

/// RUN instruction.
#[derive(Debug, Clone, PartialEq)]
pub struct RunInstruction {
    pub command: String,
    pub is_shell_form: bool,
}

/// CMD instruction.
#[derive(Debug, Clone, PartialEq)]
pub struct CmdInstruction {
    pub command: Vec<String>,
    pub is_shell_form: bool,
}

/// LABEL instruction.
#[derive(Debug, Clone, PartialEq)]
pub struct LabelInstruction {
    pub labels: Vec<(String, String)>,
}

/// EXPOSE instruction.
#[derive(Debug, Clone, PartialEq)]
pub struct ExposeInstruction {
    pub ports: Vec<String>,
}

/// ENV instruction.
#[derive(Debug, Clone, PartialEq)]
pub struct EnvInstruction {
    pub key: String,
    pub value: String,
}

/// ADD instruction.
#[derive(Debug, Clone, PartialEq)]
pub struct AddInstruction {
    pub sources: Vec<String>,
    pub destination: String,
    pub chmod: Option<String>,
    pub chown: Option<String>,
    pub link: bool,
}

/// COPY instruction.
#[derive(Debug, Clone, PartialEq)]
pub struct CopyInstruction {
    pub sources: Vec<String>,
    pub destination: String,
    pub from: Option<String>,
    pub chmod: Option<String>,
    pub chown: Option<String>,
    pub link: bool,
}

/// ENTRYPOINT instruction.
#[derive(Debug, Clone, PartialEq)]
pub struct EntrypointInstruction {
    pub command: Vec<String>,
    pub is_shell_form: bool,
}

/// VOLUME instruction.
#[derive(Debug, Clone, PartialEq)]
pub struct VolumeInstruction {
    pub paths: Vec<String>,
}

/// USER instruction.
#[derive(Debug, Clone, PartialEq)]
pub struct UserInstruction {
    pub user: String,
}

/// WORKDIR instruction.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkdirInstruction {
    pub path: String,
}

/// ARG instruction.
#[derive(Debug, Clone, PartialEq)]
pub struct ArgInstruction {
    pub name: String,
    pub default_value: Option<String>,
}

/// ONBUILD instruction.
#[derive(Debug, Clone, PartialEq)]
pub struct OnbuildInstruction {
    pub instruction: Box<Instruction>,
}

/// STOPSIGNAL instruction.
#[derive(Debug, Clone, PartialEq)]
pub struct StopSignalInstruction {
    pub signal: String,
}

/// HEALTHCHECK instruction.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HealthcheckInstruction {
    pub cmd: Option<String>,
    pub interval: Option<String>,
    pub timeout: Option<String>,
    pub start_period: Option<String>,
    pub retries: Option<u32>,
    pub is_none: bool,
}

/// SHELL instruction.
#[derive(Debug, Clone, PartialEq)]
pub struct ShellInstruction {
    pub shell: Vec<String>,
}

/// MAINTAINER instruction (deprecated).
#[derive(Debug, Clone, PartialEq)]
pub struct MaintainerInstruction {
    pub name: String,
}