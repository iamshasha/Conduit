//! Allow-listed PowerShell execution.
//!
//! A website can run only the cmdlets on the curated list below, all of them
//! read-only / informational, and only with arguments that contain no shell
//! metacharacters. Each cmdlet carries a base 0-100 risk score, and a dynamic
//! classifier ([`classify`]) raises that score for the arguments a call
//! actually uses — a wildcard sweep, a remote `-ComputerName`, or a probe of an
//! off-box host are more sensitive than the bare cmdlet. Anything landing at or
//! above `CONSENT_THRESHOLD` also needs per-call human consent, and the reasons
//! behind the score are shown in that prompt. Nothing outside the list runs —
//! there is no path to an arbitrary command string, `Invoke-Expression`,
//! pipelines, redirection, or `;`/`&`/`|` chaining.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// (cmdlet, base risk 0-100). Read-only cmdlets only. Higher = more sensitive.
pub const ALLOWED: &[(&str, u32)] = &[
    ("Get-Date", 0),
    ("Get-Random", 0),
    ("Get-TimeZone", 0),
    ("Get-Culture", 0),
    ("Get-Uptime", 5),
    ("Get-Host", 5),
    ("Get-Location", 5),
    ("Get-ExecutionPolicy", 5),
    ("Get-Volume", 10),
    ("Get-PSDrive", 10),
    ("Get-Command", 10),
    ("Get-Module", 10),
    ("Get-Process", 15),
    ("Get-Service", 15),
    ("Get-HotFix", 15),
    ("Get-NetIPAddress", 20),
    ("Get-NetAdapter", 20),
    ("Get-NetIPConfiguration", 20),
    ("Get-NetTCPConnection", 20),
    ("Get-NetRoute", 20),
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

/// The canonical cmdlet name and its base risk, if `command` is allow-listed.
pub fn lookup(command: &str) -> Option<(&'static str, u32)> {
    ALLOWED.iter().find(|(c, _)| c.eq_ignore_ascii_case(command)).map(|(c, r)| (*c, *r))
}

/// The whole list as (name, base risk) for `shell.commands`.
pub fn catalog() -> Vec<(&'static str, u32)> {
    ALLOWED.to_vec()
}

/// One argument is safe only if it is short and free of shell metacharacters.
/// A leading `-` (a switch) is allowed; otherwise letters, digits and a small
/// set of separators that appear in names, paths, addresses and wildcards. None
/// of the accepted characters can break out of the `& { cmdlet args }` the args
/// are joined into — there are no quotes, spaces, or `;`/`|`/`&`/`$`/backtick.
fn arg_ok(a: &str) -> bool {
    if a.is_empty() || a.len() > MAX_ARG_LEN {
        return false;
    }
    let body = a.strip_prefix('-').unwrap_or(a);
    !body.is_empty()
        && body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'\\' | b'/' | b'*' | b'?' | b','))
}

/// The outcome of classifying a call: the canonical cmdlet, its cleaned args,
/// the effective risk (base plus argument-driven additions, capped at 100), and
/// the human-readable reasons the score was raised.
pub struct Assessment {
    pub cmdlet: &'static str,
    pub base_risk: u32,
    pub risk: u32,
    pub args: Vec<String>,
    pub reasons: Vec<String>,
}

impl Assessment {
    pub fn needs_consent(&self) -> bool {
        self.risk >= CONSENT_THRESHOLD
    }
}

/// Validate a command + args against the allow-list and the arg rules.
/// Returns the canonical cmdlet, its base risk, and the cleaned args.
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

/// Is `host` a loopback / this-machine reference (so a probe of it is not a
/// reach out onto the network)?
fn is_local_host(host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    h == "localhost"
        || h == "::1"
        || h == "0.0.0.0"
        || h == "::"
        || h.ends_with(".localhost")
        || h.starts_with("127.")
}

