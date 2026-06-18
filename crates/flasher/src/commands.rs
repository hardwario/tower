//! Subcommand handlers.

use anyhow::{Context, Result};

use crate::bootloader::Bootloader;
use crate::cli::{FlashArgs, GlobalOpts, ResetArgs};
use crate::flash::{self, FlashOptions};
use crate::port::Port;
use crate::{firmware, target};

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
    enter_and_identify(&mut port, global.verbose > 0)?;
    flash::erase_pages(&mut Bootloader::new(&mut port), target::PAGE_COUNT)
        .context("erasing flash")?;
    port.reset_into_app()
        .context("resetting into application")?;
    println!("Erased and reset into application.");
    Ok(())
}

pub fn flash(global: &GlobalOpts, args: &FlashArgs) -> Result<()> {
    let fw = firmware::load(&args.file)?;
    println!("Firmware: {} ({} bytes)", args.file.display(), fw.len());
    if fw.len() as u32 > target::FLASH_SIZE {
        anyhow::bail!(
            "firmware is {} bytes which exceeds the {} byte flash",
            fw.len(),
            target::FLASH_SIZE
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
