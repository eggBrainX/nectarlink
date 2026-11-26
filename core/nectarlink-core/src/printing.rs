// SPDX-License-Identifier: MPL-2.0
//! Printer sharing: a desktop PC offers its printers to paired phones, so a
//! phone can print a document on the PC's printer without the printer itself
//! being on the network.
//!
//! This module is the platform spooler layer only: it lists the printers the
//! OS knows about and hands a file to the OS print system. Moving the file
//! from the phone to the PC rides the existing encrypted file transfer; the
//! session layer decides which received files are print jobs.
//!
//! Backends:
//! - Linux and macOS: CUPS, via `lpstat` (discovery) and `lp` (submit).
//! - Windows: `Win32_Printer` via PowerShell (discovery) and the shell
//!   `PrintTo` verb (submit), which prints through the file's associated
//!   application, the same way Explorer's "Print" context menu does.
//! - Anything else: no printers, and printing reports
//!   [`PrintError::Unsupported`].

use std::{path::Path, process::Command};

use serde::{Deserialize, Serialize};

/// One printer the OS on this PC can print to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Printer {
    pub name: String,
    /// The printer the OS picks when a job names none.
    pub is_default: bool,
}

/// Options a print job carries. Built by the phone's print screen and sent
/// with the file, so they stay plain data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrintOptions {
    /// 1..=99; out-of-range values are clamped, never rejected, so a job from
    /// a newer phone still prints on an older PC.
    pub copies: u16,
    pub duplex: bool,
    pub color: bool,
}

impl Default for PrintOptions {
    fn default() -> Self {
        PrintOptions { copies: 1, duplex: false, color: true }
    }
}

