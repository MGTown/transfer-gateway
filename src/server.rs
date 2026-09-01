use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::{Result, anyhow};
use md5::{Digest, Md5};
use tokio::{
    io::{AsyncWriteExt, BufStream},
    net::{TcpListener, TcpStream},
    sync::watch,
    time::timeout,
};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{
    config::{AppConfig, LineTarget},
    dns::MinecraftDnsResolver,
    ip2region::{Ip2Region, IpLocation},
    language::Language,
    protocol::{
        self, HANDSHAKE_PACKET_ID, Handshake, LOGIN_ACKNOWLEDGED_PACKET_ID,
        LOGIN_DISCONNECT_PACKET_ID, LOGIN_SUCCESS_PACKET_ID, LoginStart, NextState,
        encode_login_disconnect, encode_login_success, encode_transfer, parse_handshake,
        parse_login_start, read_packet, write_packet,
    },
    runtime::RuntimeState,
    security::BlockReason,
};

pub async fn run(mut state_receiver: watch::Receiver<Arc<RuntimeState>>) -> Result<()> {
    let initial = state_receiver.borrow().clone();
    let mut listener = TcpListener::bind(&initial.config.server.bind).await?;
    let mut bound = initial.config.server.bind.clone();
    let connections = ConnectionLimiter::default();

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, peer) = accepted?;
                let state = state_receiver.borrow().clone();
                let Some(permit) = connections.try_acquire(state.config.server.max_connections) else {
                    let message = state.language.render("log.connection_limit", &[]);
                    warn!(%peer, "{message}");
                    continue;
                };

                let login_timeout = Duration::from_millis(state.config.server.login_timeout_ms);
                tokio::spawn(async move {
                    let _permit = permit;
                    let result = timeout(
                        login_timeout,
                        handle_connection(stream, peer, state),
                    )
                    .await;

                    match result {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => debug!(%peer, ?error, "connection finished with error"),
                        Err(_) => debug!(%peer, "connection timed out"),
                    }
                });
            }
            changed = state_receiver.changed() => {
                changed?;
                let state = state_receiver.borrow().clone();
                if state.config.server.bind != bound {
                    match TcpListener::bind(&state.config.server.bind).await {
                        Ok(new_listener) => {
                            info!(bind = %state.config.server.bind, "server listener rebound after configuration reload");
                            listener = new_listener;
                            bound = state.config.server.bind.clone();
                        }
                        Err(error) => {
                            warn!(
                                bind = %state.config.server.bind,
                                ?error,
                                "unable to rebind server listener; keeping the previous listener"
                            );
                        }
                    }
                }
            }
            signal = tokio::signal::ctrl_c() => {
                signal?;
                info!("shutdown signal received");
                return Ok(());
            }
        }
    }
}

#[derive(Clone, Default)]
struct ConnectionLimiter {
    active: Arc<AtomicUsize>,
}

impl ConnectionLimiter {
    fn try_acquire(&self, maximum: usize) -> Option<ConnectionPermit> {
        let mut active = self.active.load(Ordering::Acquire);
        loop {
            if active >= maximum {
                return None;
            }
            match self.active.compare_exchange_weak(
                active,
                active + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(ConnectionPermit {
                        active: Arc::clone(&self.active),
                    });
                }
                Err(current) => active = current,
            }
        }
    }
}

