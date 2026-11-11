# Smart Webcam wire protocol (version 1)

The phone is the **server** and listens on TCP port **47800**. The PC is the **client**.

- Wi-Fi: the phone advertises `_smartwebcam._tcp` over mDNS (DNS-SD) with TXT records
  `name` (device model / user-visible name), `id` (stable random install id) and `v` (protocol version).
- USB: the PC runs `adb forward tcp:<local> tcp:47800` and connects to `127.0.0.1:<local>`.
  On the phone such connections come from the loopback address and are trusted automatically.

All integers are **big-endian**.

## Handshake

Immediately after connecting, each side sends 6 bytes:

```
"SWCM" (4 bytes ASCII)  u16 version
```

The client sends first; the server answers with its own magic and version. If the magic differs
or the major versions are incompatible the connection is closed.

## Framing

After the handshake everything is a frame:

```
u8  type
u32 len        (payload length, max 16 MiB)
u8[len] payload
```

| type | name  | payload                                   |
|------|-------|-------------------------------------------|
| 1    | CTRL  | UTF-8 JSON object with a `"type"` field   |
| 2    | VIDEO | video packet, see below                   |

### VIDEO payload

```
u64 pts_us
u8  flags      bit0 = KEYFRAME, bit1 = CONFIG (SPS/PPS only)
u8[] data      H.264 Annex-B NAL units (with 00 00 00 01 start codes)
```

- The stream is H.264 (Constrained) Baseline, no B-frames, in the camera **sensor orientation**
  (usually landscape). The client rotates and mirrors as configured.
- SPS/PPS are sent as a CONFIG packet when the encoder starts **and** are prepended to
  every keyframe, so a decoder can start at any keyframe.
- The server may drop packets under backpressure. It then skips forward to the next keyframe.

## CTRL messages

Client → server:

| message | fields | meaning |
|---|---|---|
| `hello` | `pc_id`, `pc_name` | first message; identifies the PC for pairing |
| `config` | `camera`, `w`, `h`, `fps`, `bitrate` | (re)start capture/encoding with these settings |
| `torch` | `on` (bool) | flash on/off (back camera) |
| `bitrate` | `bps` | change encoder bitrate on the fly |
| `request_keyframe` | – | ask the encoder for an IDR frame now |
| `ping` | – | keepalive; server answers `pong` |
| `bye` | – | client is disconnecting |

Server → client:

| message | fields | meaning |
|---|---|---|
| `pairing` | `state`: `"pending"` | waiting for the user to approve this PC on the phone |
| `denied` | `reason` | the user rejected the PC (connection is closed afterwards) |
| `caps` | `device_name`, `device_id`, `cameras: [{id, facing, orientation, sizes: [[w,h]...], fps: [..], torch}]` | sent once the PC is trusted |
| `started` | `camera`, `w`, `h`, `fps`, `bitrate`, `orientation`, `facing` | encoder (re)started with the actual parameters |
| `error` | `message` | a non-fatal error (e.g. camera unavailable) |
| `pong` | – | answer to `ping` |
| `bye` | – | server is shutting down the stream |

`camera` ids are the Camera2 ids reported in `caps`. `facing` is `"back"`, `"front"` or `"external"`.
`orientation` is the sensor orientation in degrees (0/90/180/270).

## Session flow

```
C: SWCM 0001                  S: SWCM 0001
C: CTRL hello{pc_id, pc_name}
                              S: CTRL pairing{pending}      (only for untrusted, non-loopback PCs)
                              S: CTRL caps{...}             (or denied + close)
C: CTRL config{camera, w, h, fps, bitrate}
                              S: CTRL started{...}
                              S: VIDEO (CONFIG) ...
                              S: VIDEO (KEYFRAME) ...
                              S: VIDEO ... 
C: CTRL request_keyframe / torch / config / ping ...
C: CTRL bye  (or just close)
```

Only one client streams at a time. A new trusted connection replaces the old one.
Either side treats 10 s without any received data as a dead connection (the client sends
`ping` every 2 s while idle).
