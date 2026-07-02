//! Subcommand handlers.
//!
//! All terminal output lives here: the library flash/erase engine is UI-free
//! and emits [`flash::Progress`] events, which [`Reporter`] renders as
//! `indicatif` progress bars. This keeps `indicatif` (and every `println!`) out
//! of the library so a consumer's TUI or machine-readable frontend isn't
//! corrupted.

use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use serialport::{DataBits, Parity, StopBits};

use crate::cli::{FlashArgs, GlobalOpts, MonitorArgs, ParityArg, ResetArgs};
use jolt::bootloader::Bootloader;
use jolt::flash::{self, FlashOptions, Progress};
use jolt::port::Port;
use jolt::{firmware, target};

/// Renders [`Progress`] events from the flash/erase engine as progress bars and
/// status lines. Holds the currently-active bar (erase, write, or verify) and
/// swaps it as the phases advance.
struct Reporter {
    verbose: bool,
    bar: Option<ProgressBar>,
}

impl Reporter {
    fn new(verbose: bool) -> Self {
        Reporter { verbose, bar: None }
    }

    fn pages_bar(total: usize, msg: &'static str) -> ProgressBar {
        let bar = ProgressBar::new(total as u64);
        bar.set_style(
            ProgressStyle::with_template("{msg:>9} [{bar:30.cyan/blue}] {pos:>4}/{len} pages")
                .unwrap()
                .progress_chars("=>-"),
        );
        bar.set_message(msg);
        bar
    }

    fn bytes_bar(total: u64, msg: &'static str) -> ProgressBar {
        let bar = ProgressBar::new(total);
        bar.set_style(
            ProgressStyle::with_template(
                "{msg:>9} [{bar:30.cyan/blue}] {bytes:>8}/{total_bytes:<8} {percent:>3}%",
            )
            .unwrap()
            .progress_chars("=>-"),
        );
        bar.set_message(msg);
        bar
    }

    /// Finish and clear the active bar, if any, with a completion message.
    fn finish_active(&mut self, msg: &'static str) {
        if let Some(bar) = self.bar.take() {
            bar.finish_with_message(msg);
        }
    }

    fn handle(&mut self, p: Progress) {
        match p {
            Progress::Connecting { attempt, of } => {
                if attempt == 1 {
                    println!("Entering bootloader...");
                } else if self.verbose {
                    println!("  retry {attempt}/{of}");
                }
            }
            Progress::ConnectError { attempt, of, error } => {
                if self.verbose {
                    eprintln!("  init attempt {attempt}/{of}: {error}");
                }
            }
            Progress::ChipIdentified { id } => {
                println!("Chip id : 0x{id:03X} ({})", target::chip_name(id));
                if !target::is_known(id) {
                    eprintln!(
                        "warning: 0x{id:03X} is not a recognized STM32L0 family id — continuing anyway"
                    );
                }
            }
            Progress::Erase {
                pages_done,
                pages_total,
            } => {
                let bar = self
                    .bar
                    .get_or_insert_with(|| Self::pages_bar(pages_total, "Erasing"));
                bar.set_length(pages_total as u64);
                bar.set_position(pages_done as u64);
                if pages_done == pages_total {
                    self.finish_active("Erased");
                }
            }
            Progress::Write {
                bytes_done,
                bytes_total,
            } => {
                let bar = self
                    .bar
                    .get_or_insert_with(|| Self::bytes_bar(bytes_total as u64, "Writing"));
                bar.set_position(bytes_done as u64);
                if bytes_done == bytes_total {
                    self.finish_active("Written");
                }
            }
            Progress::Verify {
                bytes_done,
                bytes_total,
            } => {
                let bar = self
                    .bar
                    .get_or_insert_with(|| Self::bytes_bar(bytes_total as u64, "Verifying"));
                bar.set_position(bytes_done as u64);
                if bytes_done == bytes_total {
                    self.finish_active("Verified");
                }
            }
            Progress::Starting => {
                // Any write/verify bar has already finished at 100%.
                self.finish_active("Done");
                println!("Starting application...");
            }
        }
    }

    /// Abandon the active bar with a failure message (call before returning an
    /// error so a half-drawn bar doesn't linger).
    fn fail(&mut self, msg: &'static str) {
        if let Some(bar) = self.bar.take() {
            bar.abandon_with_message(msg);
        }
    }
}

/// Resolve which serial port to use: explicit `--port`, else the only port
/// present, otherwise an error asking the user to pick one.
fn resolve_port(global: &GlobalOpts) -> Result<String> {
    if let Some(p) = &global.port {
        return Ok(p.clone());
    }
    let ports = serialport::available_ports().context("listing serial ports")?;
    match ports.as_slice() {
        [only] => Ok(only.port_name.clone()),
        [] => anyhow::bail!("no serial ports found; connect the device or pass --port"),
        _ => anyhow::bail!("multiple serial ports found; pass --port (see `jolt list`)"),
    }
}

fn open(global: &GlobalOpts) -> Result<Port> {
    let path = resolve_port(global)?;
    println!("Port    : {path} @ {} baud, 8E1", target::BAUD);
    Port::open(&path).with_context(|| format!("failed to open serial port {path}"))
}

