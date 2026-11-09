use base64::{engine::general_purpose::STANDARD, Engine as _};
use rumqttc::{
    AsyncClient, ConnectReturnCode, Event, MqttOptions, Packet, QoS, TlsConfiguration, Transport,
};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use serde::Serialize;
use socket2::{Domain, Protocol, Socket, Type};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State as TauriState};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{MappedMutexGuard, Mutex, MutexGuard},
    task::AbortHandle,
};

mod ftp;
use ftp::FileEntry;

// ── TLS: accept any certificate (Bambu printers use self-signed) ─────────────

#[derive(Debug)]
struct SkipVerifier;

impl ServerCertVerifier for SkipVerifier {
    fn verify_server_cert(
        &self,
        _: &CertificateDer,
        _: &[CertificateDer],
        _: &ServerName,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
        ]
    }
}

pub(crate) fn lan_tls_config() -> Arc<rustls::ClientConfig> {
    Arc::new(
        rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(SkipVerifier))
            .with_no_client_auth(),
    )
}

// ── Printer status types (mirrored in vite-env.d.ts) ─────────────────────────

#[derive(Debug, Clone, Serialize, Default)]
pub struct PrinterStatus {
    pub nozzle_temp: f64,
    pub nozzle_target: f64,
    pub bed_temp: f64,
    pub bed_target: f64,
    pub progress: u8,
    pub remaining_mins: u32,
    pub layer_num: u32,
    pub total_layer_num: u32,
    pub stage: String,
    pub gcode_state: String,
    pub ams: Vec<AmsUnit>,
    pub vt_tray: Option<AmsTray>,
    pub chamber_light: bool,
    pub spd_lvl: u8,
    pub subtask_name: String,
    pub task_id: String,
    pub hms: Vec<String>,
    pub device_name: String,
    /// Global slot ID of the tray currently loaded in the nozzle.
    /// 255 = nothing loaded, 254 = external spool, 0-15 = AMS slots.
    pub tray_now: u8,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct AmsUnit {
    pub id: u8,
    pub humidity: u8,
    pub trays: Vec<AmsTray>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AmsTray {
    pub id: u8,
    pub tray_type: String,
    pub color: String,
    pub name: String,
    /// Default print temperature for this filament (°C). Sent in filament-change commands.
    pub tray_temp: u16,
}

impl Default for AmsTray {
    fn default() -> Self {
        AmsTray {
            id: 0,
            tray_type: String::new(),
            color: String::new(),
            name: String::new(),
            tray_temp: 210, // OrcaSlicer default
        }
    }
}

// ── Android notification plugin handle (mobile only) ─────────────────────────
//
// On Android the foreground-service notification must be driven from Rust so
// it keeps updating even when Android suspends WebView JavaScript execution
// (which happens whenever the screen turns off / app is backgrounded).
//
// `api.register_android_plugin` (called in the plugin setup below) registers
// the Kotlin PrintNotificationPlugin and returns a PluginHandle we can call
// directly — bypassing the JS bridge entirely.

/// Managed state: handle to the Android PrintNotificationPlugin.
/// Stored via `app.manage()` in the plugin setup; retrieved later with
/// `app.try_state::<PrintNotifHandle>()` inside the MQTT task.
#[cfg(mobile)]
pub(crate) struct PrintNotifHandle(pub tauri::plugin::PluginHandle<tauri::Wry>);

/// Payload for startNotification / updateNotification commands.
#[cfg(mobile)]
#[derive(Serialize)]
struct NotifArgs {
    title: String,
    body: String,
    /// Print progress 0-100.  The Kotlin side uses this to drive a native
    /// progress bar on the notification.  0 means "no bar" (pre-print stages).
    progress: u8,
}

/// Formats layer / progress / ETA into a compact notification body string.
/// Mirrors the `buildNotifBody` function in PrinterContext.tsx.
#[cfg(mobile)]
fn build_notif_body_rs(s: &PrinterStatus) -> String {
    let mut parts: Vec<String> = Vec::new();
    if s.total_layer_num > 0 {
        parts.push(format!("Layer {}/{}", s.layer_num, s.total_layer_num));
    }
    if s.progress > 0 {
        parts.push(format!("{}%", s.progress));
    }
    if s.remaining_mins > 0 {
        let h = s.remaining_mins / 60;
        let m = s.remaining_mins % 60;
        if h > 0 {
            parts.push(if m > 0 {
                format!("{}h {}m left", h, m)
            } else {
                format!("{}h left", h)
            });
        } else {
            parts.push(format!("{}m left", m));
        }
    }
    parts.join(" · ")
}

/// Drives the Android foreground-service notification entirely from Rust.
///
/// Call on every MQTT status update.  Handles RUNNING / PAUSE / stop
/// transitions without going through the JavaScript layer, so the notification
/// continues to update even when the WebView is suspended in the background.
///
/// Rate-limiting: updates fire on every layer change, or at least every 10 s
/// (the heartbeat also serves to re-post the notification after Android 14+
/// users swipe it away — it reappears within 10 s).
#[cfg(mobile)]
fn drive_notification(
    handle: &tauri::plugin::PluginHandle<tauri::Wry>,
    st: &PrinterStatus,
    prev_gcode_state: &mut String,
    prev_layer: &mut u32,
    last_notif_at: &mut std::time::Instant,
) {
    let next = st.gcode_state.as_str();
    let prev = prev_gcode_state.as_str();
    let layer_changed = st.layer_num != *prev_layer;
    let job_name = if st.subtask_name.is_empty() {
        "Print in progress"
    } else {
        &st.subtask_name
    };

    match next {
        "RUNNING" => {
            let body = build_notif_body_rs(st);
            let now = std::time::Instant::now();
            if prev != "RUNNING" {
                // Just started (or resumed from pause) — start the service.
                let _ = handle.run_mobile_plugin::<serde_json::Value>(
                    "startNotification",
                    NotifArgs {
                        title: format!("Printing: {}", job_name),
                        body,
                        progress: st.progress,
                    },
                );
                *last_notif_at = now;
            } else if layer_changed || now.duration_since(*last_notif_at).as_millis() >= 10_000 {
                // New layer or 10-second heartbeat — update / re-post.
                let _ = handle.run_mobile_plugin::<serde_json::Value>(
                    "updateNotification",
                    NotifArgs {
                        title: format!("Printing: {}", job_name),
                        body,
                        progress: st.progress,
                    },
                );
                *last_notif_at = now;
            }
        }
        "PAUSE" => {
            let body = build_notif_body_rs(st);
            let body = if body.is_empty() {
                format!("{}%", st.progress)
            } else {
                body
            };
            let now = std::time::Instant::now();
            if prev != "PAUSE" {
                // Freshly paused — update title.
                let _ = handle.run_mobile_plugin::<serde_json::Value>(
                    "updateNotification",
                    NotifArgs {
                        title: format!("Paused: {}", job_name),
                        body,
                        progress: st.progress,
                    },
                );
                *last_notif_at = now;
            } else if now.duration_since(*last_notif_at).as_millis() >= 10_000 {
                // Heartbeat while paused.
                let _ = handle.run_mobile_plugin::<serde_json::Value>(
                    "updateNotification",
                    NotifArgs {
                        title: format!("Paused: {}", job_name),
                        body,
                        progress: st.progress,
                    },
                );
                *last_notif_at = now;
            }
        }
        _ => {
            if prev == "RUNNING" || prev == "PAUSE" {
                // Print ended (FINISH, FAILED, …) — stop the foreground service.
                let _ = handle.run_mobile_plugin::<serde_json::Value>(
                    "stopNotification",
                    serde_json::json!({}),
                );
            }
        }
    }

    *prev_gcode_state = next.to_owned();
    *prev_layer = st.layer_num;
}

// ── App state ─────────────────────────────────────────────────────────────────

pub(crate) struct AppState {
    pub(crate) connection: Mutex<Option<ConnectionHandles>>,
    // Kept open across commands so TCP, TLS and login are paid once.
    pub(crate) ftp: Mutex<Option<ftp::Ftp>>,
}

struct ConnectionHandles {
    ip: String,
    access_code: String,
    serial: String,
    mqtt_client: AsyncClient,
    abort_handles: Vec<AbortHandle>,
    status: Arc<Mutex<PrinterStatus>>,
}

// ── MQTT payload parsing ──────────────────────────────────────────────────────

fn parse_status(payload: &[u8], status: &mut PrinterStatus) {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(payload) else {
        return;
    };

    // device_name is set via SSDP discovery (DevName.bambu.com), not MQTT.

    let Some(p) = v.get("print") else { return };

    macro_rules! f64_field {
        ($dst:expr, $key:expr) => {
            if let Some(n) = p.get($key).and_then(|v| {
                v.as_f64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            }) {
                $dst = n;
            }
        };
    }
    macro_rules! u64_field {
        ($dst:expr, $key:expr) => {
            if let Some(n) = p.get($key).and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            }) {
                $dst = n as _;
            }
        };
    }
    macro_rules! str_field {
        ($dst:expr, $key:expr) => {
            if let Some(v) = p.get($key) {
                if let Some(s) = v.as_str() {
                    $dst = s.to_owned();
                } else if let Some(n) = v.as_u64() {
                    $dst = n.to_string();
                } else if let Some(n) = v.as_i64() {
                    $dst = n.to_string();
                }
            }
        };
    }

