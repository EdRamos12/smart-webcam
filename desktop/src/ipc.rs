//! Local socket between the tray app (server) and the settings/preview window (client).
//!
//! Frames reuse the `[u8 type][u32 len][payload]` layout of the phone protocol:
//! type 1 = JSON (`TrayState` to the window, `Command` from it),
//! type 2 = preview: `u32 width, u32 height, u64 written_us` + RGB bytes.

use crate::protocol::encode_frame;
use crate::tray::{Command, TrayState};
use crate::video::Preview;
use anyhow::{bail, Result};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixListener;
use tokio::sync::{mpsc::UnboundedSender, watch};

const TYPE_JSON: u8 = 1;
const TYPE_PREVIEW: u8 = 2;
const MAX_FRAME: usize = 64 << 20;

pub fn socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("smart-webcam.sock")
}

fn encode_preview(p: &Preview) -> Vec<u8> {
    let mut payload = Vec::with_capacity(16 + p.rgb.len());
    payload.extend_from_slice(&(p.width as u32).to_be_bytes());
    payload.extend_from_slice(&(p.height as u32).to_be_bytes());
    payload.extend_from_slice(&p.written_us.to_be_bytes());
    payload.extend_from_slice(&p.rgb);
    encode_frame(TYPE_PREVIEW, &payload)
}

/// Serves windows until the process exits. Only called while holding the single-instance lock,
/// so removing a stale socket file is safe.
pub async fn serve(
    state: watch::Receiver<TrayState>,
    preview: watch::Receiver<Option<Arc<Preview>>>,
    viewers: Arc<AtomicUsize>,
    commands: UnboundedSender<Command>,
) -> Result<()> {
    let path = socket_path();
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    loop {
        let (stream, _) = listener.accept().await?;
        let (state, preview, viewers, commands) = (state.clone(), preview.clone(), viewers.clone(), commands.clone());
        tokio::spawn(async move {
            viewers.fetch_add(1, Ordering::Relaxed);
            if let Err(e) = serve_client(stream, state, preview, commands).await {
                log::debug!("window disconnected: {e:#}");
            }
            viewers.fetch_sub(1, Ordering::Relaxed);
        });
    }
}

async fn serve_client(
    stream: tokio::net::UnixStream,
    mut state: watch::Receiver<TrayState>,
    mut preview: watch::Receiver<Option<Arc<Preview>>>,
    commands: UnboundedSender<Command>,
) -> Result<()> {
    let (mut rd, mut wr) = stream.into_split();
    let mut reader = tokio::spawn(async move {
        loop {
            match crate::protocol::read_frame(&mut rd).await? {
                crate::protocol::Frame::Ctrl(v) => match serde_json::from_value::<Command>(v) {
                    Ok(c) => {
                        let _ = commands.send(c);
                    }
                    Err(e) => log::warn!("bad command from window: {e}"),
                },
                _ => {}
            }
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    });

    state.mark_changed();
    loop {
        tokio::select! {
            r = state.changed() => {
                r?;
                let json = serde_json::to_vec(&*state.borrow_and_update())?;
                wr.write_all(&encode_frame(TYPE_JSON, &json)).await?;
            }
            r = preview.changed() => {
                r?;
                let frame = preview.borrow_and_update().as_ref().map(|p| encode_preview(p));
                if let Some(bytes) = frame {
                    wr.write_all(&bytes).await?;
                }
            }
            r = &mut reader => {
                return r?;
            }
        }
    }
}

pub enum Incoming {
    State(Box<TrayState>),
    Preview(Preview),
}

/// Blocking client used by the window.
pub struct Client {
    stream: std::os::unix::net::UnixStream,
}

impl Client {
    pub fn connect() -> std::io::Result<Client> {
        Ok(Client { stream: std::os::unix::net::UnixStream::connect(socket_path())? })
    }

    pub fn writer(&self) -> std::io::Result<std::os::unix::net::UnixStream> {
        self.stream.try_clone()
    }

    pub fn read(&mut self) -> Result<Incoming> {
        let mut head = [0u8; 5];
        self.stream.read_exact(&mut head)?;
        let len = u32::from_be_bytes(head[1..].try_into().unwrap()) as usize;
        if len > MAX_FRAME {
            bail!("frame too large");
        }
        let mut payload = vec![0u8; len];
        self.stream.read_exact(&mut payload)?;
        match head[0] {
            TYPE_JSON => Ok(Incoming::State(Box::new(serde_json::from_slice(&payload)?))),
            TYPE_PREVIEW if len >= 16 => {
                let width = u32::from_be_bytes(payload[..4].try_into().unwrap()) as usize;
                let height = u32::from_be_bytes(payload[4..8].try_into().unwrap()) as usize;
                let written_us = u64::from_be_bytes(payload[8..16].try_into().unwrap());
                payload.drain(..16);
                if payload.len() != width * height * 3 {
                    bail!("bad preview size");
                }
                Ok(Incoming::Preview(Preview { width, height, rgb: payload, written_us }))
            }
            other => bail!("unknown frame type {other}"),
        }
    }
}

pub fn send_command(stream: &mut std::os::unix::net::UnixStream, cmd: &Command) -> std::io::Result<()> {
    let json = serde_json::to_vec(cmd).map_err(std::io::Error::other)?;
    stream.write_all(&encode_frame(TYPE_JSON, &json))
}
