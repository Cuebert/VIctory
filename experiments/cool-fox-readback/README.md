# Cool Fox pending-readback experiment

This source-only patch targets public VIctory commit [`f4b48ad`](https://github.com/Cuebert/VIctory/commit/f4b48ad766ddbec1a1867022cd42f7645b0dd55e). It applies to that plain commit with `git apply --check --ignore-space-change --whitespace=error`.

The patch adds pending texture readback ranges to submission dependency tracking and includes opt-in diagnostics for write-watch attribution and fenced visual samples. Those diagnostics default off. The patch also includes an opt-in WARP regression. A Windows D3D12 WARP run passed: two fixed readback rows were validated with 16-byte padding untouched, the destination stayed unchanged before landing, and a later overlapping CPU read was recognized as pending. This validates ordering only, not game performance or visual improvement. The combined patch was built and run only in a local hybrid source tree with additional overlays, not in a clean checkout of `f4b48ad`.

The local candidate admitted all 53 RT builder pipelines and presented a stationary native-4K Series X bridge for 90 seconds. Seventeen host intervals averaged 57.335 ms (17.44 FPS equivalent), ranged from 16.9–18.0 FPS, and averaged about 3,924.8 draws. Earlier matched THIN-on intervals measured 17.67 and 17.64 FPS, so the candidate showed no gain. Visible face, sky and hull defects remained.

A one-present hull probe found different HDR values before the selected indirect-light stage `1cd9`; the stage left both sampled 3×3 blocks unchanged. The sampled base centers were equal and depths close, but those taps were fenced later, so this does not establish a causal chain through those passes.

The repository declares GPL-3.0-or-later. The texture-readback dependency change and its regression test are adapted from gomeaw’s [upstream VIctory PR #150](https://github.com/avatarmanz/VIctory-7900XTX/pull/150), under compatible GPL terms; the write-watch and visual-sample diagnostics are additions in this patch. No binaries, title assets, captures, logs, saves or local launch scripts are included.

## Separate Series S bridge row

A separate D508 run on RTX 5090 adapter 0 used the Series S profile at native 1920×1080 on the stationary bridge. The RT-on arm admitted all 53 RT builder pipelines; read/build dispatches were logged. Seven settled host intervals averaged 50.914 ms (19.64 FPS equivalent, 19.2–20.1 FPS, 3,510.7 draws), with zero compiling pipelines and about one host/two indirect drops per sample. The RT-off control is separate and covers only three intervals: 42.4 ms (23.58 FPS equivalent), about 3,448 draws, 75–78 missing host pipelines and 481–494 indirect drops.

These arms executed unequal work, so this is not an isolated RT-cost or render-parity comparison. There was no same-scene alternate native-resolution or movement comparison; later water-pass and input behavior remain unresolved. This row is from the separate D508 runtime, not a benchmark of the readback patch.
