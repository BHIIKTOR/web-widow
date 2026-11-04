//! Loopback filtering proxy for the authenticated browser (§2 step 7,
//! /M-3).
//!
//! `Fetch` is the attribution layer, but it cannot see a WebSocket handshake and
//! it never sees a preconnect. The proxy is the enforcement layer below the
//! renderer: Chrome runs with `--proxy-server` **and**
//! `--proxy-bypass-list=<-loopback>` (so loopback is not silently bypassed), and
//! every absolute-form request or `CONNECT` tunnel is decided by the same
//! [`AuthEgressPolicy`]. A credential-bearing request to a non-binding origin is
//! refused as well.
//!
//! Only plain `http` forwarding and `CONNECT` tunnelling are implemented: the
//! authenticated browser speaks http(s) through it, and everything else is
//! refused by the policy before a socket is opened.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use url::Url;

use crate::auth::egress::{AuthEgressPolicy, EgressDecision};
use crate::core::error::{Result, WidowError};

/// One refused connection, value-free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyDenial {
    pub host: String,
    pub reason: String,
}

/// A running loopback proxy bound to an ephemeral port.
pub struct FilteringProxy {
    listener: Option<TcpListener>,
    port: u16,
    stop: Arc<AtomicBool>,
    denials: Arc<Mutex<Vec<ProxyDenial>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FilteringProxy {
    pub fn start(
        policy: AuthEgressPolicy,
        host_map: std::collections::BTreeMap<String, String>,
    ) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|error| {
            WidowError::RenderFailed(format!("auth proxy could not bind loopback: {error}"))
        })?;
        listener
            .set_nonblocking(true)
            .map_err(|error| WidowError::RenderFailed(format!("auth proxy: {error}")))?;
        let port = listener
            .local_addr()
            .map_err(|error| WidowError::RenderFailed(format!("auth proxy: {error}")))?
            .port();
        let stop = Arc::new(AtomicBool::new(false));
        let denials: Arc<Mutex<Vec<ProxyDenial>>> = Arc::new(Mutex::new(Vec::new()));
        let thread = {
            let stop = Arc::clone(&stop);
            let denials = Arc::clone(&denials);
            let listener = listener.try_clone().map_err(|error| {
                WidowError::RenderFailed(format!(
                    "auth proxy could not clone its listener: {error}"
                ))
            })?;
            std::thread::spawn(move || serve(listener, policy, host_map, stop, denials))
        };
        Ok(Self {
            listener: Some(listener),
            port,
            stop,
            denials,
            thread: Some(thread),
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn denials(&self) -> Vec<ProxyDenial> {
        self.denials
            .lock()
            .map(|denials| denials.clone())
            .unwrap_or_default()
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.listener.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for FilteringProxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.listener.take();
    }
}

fn serve(
    listener: TcpListener,
    policy: AuthEgressPolicy,
    host_map: std::collections::BTreeMap<String, String>,
    stop: Arc<AtomicBool>,
    denials: Arc<Mutex<Vec<ProxyDenial>>>,
) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let policy = policy.clone();
                let host_map = host_map.clone();
                let denials = Arc::clone(&denials);
                std::thread::spawn(move || handle(stream, &policy, &host_map, &denials));
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return,
        }
    }
}

fn deny(denials: &Arc<Mutex<Vec<ProxyDenial>>>, host: &str, reason: &str) {
    if let Ok(mut denials) = denials.lock()
        && denials.len() < 64
    {
        denials.push(ProxyDenial {
            host: host.to_string(),
            reason: reason.to_string(),
        });
    }
}

fn refuse(stream: &mut TcpStream) {
    let _ = stream
        .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
}

fn handle(
    mut stream: TcpStream,
    policy: &AuthEgressPolicy,
    host_map: &std::collections::BTreeMap<String, String>,
    denials: &Arc<Mutex<Vec<ProxyDenial>>>,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return, // bare accept (a preconnect probe)
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
        }
        if buffer.len() > 64 * 1024 {
            return;
        }
    }
    let head = String::from_utf8_lossy(&buffer).to_string();
    let mut lines = head.split("\r\n");
    let Some(request_line) = lines.next() else {
        return;
    };
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return;
    };
    let carries_credential = lines.clone().any(|line| {
        let lower = line.to_ascii_lowercase();
        lower.starts_with("cookie:") || lower.starts_with("authorization:")
    });
    if method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = split_authority(target);
        let check = policy.check(&origin(&host, port));
        if let EgressDecision::Block(denial) = check {
            deny(denials, &host, denial.as_str());
            refuse(&mut stream);
            return;
        }
        let resolved = host_map.get(&host).map(String::as_str).unwrap_or(&host);
        match TcpStream::connect((resolved, port)) {
            Ok(mut upstream) => {
                let _ = stream.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n");
                splice(&mut stream, &mut upstream);
            }
            Err(_) => refuse(&mut stream),
        }
        return;
    }
    let Ok(url) = Url::parse(target) else {
        // origin-form without a proxy authority: not a proxied request.
        refuse(&mut stream);
        return;
    };
    if let EgressDecision::Block(denial) = policy.check(&url) {
        deny(
            denials,
            &crate::auth::canonical_url_host(&url),
            denial.as_str(),
        );
        refuse(&mut stream);
        return;
    }
    if carries_credential && let EgressDecision::Block(denial) = policy.may_bear_credential(&url) {
        deny(
            denials,
            &crate::auth::canonical_url_host(&url),
            denial.as_str(),
        );
        refuse(&mut stream);
        return;
    }
    forward(&mut stream, &head, &url, host_map);
}

