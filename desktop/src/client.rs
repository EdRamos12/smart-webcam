//! One protocol session with a phone. Reconnecting is done by the caller (`app.rs`).

use crate::config::Facing;
use crate::protocol::{self, Caps, Frame, Started};
use crate::video::{VideoMsg, VideoSink};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::{interval, timeout, MissedTickBehavior};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
const READ_TIMEOUT: Duration = Duration::from_secs(10);
const PING_EVERY: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq)]
pub struct StreamParams {
    pub facing: Facing,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
}

#[derive(Debug)]
pub enum SessionCmd {
    Configure(StreamParams),
    Torch(bool),
    RequestKeyframe,
}

#[derive(Debug)]
pub enum SessionEvent {
    PairingPending,
    Caps(Caps),
    Started(Started),
    PhoneError(String),
    /// Phone-side delays: camera → encoder output (ms) and packets waiting in its send queue.
    PhoneStats { encode_ms: f32, queued: u32 },
    /// Round-trip time of the last ping.
    Rtt(f32),
}

#[derive(Debug, PartialEq)]
pub enum SessionEnd {
    /// The phone closed the stream (bye/EOF); worth reconnecting.
    Closed,
    /// The user rejected this PC on the phone; don't retry.
    Denied(String),
}

pub struct Hello {
    pub pc_id: String,
    pub pc_name: String,
}

/// Picks the camera and frame rate to request from what the phone offers.
pub fn build_config(caps: &Caps, p: &StreamParams) -> Value {
    let cam = caps
        .cameras
        .iter()
        .find(|c| c.facing == p.facing.as_str())
        .or_else(|| caps.cameras.first());
    let (camera, fps) = match cam {
        Some(c) => {
            let fps = c.fps.iter().copied().filter(|&f| f <= p.fps).max().or_else(|| c.fps.iter().copied().min()).unwrap_or(p.fps);
            (c.id.clone(), fps)
        }
        None => (String::new(), p.fps),
    };
    json!({"type": "config", "camera": camera, "w": p.width, "h": p.height, "fps": fps, "bitrate": p.bitrate})
}

