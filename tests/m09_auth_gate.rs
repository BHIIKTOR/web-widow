//! v0.9 — the authenticated browser gate, measured at the enforcement site.
//!
//! An HTTPS fixture (test CA + resolver rules, test-only launch arguments) on
//! three loopback addresses provides: the binding host `bound.test`, its
//! subdomain `sub.bound.test` on the **same port**, and an undeclared
//! `evil.test`. The jar is constructed in memory, so the gate is exercised
//! directly rather than through the keyring.
//!
//! Find-ing-2 proof: with `allow_subdomains = true` the subdomain is *reachable*
//! while the cookie is **never** attached there, and a domain-wide jar entry is
//! refused at injection.

#![cfg(feature = "http")]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde_json::json;

use web_widow::auth::egress::AuthEgressPolicy;
use web_widow::auth::gate::AuthGate;
use web_widow::auth::login::CapturedCookie;
use web_widow::config::{AuthKind, AuthProfileConfig, Config};

const BOUND: &str = "bound.test";
const SUB: &str = "sub.bound.test";
const EVIL: &str = "evil.test";

#[derive(Clone, Debug)]
struct Hit {
    path: String,
    cookie: Option<String>,
    authorization: bool,
}

type Log = Arc<Mutex<Vec<Hit>>>;
type Routes = Arc<dyn Fn(&str) -> (u16, Vec<String>, String) + Send + Sync>;

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn material() -> Arc<ServerConfig> {
    let dir = std::env::temp_dir().join(format!("ww-gate-ca-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "web-widow gate test CA");
    let ca_key = KeyPair::generate().unwrap();
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let mut leaf_params = CertificateParams::new(vec![
        BOUND.to_string(),
        SUB.to_string(),
        EVIL.to_string(),
        "127.0.0.1".to_string(),
        "127.0.0.2".to_string(),
        "127.0.0.3".to_string(),
    ])
    .unwrap();
    leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let leaf_key = KeyPair::generate().unwrap();
    let leaf = leaf_params
        .signed_by(&leaf_key, &Issuer::from_params(&ca_params, &ca_key))
        .unwrap();
    std::fs::write(dir.join("ca.pem"), ca.pem()).unwrap();
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(leaf.der().to_vec())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der())),
        )
        .unwrap();
    Arc::new(config)
}

/// One TLS listener that answers one request per connection and logs it.
fn serve(address: &str, config: Arc<ServerConfig>, log: Log, routes: Routes) -> u16 {
    let listener = TcpListener::bind(address).unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let config = Arc::clone(&config);
            let log = Arc::clone(&log);
            let routes = Arc::clone(&routes);
            std::thread::spawn(move || {
                use rustls::StreamOwned;
                let Ok(connection) = rustls::ServerConnection::new(config) else {
                    return;
                };
                let mut tls = StreamOwned::new(connection, stream);
                let mut reader = BufReader::new(&mut tls);
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    return;
                }
                let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
                let mut headers: BTreeMap<String, String> = BTreeMap::new();
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0 {
                        break;
                    }
                    let trimmed = header.trim_end();
                    if trimmed.is_empty() {
                        break;
                    }
                    if let Some((name, value)) = trimmed.split_once(':') {
                        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
                    }
                }
                log.lock().unwrap().push(Hit {
                    path: path.clone(),
                    cookie: headers.get("cookie").cloned(),
                    authorization: headers.contains_key("authorization"),
                });
                let (status, extra, body) = routes(&path);
                let mut response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                    body.len()
                );
                for header in extra {
                    response.push_str(&header);
                    response.push_str("\r\n");
                }
                response.push_str("\r\n");
                response.push_str(&body);
                let mut writer = tls;
                let _ = writer.write_all(response.as_bytes());
                let _ = writer.flush();
            });
        }
    });
    port
}

struct Fixture {
    port: u16,
    log_binding: Log,
    log_sub: Log,
    log_evil: Log,
}

