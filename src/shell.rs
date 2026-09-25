//! Allow-listed PowerShell execution.
//!
//! A website can run only the cmdlets on the curated list below, all of them
//! read-only / informational, and only with arguments that contain no shell
//! metacharacters. Each cmdlet carries a 0-100 risk score; anything at or above
//! `CONSENT_THRESHOLD` also needs per-call human consent. Nothing outside the
//! list runs — there is no path to an arbitrary command string, `Invoke-
//! Expression`, pipelines, redirection, or `;`/`&`/`|` chaining.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// (cmdlet, risk 0-100). Read-only cmdlets only. Higher = more sensitive.
pub const ALLOWED: &[(&str, u32)] = &[
    ("Get-Date", 0),
    ("Get-TimeZone", 0),
    ("Get-Culture", 0),
    ("Get-Uptime", 5),
    ("Get-Host", 5),
    ("Get-Location", 5),
    ("Get-Volume", 10),
    ("Get-PSDrive", 10),
    ("Get-Process", 15),
    ("Get-Service", 15),
    ("Get-HotFix", 15),
    ("Get-NetIPAddress", 20),
    ("Get-NetAdapter", 20),
    ("Get-NetIPConfiguration", 20),
    ("Get-ComputerInfo", 25),
    ("Test-Connection", 25),
    ("Resolve-DnsName", 25),
];

/// Cmdlets at or above this risk also require per-call consent.
pub const CONSENT_THRESHOLD: u32 = 20;

const MAX_ARGS: usize = 12;
const MAX_ARG_LEN: usize = 64;
const MAX_OUTPUT: usize = 256 * 1024;
const TIMEOUT: Duration = Duration::from_secs(12);

/// The canonical cmdlet name and its risk, if `command` is allow-listed.
pub fn lookup(command: &str) -> Option<(&'static str, u32)> {
    ALLOWED.iter().find(|(c, _)| c.eq_ignore_ascii_case(command)).map(|(c, r)| (*c, *r))
}

/// The whole list as (name, risk) for `shell.commands`.
pub fn catalog() -> Vec<(&'static str, u32)> {
    ALLOWED.to_vec()
}

/// One argument is safe only if it is short and free of shell metacharacters.
/// A leading `-` (a switch) is allowed; otherwise letters, digits and a small
/// set of separators that appear in names, paths and addresses.
fn arg_ok(a: &str) -> bool {
    if a.is_empty() || a.len() > MAX_ARG_LEN {
        return false;
    }
    let body = a.strip_prefix('-').unwrap_or(a);
    !body.is_empty()
        && body.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'\\' | b'/'))
}

/// Validate a command + args against the allow-list and the arg rules.
/// Returns the canonical cmdlet, its risk, and the cleaned args.
pub fn validate(command: &str, args: &[String]) -> Result<(&'static str, u32, Vec<String>), String> {
    let (cmdlet, risk) = lookup(command).ok_or_else(|| format!("{command:?} is not an allowed command"))?;
    if args.len() > MAX_ARGS {
        return Err(format!("too many arguments (max {MAX_ARGS})"));
    }
    for a in args {
        if !arg_ok(a) {
            return Err(format!("argument {a:?} contains characters that are not allowed"));
        }
    }
    Ok((cmdlet, risk, args.to_vec()))
}

/// The PowerShell executable to use: pwsh (cross-platform) if present, else
/// Windows PowerShell. None if neither is available.
fn powershell() -> Option<&'static str> {
    if which("pwsh") {
        return Some("pwsh");
    }
    if cfg!(windows) && which("powershell") {
        return Some("powershell");
    }
    None
}

fn which(bin: &str) -> bool {
    // A cheap presence check: try to run `<bin> -NoProfile -Command $PSVersionTable`.
    Command::new(bin)
        .args(["-NoProfile", "-NonInteractive", "-Command", "$null"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub struct Output {
    pub text: String,
    pub exit_code: i32,
    pub truncated: bool,
}

/// Run an already-validated cmdlet with its args. The composed command is built
/// only from allow-listed, metacharacter-free pieces, so `-Command` cannot be
/// steered into anything else. Output is capped and the process is killed on
/// timeout.
pub fn run(cmdlet: &str, args: &[String]) -> Result<Output, String> {
    let ps = powershell().ok_or("PowerShell is not available on this system")?;
    // Safe to join: cmdlet is from the list and every arg passed arg_ok.
    let composed = if args.is_empty() {
        cmdlet.to_string()
    } else {
        format!("{cmdlet} {}", args.join(" "))
    };
    let mut child = Command::new(ps)
        .args(["-NoProfile", "-NonInteractive", "-NoLogo", "-Command", &format!("& {{ {composed} }}")])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot start PowerShell: {e}"))?;

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = String::new();
                if let Some(so) = child.stdout.take() {
                    let mut buf = Vec::new();
                    let _ = so.take(MAX_OUTPUT as u64 + 1).read_to_end(&mut buf);
                    out = String::from_utf8_lossy(&buf).into_owned();
                }
                if out.trim().is_empty() {
                    if let Some(se) = child.stderr.take() {
                        let mut buf = Vec::new();
                        let _ = se.take(MAX_OUTPUT as u64 + 1).read_to_end(&mut buf);
                        out = String::from_utf8_lossy(&buf).into_owned();
                    }
                }
                let truncated = out.len() > MAX_OUTPUT;
                if truncated {
                    out.truncate(MAX_OUTPUT);
                }
                return Ok(Output { text: out, exit_code: status.code().unwrap_or(-1), truncated });
            }
            Ok(None) => {
                if start.elapsed() > TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("command timed out".into());
                }
                std::thread::sleep(Duration::from_millis(40));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unlisted_and_metacharacters() {
        assert!(validate("Remove-Item", &[]).is_err());
        assert!(validate("Invoke-Expression", &["x".into()]).is_err());
        // metacharacters in args
        for bad in ["a;b", "$env:PATH", "a|b", "a&b", "a`b", "$(x)", "a b", "\"x\"", "'x'"] {
            assert!(validate("Get-Process", &[bad.to_string()]).is_err(), "{bad} must be rejected");
        }
    }

    #[test]
    fn accepts_listed_with_safe_args() {
        let (c, r, a) = validate("get-process", &["-Name".into(), "explorer".into()]).unwrap();
        assert_eq!(c, "Get-Process");
        assert_eq!(r, 15);
        assert_eq!(a, vec!["-Name", "explorer"]);
    }

    #[test]
    fn threshold_split() {
        assert!(lookup("Get-Date").unwrap().1 < CONSENT_THRESHOLD);
        assert!(lookup("Get-ComputerInfo").unwrap().1 >= CONSENT_THRESHOLD);
    }
}
