mod config;
mod runtime;
mod util;
mod wrapper;

use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;
use config::Cli;

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("teleport-box: {err:#}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<ExitCode> {
    if wrapper::maybe_run_wrapper()? {
        return Ok(ExitCode::SUCCESS);
    }

    let cli = Cli::parse();
    runtime::dispatch(cli)
}
