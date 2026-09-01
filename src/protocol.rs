use std::{io, str::Utf8Error};

use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use uuid::Uuid;

pub const HANDSHAKE_PACKET_ID: i32 = 0x00;
pub const LOGIN_START_PACKET_ID: i32 = 0x00;
pub const LOGIN_SUCCESS_PACKET_ID: i32 = 0x02;
pub const LOGIN_ACKNOWLEDGED_PACKET_ID: i32 = 0x03;
pub const LOGIN_DISCONNECT_PACKET_ID: i32 = 0x00;
pub const CONFIG_TRANSFER_PACKET_ID: i32 = 0x0B;
pub const CONFIG_TRANSFER_PACKET_ID_26_3: i32 = 0x0C;

pub const FIRST_TRANSFER_SNAPSHOT_PROTOCOL: i32 = 1_073_741_995;
pub const LATEST_SNAPSHOT_PROTOCOL: i32 = 1_073_742_156;
pub const LAST_STRICT_ERROR_HANDLING_SNAPSHOT_PROTOCOL: i32 = 1_073_742_033;
pub const FIRST_SESSION_ID_RELEASE_PROTOCOL: i32 = 776;
pub const FIRST_SESSION_ID_SNAPSHOT_PROTOCOL: i32 = 1_073_742_149;
pub const FIRST_TRANSFER_PACKET_ID_26_3_SNAPSHOT_PROTOCOL: i32 = 1_073_742_149;

pub const SUPPORTED_VERSION_RANGE: &str = "Java 1.20.5 through 26.3 Snapshot 10";