fn start() -> Fixture {
    let config = material();
    let log_binding: Log = Arc::new(Mutex::new(Vec::new()));
    let log_sub: Log = Arc::new(Mutex::new(Vec::new()));
    let log_evil: Log = Arc::new(Mutex::new(Vec::new()));
    // Ports must match: the policy pins a subdomain to the bound port.
    let port_slot = Arc::new(std::sync::atomic::AtomicU16::new(0));
    let slot = Arc::clone(&port_slot);
    let binding_routes: Routes = Arc::new(move |path: &str| {
        let (path_only, query) = path.split_once('?').unwrap_or((path, ""));
        match path_only {
            "/webrtc" => {
                let port = query
                    .split('&')
                    .find_map(|pair| pair.strip_prefix("port="))
                    .unwrap_or("9");
                let address = query
                    .split('&')
                    .find_map(|pair| pair.strip_prefix("addr="))
                    .unwrap_or("127.0.0.1");
                (
                    200,
                    vec!["Content-Type: text/html".to_string()],
                    format!(
                        "<html><body><script>const pc=new RTCPeerConnection({{iceServers:[{{urls:'stun:{address}:{port}'}}]}});pc.createDataChannel('x');window.__ww_pc=1;pc.createOffer().then(o=>pc.setLocalDescription(o)).then(()=>{{window.__ww_state=pc.iceGatheringState}}).catch(()=>{{window.__ww_state='err'}});</script></body></html>"
                    ),
                )
            }
            "/ws" => {
                let p = slot.load(std::sync::atomic::Ordering::Relaxed);
                (
                    200,
                    vec!["Content-Type: text/html".to_string()],
                    format!(
                        "<html><body><script>try{{new WebSocket('wss://evil.test:{p}/ws')}}catch(e){{}}</script></body></html>"
                    ),
                )
            }
            "/challenge" => (
                401,
                vec![
                    "WWW-Authenticate: Basic realm=\"ww\"".to_string(),
                    "Content-Type: text/plain".to_string(),
                ],
                "challenge".to_string(),
            ),
            _ => (
                200,
                vec![
                    "Set-Cookie: transient=1; Path=/; Secure".to_string(),
                    "Content-Type: text/html".to_string(),
                ],
                "<html><body>bound</body></html>".to_string(),
            ),
        }
    });
    // Bind the binding server on port 0, then reuse the port on the other IPs.
    let first = serve(
        "127.0.0.1:0",
        Arc::clone(&config),
        Arc::clone(&log_binding),
        binding_routes.clone(),
    );
    port_slot.store(first, std::sync::atomic::Ordering::Relaxed);
    serve(
        &format!("127.0.0.2:{first}"),
        Arc::clone(&config),
        Arc::clone(&log_sub),
        Arc::new(|_path: &str| {
            (
                200,
                vec!["Content-Type: text/html".to_string()],
                "<html><body>sub</body></html>".to_string(),
            )
        }),
    );
    serve(
        &format!("127.0.0.3:{first}"),
        Arc::clone(&config),
        Arc::clone(&log_evil),
        Arc::new(|_path: &str| {
            (
                200,
                vec!["Content-Type: text/html".to_string()],
                "<html><body>evil</body></html>".to_string(),
            )
        }),
    );
    Fixture {
        port: first,
        log_binding,
        log_sub,
        log_evil,
    }
}

fn profile_config(port: u16, allow_subdomains: bool) -> Config {
    let mut config = Config::default();
    config.auth.profiles.insert(
        "site".to_string(),
        AuthProfileConfig {
            kind: AuthKind::Session,
            host: BOUND.to_string(),
            port,
            login_url: Some(format!("https://{BOUND}/login")),
            login_origins: vec![BOUND.to_string()],
            verify_url: Some(format!("https://{BOUND}/verify")),
            allow_subdomains,
            session_max_age_days: Some(7),
            ..Default::default()
        },
    );
    config
}

fn cookie(name: &str, value: &str, domain: &str) -> CapturedCookie {
    CapturedCookie {
        name: name.to_string(),
        value: zeroize::Zeroizing::new(value.to_string()),
        domain: domain.to_string(),
        path: "/".to_string(),
        secure: true,
        http_only: false,
        same_site: None,
        expires: Some(now() + 3600),
    }
}

