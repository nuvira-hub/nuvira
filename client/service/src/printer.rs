//! The "NuviraHub" printer (remote printing, docs/29-remote-printing.md): Windows'
//! own "Microsoft Print To PDF" driver on a local port that is a named pipe. What is
//! printed on it arrives as PDF at the agent listening on the pipe during a session,
//! which sends it to the technician's computer to be printed there.
//!
//! Installed with the service (administrator rights), removed with it. The agent has
//! the same names (`nuvira_agent::printer`).

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, HANDLE, WIN32_ERROR};
use windows::Win32::Graphics::Printing::{
    AddPrinterW, ClosePrinter, DRIVER_INFO_1W, DeletePrinter, EnumPrinterDriversW, OpenPrinterW,
    PRINTER_ACCESS_ADMINISTER, PRINTER_ACCESS_RIGHTS, PRINTER_ALL_ACCESS, PRINTER_ATTRIBUTE_LOCAL,
    PRINTER_DEFAULTSW, PRINTER_HANDLE, PRINTER_INFO_2W, SERVER_ACCESS_ADMINISTER, XcvDataW,
};
use windows::Win32::Storage::FileSystem::PIPE_ACCESS_INBOUND;
use windows::Win32::System::Pipes::{CreateNamedPipeW, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES};
use windows::core::{HSTRING, PCWSTR, PWSTR};

/// Name of the printer, as the person at the PC sees it.
pub const PRINTER: &str = "NuviraHub";
/// The port: the agent of a session with printing listens on it.
pub const PIPE: &str = r"\\.\pipe\nuvira-print";
const DRIVER: &str = "Microsoft Print To PDF";

/// Adds the printer, if it is not there yet. Without the driver (an optional feature of
/// Windows that can be removed) there is no remote printing, and it says so.
pub fn install() -> Result<&'static str> {
    if exists() {
        return Ok("NuviraHub printer already installed");
    }
    if !driver_installed()? {
        bail!(
            "the \"{DRIVER}\" printer driver is not installed (optional Windows feature): no remote printing"
        );
    }
    // The local port monitor accepts a port only if it can open it: an instance of the
    // pipe for the time it takes (if an agent already listens, its own is enough).
    let pipe = unsafe {
        CreateNamedPipeW(
            &HSTRING::from(PIPE),
            PIPE_ACCESS_INBOUND,
            PIPE_TYPE_BYTE,
            PIPE_UNLIMITED_INSTANCES,
            0,
            0,
            0,
            None,
        )
    };
    let result = add_port().and_then(|()| add_printer());
    if !pipe.is_invalid() {
        unsafe {
            let _ = CloseHandle(pipe);
        }
    }
    result.map(|()| "NuviraHub printer installed (remote printing)")
}

/// Removes printer and port (nothing to do if they are not there).
pub fn uninstall() -> Result<()> {
    if let Some(printer) = open(PRINTER_ALL_ACCESS)? {
        let deleted = unsafe { DeletePrinter(printer.0) };
        drop(printer);
        deleted.context("DeletePrinter")?;
    }
    let _ = xcv("DeletePort");
    Ok(())
}

fn exists() -> bool {
    open(PRINTER_ACCESS_ADMINISTER).ok().flatten().is_some()
}

struct Printer(PRINTER_HANDLE);

impl Drop for Printer {
    fn drop(&mut self) {
        unsafe {
            let _ = ClosePrinter(self.0);
        }
    }
}

fn open(access: PRINTER_ACCESS_RIGHTS) -> Result<Option<Printer>> {
    open_name(&HSTRING::from(PRINTER), access)
}

fn open_name(name: &HSTRING, access: PRINTER_ACCESS_RIGHTS) -> Result<Option<Printer>> {
    let defaults = PRINTER_DEFAULTSW { DesiredAccess: access, ..Default::default() };
    let mut handle = PRINTER_HANDLE::default();
    match unsafe { OpenPrinterW(name, &mut handle, Some(&defaults)) } {
        Ok(()) => Ok(Some(Printer(handle))),
        // ERROR_INVALID_PRINTER_NAME: not there.
        Err(e) if e.code().0 as u32 == 0x8007_0709 => Ok(None),
        Err(e) => Err(e).context("OpenPrinter"),
    }
}

fn driver_installed() -> Result<bool> {
    let (mut needed, mut count) = (0u32, 0u32);
    let _ = unsafe { EnumPrinterDriversW(None, None, 1, None, &mut needed, &mut count) };
    if needed == 0 {
        return Ok(false);
    }
    let mut buffer = vec![0u8; needed as usize];
    unsafe { EnumPrinterDriversW(None, None, 1, Some(&mut buffer), &mut needed, &mut count) }
        .context("EnumPrinterDrivers")?;
    // SAFETY: the buffer holds `count` DRIVER_INFO_1W, followed by their strings.
    let drivers =
        unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<DRIVER_INFO_1W>(), count as usize) };
    Ok(drivers.iter().any(|d| unsafe { d.pName.to_string() }.is_ok_and(|n| n.eq_ignore_ascii_case(DRIVER))))
}

/// `AddPort` or `DeletePort` of the local port monitor, for [`PIPE`].
fn xcv(command: &str) -> Result<()> {
    let Some(monitor) = open_name(&HSTRING::from(",XcvMonitor Local Port"), SERVER_ACCESS_ADMINISTER)? else {
        bail!("local port monitor not available");
    };
    let port: Vec<u16> = PIPE.encode_utf16().chain([0]).collect();
    let (mut needed, mut status) = (0u32, 0u32);
    // SAFETY: `port` is a NUL-terminated UTF-16 string, as the monitor expects.
    let bytes = unsafe { std::slice::from_raw_parts(port.as_ptr().cast::<u8>(), port.len() * 2) };
    let ok = unsafe {
        XcvDataW(
            HANDLE(monitor.0.Value),
            &HSTRING::from(command),
            Some(bytes),
            None,
            &mut needed,
            Some(&mut status),
        )
    };
    if !ok.as_bool() {
        bail!("{command}: {}", windows::core::Error::from_thread());
    }
    if status != 0 && status != ERROR_ALREADY_EXISTS.0 {
        bail!("{command}: {}", windows::core::Error::from_hresult(WIN32_ERROR(status).to_hresult()));
    }
    Ok(())
}

fn add_port() -> Result<()> {
    xcv("AddPort").context("printer port")
}

fn add_printer() -> Result<()> {
    let (mut name, mut port, mut driver) = (wide(PRINTER), wide(PIPE), wide(DRIVER));
    let (mut processor, mut datatype) = (wide("winprint"), wide("RAW"));
    let mut comment = wide("Prints on the printer of whoever controls this computer with NuviraHub");
    let info = PRINTER_INFO_2W {
        pPrinterName: PWSTR(name.as_mut_ptr()),
        pPortName: PWSTR(port.as_mut_ptr()),
        pDriverName: PWSTR(driver.as_mut_ptr()),
        pPrintProcessor: PWSTR(processor.as_mut_ptr()),
        pDatatype: PWSTR(datatype.as_mut_ptr()),
        pComment: PWSTR(comment.as_mut_ptr()),
        Attributes: PRINTER_ATTRIBUTE_LOCAL,
        ..Default::default()
    };
    // SAFETY: the strings live until the end of the call.
    let printer =
        unsafe { AddPrinterW(PCWSTR::null(), 2, (&raw const info).cast()) }.context("AddPrinter")?;
    drop(Printer(PRINTER_HANDLE { Value: printer.0 }));
    Ok(())
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}