impl PrintOptions {
    pub fn clamped(self) -> PrintOptions {
        PrintOptions { copies: self.copies.clamp(1, 99), ..self }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PrintError {
    #[error("printing is not supported on this operating system")]
    Unsupported,
    #[error("no printer named {0:?} is installed on this PC")]
    UnknownPrinter(String),
    #[error("the print tool failed: {0}")]
    ToolFailed(String),
    #[error("could not run the print tool: {0}")]
    ToolSpawn(#[from] std::io::Error),
    #[error("invalid printer name {0:?}")]
    InvalidPrinterName(String),
}

/// Printer names reach a PowerShell script on Windows, so keep them to what
/// real spoolers produce: no quotes, no newlines, no shell metacharacters.
fn valid_printer_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 220
        && name.chars().all(|c| !matches!(c, '"' | '\'' | '`' | '$' | '\n' | '\r' | ';' | '|' | '&'))
}

/// The `lp` argument vector (without the leading `lp`) for one job.
pub fn lp_args(printer: Option<&str>, options: PrintOptions, file: &Path) -> Vec<String> {
    let options = options.clamped();
    let mut args: Vec<String> = Vec::new();
    if let Some(name) = printer {
        args.push("-d".into());
        args.push(name.into());
    }
    args.push("-n".into());
    args.push(options.copies.to_string());
    args.push("-o".into());
    args.push(if options.duplex { "sides=two-sided-long-edge".into() } else { "sides=one-sided".into() });
    args.push("-o".into());
    args.push(if options.color { "print-color-mode=color".into() } else { "print-color-mode=monochrome".into() });
    args.push("--".into());
    args.push(file.to_string_lossy().into_owned());
    args
}

/// Printer names from `lpstat -e` output: one name per line, in spooler
/// order. Blank lines are skipped; `lpstat` prints nothing when CUPS has no
/// queues.
pub fn parse_lpstat_printers(output: &str) -> Vec<String> {
    output.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect()
}

/// The default queue from `lpstat -d` output
/// ("system default destination: OfficePrinter"), or `None` when the output
/// says there is none.
pub fn parse_lpstat_default(output: &str) -> Option<String> {
    for line in output.lines() {
        if let Some((_, name)) = line.split_once(':') {
            let name = name.trim();
            if !name.is_empty() && !name.eq_ignore_ascii_case("no system default destination") {
                return Some(name.to_owned());
            }
        }
    }
    None
}

/// Printers from the Windows discovery command
/// (`Get-CimInstance Win32_Printer | Select-Object Name, Default |
/// ConvertTo-Json -Compress`). PowerShell emits a bare object for a single
/// printer and an array for several, so both shapes parse.
pub fn parse_windows_printers(json: &str) -> Result<Vec<Printer>, PrintError> {
    #[derive(Deserialize)]
    struct RawPrinter {
        #[serde(rename = "Name")]
        name: String,
        #[serde(rename = "Default", default)]
        is_default: bool,
    }
    let trimmed = json.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let raw: Vec<RawPrinter> = if trimmed.starts_with('[') {
        serde_json::from_str(trimmed)
    } else {
        serde_json::from_str::<RawPrinter>(trimmed).map(|p| vec![p])
    }
    .map_err(|e| PrintError::ToolFailed(format!("unreadable printer list: {e}")))?;
    Ok(raw.into_iter().map(|p| Printer { name: p.name, is_default: p.is_default }).collect())
}

/// The PowerShell script that prints `file` through the shell PrintTo verb,
/// on `printer` or the default printer when `printer` is `None`.
pub fn windows_print_script(printer: Option<&str>, file: &Path) -> Result<String, PrintError> {
    if let Some(name) = printer {
        if !valid_printer_name(name) {
            return Err(PrintError::InvalidPrinterName(name.to_owned()));
        }
    }
    let path = file.to_string_lossy().replace('\'', "''");
    Ok(match printer {
        Some(name) => format!(
            "Start-Process -FilePath '{path}' -Verb PrintTo -ArgumentList '\"{name}\"' -Wait -WindowStyle Hidden"
        ),
        None => format!("Start-Process -FilePath '{path}' -Verb Print -Wait -WindowStyle Hidden"),
    })
}

fn run_output(program: &str, args: &[&str]) -> Result<String, PrintError> {
    let out = Command::new(program).args(args).output()?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        return Err(PrintError::ToolFailed(if stderr.is_empty() {
            format!("{program} exited with {}", out.status)
        } else {
            stderr
        }));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Every printer this PC can print to right now, default first.
pub fn list_printers() -> Result<Vec<Printer>, PrintError> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let names = parse_lpstat_printers(&run_output("lpstat", &["-e"])?);
        let default = parse_lpstat_default(&run_output("lpstat", &["-d"]).unwrap_or_default());
        let mut printers: Vec<Printer> = names
            .into_iter()
            .map(|name| Printer { is_default: default.as_deref() == Some(name.as_str()), name })
            .collect();
        printers.sort_by_key(|p| !p.is_default);
        Ok(printers)
    }
    #[cfg(target_os = "windows")]
    {
        let json = run_output(
            "powershell",
            &[
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Get-CimInstance Win32_Printer | Select-Object Name, Default | ConvertTo-Json -Compress",
            ],
        )?;
        let mut printers = parse_windows_printers(&json)?;
        printers.sort_by_key(|p| !p.is_default);
        Ok(printers)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        Err(PrintError::Unsupported)
    }
}

/// Print `file` on `printer` (the OS default when `None`) and return once the
/// spooler has accepted the job; the file itself is not deleted.
pub fn print_file(file: &Path, printer: Option<&str>, options: PrintOptions) -> Result<(), PrintError> {
    if let Some(name) = printer {
        if !valid_printer_name(name) {
            return Err(PrintError::InvalidPrinterName(name.to_owned()));
        }
        if !list_printers()?.iter().any(|p| p.name == name) {
            return Err(PrintError::UnknownPrinter(name.to_owned()));
        }
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let args = lp_args(printer, options, file);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        run_output("lp", &refs)?;
        Ok(())
    }
    #[cfg(target_os = "windows")]
    {
        let script = windows_print_script(printer, file)?;
        run_output("powershell", &["-NoProfile", "-NonInteractive", "-Command", &script])?;
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = (file, options);
        Err(PrintError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lpstat_names_parse_and_trim() {
        let out = "OfficePrinter\n\n  PDF  \nHP_LaserJet\n";
        assert_eq!(parse_lpstat_printers(out), ["OfficePrinter", "PDF", "HP_LaserJet"]);
        assert!(parse_lpstat_printers("").is_empty());
    }

    #[test]
    fn lpstat_default_parses_both_shapes() {
        assert_eq!(
            parse_lpstat_default("system default destination: OfficePrinter\n").as_deref(),
            Some("OfficePrinter")
        );
        assert_eq!(parse_lpstat_default("no system default destination\n"), None);
        assert_eq!(parse_lpstat_default(""), None);
    }

    #[test]
    fn windows_printers_parse_array_object_and_empty() {
        let many = parse_windows_printers(
            r#"[{"Name":"HP LaserJet","Default":true},{"Name":"Microsoft Print to PDF","Default":false}]"#,
        )
        .unwrap();
        assert_eq!(many.len(), 2);
        assert!(many[0].is_default);
        let one = parse_windows_printers(r#"{"Name":"Only One","Default":false}"#).unwrap();
        assert_eq!(one.len(), 1);
        assert!(parse_windows_printers("").unwrap().is_empty());
        assert!(parse_windows_printers("not json").is_err());
    }

    #[test]
    fn lp_args_carry_printer_copies_and_sides() {
        let args = lp_args(Some("Office"), PrintOptions { copies: 3, duplex: true, color: false }, Path::new("/tmp/a.pdf"));
        assert_eq!(
            args,
            ["-d", "Office", "-n", "3", "-o", "sides=two-sided-long-edge", "-o", "print-color-mode=monochrome", "--", "/tmp/a.pdf"]
        );
        let defaulted = lp_args(None, PrintOptions::default(), Path::new("x.txt"));
        assert!(!defaulted.iter().any(|a| a == "-d"));
        assert!(defaulted.iter().any(|a| a == "sides=one-sided"));
    }

    #[test]
    fn copies_clamp_to_the_spooler_range() {
        assert_eq!(PrintOptions { copies: 0, ..Default::default() }.clamped().copies, 1);
        assert_eq!(PrintOptions { copies: 500, ..Default::default() }.clamped().copies, 99);
    }

    #[test]
    fn windows_script_quotes_and_rejects_bad_names() {
        let s = windows_print_script(Some("HP LaserJet"), Path::new("C:\\docs\\a.pdf")).unwrap();
        assert!(s.contains("PrintTo") && s.contains("\"HP LaserJet\""));
        let d = windows_print_script(None, Path::new("C:\\docs\\a.pdf")).unwrap();
        assert!(d.contains("-Verb Print "));
        assert!(windows_print_script(Some("bad\"; rm"), Path::new("x")).is_err());
        assert!(windows_print_script(Some(""), Path::new("x")).is_err());
        assert!(!valid_printer_name("a\nb"));
        assert!(valid_printer_name("HP LaserJet Pro"));
    }
}

// ---- Sessions: a phone asking, this PC answering ----

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use nectarlink_protocol::{
    DeviceId, Envelope, ErrorCode,
    messages::{PrintStatus, PrintSubmit, PrinterInfo, PrintersGet, PrintersList, print, types},
};

use crate::{Error, Result, node::Shared, session::Session};

/// The device toggle that allows printing for a device.
pub(crate) const TOGGLE: &str = "printing";

/// A job announced by `print.submit`, waiting for its file to arrive. Jobs
/// are matched to a finished transfer by (device, file name, size) and
/// expire after an hour, so a file that never comes can't print later by
/// accident.
struct PendingJob {
    printer: Option<String>,
    options: PrintOptions,
    at: Instant,
}

fn pending_jobs() -> &'static Mutex<HashMap<(DeviceId, String, u64), PendingJob>> {
    static JOBS: OnceLock<Mutex<HashMap<(DeviceId, String, u64), PendingJob>>> = OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn register_job(peer: DeviceId, submit: &PrintSubmit) {
    let mut jobs = pending_jobs().lock().unwrap_or_else(|e| e.into_inner());
    jobs.retain(|_, j| j.at.elapsed() < Duration::from_secs(3600));
    jobs.insert(
        (peer, submit.name.clone(), submit.size),
        PendingJob {
            printer: submit.printer.clone(),
            options: PrintOptions { copies: submit.copies as u16, duplex: submit.duplex, color: submit.color },
            at: Instant::now(),
        },
    );
}

fn take_job(peer: &DeviceId, name: &str, size: u64) -> Option<PendingJob> {
    let mut jobs = pending_jobs().lock().unwrap_or_else(|e| e.into_inner());
    jobs.remove(&(*peer, name.to_owned(), size))
        .filter(|j| j.at.elapsed() < Duration::from_secs(3600))
}

/// Handles `print.*`. Returns false for other types.
pub(crate) async fn handle(shared: &Arc<Shared>, session: &Arc<Session>, env: &Envelope) -> Result<bool> {
    if env.t != types::PRINT_LIST && env.t != types::PRINT_SUBMIT {
        return Ok(false);
    }
    let (shared, session, env) = (shared.clone(), session.clone(), env.clone());
    tokio::spawn(async move {
        let reply = answer(&shared, &session, &env).await.unwrap_or_else(|e| {
            tracing::debug!(error = %e, "bad print request");
            Envelope::error(ErrorCode::BadMessage, "invalid request")
        });
        let _ = session.send(reply.reply_to(env.id)).await;
    });
    Ok(true)
}

async fn answer(shared: &Arc<Shared>, session: &Session, env: &Envelope) -> Result<Envelope> {
    if !shared.toggle_on(&session.peer, TOGGLE) {
        return Ok(Envelope::error(ErrorCode::Denied, "printing is off for this device"));
    }
    if !shared.we_offer(print::SHARE) {
        return Ok(Envelope::error(ErrorCode::Unsupported, "this PC doesn't share printers"));
    }
    match env.t.as_str() {
        types::PRINT_LIST => {
            let printers = tokio::task::spawn_blocking(list_printers)
                .await
                .map_err(|e| Error::Internal(e.to_string()))?
                .map_err(|e| Error::Internal(e.to_string()))?;
            let printers = printers
                .into_iter()
                .take(print::MAX_PRINTERS)
                .map(|p| PrinterInfo { name: p.name, is_default: p.is_default })
                .collect();
            Ok(Envelope::new(types::PRINT_LIST, &PrintersList { printers })?)
        }
        types::PRINT_SUBMIT => {
            let Some(submit) = env.body::<PrintSubmit>()?.sanitized() else {
                return Ok(Envelope::error(ErrorCode::BadMessage, "invalid print job"));
            };
            register_job(session.peer, &submit);
            Ok(Envelope::empty(types::OK))
        }
        _ => Ok(Envelope::error(ErrorCode::BadMessage, "unknown print request")),
    }
}

/// A files transfer from `peer` just landed: print the files that were
/// announced with `print.submit`, and tell the phone how each job ended.
pub(crate) async fn print_arrived(
    shared: &Arc<Shared>,
    peer: DeviceId,
    files: &[(String, u64)],
    saved: &[PathBuf],
) {
    if !shared.toggle_on(&peer, TOGGLE) {
        return;
    }
    for ((name, size), path) in files.iter().zip(saved.iter()) {
        let Some(job) = take_job(&peer, name, *size) else { continue };
        let path = path.clone();
        let result = tokio::task::spawn_blocking(move || print_file(&path, job.printer.as_deref(), job.options))
            .await
            .unwrap_or_else(|e| Err(PrintError::ToolFailed(e.to_string())));
        let status = match &result {
            Ok(()) => PrintStatus { name: name.clone(), state: print::STATE_DONE.into(), detail: None },
            Err(e) => {
                tracing::warn!(error = %e, "a print job from a phone failed");
                PrintStatus { name: name.clone(), state: print::STATE_ERROR.into(), detail: Some(e.to_string()) }
            }
        };
        if let Some(session) = shared.session(&peer)
            && let Ok(env) = Envelope::new(types::PRINT_STATUS, &status)
        {
            let _ = session.send(env).await;
        }
    }
}

// ---- A phone asking a PC ----

/// Lists a paired PC's printers.
pub(crate) async fn list_remote(shared: &Shared, session: &Session) -> Result<Vec<PrinterInfo>> {
    if !shared.toggle_on(&session.peer, TOGGLE) {
        return Err(Error::Denied);
    }
    let env = Envelope::new(types::PRINT_LIST, &PrintersGet {})?;
    let PrintersList { mut printers } =
        session.request(env, crate::session::REQUEST_TIMEOUT).await?.expect_body(types::PRINT_LIST)?;
    printers.truncate(print::MAX_PRINTERS);
    Ok(printers)
}
