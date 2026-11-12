//! Reads the macOS system proxy by running the configuration probe and
//! parsing its dictionary output.

use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::host::net::{ProxyConfig, ProxyResolution};

/// How long the probe may run before it is killed and treated as no proxy.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) fn read_system_proxy() -> ProxyResolution {
    match run_probe() {
        Some(text) => parse_scutil_proxy(&text),
        None => ProxyResolution::direct(),
    }
}

/// Run the configuration probe and read its output within a bounded window.
/// This sits below the launcher port: it is a platform primitive with no
/// runtime dependency, so a direct spawn is the right call here rather than
/// routing through a higher crate's launcher.
#[expect(
    clippy::disallowed_methods,
    reason = "L0 platform probe below the ProcessLauncher port; no runtime dependency"
)]
fn run_probe() -> Option<String> {
    let mut child = Command::new("/usr/sbin/scutil")
        .arg("--proxy")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let drain = child.stdout.take().map(|mut reader| {
        thread::spawn(move || {
            let mut buf = Vec::new();
            drop(reader.read_to_end(&mut buf));
            buf
        })
    });
    let status = wait_bounded(&mut child, PROBE_TIMEOUT);
    drop(child.kill());
    drop(child.wait());
    let bytes = drain
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    let status = status?;
    if !status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Poll the child until it exits or the deadline passes.
fn wait_bounded(child: &mut Child, limit: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {}
            Err(_) => return None,
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(1));
    }
}

/// A configuration from the probe dictionary output. The parser is pure so its
/// behavior is testable without running the probe.
fn parse_scutil_proxy(text: &str) -> ProxyResolution {
    let mut http_enable = false;
    let mut http_host: Option<String> = None;
    let mut http_port: Option<u16> = None;
    let mut https_enable = false;
    let mut https_host: Option<String> = None;
    let mut https_port: Option<u16> = None;
    let mut auto_config_enable = false;
    let mut auto_config_url: Option<String> = None;
    let mut exceptions: Vec<String> = Vec::new();
    let mut in_exceptions = false;

    for raw in text.lines() {
        let line = raw.trim();
        if in_exceptions {
            if line == "}" {
                in_exceptions = false;
                continue;
            }
            // The dictionary prints array entries with a numeric index prefix
            // (for example "0 : *.local"). Strip it when the left side is all
            // digits, so the bypass rule is the entry alone. Splitting on the
            // spaced colon leaves an IPv6 address like fe80::1 intact.
            let item = match line.split_once(" : ") {
                Some((idx, value))
                    if !idx.is_empty() && idx.chars().all(|c| c.is_ascii_digit()) =>
                {
                    value.trim()
                }
                _ => line,
            };
            if !item.is_empty() {
                exceptions.push(item.to_string());
            }
            continue;
        }
        if line.starts_with("ExceptionsList") && line.ends_with('{') {
            in_exceptions = true;
            continue;
        }
        let Some((key, val)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let val = val.trim();
        match key {
            "HTTPEnable" => http_enable = val == "1",
            "HTTPProxy" if !val.is_empty() => http_host = Some(val.to_string()),
            "HTTPPort" => http_port = val.parse().ok(),
            "HTTPSEnable" => https_enable = val == "1",
            "HTTPSProxy" if !val.is_empty() => https_host = Some(val.to_string()),
            "HTTPSPort" => https_port = val.parse().ok(),
            "ProxyAutoConfigEnable" => auto_config_enable = val == "1",
            "ProxyAutoConfigURLString" if !val.is_empty() => {
                auto_config_url = Some(val.to_string());
            }
            _ => {}
        }
    }

    let http = match (http_enable, http_host, http_port) {
        (true, Some(host), Some(port)) => Some(format!("http://{host}:{port}")),
        _ => None,
    };
    let https = match (https_enable, https_host, https_port) {
        (true, Some(host), Some(port)) => Some(format!("http://{host}:{port}")),
        _ => None,
    };
    if http.is_some() || https.is_some() {
        return ProxyResolution::system(ProxyConfig::new(http, https, exceptions.join(",")));
    }
    match (auto_config_enable, auto_config_url) {
        (true, Some(url)) => ProxyResolution::pac(url),
        _ => ProxyResolution::direct(),
    }
}

#[cfg(test)]
mod tests;