    f64_field!(status.nozzle_temp, "nozzle_temper");
    f64_field!(status.nozzle_target, "nozzle_target_temper");
    f64_field!(status.bed_temp, "bed_temper");
    f64_field!(status.bed_target, "bed_target_temper");
    u64_field!(status.progress, "mc_percent");
    u64_field!(status.remaining_mins, "mc_remaining_time");
    u64_field!(status.layer_num, "layer_num");
    u64_field!(status.total_layer_num, "total_layer_num");
    u64_field!(status.spd_lvl, "spd_lvl");
    str_field!(status.gcode_state, "gcode_state");
    str_field!(status.subtask_name, "subtask_name");
    str_field!(status.task_id, "task_id");

    if let Some(n) = p.get("stg_cur").and_then(|v| v.as_u64()) {
        status.stage = stage_name(n);
    }

    if let Some(lights) = p.get("lights_report").and_then(|v| v.as_array()) {
        for light in lights {
            if light.get("node").and_then(|v| v.as_str()) == Some("chamber_light") {
                status.chamber_light = light.get("mode").and_then(|v| v.as_str()) == Some("on");
            }
        }
    }

    // Bambu firmware sends numeric IDs as JSON strings ("0", "1", …).
    // parse_u8 accepts both forms and is reused for AMS and vt_tray parsing.
    let parse_u8 = |v: &serde_json::Value| -> u8 {
        v.as_u64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            .unwrap_or(0) as u8
    };

