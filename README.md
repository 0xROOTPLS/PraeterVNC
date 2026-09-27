# PraeterVNC

A performance focused VNC (RFB 3.3/3.7/3.8) server for Windows 10/11, written in Rust.

## How To Run?

- **Portable**: run `PraeterVNC-<ver>-portable.exe`. Settings go in `praetervnc.ini` next to it (`%LOCALAPPDATA%\PraeterVNC` if that folder is read-only).
- **Service**: install the MSI, or use tray menu -> *Install as service…* (or `praetervnc --install`). Runs as a SYSTEM service & starts a helper. Settings from the portable copy carry over.
- **Console**: `praetervnc [options]` runs the server without a tray.

| Option | Default | |
|---|---|---|
| `--port N` / `--bind ADDR` | 5900 / 127.0.0.1 (0.0.0.0 with a password) | |
| `--password PW` | none | VNC authentication (DES; tunnel it on untrusted networks) |
| `--monitor N` / `--list` | all monitors | capture one output / list outputs |
| `--view-only` | off | ignore input and client clipboard |
| `--inflight N` | 32 | max updates in flight |
| `--alr-ms N` | 150 | idle time before lossless refresh of lossy areas |
| `--budget-ms N` | 25 | encode budget per update |
| `--no-scroll` `--no-motion` `--no-pipeline` | | disable scroll detection / motion JPEG / legacy pipelining |
| `--verbose` | off | per-client stats every 5 s |

Requires an AVX2 CPU and Windows 10/11 x64!

## Security

- Without a password Praeter listens on 127.0.0.1 only.
- Failed logins are counted per IP. Blocked IPs show in the tray.
- VNC auth is DES challenge/response and the session is unencrypted.

## Recovery

- A watchdog tracks capture, encode, and session loop. Any problem is logged Praeter restarts itself or is restarted by the service (helper).
- The service pings the helper via the control pipe, and will restart it if it stops answering.
- *Release stuck keys* sends key-up for any held modifier and mouse button.
- TCP keepalive (10 s) and a 30 s write timeout autodrops dead viewers.
- Logs: `logs\praetervnc.log`. *Rotated at 2 MB.*

## Build

```
cargo build --release          # target\release\praetervnc.exe
python tools/package.py        # dist\: MSI (WiX 5, repo-local dotnet tool), portable exe, licenses, SHA256SUMS
```

## Design

Capture
- DXGI Desktop Duplication, one thread per output (MMCSS, raised GPU priority).
- Dirty and move rects -> 64 px tiles, diffed in parallel against immutable `Arc` snapshots.
- Per-client tile model: dirty = `Arc` pointer inequality. All clients share one capture.

Encoding
- Tight (fill, mono, palette, zlib, JPEG), ZRLE, TRLE, JPEG-21, Hextile, Raw, CopyRect.
- Rich and alpha cursors, PointerPos, DesktopSize/ExtendedDesktopSize, LastRect, Fence, ContinuousUpdates, QEMU key events, ExtendedClipboard (UTF-8).
- Parallel zlib in one valid stream: dictionary-primed chunks, sync-flushed.
- Photos go to JPEG, text stays lossless.
- Scroll detection -> CopyRect.
- Motion tiles get cheaper JPEG (4:2:0), then a lossless refresh 150 ms after they settle.

Scheduling and flow control
- Per update: urgent work (small changes, near the pointer, scroll-exposed), then oldest work within the encode budget, then lossless refresh.
- Legacy viewers are pipelined. ContinuousUpdates clients are paced by fences.
- `pipe.rs` models the bottleneck from acks alone. Works through proxies and SSH tunnels.
- Updates leave just before the link drains, never queued.
- Motion JPEG quality follows the link: down above ~28 ms link time per update, up below ~14 ms.
- Large changes and lossless refresh are split into link-sized chunks.
- Below ~100 Mbit: more zlib effort, ZRLE instead of TRLE.

## Results

See [BENCHMARKS.md](BENCHMARKS.md).

## Tooling

- `crates/testapp`: test window playing scenarios (ticker, typing, scroll, drag, video) with a frame-ID barcode. Logs each frame's present time.
- `crates/bench`: headless RFB client. Reports frames, latency (p50/p90/p99), bandwidth, full-paint time and server CPU. `--verify` checks the final framebuffer.
- `crates/netem`: TCP proxy with delay and a bandwidth cap.
- `tools/bench.py --servers praeter,tightvnc --scenarios ... --profiles tightvnc28,realvnc7,tigervnc [--rtt MS] [--mbit N] [--verify]`
- `PRAETER_PROBE=1`: per-stage latency. `PRAETER_TRACE=1`: logs every update and ack.

## Limitations

- Secure desktop (UAC, lock and login screens) needs service mode.
- VNC auth only (no TLS/VeNCrypt), 8-character ASCII passwords.
- Reduced-colour viewer settings (e.g. RealVNC *Picture quality: Low*) are honored. JPEG needs 16-bit colour or more.
- Clipboard is text only. No file transfer, IPv6 or rotated monitors.
- Unsigned exe: expect SmartScreen warnings. Some antivirus flags any VNC server.

## License

MIT, see [LICENSE](LICENSE). Third-party licenses: [THIRD-PARTY-NOTICES.txt](THIRD-PARTY-NOTICES.txt). `praetervnc --licenses` prints both.
