//! Command-line interface definition (clap derive).

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "jolt",
    version,
    about = "jolt — flash an STM32L083CZ over the UART bootloader"
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalOpts,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Args)]
pub struct GlobalOpts {
    /// Serial port path (default: the only port present, otherwise required)
    #[arg(short, long, global = true)]
    pub port: Option<String>,

    /// Verbose output (repeatable)
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

#[derive(Subcommand)]
pub enum Command {
    /// List available serial ports
    List,
    /// Read bootloader info (chip id, version, supported commands) — read-only
    Info,
    /// Flash a raw firmware .bin file
    Flash(FlashArgs),
    /// Erase the entire flash
    Erase,
    /// Reset the device into the application or the bootloader
    Reset(ResetArgs),
}

#[derive(Args)]
pub struct FlashArgs {
    /// Path to the raw firmware .bin
    pub file: PathBuf,

    /// Skip erasing before writing
    #[arg(long)]
    pub no_erase: bool,

    /// Skip read-back verification
    #[arg(long)]
    pub no_verify: bool,

    /// Do not reset/jump into the application after flashing
    #[arg(long)]
    pub no_run: bool,

    /// Use the bootloader Go command instead of a hardware reset to start the app
    #[arg(long)]
    pub go: bool,
}

#[derive(Args)]
pub struct ResetArgs {
    /// Reset into the system bootloader (default is the application)
    #[arg(long, conflicts_with = "app")]
    pub bootloader: bool,

    /// Reset into the application (default)
    #[arg(long)]
    pub app: bool,
}