    if let Some(ams_obj) = p.get("ams") {
        // tray_now: global slot ID of currently-loaded tray. "255" = nothing loaded.
        if let Some(tn) = ams_obj.get("tray_now") {
            status.tray_now = parse_u8(tn);
        }

        if let Some(ams_arr) = ams_obj.get("ams").and_then(|v| v.as_array()) {
            status.ams = ams_arr
                .iter()
                .filter_map(|unit| {
                    let id = parse_u8(unit.get("id")?);
                    let humidity = unit.get("humidity").map(parse_u8).unwrap_or(0);
                    let trays = unit
                        .get("tray")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .map(|t| {
                                    let id =
                                        parse_u8(t.get("id").unwrap_or(&serde_json::Value::Null));
                                    let tray_type = t
                                        .get("tray_type")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_owned();
                                    let raw = t
                                        .get("tray_color")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("3F3F4600");
                                    // colour is RRGGBBAA; we only need RRGGBB
                                    let color = raw[..raw.len().min(6)].to_owned();
                                    let name = t
                                        .get("tray_sub_brands")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_owned();
                                    // tray_temp is "220" etc. — may be absent for empty slots.
                                    let tray_temp: u16 = t
                                        .get("tray_temp")
                                        .and_then(|v| {
                                            v.as_u64()
                                                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                                        })
                                        .unwrap_or(220) as u16;
                                    AmsTray {
                                        id,
                                        tray_type,
                                        color,
                                        name,
                                        tray_temp,
                                    }
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    Some(AmsUnit {
                        id,
                        humidity,
                        trays,
                    })
                })
                .collect();
        }
    }

    // External spool (mounted outside AMS)
    if let Some(vt) = p.get("vt_tray") {
        let tray_type = vt
            .get("tray_type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        status.vt_tray = if !tray_type.is_empty() {
            let id = parse_u8(vt.get("id").unwrap_or(&serde_json::Value::Null));
            let raw = vt
                .get("tray_color")
                .and_then(|v| v.as_str())
                .unwrap_or("3F3F4600");
            let color = raw[..raw.len().min(6)].to_owned();
            let name = vt
                .get("tray_sub_brands")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            let tray_temp: u16 = vt
                .get("tray_temp")
                .and_then(|v| {
                    v.as_u64()
                        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                })
                .unwrap_or(220) as u16;
            Some(AmsTray {
                id,
                tray_type,
                color,
                name,
                tray_temp,
            })
        } else {
            None
        };
    }

    // HMS error codes — each entry has `attr` and `code` (both u32 hex strings).
    // Combine them into the 8-char key used in errors.json, e.g. "03008001".
    // Only update hms when the key is present. Bambu sends incremental updates
    // so most messages omit `hms` entirely — clearing on absence would cause
    // errors to disappear after a single tick.
    if let Some(hms_val) = p.get("hms") {
        status.hms = hms_val
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|entry| {
                        let attr = entry.get("attr").and_then(|v| v.as_u64())?;
                        let code = entry.get("code").and_then(|v| v.as_u64())?;
                        let s = format!("{:08X}{:08X}", attr, code);
                        Some(format!(
                            "{}-{}-{}-{}",
                            &s[0..4],
                            &s[4..8],
                            &s[8..12],
                            &s[12..16]
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
    }
}

fn stage_name(stg: u64) -> String {
    match stg {
        0 => "Idle",
        1 => "Auto bed leveling",
        2 => "Heatbed preheating",
        4 => "Changing filament",
        7 => "Heating hotend",
        9 => "Scanning bed surface",
        10 => "Inspecting first layer",
        13 => "Homing toolhead",
        14 => "Cleaning nozzle",
        17 => "Printing",
        20 => "Paused by user",
        22 => "Filament unloading",
        24 => "Filament loading",
        _ => "Working",
    }
    .to_owned()
}

// ── SSDP: retrieve DevName.bambu.com via unicast M-SEARCH + multicast NOTIFY ──
//
// Unicast M-SEARCH (primary): sent directly to printer_ip:1900 — works over
// Tailscale subnet routes and any network where the IP is reachable.
// Multicast NOTIFY (bonus): listen on 239.255.255.250:1900 for the printer's
// periodic 5-second broadcasts — local network only.

// ── UPnP device description via plain HTTP on port 80 ────────────────────────
// The printer's SSDP Location header points to http://<ip>/ which serves a
// standard UPnP XML description containing <friendlyName> — the user-set name.
// Plain TCP to port 80 routes fine over Tailscale or any VPN.

async fn fetch_upnp_name(ip: &str, app: &AppHandle) -> Option<String> {
    let _ = app.emit("ssdp-debug", format!("[upnp] trying http://{}:80/", ip));

    let mut stream = match tokio::time::timeout(
        std::time::Duration::from_secs(3),
        tokio::net::TcpStream::connect(format!("{}:80", ip)),
    )
    .await
    {
        Ok(Ok(s)) => s,
        _ => {
            let _ = app.emit(
                "ssdp-debug",
                format!("[upnp] port 80 not reachable on {}", ip),
            );
            return None;
        }
    };

    let req = format!("GET / HTTP/1.0\r\nHost: {}\r\n\r\n", ip);
    if stream.write_all(req.as_bytes()).await.is_err() {
        return None;
    }

    let mut buf = Vec::new();
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        stream.read_to_end(&mut buf),
    )
    .await;

    let text = String::from_utf8_lossy(&buf);
    let _ = app.emit(
        "ssdp-debug",
        format!(
            "[upnp] response ({} bytes): {}",
            buf.len(),
            &text[..text.len().min(400)]
        ),
    );

    // UPnP XML: <friendlyName>My P1S</friendlyName>
    if let Some(start) = text.find("<friendlyName>") {
        let rest = &text[start + "<friendlyName>".len()..];
        if let Some(end) = rest.find("</friendlyName>") {
            let name = rest[..end].trim().to_string();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }

    None
}

fn ssdp_extract_name(msg: &str, serial_upper: &str) -> Option<String> {
    // Accept the packet if it mentions our serial (case-insensitive) OR if serial
    // is empty. This handles the rare case of a misconfigured serial entry.
    if !serial_upper.is_empty() && !msg.to_uppercase().contains(serial_upper) {
        return None;
    }
    for line in msg.lines() {
        if let Some(name) = line.trim().strip_prefix("DevName.bambu.com:") {
            let name = name.trim();
            if !name.is_empty() {
                return Some(name.to_owned());
            }
        }
    }
    None
}

async fn ssdp_apply(name: String, status: &Arc<Mutex<PrinterStatus>>, app: &AppHandle) {
    let mut st = status.lock().await;
    if st.device_name != name {
        st.device_name = name.clone();
        let _ = app.emit("printer-name", &name);
    }
}

async fn ssdp_name_loop(
    ip: String,
    serial: String,
    status: Arc<Mutex<PrinterStatus>>,
    app: AppHandle,
) {
    let serial_upper = serial.to_uppercase();
    let msearch = concat!(
        "M-SEARCH * HTTP/1.1\r\n",
        "HOST: 239.255.255.250:1900\r\n",
        "MAN: \"ssdp:discover\"\r\n",
        "MX: 1\r\n",
        "ST: urn:bambulab-com:device:3dprinter:1\r\n",
        "\r\n",
    );

    let _ = app.emit("ssdp-debug", format!("[ssdp] starting for printer {}", ip));

    // Multicast listener on :1900
    {
        let su = serial_upper.clone();
        let st = status.clone();
        let ap = app.clone();
        tokio::spawn(async move {
            ssdp_multicast_listen(su, st, ap).await;
        });
    }
    // Listener on port 2021 (Bambu direct broadcast port)
    {
        let su = serial_upper.clone();
        let st = status.clone();
        let ap = app.clone();
        tokio::spawn(async move {
            ssdp_port_listen(2021, su, st, ap).await;
        });
    }

    // Main discovery loop — runs every 30 s, tries all unicast methods.
    let targets = [
        "239.255.255.250:1900".to_string(), // multicast — standard SSDP, works on LAN
        format!("{}:1900", ip),             // unicast to printer — works over Tailscale
        format!("{}:2021", ip),             // unicast on Bambu's alternate port
    ];
    loop {
        // UPnP HTTP — plain TCP, works over Tailscale
        if let Some(name) = fetch_upnp_name(&ip, &app).await {
            ssdp_apply(name, &status, &app).await;
        }

        // SSDP M-SEARCH
        if let Ok(sock) = tokio::net::UdpSocket::bind("0.0.0.0:0").await {
            for target in &targets {
                match sock.send_to(msearch.as_bytes(), target).await {
                    Err(e) => {
                        let _ = app.emit(
                            "ssdp-debug",
                            format!("[msearch] send to {} failed: {}", target, e),
                        );
                    }
                    Ok(_) => {
                        let _ = app.emit("ssdp-debug", format!("[msearch] sent to {}", target));
                    }
                }
            }
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            let mut buf = vec![0u8; 2048];
            loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                match tokio::time::timeout(remaining, sock.recv_from(&mut buf)).await {
                    Ok(Ok((len, src))) => {
                        let msg = String::from_utf8_lossy(&buf[..len]).to_string();
                        let _ = app.emit(
                            "ssdp-debug",
                            format!(
                                "[msearch] response from {}: {}",
                                src,
                                &msg[..msg.len().min(300)]
                            ),
                        );
                        if let Some(name) = ssdp_extract_name(&msg, &serial_upper) {
                            ssdp_apply(name, &status, &app).await;
                        }
                    }
                    _ => break,
                }
            }
        }

        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    }
}

async fn ssdp_port_listen(
    port: u16,
    serial_upper: String,
    status: Arc<Mutex<PrinterStatus>>,
    app: AppHandle,
) {
    use std::net::SocketAddr;
    let std_socket = (|| -> std::io::Result<std::net::UdpSocket> {
        let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        sock.set_reuse_address(true)?;
        #[cfg(unix)]
        sock.set_reuse_port(true)?;
        sock.set_broadcast(true)?;
        sock.bind(&SocketAddr::from(([0, 0, 0, 0], port)).into())?;
        Ok(sock.into())
    })();
    let std_socket = match std_socket {
        Ok(s) => {
            let _ = app.emit("ssdp-debug", format!("[port{}] listening", port));
            s
        }
        Err(e) => {
            let _ = app.emit("ssdp-debug", format!("[port{}] bind failed: {}", port, e));
            return;
        }
    };
    std_socket.set_nonblocking(true).ok();
    let socket = match tokio::net::UdpSocket::from_std(std_socket) {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut buf = vec![0u8; 2048];
    loop {
        let (len, src) = match socket.recv_from(&mut buf).await {
            Ok(r) => r,
            Err(_) => {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                continue;
            }
        };
        let msg = String::from_utf8_lossy(&buf[..len]).to_string();
        let _ = app.emit(
            "ssdp-debug",
            format!(
                "[port{}] packet from {}: {}",
                port,
                src,
                &msg[..msg.len().min(200)]
            ),
        );
        if let Some(name) = ssdp_extract_name(&msg, &serial_upper) {
            ssdp_apply(name, &status, &app).await;
        }
    }
}

async fn ssdp_multicast_listen(
    serial_upper: String,
    status: Arc<Mutex<PrinterStatus>>,
    app: AppHandle,
) {
    use std::net::{Ipv4Addr, SocketAddr};

    let std_socket = (|| -> std::io::Result<std::net::UdpSocket> {
        let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        sock.set_reuse_address(true)?;
        #[cfg(unix)]
        sock.set_reuse_port(true)?;
        sock.bind(&SocketAddr::from(([0, 0, 0, 0], 1900)).into())?;
        Ok(sock.into())
    })();

    let std_socket = match std_socket {
        Ok(s) => s,
        Err(e) => {
            let _ = app.emit(
                "ssdp-debug",
                format!("[multicast] bind :1900 failed: {}", e),
            );
            return;
        }
    };
    std_socket.set_nonblocking(true).ok();
    let multicast = Ipv4Addr::new(239, 255, 255, 250);
    match std_socket.join_multicast_v4(&multicast, &Ipv4Addr::UNSPECIFIED) {
        Ok(_) => {
            let _ = app.emit(
                "ssdp-debug",
                "[multicast] joined 239.255.255.250 on :1900".to_string(),
            );
        }
        Err(e) => {
            let _ = app.emit("ssdp-debug", format!("[multicast] join failed: {}", e));
        }
    }

    let socket = match tokio::net::UdpSocket::from_std(std_socket) {
        Ok(s) => s,
        Err(e) => {
            let _ = app.emit(
                "ssdp-debug",
                format!("[multicast] tokio wrap failed: {}", e),
            );
            return;
        }
    };

    let mut buf = vec![0u8; 2048];
    loop {
        let (len, src) = match socket.recv_from(&mut buf).await {
            Ok(r) => r,
            Err(_) => {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                continue;
            }
        };
        let msg = String::from_utf8_lossy(&buf[..len]).to_string();
        let _ = app.emit(
            "ssdp-debug",
            format!(
                "[multicast] packet from {}: {}",
                src,
                &msg[..msg.len().min(200)]
            ),
        );
        if let Some(name) = ssdp_extract_name(&msg, &serial_upper) {
            ssdp_apply(name, &status, &app).await;
        }
    }
}

// ── Camera: MJPG over TLS on port 6000 ───────────────────────────────────────
//
// Protocol (per OpenBambuAPI/video.md):
//   1. TLS connect to <ip>:6000 (self-signed cert, no verification)
//   2. Send 80-byte auth packet:
//        [0..3]   u32 LE = 0x40        (payload size)
//        [4..7]   u32 LE = 0x3000      (packet type: auth)
//        [8..15]  u64    = 0            (flags + padding)
//        [16..47] 32 bytes: "bblp" NUL-padded
//        [48..79] 32 bytes: access_code NUL-padded
//   3. Loop: read 16-byte frame header (payload_size u32 LE, ...) then JPEG.

async fn camera_loop(ip: String, access_code: String, app: AppHandle) {
    let tls_cfg = Arc::new(
        rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(SkipVerifier))
            .with_no_client_auth(),
    );
    let connector = tokio_rustls::TlsConnector::from(tls_cfg);

    // Derive a ServerName from the IP (no SNI for raw IPs, but verification
    // is disabled anyway so the printer's self-signed cert is accepted).
    let server_name: ServerName<'static> = match ip.parse::<std::net::IpAddr>() {
        Ok(addr) => ServerName::IpAddress(rustls::pki_types::IpAddr::from(addr)),
        Err(_) => match ServerName::try_from(ip.clone()) {
            Ok(n) => n,
            Err(_) => return,
        },
    };

    loop {
        let Ok(stream) = tokio::net::TcpStream::connect(format!("{}:6000", ip)).await else {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            continue;
        };

        let Ok(mut tls) = connector.connect(server_name.clone(), stream).await else {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            continue;
        };

        // Build and send 80-byte auth packet
        let mut auth = [0u8; 80];
        auth[0..4].copy_from_slice(&0x40u32.to_le_bytes());
        auth[4..8].copy_from_slice(&0x3000u32.to_le_bytes());
        let user = b"bblp";
        auth[16..16 + user.len()].copy_from_slice(user);
        let pass = access_code.as_bytes();
        let plen = pass.len().min(32);
        auth[48..48 + plen].copy_from_slice(&pass[..plen]);

        if tls.write_all(&auth).await.is_err() {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            continue;
        }

        // Read frames until error or disconnect
        loop {
            let mut hdr = [0u8; 16];
            if tls.read_exact(&mut hdr).await.is_err() {
                break;
            }

            let payload_size = u32::from_le_bytes(hdr[0..4].try_into().unwrap()) as usize;
            if payload_size == 0 || payload_size > 1_048_576 {
                break;
            }

            let mut frame = vec![0u8; payload_size];
            if tls.read_exact(&mut frame).await.is_err() {
                break;
            }

            // Validate JPEG start-of-image marker
            if payload_size >= 2 && frame[0] == 0xFF && frame[1] == 0xD8 {
                let b64 = STANDARD.encode(&frame);
                let _ = app.emit("camera-frame", b64);
            }
        }

        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

// ── Tauri commands ────────────────────────────────────────────────────────────

// rumqttc drops larger packets and reconnects; full reports from printers such
// as the X2D exceed its default limit.
const MQTT_MAX_PACKET_SIZE: usize = 256 * 1024;

#[tauri::command]
async fn connect_printer(
    ip: String,
    access_code: String,
    serial: String,
    app: AppHandle,
    state: TauriState<'_, AppState>,
) -> Result<(), String> {
    let mut conn = state.connection.lock().await;
    if conn.is_some() {
        return Err("Already connected".into());
    }

    // MQTT with TLS (skip cert verification for Bambu self-signed cert)
    let tls_cfg = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipVerifier))
        .with_no_client_auth();

    let mut opts = MqttOptions::new("bamboo-mobile-lan", &ip, 8883);
    opts.set_credentials("bblp", &access_code);
    opts.set_keep_alive(std::time::Duration::from_secs(10));
    opts.set_max_packet_size(MQTT_MAX_PACKET_SIZE, MQTT_MAX_PACKET_SIZE);
    opts.set_transport(Transport::Tls(TlsConfiguration::Rustls(Arc::new(tls_cfg))));

    let (mqtt_client, mut eventloop) = AsyncClient::new(opts, 64);

    // Poll until CONNACK with 10s timeout
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            match eventloop.poll().await {
                Ok(Event::Incoming(Packet::ConnAck(ack))) => {
                    return if ack.code == ConnectReturnCode::Success {
                        Ok(())
                    } else {
                        Err(format!(
                            "Authentication failed ({:?}) — check access code",
                            ack.code
                        ))
                    };
                }
                Ok(_) => continue,
                Err(e) => return Err(format!("Cannot reach printer: {}", e)),
            }
        }
    })
    .await
    .unwrap_or_else(|_| Err("Timed out — check printer IP and network".to_string()))?;

