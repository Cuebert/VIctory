# Cool Fox RT CPU experiment

This source-only patch is based on public VIctory commit [`f4b48ad`](https://github.com/Cuebert/VIctory/commit/f4b48ad766ddbec1a1867022cd42f7645b0dd55e) and adds two opt-in CPU paths: fill planning and in-place interval compaction. Both are disabled unless their environment variables are set to `1`.

The local source-extracted four-arm comparison checked 30,003 planner cases and fallback witnesses plus 50,000 repeated interval joins per arm. Those checks used a separate hybrid source snapshot; their results do not establish equivalence on this public base. The full renderer was not compiled with this patch, and no end-to-end run or frame-time benchmark has been completed.

The patch changes GPL-3.0-or-later VIctory source. No game assets, binaries, runtime logs, saves, or test harness are included.
