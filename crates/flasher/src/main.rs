//! `jolt` CLI — a thin shell over the `jolt` library crate (see `src/lib.rs`).
//! Argument parsing (`cli`) and the subcommand handlers (`commands`) live here;
//! the bootloader/flash engine lives in the library and is shared with other
//! tools (e.g. tower-cli).

mod cli;
mod commands;

use clap::Parser;

use cli::{Cli, Command};

fn main() {
    let cli = Cli::parse();
    let result = match &cli.command {
        Command::Devices => commands::devices(),
        Command::Info => commands::info(&cli.global),
        Command::Flash(args) => commands::flash(&cli.global, args),
        Command::Erase => commands::erase(&cli.global),
        Command::Reset(args) => commands::reset(&cli.global, args),
        Command::Monitor(args) => commands::monitor(&cli.global, args),
    };
    if let Err(e) = result {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
