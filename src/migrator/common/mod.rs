//pub mod mig_error;
use failure::ResultExt;
use log::debug;
use std::process::{Command, ExitStatus, Stdio};
use std::fmt::{self, Display, Formatter};

pub mod mig_error;
use mig_error::{MigErrCtx, MigError, MigErrorKind};

pub mod os_release;

pub mod config;

const MODULE: &str = "common";

#[derive(Debug)]
pub enum OSArch {
    AMD64,
    ARM64,
    ARMEL,
    ARMHF,
    I386,
    MIPS,
    MIPSEL,
    Powerpc,
    PPC64EL,
    S390EX,
}

impl Display for OSArch {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[derive(Debug)]
pub(crate) struct CmdRes {
    pub stdout: String,
    pub stderr: String,
    pub status: ExitStatus,
}

pub(crate) fn call(cmd: &str, args: &[&str], trim_stdout: bool) -> Result<CmdRes, MigError> {
    debug!(
        "{}::call(): '{}' called with {:?}, {}",
        MODULE,
        cmd,
        args,
        trim_stdout
    );

    let output = Command::new(cmd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context(MigErrCtx::from_remark(
            MigErrorKind::Upstream,
            &format!(
                "{}::call: failed to execute: command {} '{:?}'",
                MODULE, cmd, args
            ),
        ))?;

    Ok(CmdRes {
        stdout: match trim_stdout {
            true => String::from(String::from_utf8_lossy(&output.stdout).trim()),
            false => String::from(String::from_utf8_lossy(&output.stdout)),
        },
        stderr: String::from(String::from_utf8_lossy(&output.stderr)),
        status: output.status,
    })
}