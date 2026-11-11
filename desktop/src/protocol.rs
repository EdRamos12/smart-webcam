//! Wire protocol, see `protocol/PROTOCOL.md`. Mirrors `Protocol.kt` on the phone.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const PORT: u16 = 47800;
pub const VERSION: u16 = 1;
pub const MAGIC: &[u8; 4] = b"SWCM";
pub const SERVICE_TYPE: &str = "_smartwebcam._tcp.local.";

pub const TYPE_CTRL: u8 = 1;
pub const TYPE_VIDEO: u8 = 2;

pub const FLAG_KEYFRAME: u8 = 1;
pub const FLAG_CONFIG: u8 = 2;

pub const MAX_FRAME: u32 = 16 << 20;

#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Ctrl(Value),
    Video(VideoPacket),
    Unknown(u8),
}

#[derive(Debug, Clone, PartialEq)]
pub struct VideoPacket {
    pub pts_us: u64,
    pub flags: u8,
    pub data: Vec<u8>,
}

impl VideoPacket {
    pub fn is_keyframe(&self) -> bool {
        self.flags & FLAG_KEYFRAME != 0
    }
    pub fn is_config(&self) -> bool {
        self.flags & FLAG_CONFIG != 0
    }
}

pub async fn write_handshake<W: AsyncWrite + Unpin>(w: &mut W) -> Result<()> {
    let mut buf = [0u8; 6];
    buf[..4].copy_from_slice(MAGIC);
    buf[4..].copy_from_slice(&VERSION.to_be_bytes());
    w.write_all(&buf).await?;
    w.flush().await?;
    Ok(())
}

pub async fn read_handshake<R: AsyncRead + Unpin>(r: &mut R) -> Result<u16> {
    let mut buf = [0u8; 6];
    r.read_exact(&mut buf).await.context("reading handshake")?;
    if &buf[..4] != MAGIC {
        bail!("not a Smart Webcam server (bad magic)");
    }
    Ok(u16::from_be_bytes([buf[4], buf[5]]))
}

pub fn encode_frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(kind);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

pub fn encode_ctrl(msg: &Value) -> Vec<u8> {
    encode_frame(TYPE_CTRL, msg.to_string().as_bytes())
}

#[cfg(test)]
pub fn encode_video(p: &VideoPacket) -> Vec<u8> {
    let mut payload = Vec::with_capacity(9 + p.data.len());
    payload.extend_from_slice(&p.pts_us.to_be_bytes());
    payload.push(p.flags);
    payload.extend_from_slice(&p.data);
    encode_frame(TYPE_VIDEO, &payload)
}

pub fn decode_payload(kind: u8, payload: Vec<u8>) -> Result<Frame> {
    Ok(match kind {
        TYPE_CTRL => Frame::Ctrl(serde_json::from_slice(&payload).context("bad CTRL json")?),
        TYPE_VIDEO => {
            if payload.len() < 9 {
                bail!("short video frame");
            }
            let pts_us = u64::from_be_bytes(payload[..8].try_into().unwrap());
            let flags = payload[8];
            let mut data = payload;
            data.drain(..9);
            Frame::Video(VideoPacket { pts_us, flags, data })
        }
        other => Frame::Unknown(other),
    })
}

pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Frame> {
    let kind = r.read_u8().await?;
    let len = r.read_u32().await?;
    if len > MAX_FRAME {
        bail!("frame too large: {len}");
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload).await?;
    decode_payload(kind, payload)
}

pub async fn write_ctrl<W: AsyncWrite + Unpin>(w: &mut W, msg: &Value) -> Result<()> {
    w.write_all(&encode_ctrl(msg)).await?;
    w.flush().await?;
    Ok(())
}

// ---- typed CTRL payloads (server → client) ----

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct CameraCaps {
    pub id: String,
    pub facing: String,
    #[serde(default)]
    pub orientation: u32,
    #[serde(default)]
    pub torch: bool,
    #[serde(default)]
    pub sizes: Vec<(u32, u32)>,
    #[serde(default)]
    pub fps: Vec<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Caps {
    #[serde(default)]
    pub device_name: String,
    #[serde(default)]
    pub device_id: String,
    #[serde(default)]
    pub cameras: Vec<CameraCaps>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Started {
    pub camera: String,
    pub w: u32,
    pub h: u32,
    pub fps: u32,
    #[serde(default)]
    pub bitrate: u32,
    #[serde(default)]
    pub orientation: u32,
    #[serde(default)]
    pub facing: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn frame_round_trip() {
        let video = VideoPacket { pts_us: 123_456_789, flags: FLAG_KEYFRAME, data: vec![0, 0, 0, 1, 0x65, 1, 2, 3] };
        let ctrl = json!({"type": "hello", "pc_id": "abc", "pc_name": "box"});

        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&VERSION.to_be_bytes());
        bytes.extend(encode_ctrl(&ctrl));
        bytes.extend(encode_video(&video));
        bytes.extend(encode_frame(99, b"xyz"));

        let mut r = bytes.as_slice();
        assert_eq!(read_handshake(&mut r).await.unwrap(), VERSION);
        assert_eq!(read_frame(&mut r).await.unwrap(), Frame::Ctrl(ctrl));
        assert_eq!(read_frame(&mut r).await.unwrap(), Frame::Video(video));
        assert_eq!(read_frame(&mut r).await.unwrap(), Frame::Unknown(99));
        assert!(read_frame(&mut r).await.is_err());
    }

    #[tokio::test]
    async fn handshake_written_matches_read() {
        let mut out = Vec::new();
        write_handshake(&mut out).await.unwrap();
        assert_eq!(read_handshake(&mut out.as_slice()).await.unwrap(), VERSION);
        assert!(read_handshake(&mut &b"NOPE\0\x01"[..]).await.is_err());
    }

    #[tokio::test]
    async fn rejects_oversized_and_short_frames() {
        let mut big = vec![TYPE_VIDEO];
        big.extend_from_slice(&(MAX_FRAME + 1).to_be_bytes());
        assert!(read_frame(&mut big.as_slice()).await.is_err());
        let short = encode_frame(TYPE_VIDEO, &[1, 2, 3]);
        assert!(read_frame(&mut short.as_slice()).await.is_err());
    }

    #[test]
    fn parses_caps() {
        let v = json!({"type":"caps","device_name":"SM-A356E","device_id":"x","cameras":[
            {"id":"0","facing":"back","orientation":90,"torch":true,"sizes":[[1920,1080],[1280,720]],"fps":[15,30]}]});
        let caps: Caps = serde_json::from_value(v).unwrap();
        assert_eq!(caps.cameras[0].sizes[1], (1280, 720));
        assert!(caps.cameras[0].torch);
    }
}
