//! Implementación mínima RFC 6455 WebSocket en Rust puro para el servidor CDP
//! de `wisp-agent` (sin dependencias externas ni `unsafe`).

use crate::cdp::base64_encode;
use std::io::{self, Read, Write};
use std::net::TcpStream;

const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const MAX_WS_FRAME_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsMessage {
    Text(String),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close,
}

/// SHA-1 (FIPS 180-4) exclusivamente para el handshake `Sec-WebSocket-Accept`
/// requerido por RFC 6455 §1.3.
pub fn sha1_digest(input: &[u8]) -> [u8; 20] {
    let mut h0: u32 = 0x6745_2301;
    let mut h1: u32 = 0xEFCD_AB89;
    let mut h2: u32 = 0x98BA_DCFE;
    let mut h3: u32 = 0x1032_5476;
    let mut h4: u32 = 0xC3D2_E1F0;

    let bit_len = (input.len() as u64).wrapping_mul(8);
    let mut msg = input.to_vec();
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    let (blocks, _) = msg.as_chunks::<64>();
    for chunk in blocks {
        let mut w = [0u32; 80];
        let (words, _) = chunk.as_chunks::<4>();
        for (i, word) in words.iter().enumerate() {
            w[i] = u32::from_be_bytes(*word);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let (mut a, mut b, mut c, mut d, mut e) = (h0, h1, h2, h3, h4);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1u32),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDCu32),
                _ => (b ^ c ^ d, 0xCA62_C1D6u32),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }

        h0 = h0.wrapping_add(a);
        h1 = h1.wrapping_add(b);
        h2 = h2.wrapping_add(c);
        h3 = h3.wrapping_add(d);
        h4 = h4.wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (i, word) in [h0, h1, h2, h3, h4].into_iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

pub fn websocket_accept_key(client_key: &str) -> String {
    let mut combined = String::with_capacity(client_key.trim().len() + WS_GUID.len());
    combined.push_str(client_key.trim());
    combined.push_str(WS_GUID);
    let digest = sha1_digest(combined.as_bytes());
    base64_encode(&digest)
}

pub fn write_handshake(stream: &mut TcpStream, client_key: &str) -> io::Result<()> {
    let accept = websocket_accept_key(client_key);
    let resp = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    stream.write_all(resp.as_bytes())?;
    stream.flush()
}

pub fn write_ws_frame(stream: &mut TcpStream, opcode: u8, payload: &[u8]) -> io::Result<()> {
    // Un solo `write_all`: un frame parcial deja al cliente esperando el resto
    // y rompe lecturas que asumen que cabecera y payload llegan juntos.
    let mut frame = Vec::with_capacity(payload.len() + 10);
    frame.push(0x80 | (opcode & 0x0f));
    let len = payload.len();
    if len <= 125 {
        frame.push(len as u8);
    } else if len <= 65535 {
        frame.push(126);
        frame.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        frame.push(127);
        frame.extend_from_slice(&(len as u64).to_be_bytes());
    }
    frame.extend_from_slice(payload);
    stream.write_all(&frame)?;
    stream.flush()
}

pub fn write_ws_text(stream: &mut TcpStream, text: &str) -> io::Result<()> {
    write_ws_frame(stream, 0x1, text.as_bytes())
}

pub fn write_ws_pong(stream: &mut TcpStream, payload: &[u8]) -> io::Result<()> {
    write_ws_frame(stream, 0xA, payload)
}

pub fn write_ws_close(stream: &mut TcpStream) -> io::Result<()> {
    write_ws_frame(stream, 0x8, &[])
}

pub fn read_ws_message(stream: &mut TcpStream) -> io::Result<Option<WsMessage>> {
    let mut fragmented = Vec::new();
    let mut fragmented_opcode = 0u8;

    loop {
        let mut head = [0u8; 2];
        match stream.read_exact(&mut head) {
            Ok(()) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
                ) =>
            {
                return Ok(None);
            }
            Err(e) => return Err(e),
        }

        let fin = (head[0] & 0x80) != 0;
        let opcode = head[0] & 0x0f;
        let masked = (head[1] & 0x80) != 0;
        let len_code = head[1] & 0x7f;

        let len: usize = match len_code {
            0..=125 => len_code as usize,
            126 => {
                let mut ext = [0u8; 2];
                stream.read_exact(&mut ext)?;
                u16::from_be_bytes(ext) as usize
            }
            _ => {
                let mut ext = [0u8; 8];
                stream.read_exact(&mut ext)?;
                let raw = u64::from_be_bytes(ext);
                if raw > MAX_WS_FRAME_BYTES as u64 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "WebSocket frame too large",
                    ));
                }
                raw as usize
            }
        };

        if len > MAX_WS_FRAME_BYTES || fragmented.len() + len > MAX_WS_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "WebSocket payload exceeds limit",
            ));
        }

        let mask = if masked {
            let mut m = [0u8; 4];
            stream.read_exact(&mut m)?;
            Some(m)
        } else {
            None
        };

        let mut payload = vec![0u8; len];
        stream.read_exact(&mut payload)?;
        if let Some(m) = mask {
            for (i, b) in payload.iter_mut().enumerate() {
                *b ^= m[i % 4];
            }
        }

        match opcode {
            0x8 => return Ok(Some(WsMessage::Close)),
            0x9 => return Ok(Some(WsMessage::Ping(payload))),
            0xA => return Ok(Some(WsMessage::Pong(payload))),
            0x0 => {
                fragmented.extend_from_slice(&payload);
                if fin {
                    let msg = match fragmented_opcode {
                        0x1 => String::from_utf8(fragmented)
                            .map(WsMessage::Text)
                            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
                        _ => WsMessage::Text(String::from_utf8_lossy(&fragmented).into_owned()),
                    };
                    return Ok(Some(msg));
                }
            }
            0x1 | 0x2 => {
                if fin {
                    let text = String::from_utf8(payload)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                    return Ok(Some(WsMessage::Text(text)));
                } else {
                    fragmented_opcode = opcode;
                    fragmented.extend_from_slice(&payload);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc6455_accept_key_matches_spec_example() {
        assert_eq!(
            websocket_accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn roundtrip_masked_client_frame_and_server_text_frame() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let mut client = TcpStream::connect(addr).unwrap();
            let payload = br#"{"id":1,"method":"Runtime.enable"}"#;
            let mask = [0x37, 0xfa, 0x21, 0x3d];
            let mut frame = vec![0x81, 0x80 | (payload.len() as u8)];
            frame.extend_from_slice(&mask);
            for (i, b) in payload.iter().enumerate() {
                frame.push(b ^ mask[i % 4]);
            }
            client.write_all(&frame).unwrap();
            let mut buf = [0u8; 64];
            let n = client.read(&mut buf).unwrap();
            assert_eq!(buf[0], 0x81);
            assert_eq!(&buf[2..n], br#"{"id":1,"result":{}}"#);
        });

        let (mut server, _) = listener.accept().unwrap();
        let msg = read_ws_message(&mut server).unwrap().unwrap();
        assert_eq!(
            msg,
            WsMessage::Text(r#"{"id":1,"method":"Runtime.enable"}"#.to_owned())
        );
        write_ws_text(&mut server, r#"{"id":1,"result":{}}"#).unwrap();
        handle.join().unwrap();
    }
}