const MAX_VARINT_BYTES: usize = 5;

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("unexpected end of packet")]
    UnexpectedEof,
    #[error("VarInt is longer than five bytes")]
    VarIntTooLong,
    #[error("negative packet length")]
    NegativePacketLength,
    #[error("packet length {length} exceeds limit {limit}")]
    PacketTooLarge { length: usize, limit: usize },
    #[error("invalid UTF-8 string")]
    InvalidUtf8(#[from] Utf8Error),
    #[error("string length {length} exceeds limit {limit}")]
    StringTooLong { length: usize, limit: usize },
    #[error("unexpected packet id {actual} (expected {expected})")]
    UnexpectedPacketId { actual: i32, expected: i32 },
    #[error("invalid protocol value: {0}")]
    InvalidValue(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub id: i32,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolSpec {
    pub version: i32,
    pub login_success_has_strict_error_handling: bool,
    pub login_success_has_session_id: bool,
    pub config_transfer_packet_id: i32,
}

pub fn protocol_spec(version: i32) -> Option<ProtocolSpec> {
    let is_release = (766..=776).contains(&version);
    let is_snapshot =
        (FIRST_TRANSFER_SNAPSHOT_PROTOCOL..=LATEST_SNAPSHOT_PROTOCOL).contains(&version);

    if !is_release && !is_snapshot {
        return None;
    }

    let login_success_has_strict_error_handling = match version {
        766..=767 => true,
        768..=776 => false,
        _ => version <= LAST_STRICT_ERROR_HANDLING_SNAPSHOT_PROTOCOL,
    };
    let login_success_has_session_id = version == FIRST_SESSION_ID_RELEASE_PROTOCOL
        || (is_snapshot && version >= FIRST_SESSION_ID_SNAPSHOT_PROTOCOL);

    Some(ProtocolSpec {
        version,
        login_success_has_strict_error_handling,
        login_success_has_session_id,
        config_transfer_packet_id: if is_snapshot
            && version >= FIRST_TRANSFER_PACKET_ID_26_3_SNAPSHOT_PROTOCOL
        {
            CONFIG_TRANSFER_PACKET_ID_26_3
        } else {
            CONFIG_TRANSFER_PACKET_ID
        },
    })
}

pub fn is_supported_protocol(version: i32) -> bool {
    protocol_spec(version).is_some()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NextState {
    Status,
    Login,
    Transfer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handshake {
    pub protocol_version: i32,
    pub host: String,
    pub port: u16,
    pub next_state: NextState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginStart {
    pub username: String,
    pub uuid: Uuid,
}

pub async fn read_packet<R: AsyncRead + Unpin>(
    reader: &mut R,
    max_frame_length: usize,
) -> Result<Option<Packet>, ProtocolError> {
    let Some(length) = read_varint_async(reader, true).await? else {
        return Ok(None);
    };
    if length < 0 {
        return Err(ProtocolError::NegativePacketLength);
    }

    let length = length as usize;
    if length > max_frame_length {
        return Err(ProtocolError::PacketTooLarge {
            length,
            limit: max_frame_length,
        });
    }

    let mut frame = vec![0; length];
    reader.read_exact(&mut frame).await?;

    let mut packet_reader = PacketReader::new(&frame);
    let id = packet_reader.read_varint()?;
    let payload = frame[packet_reader.position()..].to_vec();
    Ok(Some(Packet { id, payload }))
}

pub async fn write_packet<W: AsyncWrite + Unpin>(
    writer: &mut W,
    id: i32,
    payload: &[u8],
) -> Result<(), ProtocolError> {
    let mut body = Vec::with_capacity(5 + payload.len());
    write_varint(&mut body, id);
    body.extend_from_slice(payload);

    let mut frame = Vec::with_capacity(5 + body.len());
    write_varint(&mut frame, body.len() as i32);
    frame.extend_from_slice(&body);

    writer.write_all(&frame).await?;
    writer.flush().await?;
    Ok(())
}

pub fn encode_handshake(protocol_version: i32, host: &str, port: u16, next_state: i32) -> Vec<u8> {
    let mut payload = Vec::with_capacity(host.len() + 16);
    write_varint(&mut payload, protocol_version);
    write_string(&mut payload, host);
    payload.extend_from_slice(&port.to_be_bytes());
    write_varint(&mut payload, next_state);
    payload
}

pub fn parse_handshake(packet: &Packet) -> Result<Handshake, ProtocolError> {
    if packet.id != HANDSHAKE_PACKET_ID {
        return Err(ProtocolError::UnexpectedPacketId {
            actual: packet.id,
            expected: HANDSHAKE_PACKET_ID,
        });
    }

    let mut reader = PacketReader::new(&packet.payload);
    let protocol_version = reader.read_varint()?;
    let host = reader.read_string(255)?;
    let port = reader.read_u16()?;
    let next_state = match reader.read_varint()? {
        1 => NextState::Status,
        2 => NextState::Login,
        3 => NextState::Transfer,
        _ => {
            return Err(ProtocolError::InvalidValue(
                "unsupported handshake next state",
            ));
        }
    };

    reader.expect_end()?;
    Ok(Handshake {
        protocol_version,
        host,
        port,
        next_state,
    })
}

pub fn parse_login_start(packet: &Packet) -> Result<LoginStart, ProtocolError> {
    if packet.id != LOGIN_START_PACKET_ID {
        return Err(ProtocolError::UnexpectedPacketId {
            actual: packet.id,
            expected: LOGIN_START_PACKET_ID,
        });
    }

    let mut reader = PacketReader::new(&packet.payload);
    let username = reader.read_string(64)?;
    let uuid = reader.read_uuid()?;
    reader.expect_end()?;
    Ok(LoginStart { username, uuid })
}

pub fn encode_login_success(
    spec: ProtocolSpec,
    uuid: Uuid,
    username: &str,
    strict_error_handling: bool,
    session_id: Uuid,
) -> Vec<u8> {
    let mut payload = Vec::with_capacity(48 + username.len());
    payload.extend_from_slice(uuid.as_bytes());
    write_string(&mut payload, username);
    write_varint(&mut payload, 0); // profile properties length
    if spec.login_success_has_strict_error_handling {
        payload.push(u8::from(strict_error_handling));
    }
    if spec.login_success_has_session_id {
        payload.extend_from_slice(session_id.as_bytes());
    }
    payload
}

pub fn encode_transfer(host: &str, port: u16) -> Vec<u8> {
    let mut payload = Vec::with_capacity(host.len() + 8);
    write_string(&mut payload, host);
    write_varint(&mut payload, i32::from(port));
    payload
}

pub fn encode_login_disconnect(reason: &str) -> Vec<u8> {
    let mut payload = Vec::with_capacity(reason.len() + 5);
    write_string(&mut payload, reason);
    payload
}

pub fn legacy_text_to_json(text: &str) -> serde_json::Value {
    let characters: Vec<char> = text.chars().collect();
    let mut components = Vec::new();
    let mut style = LegacyStyle::default();
    let mut buffer = String::new();
    let mut index = 0;

    while index < characters.len() {
        let character = characters[index];
        if (character == '&' || character == '§') && index + 1 < characters.len() {
            if let Some((color, consumed)) = parse_hex_color(&characters, index, character) {
                push_text_component(&mut components, &mut buffer, &style);
                style = LegacyStyle {
                    color: Some(color),
                    ..LegacyStyle::default()
                };
                index += consumed;
                continue;
            }

            let code = characters[index + 1].to_ascii_lowercase();
            if let Some(color) = legacy_color(code) {
                push_text_component(&mut components, &mut buffer, &style);
                style = LegacyStyle {
                    color: Some(color.to_owned()),
                    ..LegacyStyle::default()
                };
                index += 2;
                continue;
            }

            let previous_style = style.clone();
            if apply_legacy_format(&mut style, code) {
                push_text_component(&mut components, &mut buffer, &previous_style);
                index += 2;
                continue;
            }
        }

        buffer.push(character);
        index += 1;
    }
    push_text_component(&mut components, &mut buffer, &style);

    let mut root = serde_json::Map::new();
    root.insert("text".to_owned(), serde_json::Value::String(String::new()));
    if !components.is_empty() {
        root.insert("extra".to_owned(), serde_json::Value::Array(components));
    }
    serde_json::Value::Object(root)
}

#[derive(Debug, Clone, Default)]
struct LegacyStyle {
    color: Option<String>,
    obfuscated: bool,
    bold: bool,
    strikethrough: bool,
    underlined: bool,
    italic: bool,
}

fn push_text_component(
    components: &mut Vec<serde_json::Value>,
    buffer: &mut String,
    style: &LegacyStyle,
) {
    if buffer.is_empty() {
        return;
    }

    let mut component = serde_json::Map::new();
    component.insert(
        "text".to_owned(),
        serde_json::Value::String(std::mem::take(buffer)),
    );
    if let Some(color) = &style.color {
        component.insert("color".to_owned(), serde_json::Value::String(color.clone()));
    }
    if style.obfuscated {
        component.insert("obfuscated".to_owned(), serde_json::Value::Bool(true));
    }
    if style.bold {
        component.insert("bold".to_owned(), serde_json::Value::Bool(true));
    }
    if style.strikethrough {
        component.insert("strikethrough".to_owned(), serde_json::Value::Bool(true));
    }
    if style.underlined {
        component.insert("underlined".to_owned(), serde_json::Value::Bool(true));
    }
    if style.italic {
        component.insert("italic".to_owned(), serde_json::Value::Bool(true));
    }
    components.push(serde_json::Value::Object(component));
}

fn parse_hex_color(characters: &[char], index: usize, marker: char) -> Option<(String, usize)> {
    if characters.get(index + 1) == Some(&'#') {
        let end = index + 8;
        let digits = characters.get(index + 2..end)?;
        if digits.iter().all(char::is_ascii_hexdigit) {
            return Some((format!("#{}", digits.iter().collect::<String>()), 8));
        }
    }

    if characters
        .get(index + 1)
        .map(|character| character.eq_ignore_ascii_case(&'x'))
        != Some(true)
    {
        return None;
    }

    let mut digits = String::with_capacity(6);
    for offset in 0..6 {
        let separator = characters.get(index + 2 + offset * 2)?;
        let digit = *characters.get(index + 3 + offset * 2)?;
        if *separator != marker && *separator != '&' && *separator != '§' {
            return None;
        }
        if !digit.is_ascii_hexdigit() {
            return None;
        }
        digits.push(digit);
    }
    Some((format!("#{digits}"), 14))
}

fn apply_legacy_format(style: &mut LegacyStyle, code: char) -> bool {
    match code {
        'k' => style.obfuscated = true,
        'l' => style.bold = true,
        'm' => style.strikethrough = true,
        'n' => style.underlined = true,
        'o' => style.italic = true,
        'r' => *style = LegacyStyle::default(),
        _ => return false,
    }
    true
}

fn legacy_color(code: char) -> Option<&'static str> {
    Some(match code {
        '0' => "black",
        '1' => "dark_blue",
        '2' => "dark_green",
        '3' => "dark_aqua",
        '4' => "dark_red",
        '5' => "dark_purple",
        '6' => "gold",
        '7' => "gray",
        '8' => "dark_gray",
        '9' => "blue",
        'a' => "green",
        'b' => "aqua",
        'c' => "red",
        'd' => "light_purple",
        'e' => "yellow",
        'f' => "white",
        _ => return None,
    })
}

pub fn write_varint(output: &mut Vec<u8>, value: i32) {
    let mut value = value as u32;
    loop {
        if (value & !0x7F) == 0 {
            output.push(value as u8);
            return;
        }
        output.push(((value & 0x7F) as u8) | 0x80);
        value >>= 7;
    }
}

pub fn write_string(output: &mut Vec<u8>, value: &str) {
    write_varint(output, value.len() as i32);
    output.extend_from_slice(value.as_bytes());
}

pub struct PacketReader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> PacketReader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    pub fn position(&self) -> usize {
        self.position
    }

    pub fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.position)
    }

    pub fn read_varint(&mut self) -> Result<i32, ProtocolError> {
        let mut result = 0u32;

        for index in 0..MAX_VARINT_BYTES {
            let byte = self.read_u8()?;
            result |= u32::from(byte & 0x7F) << (index * 7);
            if byte & 0x80 == 0 {
                return Ok(result as i32);
            }
        }

        Err(ProtocolError::VarIntTooLong)
    }

    pub fn read_string(&mut self, max_bytes: usize) -> Result<String, ProtocolError> {
        let length = self.read_varint()?;
        if length < 0 {
            return Err(ProtocolError::InvalidValue("negative string length"));
        }

        let length = length as usize;
        if length > max_bytes {
            return Err(ProtocolError::StringTooLong {
                length,
                limit: max_bytes,
            });
        }

        let bytes = self.read_bytes(length)?;
        Ok(std::str::from_utf8(bytes)?.to_owned())
    }

    pub fn read_uuid(&mut self) -> Result<Uuid, ProtocolError> {
        let bytes = self.read_bytes(16)?;
        let mut uuid = [0u8; 16];
        uuid.copy_from_slice(bytes);
        Ok(Uuid::from_bytes(uuid))
    }

    pub fn read_u16(&mut self) -> Result<u16, ProtocolError> {
        let bytes = self.read_bytes(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    pub fn read_i64(&mut self) -> Result<i64, ProtocolError> {
        let bytes = self.read_bytes(8)?;
        Ok(i64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    pub fn expect_end(&self) -> Result<(), ProtocolError> {
        if self.remaining() == 0 {
            Ok(())
        } else {
            Err(ProtocolError::InvalidValue("trailing packet data"))
        }
    }

    fn read_u8(&mut self) -> Result<u8, ProtocolError> {
        let byte = *self
            .bytes
            .get(self.position)
            .ok_or(ProtocolError::UnexpectedEof)?;
        self.position += 1;
        Ok(byte)
    }

    fn read_bytes(&mut self, length: usize) -> Result<&'a [u8], ProtocolError> {
        let end = self
            .position
            .checked_add(length)
            .ok_or(ProtocolError::UnexpectedEof)?;
        let bytes = self
            .bytes
            .get(self.position..end)
            .ok_or(ProtocolError::UnexpectedEof)?;
        self.position = end;
        Ok(bytes)
    }
}

async fn read_varint_async<R: AsyncRead + Unpin>(
    reader: &mut R,
    allow_eof: bool,
) -> Result<Option<i32>, ProtocolError> {
    let mut result = 0u32;

    for index in 0..MAX_VARINT_BYTES {
        let byte = match reader.read_u8().await {
            Ok(byte) => byte,
            Err(error)
                if allow_eof && index == 0 && error.kind() == io::ErrorKind::UnexpectedEof =>
            {
                return Ok(None);
            }
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                return Err(ProtocolError::UnexpectedEof);
            }
            Err(error) => return Err(ProtocolError::Io(error)),
        };

        result |= u32::from(byte & 0x7F) << (index * 7);
        if byte & 0x80 == 0 {
            return Ok(Some(result as i32));
        }
    }

    Err(ProtocolError::VarIntTooLong)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uuid(byte: u8) -> Uuid {
        Uuid::from_bytes([byte; 16])
    }

    #[test]
    fn login_success_fields_match_protocol_boundaries() {
        let legacy = protocol_spec(767).expect("protocol 767 should be supported");
        assert!(legacy.login_success_has_strict_error_handling);
        assert!(!legacy.login_success_has_session_id);
        assert_eq!(legacy.config_transfer_packet_id, CONFIG_TRANSFER_PACKET_ID);

        let modern = protocol_spec(768).expect("protocol 768 should be supported");
        assert!(!modern.login_success_has_strict_error_handling);
        assert!(!modern.login_success_has_session_id);
        assert_eq!(modern.config_transfer_packet_id, CONFIG_TRANSFER_PACKET_ID);

        let release_26_2 =
            protocol_spec(FIRST_SESSION_ID_RELEASE_PROTOCOL).expect("protocol 776 should exist");
        assert!(!release_26_2.login_success_has_strict_error_handling);
        assert!(release_26_2.login_success_has_session_id);
        assert_eq!(
            release_26_2.config_transfer_packet_id,
            CONFIG_TRANSFER_PACKET_ID
        );

        let older_snapshot = protocol_spec(FIRST_TRANSFER_SNAPSHOT_PROTOCOL)
            .expect("the first transfer snapshot should be supported");
        assert!(!older_snapshot.login_success_has_session_id);
        assert_eq!(
            older_snapshot.config_transfer_packet_id,
            CONFIG_TRANSFER_PACKET_ID
        );

        let snapshot_26_3 = protocol_spec(FIRST_SESSION_ID_SNAPSHOT_PROTOCOL)
            .expect("the first session-id snapshot should be supported");
        assert!(snapshot_26_3.login_success_has_session_id);
        assert_eq!(
            snapshot_26_3.config_transfer_packet_id,
            CONFIG_TRANSFER_PACKET_ID_26_3
        );
    }

    #[test]
    fn protocol_776_login_success_contains_session_id() {
        let spec = protocol_spec(FIRST_SESSION_ID_RELEASE_PROTOCOL).unwrap();
        let session_id = uuid(0x22);
        let payload = encode_login_success(spec, uuid(0x11), "Steve", false, session_id);

        assert_eq!(&payload[payload.len() - 16..], session_id.as_bytes());
    }

    #[test]
    fn parses_transfer_handshake_as_login() {
        let packet = Packet {
            id: HANDSHAKE_PACKET_ID,
            payload: encode_handshake(776, "gateway.example.com", 25565, 3),
        };

        let handshake = parse_handshake(&packet).expect("transfer handshake should parse");
        assert_eq!(handshake.next_state, NextState::Transfer);
    }
}
