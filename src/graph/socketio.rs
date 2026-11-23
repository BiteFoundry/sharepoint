//! Minimal Engine.IO v4 / Socket.IO v4 WebSocket-only client for Microsoft Graph
//! `GET /drives/{driveId}/root/subscriptions/socketIo` change notifications.
//!
//! Follows Microsoft Graph's documented `socket.io-client` 4.8.x contract:
//! - Outbound WebSocket transport only (`EIO=4&transport=websocket`, no HTTP polling)
//! - Preserves custom namespace path (e.g. `https://*.svc.ms/<path>?<query>`)
//!   with WebSocket handshake at `/socket.io/?<query>&EIO=4&transport=websocket`
//!   and Socket.IO namespace `CONNECT` (`40` or `40<namespace>,`)
//! - Responds to Engine.IO pings (`2` -> `3`) and treats `"notification"` events
//!   as opaque wake-up signals into `driveItem: delta`.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_tungstenite::{
    connect_async, connect_async_tls_with_config, tungstenite::Message, Connector,
};
use url::Url;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_PING_INTERVAL_MS: u64 = 25_000;
const DEFAULT_PING_TIMEOUT_MS: u64 = 20_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketIoWsTarget {
    pub ws_url: String,
    pub namespace: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocketIoSignal {
    Notification { drive_id: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum SocketIoPacket {
    EngineOpen {
        ping_interval_ms: u64,
        ping_timeout_ms: u64,
    },
    EngineClose,
    EnginePing,
    EnginePong,
    SocketConnectAck {
        namespace: String,
    },
    SocketDisconnect {
        namespace: String,
    },
    SocketConnectError {
        namespace: String,
        detail: String,
    },
    SocketEvent {
        namespace: String,
        event: String,
        payload: Option<serde_json::Value>,
    },
    Other,
}

#[derive(Debug, Deserialize)]
struct EngineOpenPayload {
    #[serde(rename = "pingInterval", default)]
    ping_interval: Option<u64>,
    #[serde(rename = "pingTimeout", default)]
    ping_timeout: Option<u64>,
}

/// Converts a Graph `notificationUrl` into the Engine.IO v4 WebSocket-only handshake URL
/// and its Socket.IO namespace, matching `socket.io-client@4.8.x` with `transports: ['websocket']`.
pub fn build_socketio_ws_target(notification_url: &str) -> Result<SocketIoWsTarget> {
    let mut url = Url::parse(notification_url.trim())
        .context("Invalid Socket.IO notification URL")?;

    match url.scheme() {
        "https" => {
            url.set_scheme("wss")
                .map_err(|_| anyhow!("Failed to convert https scheme to wss"))?;
        }
        "http" => {
            url.set_scheme("ws")
                .map_err(|_| anyhow!("Failed to convert http scheme to ws"))?;
        }
        "wss" | "ws" => {}
        other => bail!("Unsupported Socket.IO scheme '{other}'"),
    }

    let raw_path = url.path().to_string();
    let trimmed_path = raw_path.trim_end_matches('/');
    let namespace = if trimmed_path.is_empty() || trimmed_path == "/socket.io" {
        "/".to_string()
    } else {
        raw_path
    };

    url.set_path("/socket.io/");

    let existing_pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let has_eio = existing_pairs.iter().any(|(k, _)| k == "EIO");
    let has_transport = existing_pairs.iter().any(|(k, _)| k == "transport");

    {
        let mut qp = url.query_pairs_mut();
        qp.clear();
        for (k, v) in &existing_pairs {
            qp.append_pair(k, v);
        }
        if !has_eio {
            qp.append_pair("EIO", "4");
        }
        if !has_transport {
            qp.append_pair("transport", "websocket");
        }
    }

    Ok(SocketIoWsTarget {
        ws_url: url.to_string(),
        namespace,
    })
}

/// Formats the Socket.IO v4 CONNECT packet for the given namespace (`"40"` or `"40<namespace>,"`).
pub fn format_socketio_connect_packet(namespace: &str) -> String {
    if namespace.is_empty() || namespace == "/" {
        "40".to_string()
    } else {
        format!("40{namespace},")
    }
}

/// Parses an Engine.IO v4 / Socket.IO v4 text frame.
pub fn parse_socketio_packet(text: &str) -> SocketIoPacket {
    let Some(first) = text.as_bytes().first().copied() else {
        return SocketIoPacket::Other;
    };
    match first {
        b'0' => {
            let parsed = serde_json::from_str::<EngineOpenPayload>(&text[1..]).ok();
            let ping_interval_ms = parsed
                .as_ref()
                .and_then(|p| p.ping_interval)
                .unwrap_or(DEFAULT_PING_INTERVAL_MS);
            let ping_timeout_ms = parsed
                .as_ref()
                .and_then(|p| p.ping_timeout)
                .unwrap_or(DEFAULT_PING_TIMEOUT_MS);
            SocketIoPacket::EngineOpen {
                ping_interval_ms,
                ping_timeout_ms,
            }
        }
        b'1' => SocketIoPacket::EngineClose,
        b'2' => SocketIoPacket::EnginePing,
        b'3' => SocketIoPacket::EnginePong,
        b'4' => parse_socketio_message(&text[1..]),
        _ => SocketIoPacket::Other,
    }
}

fn split_socketio_namespace(rest: &str) -> (&str, &str) {
    if rest.starts_with('/') {
        if let Some((nsp, tail)) = rest.split_once(',') {
            return (nsp, tail);
        }
        return (rest, "");
    }
    ("/", rest)
}

fn parse_socketio_message(body: &str) -> SocketIoPacket {
    let Some(sio_type) = body.as_bytes().first().copied() else {
        return SocketIoPacket::Other;
    };
    let rest = &body[1..];
    match sio_type {
        b'0' => {
            let (nsp, _) = split_socketio_namespace(rest);
            SocketIoPacket::SocketConnectAck {
                namespace: nsp.to_string(),
            }
        }
        b'1' => {
            let (nsp, _) = split_socketio_namespace(rest);
            SocketIoPacket::SocketDisconnect {
                namespace: nsp.to_string(),
            }
        }
        b'2' => {
            let (nsp, tail) = split_socketio_namespace(rest);
            let json_part = tail.trim_start_matches(|c: char| c.is_ascii_digit());
            let Ok(serde_json::Value::Array(arr)) = serde_json::from_str(json_part) else {
                return SocketIoPacket::Other;
            };
            let Some(event) = arr.first().and_then(|v| v.as_str()) else {
                return SocketIoPacket::Other;
            };
            let payload = arr.get(1).cloned();
            SocketIoPacket::SocketEvent {
                namespace: nsp.to_string(),
                event: event.to_string(),
                payload,
            }
        }
        b'4' => {
            let (nsp, tail) = split_socketio_namespace(rest);
            SocketIoPacket::SocketConnectError {
                namespace: nsp.to_string(),
                detail: tail.to_string(),
            }
        }
        _ => SocketIoPacket::Other,
    }
}

/// Runs a single Socket.IO v4 WebSocket session until shutdown, disconnect, error, or heartbeat timeout.
/// Sends `()` on `on_connected` once the Socket.IO namespace `CONNECT` ack (`40...`) is received.
pub async fn run_socketio_session(
    drive_id: String,
    notification_url: &str,
    mut on_connected: Option<oneshot::Sender<()>>,
    signal_tx: mpsc::UnboundedSender<SocketIoSignal>,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> Result<()> {
    let target = build_socketio_ws_target(notification_url)?;
    // Extra CAs (SHAREPOINT_EXTRA_CA_PEM and friends) need an explicit rustls config;
    // without them tokio-tungstenite's own OS + bundled roots are used.
    let tls_config = crate::net::websocket_tls_config()?;
    let connect = async {
        match tls_config {
            Some(config) => {
                connect_async_tls_with_config(
                    target.ws_url.as_str(),
                    None,
                    false,
                    Some(Connector::Rustls(config)),
                )
                .await
            }
            None => connect_async(target.ws_url.as_str()).await,
        }
    };
    let (mut ws, _) = tokio::select! {
        _ = shutdown_rx.changed() => return Ok(()),
        result = tokio::time::timeout(CONNECT_TIMEOUT, connect) => {
            result
                .map_err(|_| anyhow!("Socket.IO WebSocket connect timed out"))?
                .map_err(|err| {
                    crate::net::annotate_tls_error(
                        anyhow::Error::new(err).context("Socket.IO WebSocket connect failed"),
                    )
                })?
        }
    };

    // Wait for Engine.IO OPEN (`0{...}`) and Socket.IO CONNECT ack (`40...`)
    let mut ping_deadline_ms = DEFAULT_PING_INTERVAL_MS + DEFAULT_PING_TIMEOUT_MS;
    let mut last_ping = None;
    let mut sio_connected = false;
    let handshake_deadline = tokio::time::Instant::now() + HANDSHAKE_TIMEOUT;

    while !sio_connected {
        let remaining = handshake_deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            bail!("Socket.IO handshake timed out waiting for CONNECT ack");
        }
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    let _ = ws.close(None).await;
                    return Ok(());
                }
            }
            next = tokio::time::timeout(remaining, ws.next()) => {
                let frame = match next {
                    Ok(Some(Ok(msg))) => msg,
                    Ok(Some(Err(err))) => return Err(err).context("Socket.IO handshake read error"),
                    Ok(None) => bail!("Socket.IO server closed connection during handshake"),
                    Err(_) => bail!("Socket.IO handshake timed out"),
                };
                match frame {
                    Message::Text(txt) => match parse_socketio_packet(txt.as_str()) {
                        SocketIoPacket::EngineOpen {
                            ping_interval_ms,
                            ping_timeout_ms,
                        } => {
                            last_ping = Some(tokio::time::Instant::now());
                            ping_deadline_ms = ping_interval_ms
                                .saturating_add(ping_timeout_ms)
                                .clamp(1, 180_000);
                            let connect_pkt = format_socketio_connect_packet(&target.namespace);
                            ws.send(Message::Text(connect_pkt.into()))
                                .await
                                .context("Failed to send Socket.IO CONNECT packet")?;
                        }
                        SocketIoPacket::SocketConnectAck { namespace } => {
                            if last_ping.is_some() && namespace == target.namespace {
                                sio_connected = true;
                                if let Some(tx) = on_connected.take() {
                                    let _ = tx.send(());
                                }
                            }
                        }
                        SocketIoPacket::EnginePing if last_ping.is_some() => {
                            last_ping = Some(tokio::time::Instant::now());
                            ws.send(Message::Text("3".into())).await?;
                        }
                        SocketIoPacket::SocketConnectError { namespace, .. }
                            if namespace == target.namespace =>
                        {
                            bail!("Socket.IO namespace rejected the connection");
                        }
                        SocketIoPacket::SocketDisconnect { namespace }
                            if namespace == target.namespace =>
                        {
                            bail!("Socket.IO namespace disconnected during handshake");
                        }
                        SocketIoPacket::EngineClose => {
                            bail!("Engine.IO disconnected during handshake");
                        }
                        _ => {}
                    },
                    Message::Ping(payload) => {
                        ws.send(Message::Pong(payload)).await?;
                    }
                    Message::Close(_) => bail!("Socket.IO closed during handshake"),
                    _ => {}
                }
            }
        }
    }

    // Connected steady-state loop: process notifications and Engine.IO pings
    let heartbeat_timeout = Duration::from_millis(ping_deadline_ms);
    let mut heartbeat_deadline = last_ping.expect("connected after Engine.IO OPEN") + heartbeat_timeout;
    loop {
        if tokio::time::Instant::now() >= heartbeat_deadline {
            bail!("Socket.IO heartbeat timeout ({ping_deadline_ms} ms)");
        }
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    let _ = ws.close(None).await;
                    return Ok(());
                }
            }
            next = tokio::time::timeout_at(heartbeat_deadline, ws.next()) => {
                let frame = match next {
                    Ok(Some(Ok(msg))) => msg,
                    Ok(Some(Err(err))) => return Err(err).context("Socket.IO stream error"),
                    Ok(None) => return Ok(()),
                    Err(_) => bail!("Socket.IO heartbeat timeout ({ping_deadline_ms} ms)"),
                };
                match frame {
                    Message::Text(txt) => match parse_socketio_packet(txt.as_str()) {
                        SocketIoPacket::EnginePing => {
                            heartbeat_deadline = tokio::time::Instant::now() + heartbeat_timeout;
                            ws.send(Message::Text("3".into()))
                                .await
                                .context("Failed to send Engine.IO PONG")?;
                        }
                        SocketIoPacket::SocketEvent { namespace, event, .. } if namespace == target.namespace => {
                            if event.eq_ignore_ascii_case("notification") {
                                let _ = signal_tx.send(SocketIoSignal::Notification {
                                    drive_id: drive_id.clone(),
                                });
                            }
                        }
                        SocketIoPacket::EngineClose => return Ok(()),
                        SocketIoPacket::SocketDisconnect { namespace }
                            if namespace == target.namespace =>
                        {
                            return Ok(());
                        }
                        SocketIoPacket::SocketConnectError { namespace, .. }
                            if namespace == target.namespace =>
                        {
                            bail!("Socket.IO namespace connection error");
                        }
                        _ => {}
                    },
                    Message::Ping(payload) => {
                        ws.send(Message::Pong(payload)).await?;
                    }
                    Message::Close(_) => return Ok(()),
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn builds_ws_target_for_graph_svc_ms_namespace_and_default_namespace() {
        let t1 = build_socketio_ws_target(
            "https://f3hb0mpua.svc.ms/zbaehwg/callback?snthgk=1ff3-2345672zz831837523",
        )
        .unwrap();
        assert_eq!(t1.namespace, "/zbaehwg/callback");
        assert_eq!(
            t1.ws_url,
            "wss://f3hb0mpua.svc.ms/socket.io/?snthgk=1ff3-2345672zz831837523&EIO=4&transport=websocket"
        );
        assert_eq!(
            format_socketio_connect_packet(&t1.namespace),
            "40/zbaehwg/callback,"
        );

        let t2 = build_socketio_ws_target("http://127.0.0.1:9000/socket.io/?token=abc").unwrap();
        assert_eq!(t2.namespace, "/");
        assert_eq!(
            t2.ws_url,
            "ws://127.0.0.1:9000/socket.io/?token=abc&EIO=4&transport=websocket"
        );
        assert_eq!(format_socketio_connect_packet(&t2.namespace), "40");
    }

    #[test]
    fn parses_engine_and_socketio_v4_packets_default_and_custom_namespace() {
        assert_eq!(
            parse_socketio_packet(r#"0{"sid":"s1","pingInterval":25000,"pingTimeout":20000}"#),
            SocketIoPacket::EngineOpen {
                ping_interval_ms: 25_000,
                ping_timeout_ms: 20_000
            }
        );
        assert_eq!(parse_socketio_packet("2"), SocketIoPacket::EnginePing);
        assert_eq!(
            parse_socketio_packet(r#"40/zbaehwg/callback,{"sid":"ns1"}"#),
            SocketIoPacket::SocketConnectAck {
                namespace: "/zbaehwg/callback".into()
            }
        );
        assert_eq!(
            parse_socketio_packet(r#"42["notification",{"subscriptionId":"sub-1"}]"#),
            SocketIoPacket::SocketEvent {
                namespace: "/".into(),
                event: "notification".into(),
                payload: Some(serde_json::json!({"subscriptionId": "sub-1"})),
            }
        );
        assert_eq!(
            parse_socketio_packet(r#"42/zbaehwg/callback,17["notification",{"resource":"root"}]"#),
            SocketIoPacket::SocketEvent {
                namespace: "/zbaehwg/callback".into(),
                event: "notification".into(),
                payload: Some(serde_json::json!({"resource": "root"})),
            }
        );
    }

    #[tokio::test]
    async fn loopback_socketio_v4_handshake_ping_pong_and_notification() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            ws.send(Message::Text(
                r#"0{"sid":"mock-sid","upgrades":[],"pingInterval":5000,"pingTimeout":5000}"#
                    .into(),
            ))
            .await
            .unwrap();
            let connect = ws.next().await.unwrap().unwrap();
            assert_eq!(connect.into_text().unwrap(), "40/ns/callback,");
            ws.send(Message::Text(r#"40/ns/callback,{"sid":"ns-sid"}"#.into()))
                .await
                .unwrap();

            // Verify Engine.IO ping -> pong
            ws.send(Message::Text("2".into())).await.unwrap();
            let pong = ws.next().await.unwrap().unwrap();
            assert_eq!(pong.into_text().unwrap(), "3");

            // Emit Graph notification
            ws.send(Message::Text(
                r#"42/ns/callback,["notification",{"subscriptionId":"sub-1"}]"#.into(),
            ))
            .await
            .unwrap();

            // Keep alive briefly until client closes
            let _ = ws.next().await;
        });

        let (conn_tx, conn_rx) = oneshot::channel();
        let (sig_tx, mut sig_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

        let url = format!("http://{addr}/ns/callback?snthgk=test-token");
        let client_task = tokio::spawn(async move {
            run_socketio_session(
                "drv-1".into(),
                &url,
                Some(conn_tx),
                sig_tx,
                &mut shutdown_rx,
            )
            .await
        });

        tokio::time::timeout(Duration::from_secs(3), conn_rx)
            .await
            .unwrap()
            .unwrap();
        let sig = tokio::time::timeout(Duration::from_secs(3), sig_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            sig,
            SocketIoSignal::Notification {
                drive_id: "drv-1".into()
            }
        );

        let _ = shutdown_tx.send(true);
        client_task.await.unwrap().unwrap();
        server_task.await.unwrap();
    }
}