fn gate(fixture: &Fixture, allow_subdomains: bool, cookies: Vec<CapturedCookie>) -> AuthGate {
    let Some(chrome) = common_chrome() else {
        panic!("this evidence test needs a Chrome binary");
    };
    let config = profile_config(fixture.port, allow_subdomains);
    let mut gate =
        AuthGate::from_config(&config, "site", BOUND, fixture.port, cookies).expect("gate");
    let data = std::env::temp_dir().join(format!(
        "ww-gate-data-{}-{}",
        std::process::id(),
        fixture.port
    ));
    let _ = std::fs::remove_dir_all(&data);
    gate = gate.with_browser(chrome, data);
    gate.host_map = [
        (BOUND.to_string(), "127.0.0.1".to_string()),
        (SUB.to_string(), "127.0.0.2".to_string()),
        (EVIL.to_string(), "127.0.0.3".to_string()),
    ]
    .into_iter()
    .collect();
    gate.launch_args = platform_args();
    gate.launch_args
        .push("--ignore-certificate-errors".to_string());
    gate.launch_args.push(format!(
        "--host-resolver-rules=MAP {BOUND} 127.0.0.1, MAP {SUB} 127.0.0.2, MAP {EVIL} 127.0.0.3"
    ));
    gate
}

fn platform_args() -> Vec<String> {
    Vec::new()
}