/// Enter the bootloader and print the chip id. Leaves the port in the
/// bootloader, ready for further commands.
fn enter_and_identify(port: &mut Port, verbose: bool) -> Result<()> {
    let mut reporter = Reporter::new(verbose);
    flash::connect(port, &mut |p| reporter.handle(p)).context("entering bootloader")?;
    let id = Bootloader::new(port).get_id().context("Get ID")?;
    reporter.handle(Progress::ChipIdentified { id });
    Ok(())
}

pub fn list() -> Result<()> {
    let ports = serialport::available_ports().context("listing serial ports")?;
    for p in &ports {
        println!("{}", p.port_name);
    }
    Ok(())
}

pub fn info(global: &GlobalOpts) -> Result<()> {
    let mut port = open(global)?;
    enter_and_identify(&mut port, global.verbose > 0)?;
    if let Ok((version, cmds)) = Bootloader::new(&mut port).get() {
        let list: Vec<String> = cmds.iter().map(|c| format!("0x{c:02X}")).collect();
        println!("BL ver  : {}.{}", version >> 4, version & 0x0F);
        println!("Commands: {}", list.join(" "));
    }
    port.reset_into_app()
        .context("resetting into application")?;
    println!("Reset into application.");
    Ok(())
}

pub fn erase(global: &GlobalOpts) -> Result<()> {
    let mut port = open(global)?;
    let mut reporter = Reporter::new(global.verbose > 0);
    let result = flash::erase(&mut port, &mut |p| reporter.handle(p));
    match result {
        Ok(pages) => {
            println!(
                "Erased {} KiB and reset into application.",
                pages * target::PAGE_SIZE / 1024
            );
            Ok(())
        }
        Err(e) => {
            reporter.fail("Erase FAILED");
            Err(anyhow::Error::from(e).context("erasing flash"))
        }
    }
}

pub fn flash(global: &GlobalOpts, args: &FlashArgs) -> Result<()> {
    let fw = firmware::load(&args.file).map_err(anyhow::Error::from)?;
    println!("Firmware: {} ({} bytes)", args.file.display(), fw.len());
    let mut port = open(global)?;
    // `FlashOptions` is `#[non_exhaustive]`: build from Default, then override.
    let mut opts = FlashOptions::default();
    opts.erase = !args.no_erase;
    opts.verify = !args.no_verify;
    opts.run = !args.no_run;
    opts.go = args.go;
    let mut reporter = Reporter::new(global.verbose > 0);
    let start = Instant::now();
    match flash::flash(&mut port, &fw, &opts, &mut |p| reporter.handle(p)) {
        Ok(()) => {
            println!(
                "Done: {} bytes in {:.1}s.",
                fw.len(),
                start.elapsed().as_secs_f64()
            );
            Ok(())
        }
        Err(e) => {
            reporter.fail("FAILED");
            Err(anyhow::Error::from(e))
        }
    }
}

pub fn reset(global: &GlobalOpts, args: &ResetArgs) -> Result<()> {
    let mut port = open(global)?;
    if args.bootloader {
        port.reset_into_bootloader()
            .context("reset into bootloader")?;
        println!("Reset into bootloader.");
    } else {
        port.reset_into_app().context("reset into application")?;
        println!("Reset into application.");
    }
    Ok(())
}

/// Open the port with the requested frame format and stream incoming bytes to
/// stdout until interrupted (Ctrl-C) or the device disconnects. Read-only —
/// nothing is sent to the device, but `--reset` first pulses NRST into the
/// application.
pub fn monitor(global: &GlobalOpts, args: &MonitorArgs) -> Result<()> {
    let path = resolve_port(global)?;

    let data_bits = match args.databits {
        5 => DataBits::Five,
        6 => DataBits::Six,
        7 => DataBits::Seven,
        _ => DataBits::Eight,
    };
    let (parity, parity_letter) = match args.parity {
        ParityArg::None => (Parity::None, 'N'),
        ParityArg::Even => (Parity::Even, 'E'),
        ParityArg::Odd => (Parity::Odd, 'O'),
    };
    let stop_bits = match args.stopbits {
        2 => StopBits::Two,
        _ => StopBits::One,
    };

    let mut port = Port::open_with(&path, args.baudrate, data_bits, parity, stop_bits)
        .with_context(|| format!("failed to open serial port {path}"))?;
    // Short timeout so the read loop stays responsive on an idle line.
    port.set_timeout(Duration::from_millis(100))?;

    if args.reset {
        port.reset_into_app()
            .context("resetting into application")?;
    }

    // Banner on stderr so `jolt monitor > log.txt` captures only device output.
    eprintln!(
        "Monitoring {path} @ {} baud, {}{}{} — press Ctrl-C to exit.",
        args.baudrate, args.databits, parity_letter, args.stopbits
    );

    let mut buf = [0u8; 1024];
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    loop {
        // `read_stream` yields Ok(0) on an idle-line timeout and errors with
        // Disconnected when the device is unplugged — so this loop can't
        // busy-spin at 100% CPU after a disconnect.
        let n = port
            .read_stream(&mut buf, "reading serial port")
            .context("reading serial port")?;
        if n > 0 {
            out.write_all(&buf[..n]).context("writing to stdout")?;
            out.flush().context("flushing stdout")?;
        }
    }
}
