// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! The Cast control channel ("CASTV2"): TLS to the device's port 8009, carrying length-prefixed
//! `CastMessage` protobufs whose payloads are JSON on a handful of namespaces.
//!
//! Only what launching an app needs is here: connect, heartbeat, launch, stop and receiver status.
//! The protobuf has six fields and is encoded by hand rather than pulling in a protobuf toolchain.
//!
//! The device presents a certificate signed by Google's Cast CA, not by a web CA, and for an IP
//! address. It is not checked: this is a trusted-LAN tool talking to a device the operator picked
//! from a list, and the channel carries only "open this app" (no credentials, no media).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::client::TlsStream;

pub const NS_CONNECTION: &str = "urn:x-cast:com.google.cast.tp.connection";
pub const NS_HEARTBEAT: &str = "urn:x-cast:com.google.cast.tp.heartbeat";
pub const NS_RECEIVER: &str = "urn:x-cast:com.google.cast.receiver";

pub const SENDER_ID: &str = "sender-0";
pub const RECEIVER_ID: &str = "receiver-0";

/// The largest message accepted from a device. Receiver status is a few KiB.
const MAX_MESSAGE: usize = 64 * 1024;

/// One `CastMessage`, string payloads only (Cast's binary payloads are not used here).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CastMessage {
    pub source: String,
    pub destination: String,
    pub namespace: String,
    pub payload: String,
}

impl CastMessage {
    pub fn new(source: &str, destination: &str, namespace: &str, payload: impl Into<String>) -> Self {
        CastMessage {
            source: source.into(),
            destination: destination.into(),
            namespace: namespace.into(),
            payload: payload.into(),
        }
    }

    /// The protobuf encoding (without the length prefix).
    pub fn encode(&self) -> Vec<u8> {
        fn varint(out: &mut Vec<u8>, mut v: u64) {
            loop {
                let b = (v & 0x7F) as u8;
                v >>= 7;
                if v == 0 {
                    out.push(b);
                    return;
                }
                out.push(b | 0x80);
            }
        }
        fn string(out: &mut Vec<u8>, field: u8, s: &str) {
            out.push((field << 3) | 2);
            varint(out, s.len() as u64);
            out.extend_from_slice(s.as_bytes());
        }
        let mut out = Vec::with_capacity(64 + self.payload.len());
        out.extend_from_slice(&[0x08, 0x00]); // protocol_version = CASTV2_1_0
        string(&mut out, 2, &self.source);
        string(&mut out, 3, &self.destination);
        string(&mut out, 4, &self.namespace);
        out.extend_from_slice(&[0x28, 0x00]); // payload_type = STRING
        string(&mut out, 6, &self.payload);
        out
    }

    /// Parse the protobuf encoding. Unknown fields are skipped; a binary payload is left empty.
    pub fn decode(b: &[u8]) -> Result<CastMessage> {
        fn varint(b: &[u8], pos: &mut usize) -> Result<u64> {
            let mut v = 0u64;
            for shift in (0..64).step_by(7) {
                let byte = *b.get(*pos).context("truncated varint")?;
                *pos += 1;
                v |= ((byte & 0x7F) as u64) << shift;
                if byte & 0x80 == 0 {
                    return Ok(v);
                }
            }
            bail!("varint too long")
        }
        let mut m = CastMessage::default();
        let mut pos = 0;
        while pos < b.len() {
            let key = varint(b, &mut pos)?;
            let (field, wire) = (key >> 3, key & 7);
            match wire {
                0 => {
                    varint(b, &mut pos)?;
                }
                2 => {
                    let len = varint(b, &mut pos)? as usize;
                    let end = pos.checked_add(len).filter(|&e| e <= b.len()).context("truncated field")?;
                    let s = || String::from_utf8_lossy(&b[pos..end]).into_owned();
                    match field {
                        2 => m.source = s(),
                        3 => m.destination = s(),
                        4 => m.namespace = s(),
                        6 => m.payload = s(),
                        _ => {}
                    }
                    pos = end;
                }
                5 => pos += 4,
                1 => pos += 8,
                _ => bail!("unsupported protobuf wire type {wire}"),
            }
        }
        Ok(m)
    }
}

/// An open control channel to one device. Reading happens on its own task, so [`Connection::recv`]
/// can be raced against timers in `select!` without ever losing half a message.
pub struct Connection {
    tx: WriteHalf<TlsStream<TcpStream>>,
    rx: mpsc::Receiver<std::result::Result<CastMessage, String>>,
}

impl Connection {
    /// Open the TLS channel and the virtual connection to the device's platform receiver.
    pub async fn open(addr: SocketAddr) -> Result<Connection> {
        let tcp = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(addr))
            .await
            .context("the device did not answer")?
            .context("could not reach the device")?;
        let _ = tcp.set_nodelay(true);
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .context("TLS setup")?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyCert(provider)))
            .with_no_client_auth();
        let name = rustls::pki_types::ServerName::IpAddress(addr.ip().into());
        let tls = tokio::time::timeout(
            Duration::from_secs(5),
            tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name, tcp),
        )
        .await
        .context("the device did not finish the secure handshake")?
        .context("secure handshake with the device failed")?;
        let (r, w) = tokio::io::split(tls);
        let (msg_tx, rx) = mpsc::channel(32);
        tokio::spawn(read_loop(r, msg_tx));
        let mut c = Connection { tx: w, rx };
        c.send(&CastMessage::new(SENDER_ID, RECEIVER_ID, NS_CONNECTION, r#"{"type":"CONNECT"}"#))
            .await?;
        Ok(c)
    }

    pub async fn send(&mut self, m: &CastMessage) -> Result<()> {
        let body = m.encode();
        let mut frame = Vec::with_capacity(4 + body.len());
        frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
        frame.extend_from_slice(&body);
        self.tx.write_all(&frame).await.context("send to the device")?;
        self.tx.flush().await.context("send to the device")?;
        Ok(())
    }

    /// The next message, or why the connection ended. Cancel-safe.
    pub async fn recv(&mut self) -> Result<CastMessage> {
        match self.rx.recv().await {
            Some(Ok(m)) => Ok(m),
            Some(Err(e)) => bail!("{e}"),
            None => bail!("the device closed the connection"),
        }
    }
}

