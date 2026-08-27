use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Result, anyhow};
use md5::{Digest, Md5};
use tokio::{
    io::{AsyncWriteExt, BufStream},
    net::{TcpListener, TcpStream},
    sync::Semaphore,
    time::timeout,
};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{
    config::{AppConfig, LineTarget},
    ip2region::{Ip2Region, IpLocation},
    language::Language,
    protocol::{
        self, HANDSHAKE_PACKET_ID, Handshake, LOGIN_ACKNOWLEDGED_PACKET_ID,
        LOGIN_DISCONNECT_PACKET_ID, LOGIN_SUCCESS_PACKET_ID, LoginStart, NextState,
        encode_login_disconnect, encode_login_success, encode_transfer, parse_handshake,
        parse_login_start, read_packet, write_packet,
    },
};

pub async fn run(
    config: Arc<AppConfig>,
    ip2region: Arc<Ip2Region>,
    language: Arc<Language>,
) -> Result<()> {
    let listener = TcpListener::bind(&config.server.bind).await?;
    let connections = Arc::new(Semaphore::new(config.server.max_connections));
    let login_timeout = Duration::from_millis(config.server.login_timeout_ms);

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, peer) = accepted?;
                let Some(permit) = connections.clone().try_acquire_owned().ok() else {
                    let message = language.render("log.connection_limit", &[]);
                    warn!(%peer, "{message}");
                    continue;
                };

                let config = Arc::clone(&config);
                let ip2region = Arc::clone(&ip2region);
                let language = Arc::clone(&language);
                tokio::spawn(async move {
                    let _permit = permit;
                    let result = timeout(
                        login_timeout,
                        handle_connection(stream, peer, config, ip2region, language),
                    )
                    .await;

                    match result {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => debug!(%peer, ?error, "connection finished with error"),
                        Err(_) => debug!(%peer, "connection timed out"),
                    }
                });
            }
            signal = tokio::signal::ctrl_c() => {
                signal?;
                info!("shutdown signal received");
                return Ok(());
            }
        }
    }
}

async fn handle_connection(
    stream: TcpStream,
    peer: SocketAddr,
    config: Arc<AppConfig>,
    ip2region: Arc<Ip2Region>,
    language: Arc<Language>,
) -> Result<()> {
    stream.set_nodelay(true)?;
    let mut stream = BufStream::new(stream);
    let max_frame_length = config.server.max_frame_length;

    let handshake_packet = read_packet(&mut stream, max_frame_length)
        .await?
        .ok_or_else(|| anyhow!("client closed before handshake"))?;
    let handshake = parse_handshake(&handshake_packet)?;

    match handshake.next_state {
        NextState::Status => {
            handle_status(&mut stream, peer, &config, &ip2region, &language, handshake).await?
        }
        NextState::Login => {
            handle_login(&mut stream, peer, &config, &ip2region, &language, handshake).await?
        }
    }

    stream.shutdown().await?;
    Ok(())
}

async fn handle_status(
    stream: &mut BufStream<TcpStream>,
    peer: SocketAddr,
    config: &AppConfig,
    ip2region: &Ip2Region,
    language: &Language,
    handshake: Handshake,
) -> Result<()> {
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

    let location = lookup_location(peer, ip2region, language);
    let (mut backend, status_payload) = match config.routing.select_route(&location) {
        Some((line, target)) => {
            let target_address = format_target(target);
            match proxy_status_response(handshake.protocol_version, target, max_frame_length).await
            {
                Ok((backend, payload)) => {
                    debug!(
                        %peer,
                        node = line,
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
                        node = line,
                        target = %target_address,
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
    target: &LineTarget,
    max_frame_length: usize,
) -> Result<(BufStream<TcpStream>, Vec<u8>)> {
    let target_address = format_target(target);
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

    Ok((backend, response.payload))
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
    config: &AppConfig,
    ip2region: &Ip2Region,
    language: &Language,
    handshake: Handshake,
) -> Result<()> {
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

    let location = lookup_location(peer, ip2region, language);

    let Some((line, target)) = config.routing.select_route(&location) else {
        let reason = language.render("disconnect.no_route", &[]);
        send_login_disconnect(stream, &reason).await?;
        return Ok(());
    };

    let target = target.clone();
    let target_address = format_target(&target);

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

    let transfer = encode_transfer(&target.host, target.port);
    write_packet(stream, spec.config_transfer_packet_id, &transfer).await?;

    let transfer_message = language.render(
        "log.transfer",
        &[
            ("player", login_start.username.as_str()),
            ("line", line),
            ("target", target_address.as_str()),
        ],
    );
    info!(
        %peer,
        player = %login_start.username,
        protocol = handshake.protocol_version,
        node = line,
        target = %target_address,
        country = ?location.country_code,
        province = ?location.province,
        city = ?location.city,
        isp = ?location.isp,
        "{transfer_message}"
    );
    Ok(())
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