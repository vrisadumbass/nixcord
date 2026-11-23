//! Discord 74-byte UDP IP Discovery request builder, response parser, and async helper.
//!
//! Wire layout (74 bytes total, network byte order):
//! - `0..2`:  Type (`0x0001` = Request, `0x0002` = Response)
//! - `2..4`:  Length (`0x0046` = 70)
//! - `4..8`:  SSRC (`u32`)
//! - `8..72`: Address (64-byte null-terminated ASCII IP string, zero-padded)
//! - `72..74`: Port (`u16`)

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::time::{timeout, Instant};

pub const IP_DISCOVERY_PACKET_LEN: usize = 74;
pub const IP_DISCOVERY_TYPE_REQUEST: u16 = 0x0001;
pub const IP_DISCOVERY_TYPE_RESPONSE: u16 = 0x0002;
pub const IP_DISCOVERY_PAYLOAD_LEN: u16 = 70;

const DEFAULT_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum UdpError {
    #[error("invalid IP discovery packet length: expected {IP_DISCOVERY_PACKET_LEN}, got {0}")]
    InvalidLength(usize),
    #[error("invalid IP discovery packet type: expected {IP_DISCOVERY_TYPE_RESPONSE:#06x}, got {0:#06x}")]
    InvalidPacketType(u16),
    #[error("invalid IP discovery length field: expected {IP_DISCOVERY_PAYLOAD_LEN}, got {0}")]
    InvalidLengthField(u16),
    #[error("IP discovery SSRC mismatch: expected {expected}, got {got}")]
    SsrcMismatch { expected: u32, got: u32 },
    #[error("invalid IP address in IP discovery response")]
    InvalidAddress,
    #[error("UDP IP discovery timed out")]
    Timeout,
    #[error("UDP I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Build a 74-byte Discord UDP IP Discovery request packet for `ssrc`.
pub fn build_ip_discovery_request(ssrc: u32) -> [u8; IP_DISCOVERY_PACKET_LEN] {
    let mut pkt = [0u8; IP_DISCOVERY_PACKET_LEN];
    pkt[0..2].copy_from_slice(&IP_DISCOVERY_TYPE_REQUEST.to_be_bytes());
    pkt[2..4].copy_from_slice(&IP_DISCOVERY_PAYLOAD_LEN.to_be_bytes());
    pkt[4..8].copy_from_slice(&ssrc.to_be_bytes());
    pkt
}

/// Parse a 74-byte Discord UDP IP Discovery response packet and verify its SSRC.
pub fn parse_ip_discovery_response(
    packet: &[u8],
    expected_ssrc: u32,
) -> Result<SocketAddr, UdpError> {
    if packet.len() != IP_DISCOVERY_PACKET_LEN {
        return Err(UdpError::InvalidLength(packet.len()));
    }
    let pkt_type = u16::from_be_bytes([packet[0], packet[1]]);
    if pkt_type != IP_DISCOVERY_TYPE_RESPONSE {
        return Err(UdpError::InvalidPacketType(pkt_type));
    }
    let payload_len = u16::from_be_bytes([packet[2], packet[3]]);
    if payload_len != IP_DISCOVERY_PAYLOAD_LEN {
        return Err(UdpError::InvalidLengthField(payload_len));
    }
    let ssrc = u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]);
    if ssrc != expected_ssrc {
        return Err(UdpError::SsrcMismatch {
            expected: expected_ssrc,
            got: ssrc,
        });
    }

    let addr_field = &packet[8..72];
    let nul_pos = addr_field
        .iter()
        .position(|&b| b == 0)
        .ok_or(UdpError::InvalidAddress)?;
    let addr_str =
        std::str::from_utf8(&addr_field[..nul_pos]).map_err(|_| UdpError::InvalidAddress)?;
    let ip: IpAddr = addr_str.parse().map_err(|_| UdpError::InvalidAddress)?;
    let port = u16::from_be_bytes([packet[72], packet[73]]);

    Ok(SocketAddr::new(ip, port))
}