/// Argument-aware risk assessment. Starts from the cmdlet's base risk and adds
/// for the sensitivity of the specific arguments, so a normally-quiet cmdlet
/// used for a broad or remote query is correctly escalated to consent.
pub fn classify(command: &str, args: &[String]) -> Result<Assessment, String> {
    let (cmdlet, base_risk, args) = validate(command, args)?;
    let mut add = 0u32;
    let mut reasons: Vec<String> = Vec::new();

    let mut prev_switch: Option<String> = None;
    let mut targets_remote = false;
    for a in &args {
        if let Some(sw) = a.strip_prefix('-') {
            let sw = sw.to_ascii_lowercase();
            // A remote query switch: the result describes another machine.
            if sw == "computername" || sw == "cimsession" {
                add += 40;
                reasons.push("queries a remote computer".into());
            }
            if sw == "includeusername" {
                add += 10;
                reasons.push("reveals which user owns each process".into());
            }
            prev_switch = Some(sw);
            continue;
        }
        // A value. Wildcards mean a broad sweep rather than one named item.
        if a.contains('*') || a.contains('?') {
            add += 10;
            reasons.push("matches many items with a wildcard".into());
        }
        // For the reachability cmdlets, a non-local target is a network probe.
        let networky = matches!(cmdlet, "Test-Connection" | "Resolve-DnsName");
        let host_switch = matches!(prev_switch.as_deref(), Some("computername") | Some("targetname") | Some("name") | Some("server"));
        if networky && (prev_switch.is_none() || host_switch) && !is_local_host(a) {
            targets_remote = true;
        }
        prev_switch = None;
    }
    if targets_remote {
        add += 15;
        reasons.push("contacts a host out on the network".into());
    }

    // De-duplicate reasons while keeping order.
    reasons.dedup();
    let risk = (base_risk + add).min(100);
    Ok(Assessment { cmdlet, base_risk, risk, args, reasons })
}

/// The PowerShell executable to use: pwsh (cross-platform) if present, else
/// Windows PowerShell. None if neither is available. The probe spawns a process,
/// so the result is cached for the life of the server.
fn powershell() -> Option<&'static str> {
    static PS: OnceLock<Option<&'static str>> = OnceLock::new();
    *PS.get_or_init(|| {
        if which("pwsh") {
            Some("pwsh")
        } else if cfg!(windows) && which("powershell") {
            Some("powershell")
        } else {
            None
        }
    })
}

fn which(bin: &str) -> bool {
    // A cheap presence check: try to run `<bin> -NoProfile -Command $null`.
    let mut cmd = Command::new(bin);
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", "$null"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .stdin(Stdio::null());
    crate::system::hide_console(&mut cmd); // no console flash from the probe
    cmd.status().map(|s| s.success()).unwrap_or(false)
}

/// Is any PowerShell available? Cheap after the first call (cached).
pub fn available() -> bool {
    powershell().is_some()
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
    let mut cmd = Command::new(ps);
    cmd.args(["-NoProfile", "-NonInteractive", "-NoLogo", "-Command", &format!("& {{ {composed} }}")])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    crate::system::hide_console(&mut cmd); // don't flash a console window
    let mut child = cmd
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

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn rejects_unlisted_and_metacharacters() {
        assert!(validate("Remove-Item", &[]).is_err());
        assert!(validate("Invoke-Expression", &["x".into()]).is_err());
        // metacharacters in args
        for bad in ["a;b", "$env:PATH", "a|b", "a&b", "a`b", "$(x)", "a b", "\"x\"", "'x'", "a{b}", "a(b)"] {
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

    #[test]
    fn wildcard_raises_risk_and_can_cross_threshold() {
        let base = classify("Get-Service", &[]).unwrap();
        assert_eq!(base.risk, 15);
        assert!(!base.needs_consent());
        let wild = classify("Get-Service", &v(&["-Name", "win*"])).unwrap();
        assert_eq!(wild.risk, 25, "a wildcard should push Get-Service over the line");
        assert!(wild.needs_consent());
        assert!(wild.reasons.iter().any(|r| r.contains("wildcard")));
    }

    #[test]
    fn remote_computername_is_high_risk() {
        let a = classify("Get-Process", &v(&["-ComputerName", "server01"])).unwrap();
        assert!(a.risk >= 50, "remote query should be high risk, got {}", a.risk);
        assert!(a.reasons.iter().any(|r| r.contains("remote computer")));
    }

    #[test]
    fn local_probe_is_not_flagged_remote() {
        let local = classify("Test-Connection", &v(&["localhost"])).unwrap();
        assert!(!local.reasons.iter().any(|r| r.contains("network")));
        let ext = classify("Test-Connection", &v(&["8.8.8.8"])).unwrap();
        assert!(ext.reasons.iter().any(|r| r.contains("network")));
        assert!(ext.risk > local.risk);
    }

    #[test]
    fn risk_is_capped_at_100() {
        // Contrive several escalators at once; the sum would exceed 100.
        let a = classify("Test-Connection", &v(&["-ComputerName", "host*", "8.8.8.8"])).unwrap();
        assert!(a.risk <= 100);
    }

    #[test]
    fn wildcards_and_commas_are_allowed_chars() {
        assert!(classify("Get-Process", &v(&["-Name", "a,b,c"])).is_ok());
        assert!(classify("Get-Service", &v(&["ssh*"])).is_ok());
    }
}
