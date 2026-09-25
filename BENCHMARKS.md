# Benchmarks

PraeterVNC vs TightVNC Server 2.8.85 on the same machine and desktop (Windows 10 22H2, Ryzen 5 5600G 6C/12T, RTX 5060 Ti, 5120×1440 desktop across monitors). Both are measured by `crates/bench` using the encoding list the real TightVNC 2.8 viewer sends (`tightvnc28`: Tight, JPEG quality level 6, one request per update).

## Method

- `praeter-testapp` renders a 1280×720 scenario at the display rate, 100 Hz here (flip model), with a frame-ID barcode. A DXGI observer thread records the time each frame actually reached the screen.
- The bench client decodes every update, reads the barcode and records **latency** = decoded at client − presented on host. **Frames** = share of presented frames the client ever saw (the rest were merged into later updates).
- Warm-up ends 1.5 s after the first update; then 8 s are measured. **Paint** = time from connect until every pixel was received once.
- Links are emulated by `praeter-netem` (delay each way plus a bandwidth cap with an unbounded queue, i.e. a bufferbloated bottleneck).
- Scenarios: `ticker` (small text change per frame), `typing` (a glyph per frame), `scroll` (full-window text scroll), `drag` (400×300 panel moving over a document), `video` (1280×720 full-motion photo zoom/pan).
- All PraeterVNC runs also pass `--verify`: after the scenario stops, the client framebuffer matches the application pixels exactly (lossy areas are refreshed losslessly).

## Results

Frames delivered and latency p50 / p99 in ms. The latency floor on an emulated link is its one-way delay (RTT/2).

**Localhost**

| Scenario | PraeterVNC | TightVNC |
|---|---|---|
| ticker | 100% · 1.0 / 1.2 | 67% · 2.5 / 20.3 |
| typing | 100% · 1.2 / 12.7 | 71% · 2.3 / 29.2 |
| scroll | 94% · 3.3 / 26.8 | 50% · 21.1 / 35.0 |
| drag | 99% · 2.7 / 10.9 | 47% · 12.1 / 20.2 |
| video | 100% · 10.1 / 19.2 | 10% · 99.9 / 166.5 |
| full paint | 22–36 ms | 66–182 ms |

**50 ms RTT, no bandwidth cap** (floor 25 ms)

| Scenario | PraeterVNC | TightVNC |
|---|---|---|
| ticker | 98% · 26.6 / 42.5 | 19% · 37.1 / 54.6 |
| typing | 100% · 26.6 / 38.7 | 19% · 37.1 / 52.8 |
| scroll | 100% · 28.8 / 31.0 | 14% · 53.4 / 69.3 |
| drag | 99% · 28.4 / 37.7 | 15% · 47.4 / 77.2 |
| video | 96% · 33.5 / 46.5 | 12% · 72.6 / 88.8 |

**20 Mbit/s, 40 ms RTT** (floor 20 ms)

| Scenario | PraeterVNC | TightVNC |
|---|---|---|
| ticker | 100% · 21.5 / 22.2 | 24% · 35.9 / 49.8 |
| typing | 100% · 21.6 / 22.4 | 23% · 34.9 / 51.2 |
| scroll | 100% · 24.3 / 25.4 | 13% · 70.9 / 82.4 |
| drag | 100% · 30.0 / 36.4 | 15% · 60.5 / 78.8 |
| video | 47% · 61.4 / 77.7 | 7.5% · 131.2 / 152.5 |

**5 Mbit/s, 80 ms RTT** (floor 40 ms)

| Scenario | PraeterVNC | TightVNC |
|---|---|---|
| ticker | 99% · 41.7 / 52.0 | 11% · 63.4 / 83.6 |
| typing | 99% · 41.8 / 50.9 | 9.5% · 63.3 / 91.0 |
| scroll | 77% · 49.1 / 117.5 | 5.7% · 153.2 / 181.2 |
| drag | 47% · 74.6 / 121.8 | 3.4% · 139.6 / 313.8 |
| video | 26% · 87.0 / 134.4 | 2.4% · 388.3 / 535.9 |

Other links (separate runs, same method):

| Link | Scenario | PraeterVNC | TightVNC |
|---|---|---|---|
| 100 Mbit/s, 150 ms | typing | 100% · 76.7 / 77.3 | 6.4% · 93.6 / 121.6 |
| | drag | 100% · 79.7 / 85.6 | 6.0% · 103.8 / 127.9 |
| | video | 100% · 87.3 / 95.9 | 5.4% · 131.0 / 151.3 |
| 10 Mbit/s, 10 ms | typing | 99% · 6.8 / 7.9 | 51% · 11.8 / 42.2 |
| | drag | 69% · 28.7 / 45.0 | 17% · 61.9 / 101.0 |
| | video | 20% · 55.6 / 118.5 | 5.5% · 190.6 / 343.4 |

Other viewers' encoding lists against PraeterVNC at 20 Mbit/s, 40 ms: `realvnc7` (TRLE/ZRLE + JPEG-21) typing 100% · 21.6, scroll 100% · 25.0, video 48% · 54.0; `tigervnc` (ContinuousUpdates + Fence) typing 100% · 21.5, scroll 100% · 26.1, video 42% · 56.4. All pixel-exact.

Real viewers through the same 20 Mbit/s link while playing the video scenario (server-side stats; the viewer window is itself captured, roughly doubling the changing area): TightVNC Viewer 2.8 at 23 updates/s and RealVNC Viewer 7.5 at 33–41 updates/s, both saturating the link without a standing queue.

The desktop was live during these runs (other applications repainting on other monitors), which is the main source of p99 spread on localhost; it affects both servers alike.
