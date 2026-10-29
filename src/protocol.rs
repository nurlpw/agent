use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    Request {
        id: u64,
        method: String,
        uri: String,
        headers: Vec<(String, String)>,
    },
    Response {
        id: u64,
        status: u16,
        headers: Vec<(String, String)>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        upstream_ms: Option<u64>,
    },
    Error {
        id: u64,
        message: String,
    },

    WsOpen {
        id: u64,
        uri: String,
        headers: Vec<(String, String)>,
    },
    WsOpened {
        id: u64,
        protocol: Option<String>,
    },
    WsRejected {
        id: u64,
        status: u16,
        message: String,
    },
    WsMessage {
        id: u64,
        binary: bool,
    },
    WsClose {
        id: u64,
    },

    Shutdown {
        reason: String,
        message: String,
    },
}

pub fn encode(frame: &impl Serialize, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + 512 + body.len());
    out.extend_from_slice(&[0; 4]);
    serde_json::to_writer(&mut out, frame).expect("frame is always serializable");
    let len = u32::try_from(out.len() - 4).expect("frame header fits in u32");
    out[..4].copy_from_slice(&len.to_be_bytes());
    out.extend_from_slice(body);
    out
}

pub fn decode(data: &[u8]) -> Option<(Frame, &[u8])> {
    let len = u32::from_be_bytes(data.get(..4)?.try_into().ok()?) as usize;
    let header = data.get(4..4 + len)?;
    let frame = serde_json::from_slice(header).ok()?;
    Some((frame, &data[4 + len..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let frame = Frame::Response {
            id: 7,
            status: 201,
            headers: vec![("content-type".into(), "image/png".into())],
            upstream_ms: Some(12),
        };
        let body = [0u8, 159, 146, 150];
        let encoded = encode(&frame, &body);
        let (decoded, decoded_body) = decode(&encoded).unwrap();
        assert!(matches!(
            decoded,
            Frame::Response {
                id: 7,
                status: 201,
                upstream_ms: Some(12),
                ..
            }
        ));
        assert_eq!(decoded_body, body);
    }

    #[test]
    fn response_from_older_agent_has_no_timing() {
        let encoded = encode_raw(br#"{"type":"response","id":1,"status":200,"headers":[]}"#);
        let (decoded, _) = decode(&encoded).unwrap();
        assert!(matches!(
            decoded,
            Frame::Response {
                upstream_ms: None,
                ..
            }
        ));
    }

    #[test]
    fn shutdown_roundtrip() {
        let frame = Frame::Shutdown {
            reason: "stopped".into(),
            message: "bye".into(),
        };
        let (decoded, _) = decode(&encode(&frame, &[])).unwrap();
        assert!(matches!(decoded, Frame::Shutdown { reason, .. } if reason == "stopped"));
    }

    #[test]
    fn ws_frames_roundtrip() {
        let encoded = encode(
            &Frame::WsMessage {
                id: 3,
                binary: false,
            },
            b"{\"type\":\"update\"}",
        );
        let (decoded, body) = decode(&encoded).unwrap();
        assert!(matches!(
            decoded,
            Frame::WsMessage {
                id: 3,
                binary: false
            }
        ));
        assert_eq!(body, b"{\"type\":\"update\"}");

        let encoded = encode(
            &Frame::WsOpened {
                id: 4,
                protocol: Some("vite-hmr".into()),
            },
            &[],
        );
        let (decoded, body) = decode(&encoded).unwrap();
        assert!(matches!(decoded, Frame::WsOpened { id: 4, protocol: Some(p) } if p == "vite-hmr"));
        assert_eq!(body, b"");
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode(&[0, 0]).is_none());
        assert!(decode(&[0, 0, 0, 10, b'{']).is_none());
        assert!(decode(&encode_raw(br#"{"type":"teleport","id":1}"#)).is_none());
    }

    fn encode_raw(header: &[u8]) -> Vec<u8> {
        let len = u32::try_from(header.len()).unwrap();
        let mut out = len.to_be_bytes().to_vec();
        out.extend_from_slice(header);
        out
    }
}
