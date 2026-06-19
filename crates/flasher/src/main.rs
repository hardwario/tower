mod bootloader;
mod cli;
mod commands;
mod error;
mod firmware;
mod flash;
mod port;
mod target;

use clap::Parser;

use cli::{Cli, Command};

fn main() {
    let cli = Cli::parse();
    let result = match &cli.command {
        Command::List => commands::list(),
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
