//! Subcommand handlers.

use std::io::Write;
use std::time::Duration;

use anyhow::{Context, Result};
use serialport::{DataBits, Parity, StopBits};

use crate::cli::{FlashArgs, GlobalOpts, MonitorArgs, ParityArg, ResetArgs};
use jolt::bootloader::Bootloader;
use jolt::flash::{self, FlashOptions};
use jolt::port::Port;
use jolt::{firmware, target};

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
    println!("Entering bootloader...");
    flash::connect(port, verbose).context("entering bootloader")?;
    let id = Bootloader::new(port).get_id().context("Get ID")?;
    println!("Chip id : 0x{id:03X} ({})", target::chip_name(id));
    if !target::is_known(id) {
        eprintln!("warning: 0x{id:03X} is not a recognized STM32L0 family id — continuing anyway");
    }
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
    flash::erase(&mut port, global.verbose > 0).context("erasing flash")?;
    println!("Erased and reset into application.");
    Ok(())
}

pub fn flash(global: &GlobalOpts, args: &FlashArgs) -> Result<()> {
    let fw = firmware::load(&args.file)?;
    println!("Firmware: {} ({} bytes)", args.file.display(), fw.len());
    if fw.len() as u32 > target::MAX_FLASH_SIZE {
        anyhow::bail!(
            "firmware is {} bytes, exceeding the {} KiB maximum for any STM32L0 device",
            fw.len(),
            target::MAX_FLASH_SIZE / 1024
        );
    }
    let mut port = open(global)?;
    let opts = FlashOptions {
        erase: !args.no_erase,
        verify: !args.no_verify,
        run: !args.no_run,
        go: args.go,
        verbose: global.verbose > 0,
    };
    flash::flash(&mut port, &fw, &opts).map_err(Into::into)
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
/// stdout until interrupted (Ctrl-C). Read-only — nothing is sent to the
/// device, but `--reset` first pulses NRST into the application.
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
        let n = port
            .read_available(&mut buf)
            .context("reading serial port")?;
        if n > 0 {
            out.write_all(&buf[..n]).context("writing to stdout")?;
            out.flush().context("flushing stdout")?;
        }
    }
}