async fn read_loop(
    mut r: ReadHalf<TlsStream<TcpStream>>,
    tx: mpsc::Sender<std::result::Result<CastMessage, String>>,
) {
    let result: Result<()> = async {
        loop {
            let mut len = [0u8; 4];
            r.read_exact(&mut len).await.context("the device closed the connection")?;
            let len = u32::from_be_bytes(len) as usize;
            if len > MAX_MESSAGE {
                bail!("the device sent an oversized message ({len} bytes)");
            }
            let mut body = vec![0u8; len];
            r.read_exact(&mut body).await.context("the device closed the connection")?;
            if tx.send(Ok(CastMessage::decode(&body)?)).await.is_err() {
                return Ok(()); // the connection was dropped
            }
        }
    }
    .await;
    if let Err(e) = result {
        let _ = tx.send(Err(format!("{e:#}"))).await;
    }
}

/// Accepts the device's certificate without checking it (see the module docs), but still verifies
/// the handshake signatures, so the session is at least bound to whoever holds that certificate.
#[derive(Debug)]
struct AnyCert(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// What a `RECEIVER_STATUS` says is running on the device.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReceiverStatus {
    pub apps: Vec<RunningApp>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunningApp {
    pub app_id: String,
    pub display_name: String,
    pub session_id: String,
    pub transport_id: String,
    /// The device's own idle screen (the backdrop / ambient photos), not something a person chose.
    pub is_idle_screen: bool,
}

impl ReceiverStatus {
    pub fn parse(status: &serde_json::Value) -> ReceiverStatus {
        let s = |v: &serde_json::Value, k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or_default().to_string();
        let apps = status
            .get("applications")
            .and_then(|a| a.as_array())
            .map(|a| {
                a.iter()
                    .map(|app| RunningApp {
                        app_id: s(app, "appId"),
                        display_name: s(app, "displayName"),
                        session_id: s(app, "sessionId"),
                        transport_id: s(app, "transportId"),
                        is_idle_screen: app.get("isIdleScreen").and_then(|x| x.as_bool()).unwrap_or(false)
                            // The Backdrop app on devices that predate the flag.
                            || s(app, "appId") == "E8C28D3C",
                    })
                    .collect()
            })
            .unwrap_or_default();
        ReceiverStatus { apps }
    }

    pub fn app(&self, app_id: &str) -> Option<&RunningApp> {
        self.apps.iter().find(|a| a.app_id.eq_ignore_ascii_case(app_id))
    }

    /// Something a person is using right now, other than `app_id`.
    pub fn other_app(&self, app_id: &str) -> Option<&RunningApp> {
        self.apps.iter().find(|a| !a.is_idle_screen && !a.app_id.eq_ignore_ascii_case(app_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_round_trips() {
        let m = CastMessage::new(SENDER_ID, RECEIVER_ID, NS_RECEIVER, r#"{"type":"GET_STATUS","requestId":1}"#);
        assert_eq!(CastMessage::decode(&m.encode()).unwrap(), m);
    }

    #[test]
    fn long_payload_uses_a_multibyte_length() {
        let m = CastMessage::new("a", "b", "c", "x".repeat(300));
        let back = CastMessage::decode(&m.encode()).unwrap();
        assert_eq!(back.payload.len(), 300);
    }

    #[test]
    fn truncated_message_is_an_error() {
        let enc = CastMessage::new("a", "b", "c", "hello").encode();
        assert!(CastMessage::decode(&enc[..enc.len() - 2]).is_err());
    }

    /// Connects to a real device and only asks what it is running (launches nothing):
    /// `CAST_IP=192.168.1.40 cargo test -- --ignored status_live --nocapture`.
    #[tokio::test(flavor = "current_thread")]
    #[ignore]
    async fn status_live() {
        let ip: std::net::IpAddr = std::env::var("CAST_IP").expect("CAST_IP").parse().unwrap();
        let mut c = Connection::open(SocketAddr::new(ip, 8009)).await.expect("open");
        c.send(&CastMessage::new(SENDER_ID, RECEIVER_ID, NS_RECEIVER, r#"{"type":"GET_STATUS","requestId":1}"#))
            .await
            .unwrap();
        loop {
            let m = tokio::time::timeout(Duration::from_secs(5), c.recv()).await.expect("answer").unwrap();
            if m.namespace == NS_RECEIVER {
                let v: serde_json::Value = serde_json::from_str(&m.payload).unwrap();
                println!("{:?}", ReceiverStatus::parse(&v["status"]));
                break;
            }
        }
    }

    #[test]
    fn receiver_status_finds_apps() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"applications":[
                {"appId":"E8C28D3C","displayName":"Backdrop","sessionId":"s0","transportId":"t0"},
                {"appId":"A1B2C3D4","displayName":"Newfoundsync","sessionId":"s1","transportId":"t1","isIdleScreen":false}
            ]}"#,
        )
        .unwrap();
        let st = ReceiverStatus::parse(&v);
        assert_eq!(st.app("a1b2c3d4").unwrap().session_id, "s1");
        assert!(st.other_app("A1B2C3D4").is_none(), "the backdrop is not someone else's app");
        assert_eq!(st.other_app("FFFFFFFF").unwrap().display_name, "Newfoundsync");
    }
}