    // Subscribe and request a full status dump
    let topic = format!("device/{}/report", serial);
    mqtt_client
        .subscribe(&topic, QoS::AtMostOnce)
        .await
        .map_err(|e| e.to_string())?;
    // pushall is sent by the background task once SubAck confirms the subscription,
    // ensuring SUBSCRIBE is always processed by the broker before PUBLISH.

    let status = Arc::new(Mutex::new(PrinterStatus::default()));
    let mut abort_handles: Vec<AbortHandle> = Vec::new();

    // MQTT event loop task
    let status_c = status.clone();
    let app_c = app.clone();
    let mqtt_client_c = mqtt_client.clone();
    let serial_c = serial.clone();
    let handle = tokio::spawn(async move {
        let report_topic = format!("device/{}/report", serial_c);
        let req_topic = format!("device/{}/request", serial_c);
        let pushall = serde_json::json!({
            "pushing": {"sequence_id": "0", "command": "pushall", "version": 1}
        })
        .to_string();
        let get_version = serde_json::json!({
            "info": {"sequence_id": "0", "command": "get_version"}
        })
        .to_string();

        // Notification state — tracked in Rust so the foreground-service
        // notification updates even when WebView JS is suspended in the background.
        #[cfg(mobile)]
        let mut prev_gcode_state_notif = String::new();
        #[cfg(mobile)]
        let mut prev_layer_notif = 0u32;
        // Initialise far in the past so the very first RUNNING/PAUSE update fires.
        #[cfg(mobile)]
        let mut last_notif_at =
            std::time::Instant::now() - std::time::Duration::from_secs(120);

        loop {
            match eventloop.poll().await {
                Ok(Event::Incoming(Packet::ConnAck(_))) => {
                    // Re-subscribe first so the broker always sees SUBSCRIBE before
                    // PUBLISH — some firmware (X1, H2, P2S) enforces this strictly.
                    let _ = mqtt_client_c
                        .subscribe(&report_topic, QoS::AtMostOnce)
                        .await;
                }
                Ok(Event::Incoming(Packet::SubAck(_))) => {
                    // Subscription confirmed — safe to publish. Send both the full
                    // status dump and a get_version to retrieve the device name.
                    let _ = mqtt_client_c
                        .publish(&req_topic, QoS::AtMostOnce, false, pushall.clone())
                        .await;
                    let _ = mqtt_client_c
                        .publish(&req_topic, QoS::AtMostOnce, false, get_version.clone())
                        .await;
                }
                Ok(Event::Incoming(Packet::Publish(msg))) => {
                    // Emit raw payload for the debug page before any parsing.
                    if let Ok(raw) = std::str::from_utf8(&msg.payload) {
                        let _ = app_c.emit("mqtt-raw", raw);
                    }
                    // Parse status and release the lock before any further work
                    // to avoid holding a MutexGuard across the notification calls.
                    let st_snapshot = {
                        let mut st = status_c.lock().await;
                        let prev_name = st.device_name.clone();
                        parse_status(&msg.payload, &mut st);
                        if st.device_name != prev_name && !st.device_name.is_empty() {
                            let _ = app_c.emit("printer-name", &st.device_name);
                        }
                        st.clone()
                    }; // MutexGuard dropped here
                    let _ = app_c.emit("printer-status", &st_snapshot);
                    // Drive the foreground-service notification directly from Rust
                    // so it keeps updating even when the WebView is suspended.
                    #[cfg(mobile)]
                    if let Some(notif) = app_c.try_state::<PrintNotifHandle>() {
                        drive_notification(
                            &notif.0,
                            &st_snapshot,
                            &mut prev_gcode_state_notif,
                            &mut prev_layer_notif,
                            &mut last_notif_at,
                        );
                    }
                }
                Err(_) => {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
                _ => {}
            }
        }
    });
    abort_handles.push(handle.abort_handle());
    drop(handle);

    // Camera: MJPG over TLS on port 6000
    let camera_handle = tokio::spawn(camera_loop(ip.clone(), access_code.clone(), app.clone()));
    abort_handles.push(camera_handle.abort_handle());
    drop(camera_handle);

    // SSDP: unicast M-SEARCH + multicast NOTIFY to get the user-set printer name
    let ssdp_handle = tokio::spawn(ssdp_name_loop(
        ip.clone(),
        serial.clone(),
        status.clone(),
        app.clone(),
    ));
    abort_handles.push(ssdp_handle.abort_handle());
    drop(ssdp_handle);

    *conn = Some(ConnectionHandles {
        ip: ip.clone(),
        access_code: access_code.clone(),
        serial: serial.clone(),
        mqtt_client,
        abort_handles,
        status,
    });

    // Persist credentials to Android SharedPreferences so PrinterForegroundService
    // can reconnect on its own if the process is killed (swipe-away) and restarted
    // by Android's START_STICKY mechanism.
    #[cfg(mobile)]
    if let Some(notif) = app.try_state::<PrintNotifHandle>() {
        let _ = notif.0.run_mobile_plugin::<serde_json::Value>(
            "storeCredentials",
            serde_json::json!({ "ip": ip, "access_code": access_code, "serial": serial }),
        );
    }

    Ok(())
}

#[tauri::command]
async fn disconnect_printer(_app: AppHandle, state: TauriState<'_, AppState>) -> Result<(), String> {
    if let Some(mut session) = state.ftp.lock().await.take() {
        ftp::quit(&mut session).await;
    }

    let mut conn = state.connection.lock().await;
    if let Some(c) = conn.take() {
        for h in c.abort_handles {
            h.abort();
        }
        let _ = c.mqtt_client.disconnect().await;
    }

    // Clear stored credentials so the service doesn't attempt a standalone
    // reconnect — the user explicitly disconnected.
    #[cfg(mobile)]
    if let Some(notif) = _app.try_state::<PrintNotifHandle>() {
        let _ = notif.0.run_mobile_plugin::<serde_json::Value>(
            "clearCredentials",
            serde_json::json!({}),
        );
    }

    Ok(())
}

#[tauri::command]
async fn get_status(state: TauriState<'_, AppState>) -> Result<PrinterStatus, String> {
    let conn = state.connection.lock().await;
    match &*conn {
        Some(c) => Ok(c.status.lock().await.clone()),
        None => Err("Not connected".into()),
    }
}

#[tauri::command]
async fn set_print_speed(level: u8, state: TauriState<'_, AppState>) -> Result<(), String> {
    let conn = state.connection.lock().await;
    let c = conn.as_ref().ok_or("Not connected")?;
    let topic = format!("device/{}/request", c.serial);
    let payload = serde_json::json!({
        "print": {
            "sequence_id": "0",
            "command": "print_speed",
            "param": level.to_string()
        }
    });
    c.mqtt_client
        .publish(&topic, QoS::AtMostOnce, false, payload.to_string())
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn set_chamber_light(on: bool, state: TauriState<'_, AppState>) -> Result<(), String> {
    let conn = state.connection.lock().await;
    let c = conn.as_ref().ok_or("Not connected")?;
    let topic = format!("device/{}/request", c.serial);
    let payload = serde_json::json!({
        "system": {
            "sequence_id": "0",
            "command": "ledctrl",
            "led_node": "chamber_light",
            "led_mode": if on { "on" } else { "off" },
            "led_on_time": 500,
            "led_off_time": 500,
            "loop_times": 0,
            "interval_time": 0
        }
    });
    c.mqtt_client
        .publish(&topic, QoS::AtLeastOnce, false, payload.to_string())
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn send_gcode(gcode: String, state: TauriState<'_, AppState>) -> Result<(), String> {
    let conn = state.connection.lock().await;
    let c = conn.as_ref().ok_or("Not connected")?;
    let topic = format!("device/{}/request", c.serial);
    let payload = serde_json::json!({
        "print": {
            "sequence_id": "0",
            "command": "gcode_line",
            "param": gcode
        }
    });
    c.mqtt_client
        .publish(&topic, QoS::AtMostOnce, false, payload.to_string())
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn printer_command(command: String, state: TauriState<'_, AppState>) -> Result<(), String> {
    let conn = state.connection.lock().await;
    let c = conn.as_ref().ok_or("Not connected")?;
    let topic = format!("device/{}/request", c.serial);
    let payload = serde_json::json!({ "print": { "command": command } });
    c.mqtt_client
        .publish(&topic, QoS::AtLeastOnce, false, payload.to_string())
        .await
        .map_err(|e| e.to_string())
}

// ── File manager (FTPS, see ftp.rs) ──────────────────────────────────────────

async fn printer_credentials(state: &AppState) -> Result<(String, String), String> {
    let conn = state.connection.lock().await;
    let c = conn.as_ref().ok_or("Not connected")?;
    Ok((c.ip.clone(), c.access_code.clone()))
}

async fn ftp_session(state: &AppState) -> Result<MappedMutexGuard<'_, ftp::Ftp>, String> {
    let (ip, access_code) = printer_credentials(state).await?;
    let mut slot = state.ftp.lock().await;
    ftp::ensure_session(&mut slot, &ip, &access_code).await?;
    Ok(MutexGuard::map(slot, |s| {
        s.as_mut().expect("ensure_session leaves a session")
    }))
}

async fn write_cache(path: &std::path::Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let _ = tokio::fs::write(path, bytes).await;
}

// Parses the largest embedded thumbnail from a gcode header.
// Handles both `; thumbnail begin WxH SIZE` and `; thumbnail_QOI begin WxH SIZE`.
// Returns raw image bytes (PNG).
fn parse_gcode_thumbnail(data: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(data).unwrap_or("");

    let mut best_size = 0usize;
    let mut best_bytes: Option<Vec<u8>> = None;
    let mut in_thumb = false;
    let mut b64 = String::new();
    let mut current_size = 0usize;

    for line in text.lines() {
        let line = line.trim();
        if in_thumb {
            if line.starts_with("; thumbnail") && line.contains("end") {
                in_thumb = false;
                if !b64.is_empty() {
                    if let Ok(decoded) = STANDARD.decode(&b64) {
                        if current_size > best_size || best_bytes.is_none() {
                            best_size = current_size;
                            best_bytes = Some(decoded);
                        }
                    }
                }
                b64.clear();
                current_size = 0;
            } else if let Some(part) = line.strip_prefix("; ") {
                b64.push_str(part.trim());
            }
        } else if let Some(rest) = line.strip_prefix("; thumbnail").and_then(|s| {
            s.strip_prefix(" begin ")
                .or_else(|| s.strip_prefix("_QOI begin "))
                .or_else(|| s.strip_prefix("_PNG begin "))
        }) {
            // rest = "WxH SIZE"
            current_size = rest
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            in_thumb = true;
            b64.clear();
        }
    }

    best_bytes
}

fn detect_mime(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        "image/png"
    } else {
        "image/jpeg"
    }
}

#[tauri::command]
async fn list_files(
    path: String,
    state: TauriState<'_, AppState>,
) -> Result<Vec<FileEntry>, String> {
    let mut ftp = ftp_session(&state).await?;
    ftp::list(&mut ftp, &path).await
}

#[tauri::command]
async fn fetch_thumbnail(
    path: String,
    app: AppHandle,
    state: TauriState<'_, AppState>,
) -> Result<String, String> {
    let file_name = std::path::Path::new(&path)
        .file_name()
        .ok_or("invalid path")?
        .to_owned();
    let cache_dir = app.path().app_cache_dir().map_err(|e| e.to_string())?;
    let cache_path = cache_dir.join("thumbnails").join(&file_name);
    if let Ok(bytes) = tokio::fs::read(&cache_path).await {
        return Ok(STANDARD.encode(&bytes));
    }

    let mut ftp = ftp_session(&state).await?;
    let bytes = ftp::read(&mut ftp, &path, usize::MAX)
        .await?
        .ok_or_else(|| format!("{path}: not found"))?;
    write_cache(&cache_path, &bytes).await;
    Ok(STANDARD.encode(&bytes))
}

const PREVIEW_HEADER_BYTES: usize = 2 * 1024 * 1024;

#[tauri::command]
async fn fetch_print_preview(
    subtask_name: String,
    task_id: String,
    app: AppHandle,
    state: TauriState<'_, AppState>,
) -> Result<String, String> {
    if subtask_name.is_empty() {
        return Err("No active job".to_string());
    }

    // Cached without extension; the MIME type is detected from the magic bytes.
    let cache_dir = app.path().app_cache_dir().map_err(|e| e.to_string())?;
    let cache_path = cache_dir.join("previews").join(&subtask_name);
    let bytes = match tokio::fs::read(&cache_path).await {
        Ok(bytes) => bytes,
        Err(_) => {
            let mut ftp = ftp_session(&state).await?;
            let bytes = find_print_preview(&mut ftp, &subtask_name, &task_id).await?;
            write_cache(&cache_path, &bytes).await;
            bytes
        }
    };
    Ok(format!(
        "data:{};base64,{}",
        detect_mime(&bytes),
        STANDARD.encode(&bytes)
    ))
}

/// Tries the cloud task's image, the newest file in /image/ (where OrcaSlicer puts the
/// preview of a LAN print), a pre-rendered image in /cache/, then the thumbnail embedded in
/// the job's gcode header.
async fn find_print_preview(
    ftp: &mut ftp::Ftp,
    subtask_name: &str,
    task_id: &str,
) -> Result<Vec<u8>, String> {
    let mut tried = Vec::new();
    let mut images = Vec::new();
    // LAN prints report task_id "0".
    if !task_id.is_empty() && task_id != "0" {
        images.extend(["png", "jpg", "jpeg"].map(|ext| format!("/image/{task_id}.{ext}")));
    }
    match ftp::list(ftp, "/image/").await {
        Ok(entries) => images.extend(
            entries
                .into_iter()
                .filter(|e| !e.is_dir)
                .max_by_key(|e| e.modified)
                .map(|e| format!("/image/{}", e.name)),
        ),
        Err(e) => tried.push(e),
    }
    images.extend(["jpg", "jpeg", "png"].map(|ext| format!("/cache/{subtask_name}.{ext}")));
    for path in &images {
        match ftp::read(ftp, path, usize::MAX).await {
            Ok(Some(bytes)) => return Ok(bytes),
            Ok(None) => tried.push(format!("{path}: not found")),
            Err(e) => tried.push(e),
        }
    }

    let mut gcode = vec![format!("/cache/{subtask_name}.gcode")];
    if let Some(idx) = subtask_name.rfind("_plate_") {
        gcode.push(format!(
            "/cache/{subtask_name}{}.gcode",
            &subtask_name[idx..]
        ));
    }
    for path in &gcode {
        match ftp::read(ftp, path, PREVIEW_HEADER_BYTES).await {
            Ok(Some(header)) => match parse_gcode_thumbnail(&header) {
                Some(bytes) => return Ok(bytes),
                None => tried.push(format!("{path}: no thumbnail in header")),
            },
            Ok(None) => tried.push(format!("{path}: not found")),
            Err(e) => tried.push(e),
        }
    }
    Err(tried.join(" | "))
}

#[tauri::command]
async fn download_file(
    path: String,
    app: AppHandle,
    state: TauriState<'_, AppState>,
) -> Result<String, String> {
    let filename = path.split('/').last().unwrap_or("download").to_owned();
    let save_dir = app
        .path()
        .download_dir()
        .or_else(|_| app.path().app_data_dir())
        .map_err(|e| e.to_string())?;
    let _ = tokio::fs::create_dir_all(&save_dir).await;

    let mut ftp = ftp_session(&state).await?;
    ftp::download(&mut ftp, &path, &save_dir.join(&filename)).await?;
    Ok(filename)
}

#[tauri::command]
async fn delete_entry(path: String, state: TauriState<'_, AppState>) -> Result<(), String> {
    let mut ftp = ftp_session(&state).await?;
    ftp::remove(&mut ftp, &path).await
}

// ── Filament load / unload ────────────────────────────────────────────────────
//
// Protocol source: OrcaSlicer DeviceManager.cpp `command_ams_change_filament`.
//
// Both load and unload use the same MQTT command `ams_change_filament`.
// Unload is signalled by target=255 and slot_id=255 ("new protocol").
//
// Field names (different from what might be guessed):
//   ams_id   – AMS unit index (0-3), or 255 for VT/external/none
//   target   – global tray ID (unit*4+slot), or 255 for unload
//   slot_id  – per-unit slot (0-3), or 255 for unload
//   curr_temp – nozzle temp of the filament being unloaded
//   tar_temp  – nozzle temp of the filament being loaded
// OrcaSlicer defaults both temps to 210 when unspecified.

// Returns the print temperature for any global slot ID.
fn tray_temperature(status: &PrinterStatus, global_id: u8) -> u16 {
    match global_id {
        254 => status.vt_tray.as_ref().map(|t| t.tray_temp).unwrap_or(210),
        255 => 210, // nothing loaded — OrcaSlicer default
        id => {
            let unit_id = id / 4;
            let slot_id = id % 4;
            status
                .ams
                .iter()
                .find(|u| u.id == unit_id)
                .and_then(|u| u.trays.iter().find(|t| t.id == slot_id))
                .map(|t| t.tray_temp)
                .unwrap_or(210)
        }
    }
}

#[tauri::command]
async fn load_filament(tray_id: u8, state: TauriState<'_, AppState>) -> Result<(), String> {
    // Capture everything while holding locks, then drop before async publish
    // so we never hold a Mutex across an await point.
    let (mqtt_client, topic, payload) = {
        let conn = state.connection.lock().await;
        let c = conn.as_ref().ok_or("Not connected")?;
        let status = c.status.lock().await;

        let curr_tray = status.tray_now;
        let curr_temp = tray_temperature(&status, curr_tray);
        let tar_temp  = tray_temperature(&status, tray_id);

        // Decompose global tray_id into the three fields OrcaSlicer sends.
        // VT tray (254): OrcaSlicer maps ams_id 254→255, leaving target=255, slot_id=0.
        let (ams_id, target, slot_id): (u8, u8, u8) = if tray_id == 254 {
            (255, 255, 0)
        } else {
            let unit = tray_id / 4;
            let slot = tray_id % 4;
            (unit, tray_id, slot)
        };

        let topic   = format!("device/{}/request", c.serial);
        let payload = serde_json::json!({
            "print": {
                "sequence_id": "0",
                "command":     "ams_change_filament",
                "curr_temp":   curr_temp,
                "tar_temp":    tar_temp,
                "ams_id":      ams_id,
                "target":      target,
                "slot_id":     slot_id
            }
        });
        (c.mqtt_client.clone(), topic, payload)
    }; // connection + status locks dropped here

    mqtt_client
        .publish(&topic, QoS::AtLeastOnce, false, payload.to_string())
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn unload_filament(state: TauriState<'_, AppState>) -> Result<(), String> {
    let (mqtt_client, topic, payload) = {
        let conn = state.connection.lock().await;
        let c = conn.as_ref().ok_or("Not connected")?;
        let status = c.status.lock().await;

        let curr_tray = status.tray_now;
        let curr_temp = tray_temperature(&status, curr_tray);
        // ams_id for unload = the unit that is currently loaded, or 255 if none.
        let ams_id: u8 = if curr_tray == 255 || curr_tray == 254 {
            255
        } else {
            curr_tray / 4
        };

        let topic   = format!("device/{}/request", c.serial);
        // Unload: target=255 slot_id=255 ("new protocol to mark unload" per OrcaSlicer).
        let payload = serde_json::json!({
            "print": {
                "sequence_id": "0",
                "command":     "ams_change_filament",
                "curr_temp":   curr_temp,
                "tar_temp":    curr_temp,
                "ams_id":      ams_id,
                "target":      255,
                "slot_id":     255
            }
        });
        (c.mqtt_client.clone(), topic, payload)
    }; // locks dropped here

    mqtt_client
        .publish(&topic, QoS::AtLeastOnce, false, payload.to_string())
        .await
        .map_err(|e| e.to_string())
}

// ── Print from SD card ────────────────────────────────────────────────────────
//
// Protocol source: OrcaSlicer DeviceManager.cpp + OpenBambuAPI/mqtt.md +
// community MQTT captures.
//
// Key points verified against OrcaSlicer source:
//  • `url`  — must use three slashes with NO host: "ftp:///cache/file.gcode.3mf"
//             (the printer resolves the path locally; "ftp://ip/path" is rejected).
//  • `param` — for 3MF files: internal path inside the ZIP archive, always
//              "Metadata/plate_1.gcode" for single-plate prints.
//              For plain .gcode files: the FTP path itself.
//  • `project_id`, `subtask_id`, `file`, `md5` — required by firmware; all "0"/"".
//  • QoS 1 (AtLeastOnce) — same as other critical print-control commands.
//  • Lock must NOT be held across the async publish (clone mqtt_client first).

#[tauri::command]
async fn start_print(
    path: String,
    bed_leveling: bool,
    flow_cali: bool,
    timelapse: bool,
    use_ams: bool,
    state: TauriState<'_, AppState>,
) -> Result<(), String> {
    let (mqtt_client, topic, payload) = {
        let conn = state.connection.lock().await;
        let c = conn.as_ref().ok_or("Not connected")?;

        let filename = path.split('/').last().unwrap_or(&path).to_owned();
        let lower = filename.to_lowercase();
        let is_3mf = lower.ends_with(".gcode.3mf");

        // Strip known suffixes to get a clean display name.
        let subtask_name = if lower.ends_with(".gcode.3mf") {
            filename[..filename.len() - ".gcode.3mf".len()].to_owned()
        } else if lower.ends_with(".gcode") {
            filename[..filename.len() - ".gcode".len()].to_owned()
        } else {
            filename.clone()
        };

        // `param` is the path *inside* the 3MF ZIP archive — only meaningful
        // for .gcode.3mf files. For plain .gcode files it must be an empty
        // string: sending the FTP path here causes the firmware to try to
        // extract that path from the file as if it were a ZIP, which fails
        // immediately with error 05004003 "unable to parse the file".
        let param = if is_3mf {
            "Metadata/plate_1.gcode".to_owned()
        } else {
            String::new()
        };

        // `url` must use the file:// scheme pointing at the printer's SD card
        // mount path. The FTP root maps to /sdcard/ on the printer's filesystem,
        // so FTP path "/cache/file.gcode.3mf" → "file:///sdcard/cache/file.gcode.3mf".
        // This is the exact format the official Bambu app uses for SD-card prints.
        // Using ftp:///path instead routes through the printer's internal FTP server
        // and triggers spurious "MicroSD Card read/write exception" HMS alerts.
        let url = format!("file:///sdcard{}", path);

        let topic = format!("device/{}/request", c.serial);
        let payload = serde_json::json!({
            "print": {
                "sequence_id": "0",
                "command":      "project_file",
                "param":        param,
                "url":          url,
                "file":         "",
                "md5":          "",
                "project_id":   "0",
                "profile_id":   "0",
                "task_id":      "0",
                "subtask_id":   "0",
                "subtask_name": subtask_name,
                "timelapse":    timelapse,
                "bed_type":     "auto",
                "bed_leveling": bed_leveling,
                "flow_cali":    flow_cali,
                "vibration_cali": true,
                "layer_inspect":  false,
                "use_ams":      use_ams
            }
        });
        (c.mqtt_client.clone(), topic, payload)
    }; // connection lock dropped here — never hold it across an async await

    mqtt_client
        .publish(&topic, QoS::AtLeastOnce, false, payload.to_string())
        .await
        .map_err(|e| e.to_string())
}

// ── Debug helpers ─────────────────────────────────────────────────────────────

#[tauri::command]
async fn debug_send_request(
    payload: String,
    state: TauriState<'_, AppState>,
) -> Result<(), String> {
    let conn = state.connection.lock().await;
    let c = conn.as_ref().ok_or("Not connected")?;
    let topic = format!("device/{}/request", c.serial);
    c.mqtt_client
        .publish(&topic, QoS::AtMostOnce, false, payload)
        .await
        .map_err(|e| e.to_string())
}

// ── Dev-only helpers ──────────────────────────────────────────────────────────

#[tauri::command]
async fn inject_test_hms(
    code: String,
    app: AppHandle,
    state: TauriState<'_, AppState>,
) -> Result<(), String> {
    let arc = {
        let conn = state.connection.lock().await;
        conn.as_ref().map(|c| c.status.clone())
    };
    if let Some(arc) = arc {
        let mut st = arc.lock().await.clone();
        st.hms = if code.is_empty() { vec![] } else { vec![code] };
        let _ = app.emit("printer-status", st);
    }
    Ok(())
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default();

    // single-instance is a desktop-only concept; the plugin crate gates its
    // entire public API behind `#![cfg(not(android/ios))]`, so the `init`
    // function does not exist in the Android / iOS build.
    #[cfg(not(mobile))]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|_app, _args, _cwd| {}));

    // Register the Android print-notification plugin.
    //
    // `register_android_plugin` calls PluginManager.load() via JNI, instantiating
    // PrintNotificationPlugin with the Activity reference — replacing the manual
    // PluginManager.load() that used to live in MainActivity.kt.
    //
    // The returned PluginHandle is stored as managed state (PrintNotifHandle) so
    // the MQTT task can call startNotification / updateNotification / stopNotification
    // directly from Rust, bypassing the JavaScript bridge.  This makes the
    // foreground-service notification update even when the WebView is suspended
    // in the background.
    //
    // TypeScript invocations (`invoke('plugin:printNotification|...')`) continue
    // to work through the same handle — both paths converge on the same Kotlin
    // plugin method.
    #[cfg(mobile)]
    let builder = builder.plugin(
        tauri::plugin::Builder::<tauri::Wry, ()>::new("printNotification")
            .setup(|app, api| {
                match api.register_android_plugin(
                    "com.joelsgc.bamboomobilelan",
                    "PrintNotificationPlugin",
                ) {
                    Ok(handle) => {
                        app.manage(PrintNotifHandle(handle));
                    }
                    Err(e) => {
                        eprintln!("[PrintNotif] register_android_plugin failed: {:?}", e);
                    }
                }
                Ok(())
            })
            .build(),
    );

    builder
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_http::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_store::Builder::default().build())
        .plugin(tauri_plugin_notification::init())
        .manage(AppState {
            connection: Mutex::new(None),
            ftp: Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![
            connect_printer,
            disconnect_printer,
            get_status,
            printer_command,
            set_chamber_light,
            set_print_speed,
            send_gcode,
            list_files,
            delete_entry,
            fetch_thumbnail,
            fetch_print_preview,
            download_file,
            inject_test_hms,
            debug_send_request,
            start_print,
            load_filament,
            unload_filament,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