struct ConnectionPermit {
    active: Arc<AtomicUsize>,
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

async fn handle_connection(
    stream: TcpStream,
    peer: SocketAddr,
    state: Arc<RuntimeState>,
) -> Result<()> {
    stream.set_nodelay(true)?;
    let mut stream = BufStream::new(stream);
    let max_frame_length = state.config.server.max_frame_length;

    let handshake_packet = read_packet(&mut stream, max_frame_length)
        .await?
        .ok_or_else(|| anyhow!("client closed before handshake"))?;
    let handshake = parse_handshake(&handshake_packet)?;
    let location = lookup_location(peer, &state.ip2region, &state.language);

    match handshake.next_state {
        NextState::Status => {
            handle_status(&mut stream, peer, state.as_ref(), &location, handshake).await?
        }
        NextState::Login | NextState::Transfer => {
            handle_login(&mut stream, peer, state.as_ref(), &location, handshake).await?
        }
    }

    stream.shutdown().await?;
    Ok(())
}

async fn handle_status(
    stream: &mut BufStream<TcpStream>,
    peer: SocketAddr,
    state: &RuntimeState,
    location: &IpLocation,
    handshake: Handshake,
) -> Result<()> {
    let config = &state.config;
    let dns = &state.dns;
    let security = &state.security;
    let language = &state.language;
    let max_frame_length = config.server.max_frame_length;
    let request = read_packet(stream, max_frame_length)
        .await?
        .ok_or_else(|| anyhow!("client closed before status request"))?;
    if request.id != 0 {
        return Err(anyhow!(
            "unexpected status request packet id {}",
            request.id
        ));
    }

    if let Some(reason) = security.check(location) {
        log_security_block(peer, reason, location, language);
        return Ok(());
    }

    let balance_key = peer.ip().to_string();
    let (mut backend, status_payload) = match config.select_route_with_host_context(
        location,
        None,
        Some(&handshake.host),
        security,
        &balance_key,
    ) {
        Some((line, target)) => {
            let configured_target_address = format_target(&target);
            match proxy_status_response(handshake.protocol_version, &target, dns, max_frame_length)
                .await
            {
                Ok((backend, payload, resolved_target)) => {
                    let target_address = format_target(&resolved_target);
                    debug!(
                        %peer,
                        node = %line,
                        target = %target_address,
                        "proxied backend server status"
                    );
                    (Some(backend), payload)
                }
                Err(error) => {
                    let error_display = error.to_string();
                    let message = language.render(
                        "log.status_proxy_failed",
                        &[("error", error_display.as_str())],
                    );
                    warn!(
                        %peer,
                        node = %line,
                        target = %configured_target_address,
                        ?error,
                        "{message}"
                    );
                    (None, local_status_payload(config, language)?)
                }
            }
        }
        None => (None, local_status_payload(config, language)?),
    };
    write_packet(stream, 0, &status_payload).await?;

    let ping = read_packet(stream, max_frame_length)
        .await?
        .ok_or_else(|| anyhow!("client closed before status ping"))?;
    if ping.id != 1 {
        return Err(anyhow!("unexpected status ping packet id {}", ping.id));
    }

    let mut reader = protocol::PacketReader::new(&ping.payload);
    let value = reader.read_i64()?;
    reader.expect_end()?;
    write_packet(stream, 1, &value.to_be_bytes()).await?;

    if let Some(mut backend) = backend.take()
        && let Err(error) =
            complete_backend_status_ping(&mut backend, &ping.payload, max_frame_length).await
    {
        debug!(%peer, ?error, "backend status ping failed");
    }
    Ok(())
}

fn local_status_payload(config: &AppConfig, language: &Language) -> Result<Vec<u8>> {
    let motd = language.render("status.motd", &[("motd", config.server.motd.as_str())]);
    let response = serde_json::json!({
        "version": {
            "name": config.server.status_version_name,
            "protocol": config.server.status_protocol,
        },
        "players": {
            "max": config.server.max_players,
            "online": 0,
            "sample": [],
        },
        "description": protocol::legacy_text_to_json(&motd),
        "enforcesSecureChat": false,
        "previewsChat": false,
    });
    let response = serde_json::to_string(&response)?;
    let mut payload = Vec::with_capacity(response.len() + 5);
    protocol::write_string(&mut payload, &response);
    Ok(payload)
}

async fn proxy_status_response(
    protocol_version: i32,
    configured_target: &LineTarget,
    dns: &MinecraftDnsResolver,
    max_frame_length: usize,
) -> Result<(BufStream<TcpStream>, Vec<u8>, LineTarget)> {
    let target = dns
        .resolve_target(configured_target, configured_target.resolve_srv)
        .await?;
    let target_address = format_target(&target);
    let backend_stream = TcpStream::connect(&target_address).await?;
    backend_stream.set_nodelay(true)?;
    let mut backend = BufStream::new(backend_stream);

    let handshake = protocol::encode_handshake(protocol_version, &target.host, target.port, 1);
    write_packet(&mut backend, HANDSHAKE_PACKET_ID, &handshake).await?;
    write_packet(&mut backend, 0, &[]).await?;

    let response = read_packet(&mut backend, max_frame_length)
        .await?
        .ok_or_else(|| anyhow!("backend closed before status response"))?;
    if response.id != 0 {
        return Err(anyhow!(
            "unexpected backend status response packet id {}",
            response.id
        ));
    }

    Ok((backend, response.payload, target))
}

async fn complete_backend_status_ping(
    backend: &mut BufStream<TcpStream>,
    payload: &[u8],
    max_frame_length: usize,
) -> Result<()> {
    write_packet(backend, 1, payload).await?;
    let pong = read_packet(backend, max_frame_length)
        .await?
        .ok_or_else(|| anyhow!("backend closed before status pong"))?;
    if pong.id != 1 {
        return Err(anyhow!(
            "unexpected backend status pong packet id {}",
            pong.id
        ));
    }
    Ok(())
}

fn lookup_location(peer: SocketAddr, ip2region: &Ip2Region, language: &Language) -> IpLocation {
    match ip2region.lookup(peer.ip()) {
        Ok(location) => location,
        Err(error) => {
            let message = language.render("log.ip2region_failed", &[]);
            warn!(%peer, ?error, "{message}");
            IpLocation::empty(peer.ip())
        }
    }
}

async fn handle_login(
    stream: &mut BufStream<TcpStream>,
    peer: SocketAddr,
    state: &RuntimeState,
    location: &IpLocation,
    handshake: Handshake,
) -> Result<()> {
    let config = &state.config;
    let dns = &state.dns;
    let security = &state.security;
    let language = &state.language;
    let Some(spec) = protocol::protocol_spec(handshake.protocol_version) else {
        let protocol_version = handshake.protocol_version.to_string();
        let supported = supported_protocols_description(&config.server.supported_protocols);
        let reason = language.render(
            "disconnect.unsupported_protocol",
            &[
                ("supported", supported.as_str()),
                ("protocol", &protocol_version),
            ],
        );
        send_login_disconnect(stream, &reason).await?;
        return Ok(());
    };

    if !config.server.supported_protocols.is_empty()
        && !config
            .server
            .supported_protocols
            .contains(&handshake.protocol_version)
    {
        let protocol_version = handshake.protocol_version.to_string();
        let supported = supported_protocols_description(&config.server.supported_protocols);
        let reason = language.render(
            "disconnect.unsupported_protocol",
            &[
                ("supported", supported.as_str()),
                ("protocol", &protocol_version),
            ],
        );
        send_login_disconnect(stream, &reason).await?;
        return Ok(());
    }

    let login_packet = read_packet(stream, config.server.max_frame_length)
        .await?
        .ok_or_else(|| anyhow!("client closed before login start"))?;
    let login_start = parse_login_start(&login_packet)?;
    if let Err(error) = validate_username(&login_start) {
        let key = match error {
            UsernameError::Length => "disconnect.invalid_username_length",
            UsernameError::Characters => "disconnect.invalid_username_characters",
        };
        let reason = language.render(key, &[]);
        send_login_disconnect(stream, &reason).await?;
        return Ok(());
    }

    if let Some(reason) = security.check(location) {
        let disconnect = language.render(reason.disconnect_key(), &[]);
        log_security_block(peer, reason, location, language);
        send_login_disconnect(stream, &disconnect).await?;
        return Ok(());
    }

    let balance_key = peer.ip().to_string();
    let Some((line, target)) = config.select_route_with_host_context(
        location,
        Some(&login_start.username),
        Some(&handshake.host),
        security,
        &balance_key,
    ) else {
        let reason = language.render("disconnect.no_route", &[]);
        send_login_disconnect(stream, &reason).await?;
        return Ok(());
    };

    let resolved_target = match dns.resolve_target(&target, target.resolve_srv).await {
        Ok(target) => target,
        Err(error) => {
            warn!(
                %peer,
                host = %target.host,
                ?error,
                "Minecraft SRV target is unavailable"
            );
            let reason = language.render("disconnect.no_route", &[]);
            send_login_disconnect(stream, &reason).await?;
            return Ok(());
        }
    };
    let target_address = format_target(&resolved_target);

    let profile_uuid = offline_uuid(&login_start.username);
    let login_success = encode_login_success(
        spec,
        profile_uuid,
        &login_start.username,
        config.server.strict_error_handling,
        Uuid::new_v4(),
    );
    write_packet(stream, LOGIN_SUCCESS_PACKET_ID, &login_success).await?;

    let acknowledgement = read_packet(stream, config.server.max_frame_length)
        .await?
        .ok_or_else(|| anyhow!("client closed before login acknowledgement"))?;
    if acknowledgement.id != LOGIN_ACKNOWLEDGED_PACKET_ID {
        return Err(anyhow!(
            "unexpected login acknowledgement packet id {}",
            acknowledgement.id
        ));
    }

    let transfer = encode_transfer(&resolved_target.host, resolved_target.port);
    write_packet(stream, spec.config_transfer_packet_id, &transfer).await?;

    let transfer_message = language.render(
        "log.transfer",
        &[
            ("player", login_start.username.as_str()),
            ("line", line.as_str()),
            ("target", target_address.as_str()),
        ],
    );
    info!(
        %peer,
        player = %login_start.username,
        protocol = handshake.protocol_version,
        node = %line,
        target = %target_address,
        country = ?location.country_code,
        province = ?location.province,
        city = ?location.city,
        isp = ?location.isp,
        "{transfer_message}"
    );
    Ok(())
}

fn log_security_block(
    peer: SocketAddr,
    reason: BlockReason,
    location: &IpLocation,
    language: &Language,
) {
    let kind = reason.label();
    let ip = peer.ip().to_string();
    let message = language.render(
        "log.security_blocked",
        &[("kind", kind), ("ip", ip.as_str())],
    );
    warn!(
        %peer,
        reason = kind,
        isp = ?location.isp,
        country = ?location.country_code,
        province = ?location.province,
        city = ?location.city,
        "{message}"
    );
}

async fn send_login_disconnect(stream: &mut BufStream<TcpStream>, reason: &str) -> Result<()> {
    let reason = serde_json::json!({ "text": reason }).to_string();
    let payload = encode_login_disconnect(&reason);
    write_packet(stream, LOGIN_DISCONNECT_PACKET_ID, &payload).await?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UsernameError {
    Length,
    Characters,
}

fn validate_username(login_start: &LoginStart) -> std::result::Result<(), UsernameError> {
    let length = login_start.username.chars().count();
    if !(3..=16).contains(&length) {
        return Err(UsernameError::Length);
    }
    if !login_start
        .username
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        return Err(UsernameError::Characters);
    }
    Ok(())
}

fn supported_protocols_description(protocols: &[i32]) -> String {
    if protocols.is_empty() {
        protocol::SUPPORTED_VERSION_RANGE.to_owned()
    } else {
        protocols
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn offline_uuid(username: &str) -> Uuid {
    let mut hasher = Md5::new();
    hasher.update(b"OfflinePlayer:");
    hasher.update(username.as_bytes());
    let mut bytes: [u8; 16] = hasher.finalize().into();

    bytes[6] = (bytes[6] & 0x0F) | 0x30;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;
    Uuid::from_bytes(bytes)
}

fn format_target(target: &LineTarget) -> String {
    if target.host.contains(':') && !target.host.starts_with('[') {
        format!("[{}]:{}", target.host, target.port)
    } else {
        format!("{}:{}", target.host, target.port)
    }
}