fn forward(
    stream: &mut TcpStream,
    head: &str,
    url: &Url,
    host_map: &std::collections::BTreeMap<String, String>,
) {
    let host = url.host_str().unwrap_or_default().to_string();
    let port = url.port_or_known_default().unwrap_or(80);
    let resolved = host_map.get(&host).map(String::as_str).unwrap_or(&host);
    let Ok(mut upstream) = TcpStream::connect((resolved, port)) else {
        refuse(stream);
        return;
    };
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET");
    let _ = parts.next();
    let version = parts.next().unwrap_or("HTTP/1.1");
    let mut path = url.path().to_string();
    if let Some(query) = url.query() {
        path.push('?');
        path.push_str(query);
    }
    let mut out = format!("{method} {path} {version}\r\n");
    for line in lines {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        let lower = trimmed.to_ascii_lowercase();
        if lower.starts_with("proxy-connection:") || lower.starts_with("connection:") {
            continue;
        }
        out.push_str(trimmed);
        out.push_str("\r\n");
    }
    out.push_str("Connection: close\r\n\r\n");
    if upstream.write_all(out.as_bytes()).is_err() {
        refuse(stream);
        return;
    }
    let mut response = [0u8; 16 * 1024];
    loop {
        match upstream.read(&mut response) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                if stream.write_all(&response[..read]).is_err() {
                    break;
                }
            }
        }
    }
}

fn splice(left: &mut TcpStream, right: &mut TcpStream) {
    let (Ok(mut left_up), Ok(mut right_up)) = (left.try_clone(), right.try_clone()) else {
        return;
    };
    let (Ok(mut left_down), Ok(mut right_down)) = (left.try_clone(), right.try_clone()) else {
        return;
    };
    left_up.set_read_timeout(Some(Duration::from_secs(30))).ok();
    right_up
        .set_read_timeout(Some(Duration::from_secs(30)))
        .ok();
    let up = std::thread::spawn(move || {
        let _ = std::io::copy(&mut left_up, &mut right_up);
    });
    let _ = std::io::copy(&mut right_down, &mut left_down);
    let _ = up.join();
}

fn split_authority(authority: &str) -> (String, u16) {
    match authority.rsplit_once(':') {
        Some((host, port)) => (
            host.trim_start_matches('[')
                .trim_end_matches(']')
                .to_string(),
            port.parse().unwrap_or(443),
        ),
        None => (authority.to_string(), 443),
    }
}

/// A `CONNECT` tunnel is TLS (or a ws/wss upgrade): evaluate it as the https
/// origin it carries, not as plain http (which would be a scheme denial for
/// every named host).
fn origin(host: &str, port: u16) -> Url {
    Url::parse(&format!("https://{host}:{port}/"))
        .unwrap_or_else(|_| Url::parse("https://invalid.invalid/").expect("static url"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> AuthEgressPolicy {
        AuthEgressPolicy::new("bound.test", 443, false, &["cdn.test".to_string()], false).unwrap()
    }

    #[test]
    fn the_proxy_decides_with_the_shipped_policy() {
        let policy = policy();
        assert!(matches!(
            policy.check(&Url::parse("https://bound.test/x").unwrap()),
            EgressDecision::Allow
        ));
        assert!(matches!(
            policy.check(&Url::parse("https://evil.test/x").unwrap()),
            EgressDecision::Block(_)
        ));
        assert!(matches!(
            policy.check(&Url::parse("https://cdn.test/a.js").unwrap()),
            EgressDecision::Allow
        ));
    }

    #[test]
    fn an_authority_is_split_without_guessing() {
        assert_eq!(
            split_authority("bound.test:8443"),
            ("bound.test".to_string(), 8443)
        );
        assert_eq!(
            split_authority("bound.test"),
            ("bound.test".to_string(), 443)
        );
    }

    #[test]
    fn a_proxy_denies_an_undeclared_tunnel_before_connecting() {
        let proxy = FilteringProxy::start(policy(), Default::default()).expect("proxy");
        let port = proxy.port();
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        stream
            .write_all(b"CONNECT evil.test:443 HTTP/1.1\r\nHost: evil.test:443\r\n\r\n")
            .unwrap();
        let mut buffer = [0u8; 256];
        let read = stream.read(&mut buffer).unwrap_or(0);
        let response = String::from_utf8_lossy(&buffer[..read]).to_string();
        assert!(response.starts_with("HTTP/1.1 403"), "{response}");
        let denials = proxy.denials();
        assert_eq!(denials.len(), 1);
        assert_eq!(denials[0].host, "evil.test");
        assert_eq!(denials[0].reason, "host");
        proxy.stop();
    }
}
