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
        _ => anyhow::bail!("multiple serial ports found; pass --port (see `towerf list`)"),
    }
}

fn open(global: &GlobalOpts) -> Result<Port> {
    let path = resolve_port(global)?;
    println!("Port    : {path} @ {} baud, 8E1", target::BAUD);
    Port::open(&path).with_context(|| format!("failed to open serial port {path}"))
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
    println!("Entering bootloader...");
    flash::connect(&mut port, 5, global.verbose > 0).context("entering bootloader")?;
    {
        let mut bl = Bootloader::new(&mut port);
        let id = bl.get_id().context("Get ID")?;
        println!("Chip id : 0x{id:03X} ({})", target::chip_name(id));
        if let Ok((v, o1, o2)) = bl.get_version() {
            println!(
                "GetVer  : {}.{}  (option bytes 0x{o1:02X} 0x{o2:02X})",
                v >> 4,
                v & 0x0F
            );
        }
        if let Ok((v, cmds)) = bl.get() {
            let list: Vec<String> = cmds.iter().map(|c| format!("0x{c:02X}")).collect();
            println!("Get     : v{}.{}", v >> 4, v & 0x0F);
            println!("Commands: {}", list.join(" "));
            let erase = if cmds.contains(&0x44) {
                "Extended Erase (0x44)"
            } else if cmds.contains(&0x43) {
                "Erase (0x43)"
            } else {
                "NONE"
            };
            println!(
                "          erase={erase}, write(0x31)={}",
                cmds.contains(&0x31)
            );
        }
    }
    port.reset_into_app()
        .context("resetting into application")?;
    println!("Reset into application.");
    Ok(())
}

pub fn erase(global: &GlobalOpts) -> Result<()> {
    let mut port = open(global)?;
    println!("Entering bootloader...");
    flash::connect(&mut port, 5, global.verbose > 0).context("entering bootloader")?;
    {
        let mut bl = Bootloader::new(&mut port);
        let id = bl.get_id().context("Get ID")?;
        println!("Chip id : 0x{id:03X} ({})", target::chip_name(id));
        flash::erase_pages(&mut bl, target::PAGE_COUNT).context("erasing flash")?;
    }
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
        address: args.address,
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
