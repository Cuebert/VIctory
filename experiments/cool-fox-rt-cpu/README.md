# Cool Fox RT CPU experiment

This source-only patch is based on public VIctory commit [`f4b48ad`](https://github.com/Cuebert/VIctory/commit/f4b48ad766ddbec1a1867022cd42f7645b0dd55e) and adds two opt-in CPU paths: fill planning and in-place interval compaction. Both are disabled unless their environment variables are set to `1`.

The local source-extracted four-arm comparison checked 30,003 planner cases and fallback witnesses plus 50,000 repeated interval joins per arm. Those checks used a separate hybrid source snapshot; their results do not establish equivalence on this public base. The full renderer was not compiled with this patch, and no end-to-end run or frame-time benchmark of this patch has been completed.

The patch changes GPL-3.0-or-later VIctory source. No game assets, binaries, runtime logs, saves, or test harness are included.

## Separate Series X runtime observation

This is contextual evidence from a local hybrid runtime, not a build or test of this patch. At a stationary bridge view with native 3840×2160 output, the RT builder stayed enabled and all 53 BVH builder pipelines were admitted. Three settled 30-second phases toggled only the existing `VICTORY_PLACED_ORDER` setting off/on/off; no movement input was sent and no pipelines compiled:

| Placed-copy ordering | Samples | Mean | Median | p90 | FPS equivalent |
| --- | ---: | ---: | ---: | ---: | ---: |
| Off | 512 | 56.6 ms | 54.7 ms | 64.0 ms | 17.67 |
| On | 507 | 57.2 ms | 55.2 ms | 64.7 ms | 17.48 |
| Off again | 512 | 56.7 ms | 54.8 ms | 64.3 ms | 17.64 |

Each phase averaged 3,936–3,938 draws per frame. This A/B/A provides no evidence that the ordering setting improved performance. The retained views still showed distorted clouds and blotchy hull shading; no new images are included here.

An earlier, separate stationary-bridge RT comparison measured 22.75 FPS with RT off and 15.06 FPS with RT on after warm-up; all 53 builder pipelines were admitted in the RT arm. These are different conditions from the ordering A/B/A. Neither result establishes correct lighting or playable speed, and neither measures this patch. A separate 512 MiB device-local-window trial reported only 1.3–4.1 FPS near the bridge transition, never reached a steady-scene sample and later crashed during a cutscene. Those transient readings are not a matched comparison.

## Later THIN-off check

A matched native-4K Series X stationary-bridge run with THIN disabled averaged 16.65 FPS, versus about 17.65 FPS with THIN on; the RT builder still admitted all 53 pipelines. Cloud rings and transom patches persisted, while fixed-deck and vegetation controls showed no uniform sharpness gain. Source diagnostics are being prepared for integration; no diagnostics build or rendering fix is claimed. This observation is separate from the CPU patch and does not establish that the patch caused or fixes the remaining artifacts.
