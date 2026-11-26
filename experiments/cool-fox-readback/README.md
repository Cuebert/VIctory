# Cool Fox pending-readback experiment

This source-only patch targets public VIctory commit [`f4b48ad`](https://github.com/Cuebert/VIctory/commit/f4b48ad766ddbec1a1867022cd42f7645b0dd55e). It applies to that plain commit with `git apply --check --ignore-space-change --whitespace=error`.

The patch adds pending texture readback ranges to submission dependency tracking and includes opt-in diagnostics for write-watch attribution and fenced visual samples. Those diagnostics default off. The patch also contains an ignored WARP regression; that test has not been run. The combined patch was built and run only in a local hybrid source tree with additional overlays, not in a clean checkout of `f4b48ad`.

The local candidate admitted all 53 RT builder pipelines and presented a stationary native-4K Series X bridge for 90 seconds. Seventeen host intervals averaged 57.335 ms (17.44 FPS equivalent), ranged from 16.9–18.0 FPS, and averaged about 3,924.8 draws. Earlier matched THIN-on intervals measured 17.67 and 17.64 FPS, so the candidate showed no gain. Visible face, sky and hull defects remained.

A one-present hull probe found different HDR values before the selected indirect-light stage `1cd9`; the stage left both sampled 3×3 blocks unchanged. The sampled base centers were equal and depths close, but those taps were fenced later, so this does not establish a causal chain through those passes.

The repository declares GPL-3.0-or-later; this patch is distributed under that license. Upstream [VIctory PR #150](https://github.com/Marvisak/VIctory/pull/150) concerns DualSense HID input and is unrelated; this patch does not copy its implementation. No binaries, title assets, captures, logs, saves or local launch scripts are included.
