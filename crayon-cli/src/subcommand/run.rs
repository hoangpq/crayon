use clap;

use errors::*;
use cargo;

use workflow::Workflow;

pub fn execute(mut workflow: &mut Workflow, matches: &clap::ArgMatches) -> Result<()> {
    super::build::compile(&mut workflow)?;

    let mut args = vec!["run", "--color=always"];

    if matches.is_present("release") {
        args.push("--release");
    }

    cargo::call(&args)
}