fn common_chrome() -> Option<std::path::PathBuf> {
    if let Ok(path) = std::env::var("WEB_WIDOW_CHROME") {
        let path = std::path::PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

#[test]
fn finding2_a_subdomain_is_reachable_but_never_credential_bearing() {
    let fixture = start();
    let Some(_) = common_chrome() else {
        eprintln!("SKIP finding2: no WEB_WIDOW_CHROME");
        return;
    };
    // A valid binding cookie plus a hostile **domain-wide** jar entry.
    let gate = gate(
        &fixture,
        true,
        vec![
            cookie("ww", "abc", BOUND),
            cookie("parent", "leak", ".bound.test"),
        ],
    );
    let sub = format!("https://{SUB}:{}/sub", fixture.port);
    let page = gate
        .render(&url(&sub))
        .expect("the subdomain is reachable under allow_subdomains");
    println!(
        "EVIDENCE finding2 subdomain: status={} injected={} dropped={:?} denials={:?}",
        page.page.status, page.injected_cookies, page.dropped_cookies, page.denials
    );
    assert_eq!(page.page.status, 200);
    assert_eq!(
        page.injected_cookies, 1,
        "only the exact-host cookie is injected"
    );
    assert_eq!(page.dropped_cookies, vec!["parent".to_string()]);
    let sub_hits = fixture.log_sub.lock().unwrap().clone();
    assert!(!sub_hits.is_empty(), "the subdomain was reached");
    assert!(
        sub_hits.iter().all(|hit| hit.cookie.is_none()),
        "no cookie may reach a subdomain: {sub_hits:?}"
    );
    // The binding origin does receive it.
    let bound = format!("https://{BOUND}:{}/", fixture.port);
    let page = gate.render(&url(&bound)).expect("the bound origin renders");
    assert_eq!(page.injected_cookies, 1);
    let bound_hits = fixture.log_binding.lock().unwrap().clone();
    assert!(
        bound_hits
            .iter()
            .any(|hit| hit.cookie.as_deref() == Some("ww=abc")),
        "the binding origin receives the exact-host cookie: {bound_hits:?}"
    );
}

#[test]
fn p6_an_http_challenge_is_a_typed_refusal_and_is_never_answered() {
    let fixture = start();
    let Some(_) = common_chrome() else {
        eprintln!("SKIP p6: no WEB_WIDOW_CHROME");
        return;
    };
    let gate = gate(&fixture, false, vec![cookie("ww", "abc", BOUND)]);
    let error = gate
        .render(&url(&format!("https://{BOUND}:{}/challenge", fixture.port)))
        .expect_err("a challenge must refuse");
    println!(
        "EVIDENCE challenge outcome: code={} message={error}",
        error.code().as_str()
    );
    assert_eq!(error.code().as_str(), "auth_challenge_refused");
    let hits = fixture.log_binding.lock().unwrap().clone();
    println!("EVIDENCE challenge requests: {hits:?}");
    assert!(
        !hits.is_empty() && hits.len() <= 2,
        "a bounded number of attempts: {hits:?}"
    );
    assert!(
        hits.iter().all(|hit| !hit.authorization),
        "no Authorization header may ever be sent"
    );
    assert!(
        hits.iter().all(|hit| hit.path == "/challenge"),
        "every attempt addresses the challenged document: {hits:?}"
    );
}

#[test]
fn the_gate_refuses_an_undeclared_host_before_the_network() {
    let fixture = start();
    let Some(_) = common_chrome() else {
        eprintln!("SKIP egress: no WEB_WIDOW_CHROME");
        return;
    };
    let gate = gate(&fixture, false, vec![cookie("ww", "abc", BOUND)]);
    let error = gate
        .render(&url(&format!("https://{EVIL}:{}/", fixture.port)))
        .expect_err("an undeclared host is refused");
    assert_eq!(error.code().as_str(), "egress_blocked");
    assert!(
        fixture.log_evil.lock().unwrap().is_empty(),
        "the undeclared host must never be contacted"
    );
}

#[test]
fn each_render_uses_a_fresh_context_so_page_cookies_do_not_persist() {
    let fixture = start();
    let Some(_) = common_chrome() else {
        eprintln!("SKIP ephemeral: no WEB_WIDOW_CHROME");
        return;
    };
    let gate = gate(&fixture, false, vec![cookie("ww", "abc", BOUND)]);
    let bound = format!("https://{BOUND}:{}/", fixture.port);
    gate.render(&url(&bound)).expect("first render");
    // A fresh context per fetch: the next render's first (document) request must
    // not carry the cookie the previous page set for itself.
    fixture.log_binding.lock().unwrap().clear();
    gate.render(&url(&bound)).expect("second render");
    let hits = fixture.log_binding.lock().unwrap().clone();
    assert!(!hits.is_empty());
    assert_eq!(
        hits[0].cookie.as_deref(),
        Some("ww=abc"),
        "no page-set cookie survives into the next context: {hits:?}"
    );
}

fn url(raw: &str) -> url::Url {
    url::Url::parse(raw).expect("url")
}

#[test]
fn a_websocket_handshake_is_blocked_at_the_browser_network_layer() {
    let fixture = start();
    let Some(_) = common_chrome() else {
        eprintln!("SKIP websocket: no WEB_WIDOW_CHROME");
        return;
    };
    let gate = gate(&fixture, false, vec![cookie("ww", "abc", BOUND)]);
    let page = gate
        .render(&url(&format!("https://{BOUND}:{}/ws", fixture.port)))
        .expect("the page renders");
    assert_eq!(page.page.status, 200);
    // The Fetch domain cannot see a WebSocket handshake; the network-layer block
    // must stop it before the undeclared host is contacted at all.
    std::thread::sleep(std::time::Duration::from_millis(400));
    let evil = fixture.log_evil.lock().unwrap().clone();
    println!("EVIDENCE evil hits after ws page: {evil:?}");
    assert!(
        evil.is_empty(),
        "the ws handshake must never reach the undeclared host"
    );
}

#[test]
fn the_gate_policy_comes_from_the_shipped_predicate() {
    // Guard against a test that silently measures a different rule.
    let config = profile_config(8443, true);
    let gate = AuthGate::from_config(
        &config,
        "site",
        BOUND,
        8443,
        vec![cookie("ww", "abc", BOUND)],
    )
    .unwrap();
    let policy: &AuthEgressPolicy = &gate.policy;
    assert!(matches!(
        policy.check(&url(&format!("https://{SUB}:8443/"))),
        web_widow::auth::egress::EgressDecision::Allow
    ));
    assert!(matches!(
        policy.may_bear_credential(&url(&format!("https://{SUB}:8443/"))),
        web_widow::auth::egress::EgressDecision::Block(
            web_widow::auth::egress::EgressDenial::Credential
        )
    ));
    let _ = json!({});
}

/// Measure the WebRTC launch flag against a real UDP canary.
///
/// The canary is a loopback UDP socket used as a STUN server. A **control**
/// browser (same page, no gate) proves the canary can fire on this host; the
/// gate runs the same page with
/// `--force-webrtc-ip-handling-policy=disable_non_proxied_udp`.
///
/// **Measured on Chrome 156.0.8078.4 (headless):** the `force-` spelling of the
/// flag is a no-op (4 datagrams), while
/// `--webrtc-ip-handling-policy=disable_non_proxied_udp` (no prefix) suppresses
/// the datagram with `RTCPeerConnection` still present. The gate uses the
/// working spelling; this test requires **zero** datagrams through the gate and
/// a firing control.
#[test]
fn c3_webrtc_cannot_send_non_proxied_udp() {
    let fixture = start();
    let Some(_) = common_chrome() else {
        eprintln!("SKIP c3: no WEB_WIDOW_CHROME");
        return;
    };
    // One socket per phase: a STUN retransmission from the control must not be
    // counted against the gate.
    let (control_canary, control_port) = canary();
    let page = format!(
        "https://{BOUND}:{}/webrtc?port={control_port}",
        fixture.port
    );

    // Control: a plain pipe browser, same page, no gate and no flag.
    let dir = tempfile::tempdir().unwrap();
    let control = web_widow::auth::pipe::Browser::launch(
        &web_widow::auth::pipe::LaunchOptions {
            chrome: common_chrome().unwrap(),
            profile: dir.path().join("profile"),
            stderr_log: dir.path().join("stderr.log"),
            extra_args: vec![
                "--headless=new".to_string(),
                "--disable-gpu".to_string(),
                "--ignore-certificate-errors".to_string(),
                format!(
                    "--host-resolver-rules=MAP {BOUND} 127.0.0.1, MAP {SUB} 127.0.0.2, MAP {EVIL} 127.0.0.3"
                ),
            ],
        },
        Arc::new(|_, _| {}),
    )
    .expect("control browser");
    let target = control.create_target(&page).expect("target");
    let _ = control.attach(&target);
    std::thread::sleep(std::time::Duration::from_millis(1_800));
    let control_packets = drain(&control_canary);
    control.shutdown();
    println!("EVIDENCE c3 control packets: {control_packets}");
    assert!(
        control_packets > 0,
        "the UDP canary must be able to fire, or the measurement is vacuous"
    );

    // Gate: the same page through the authenticated browser, on a fresh canary.
    // Loopback is the address that leaked before the fix; it is also the hardest
    // case for the IP-handling policy.
    let (gate_canary, gate_port) = canary();
    let gate_page = format!(
        "https://{BOUND}:{}/webrtc?port={gate_port}&addr=127.0.0.1",
        fixture.port
    );
    let gate = gate(&fixture, false, vec![cookie("ww", "abc", BOUND)]);
    let _ = gate.render(&url(&gate_page));
    std::thread::sleep(std::time::Duration::from_millis(1_800));
    let gate_packets = drain(&gate_canary);
    println!("EVIDENCE c3 gate packets: {gate_packets}");
    assert_eq!(
        gate_packets, 0,
        "the gate must emit no non-proxied UDP (control: {control_packets})"
    );
}

fn canary() -> (std::net::UdpSocket, u16) {
    canary_on("127.0.0.1")
}

fn canary_on(address: &str) -> (std::net::UdpSocket, u16) {
    let socket = std::net::UdpSocket::bind(format!("{address}:0")).expect("udp canary");
    socket
        .set_read_timeout(Some(std::time::Duration::from_millis(300)))
        .unwrap();
    let port = socket.local_addr().unwrap().port();
    (socket, port)
}

fn drain(socket: &std::net::UdpSocket) -> usize {
    let mut buffer = [0u8; 2048];
    let mut packets = 0;
    while socket.recv_from(&mut buffer).is_ok() {
        packets += 1;
    }
    packets
}

/// The WebRTC mitigation probe (ignored by default; run explicitly).
///
/// For each candidate launch switch, load the canary page in a plain pipe
/// browser and measure (a) whether `RTCPeerConnection` still exists and (b) how
/// many UDP datagrams reach the canary. A switch only "works" when the API is
/// still present and the datagrams stop — otherwise it merely removes the API
/// (which is also a mitigation, but a different one).
#[test]
#[ignore = "WebRTC mitigation probe: launch one browser per candidate switch"]
fn c3_option_matrix_probe() {
    let fixture = start();
    let Some(chrome) = common_chrome() else {
        eprintln!("SKIP c3 probe: no WEB_WIDOW_CHROME");
        return;
    };
    let base = vec![
        "--headless=new".to_string(),
        "--disable-gpu".to_string(),
        "--ignore-certificate-errors".to_string(),
        format!(
            "--host-resolver-rules=MAP {BOUND} 127.0.0.1, MAP {SUB} 127.0.0.2, MAP {EVIL} 127.0.0.3"
        ),
    ];
    let options: Vec<(&str, Vec<&str>)> = vec![
        ("baseline", vec![]),
        (
            "force-ip-handling=disable_non_proxied_udp",
            vec!["--force-webrtc-ip-handling-policy=disable_non_proxied_udp"],
        ),
        (
            "webrtc-ip-handling=disable_non_proxied_udp",
            vec!["--webrtc-ip-handling-policy=disable_non_proxied_udp"],
        ),
        ("disable-features=WebRTC", vec!["--disable-features=WebRTC"]),
        (
            "disable-blink-features=WebRTC",
            vec!["--disable-blink-features=WebRTC"],
        ),
        (
            "disable-blink-features=RTCPeerConnection",
            vec!["--disable-blink-features=RTCPeerConnection"],
        ),
        (
            "disable-features=WebRTC+flag",
            vec![
                "--disable-features=WebRTC",
                "--force-webrtc-ip-handling-policy=disable_non_proxied_udp",
            ],
        ),
        (
            "webrtc-udp-port-range=0-0",
            vec!["--webrtc-udp-port-range=0-0"],
        ),
        (
            "webrtc-udp-min/max-port=0",
            vec!["--webrtc-udp-min-port=0", "--webrtc-udp-max-port=0"],
        ),
    ];
    for (label, extra) in options {
        let (canary_socket, canary_port) = canary();
        let page = format!(
            "https://{BOUND}:{}/webrtc?port={canary_port}&addr=127.0.0.1",
            fixture.port
        );
        let dir = tempfile::tempdir().unwrap();
        let mut args = base.clone();
        args.extend(extra.iter().map(|value| value.to_string()));
        let Ok(browser) = web_widow::auth::pipe::Browser::launch(
            &web_widow::auth::pipe::LaunchOptions {
                chrome: chrome.clone(),
                profile: dir.path().join("profile"),
                stderr_log: dir.path().join("stderr.log"),
                extra_args: args,
            },
            Arc::new(|_, _| {}),
        ) else {
            println!("OPTION {label} launch=failed");
            continue;
        };
        let target = match browser.create_target(&page) {
            Ok(target) => target,
            Err(error) => {
                println!("OPTION {label} target=failed {error}");
                browser.shutdown();
                continue;
            }
        };
        let session = browser.attach(&target).unwrap_or_default();
        let _ = browser.enable_page(&session);
        std::thread::sleep(std::time::Duration::from_millis(1_800));
        let packets = drain(&canary_socket);
        let page_state = browser
            .evaluate(
                &session,
                "JSON.stringify({rtcp: typeof RTCPeerConnection, created: window.__ww_pc === 1})",
            )
            .ok()
            .and_then(|value| {
                value
                    .get("value")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "unknown".to_string());
        println!("OPTION {label} datagrams={packets} page={page_state}");
        browser.shutdown();
    }
}