/// Connects, handshakes and streams until the connection ends.
pub async fn run(
    addr: SocketAddr,
    hello: &Hello,
    mut params: StreamParams,
    torch: bool,
    video: VideoSink,
    cmds: &mut mpsc::UnboundedReceiver<SessionCmd>,
    events: impl Fn(SessionEvent),
) -> Result<SessionEnd> {
    let stream = timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await.context("connect timed out")??;
    stream.set_nodelay(true)?;
    let (mut rd, mut wr) = stream.into_split();

    protocol::write_handshake(&mut wr).await?;
    let version = timeout(READ_TIMEOUT, protocol::read_handshake(&mut rd)).await.context("handshake timed out")??;
    if version != protocol::VERSION {
        bail!("phone speaks protocol v{version}, we speak v{}", protocol::VERSION);
    }
    protocol::write_ctrl(&mut wr, &json!({"type": "hello", "pc_id": hello.pc_id, "pc_name": hello.pc_name})).await?;

    // Reader task: video goes straight to the decoder thread, control messages come back here.
    let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel::<Result<Value>>();
    let (key_tx, mut key_rx) = mpsc::unbounded_channel::<()>();
    let reader_video = video.clone();
    let reader = tokio::spawn(async move {
        let video = reader_video;
        let mut dropping = false;
        loop {
            let frame = match timeout(READ_TIMEOUT, protocol::read_frame(&mut rd)).await {
                Ok(Ok(f)) => f,
                Ok(Err(e)) => {
                    let _ = ctrl_tx.send(Err(e));
                    break;
                }
                Err(_) => {
                    let _ = ctrl_tx.send(Err(anyhow::anyhow!("phone stopped responding")));
                    break;
                }
            };
            match frame {
                Frame::Ctrl(v) => {
                    if ctrl_tx.send(Ok(v)).is_err() {
                        break;
                    }
                }
                Frame::Video(p) => {
                    if dropping && !p.is_keyframe() && !p.is_config() {
                        video.count_dropped();
                        continue;
                    }
                    dropping = false;
                    if !video.push(p) {
                        // Decoder is behind: skip to the next IDR.
                        dropping = true;
                        let _ = key_tx.send(());
                    }
                }
                Frame::Unknown(_) => {}
            }
        }
    });
    let _abort = AbortOnDrop(reader);

    let mut caps: Option<Caps> = None;
    let epoch = std::time::Instant::now();
    let mut ping = interval(PING_EVERY);
    ping.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        let out: Option<Value> = tokio::select! {
            msg = ctrl_rx.recv() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => return if is_eof(&e) { Ok(SessionEnd::Closed) } else { Err(e) },
                    None => return Ok(SessionEnd::Closed),
                };
                match msg["type"].as_str().unwrap_or_default() {
                    "pairing" => { events(SessionEvent::PairingPending); None }
                    "denied" => {
                        return Ok(SessionEnd::Denied(msg["reason"].as_str().unwrap_or("denied").to_string()));
                    }
                    "caps" => {
                        let c: Caps = serde_json::from_value(msg).context("bad caps")?;
                        let config = build_config(&c, &params);
                        events(SessionEvent::Caps(c.clone()));
                        caps = Some(c);
                        Some(config)
                    }
                    "started" => {
                        let s: Started = serde_json::from_value(msg).context("bad started")?;
                        video.send(VideoMsg::Started(s.clone()));
                        events(SessionEvent::Started(s));
                        torch.then(|| json!({"type": "torch", "on": true}))
                    }
                    "error" => {
                        events(SessionEvent::PhoneError(msg["message"].as_str().unwrap_or("error").to_string()));
                        None
                    }
                    "bye" => return Ok(SessionEnd::Closed),
                    "pong" => {
                        if let Some(t) = msg["t"].as_u64() {
                            let rtt = epoch.elapsed().as_micros() as u64 - t;
                            events(SessionEvent::Rtt(rtt as f32 / 1000.0));
                        }
                        None
                    }
                    "stats" => {
                        events(SessionEvent::PhoneStats {
                            encode_ms: msg["encode_ms"].as_f64().unwrap_or(0.0) as f32,
                            queued: msg["queued"].as_u64().unwrap_or(0) as u32,
                        });
                        None
                    }
                    _ => None,
                }
            }
            cmd = cmds.recv() => match cmd {
                Some(SessionCmd::Configure(p)) => {
                    params = p;
                    caps.as_ref().map(|c| build_config(c, &params))
                }
                Some(SessionCmd::Torch(on)) => Some(json!({"type": "torch", "on": on})),
                Some(SessionCmd::RequestKeyframe) => Some(json!({"type": "request_keyframe"})),
                None => {
                    let _ = protocol::write_ctrl(&mut wr, &json!({"type": "bye"})).await;
                    return Ok(SessionEnd::Closed);
                }
            },
            Some(()) = key_rx.recv() => Some(json!({"type": "request_keyframe"})),
            _ = ping.tick() => Some(json!({"type": "ping", "t": epoch.elapsed().as_micros() as u64})),
        };
        if let Some(msg) = out {
            timeout(READ_TIMEOUT, protocol::write_ctrl(&mut wr, &msg)).await.context("send timed out")??;
        }
    }
}

fn is_eof(e: &anyhow::Error) -> bool {
    e.downcast_ref::<std::io::Error>().is_some_and(|io| {
        matches!(io.kind(), std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset)
    })
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::CameraCaps;

    fn caps() -> Caps {
        let cam = |id: &str, facing: &str, fps: Vec<u32>| CameraCaps {
            id: id.into(),
            facing: facing.into(),
            orientation: 90,
            torch: false,
            sizes: vec![(1920, 1080)],
            fps,
        };
        Caps { device_name: "x".into(), device_id: "y".into(), cameras: vec![cam("0", "back", vec![15, 30]), cam("1", "front", vec![15, 24, 30, 60])] }
    }

    #[test]
    fn picks_camera_and_fps() {
        let p = StreamParams { facing: Facing::Front, width: 1280, height: 720, fps: 60, bitrate: 1 };
        let c = build_config(&caps(), &p);
        assert_eq!(c["camera"], "1");
        assert_eq!(c["fps"], 60);
        let p = StreamParams { facing: Facing::Back, fps: 60, ..p };
        let c = build_config(&caps(), &p);
        assert_eq!(c["camera"], "0");
        assert_eq!(c["fps"], 30);
        assert_eq!(c["w"], 1280);
    }
}