/// Perform Discord UDP IP Discovery against `server_addr` using `socket`.
pub async fn discover_ip(
    socket: &UdpSocket,
    server_addr: SocketAddr,
    ssrc: u32,
) -> Result<SocketAddr, UdpError> {
    discover_ip_with_timeout(socket, server_addr, ssrc, DEFAULT_DISCOVERY_TIMEOUT).await
}

/// Perform Discord UDP IP Discovery with an explicit timeout.
pub async fn discover_ip_with_timeout(
    socket: &UdpSocket,
    server_addr: SocketAddr,
    ssrc: u32,
    wait: Duration,
) -> Result<SocketAddr, UdpError> {
    let req = build_ip_discovery_request(ssrc);
    socket.send_to(&req, server_addr).await?;

    let deadline = Instant::now() + wait;
    let mut buf = [0u8; 256];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(UdpError::Timeout);
        }
        let (len, from) = timeout(remaining, socket.recv_from(&mut buf))
            .await
            .map_err(|_| UdpError::Timeout)??;
        if from != server_addr {
            continue;
        }
        return parse_ip_discovery_response(&buf[..len], ssrc);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_response(ssrc: u32, ip: &str, port: u16) -> [u8; IP_DISCOVERY_PACKET_LEN] {
        let mut pkt = [0u8; IP_DISCOVERY_PACKET_LEN];
        pkt[0..2].copy_from_slice(&IP_DISCOVERY_TYPE_RESPONSE.to_be_bytes());
        pkt[2..4].copy_from_slice(&IP_DISCOVERY_PAYLOAD_LEN.to_be_bytes());
        pkt[4..8].copy_from_slice(&ssrc.to_be_bytes());
        pkt[8..8 + ip.len()].copy_from_slice(ip.as_bytes());
        pkt[72..74].copy_from_slice(&port.to_be_bytes());
        pkt
    }

    #[test]
    fn request_and_response_round_trip() {
        let req = build_ip_discovery_request(0x1122_3344);
        assert_eq!(req.len(), 74);
        assert_eq!(&req[0..2], &[0x00, 0x01]);
        assert_eq!(&req[2..4], &[0x00, 0x46]);
        assert_eq!(&req[4..8], &[0x11, 0x22, 0x33, 0x44]);
        assert!(req[8..].iter().all(|&b| b == 0));

        let resp = build_response(0x1122_3344, "203.0.113.9", 54321);
        let parsed = parse_ip_discovery_response(&resp, 0x1122_3344).unwrap();
        assert_eq!(parsed, "203.0.113.9:54321".parse::<SocketAddr>().unwrap());

        let resp_v6 = build_response(7, "2001:db8::1", 40000);
        let parsed_v6 = parse_ip_discovery_response(&resp_v6, 7).unwrap();
        assert_eq!(
            parsed_v6,
            "[2001:db8::1]:40000".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn malformed_responses_are_rejected() {
        // Short packet
        assert!(matches!(
            parse_ip_discovery_response(&[0u8; 73], 1),
            Err(UdpError::InvalidLength(73))
        ));

        // Wrong type (request instead of response)
        let req = build_ip_discovery_request(1);
        assert!(matches!(
            parse_ip_discovery_response(&req, 1),
            Err(UdpError::InvalidPacketType(0x0001))
        ));

        // Wrong length field
        let mut resp = build_response(1, "198.51.100.2", 5000);
        resp[2..4].copy_from_slice(&69u16.to_be_bytes());
        assert!(matches!(
            parse_ip_discovery_response(&resp, 1),
            Err(UdpError::InvalidLengthField(69))
        ));

        // Wrong SSRC
        let resp = build_response(2, "198.51.100.2", 5000);
        assert!(matches!(
            parse_ip_discovery_response(&resp, 1),
            Err(UdpError::SsrcMismatch {
                expected: 1,
                got: 2
            })
        ));

        // Invalid IP text
        let resp = build_response(1, "not-an-ip", 5000);
        assert!(matches!(
            parse_ip_discovery_response(&resp, 1),
            Err(UdpError::InvalidAddress)
        ));

        // Missing null terminator
        let mut resp = build_response(1, "198.51.100.2", 5000);
        resp[8..72].fill(b'1');
        assert!(matches!(
            parse_ip_discovery_response(&resp, 1),
            Err(UdpError::InvalidAddress)
        ));
    }
}
