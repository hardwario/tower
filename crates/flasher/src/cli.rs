//! Command-line interface definition (clap derive).

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(
    name = "jolt",
    version,
    about = "jolt — program STM32L0 microcontrollers over the UART bootloader"
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
    /// Open the serial port and print incoming data (serial monitor)
    Monitor(MonitorArgs),
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

#[derive(Args)]
pub struct MonitorArgs {
    /// Baud rate
    #[arg(short, long, default_value_t = 115_200)]
    pub baudrate: u32,

    /// Data bits per frame (5–8)
    #[arg(short, long, default_value_t = 8, value_parser = clap::value_parser!(u8).range(5..=8))]
    pub databits: u8,

    /// Parity
    #[arg(long, value_enum, default_value = "none")]
    pub parity: ParityArg,

    /// Stop bits (1 or 2)
    #[arg(short, long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=2))]
    pub stopbits: u8,

    /// Pulse NRST into the application before monitoring (to catch boot output)
    #[arg(long)]
    pub reset: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum ParityArg {
    None,
    Even,
    Odd,
}
