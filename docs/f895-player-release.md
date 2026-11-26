# F895 player source snapshot

This branch is a source-only snapshot matching the F895 player build inputs, based on the publicly attributable [fxceless/victory-fork commit `bc6ee68`](https://github.com/fxceless/victory-fork/commit/bc6ee68ea17030fa9531837e395404cfd1856656). The `LICENSE` remains GPL-3.0-or-later. The VIctory and contributor credits in the repository are retained; the earlier source history and its authors are visible at the linked upstream commit.

No game files, title executables, saves, private launcher helpers, caches or run traces are included. The private wrapper used during the historical F895 run is not part of this checkout. Its executable used an embedded switch list from another shared-target checkout, so a clean build from this source is not claimed to reproduce that executable byte for byte.

## What the F895 run showed

- Ordinary gameplay progressed through boat control at about 11 native frames per second in the sampled view. Gamepad-state readback confirmed the S/L/W inputs during the control check. This is a scene-specific observation, not a general performance guarantee.
- Faces remained continuous in the sampled F895 opening and boat cuts. Large material blocks remained on sampled clothing, and the off-glint water remained mostly smooth. A bright, continuous sun glint was visible at the bridge.
- In a separate upload-and-hash comparison, the measured work fell from 31.1 ms to 6.8 ms while the game remained near 10 frames per second. The measured subsystem cost improved; the game cadence did not.
- The shadow-cascade and interior-mip options were enabled together during the F895 visual run, so their separate visual effects are not established. The optional title BVH builder remains off by default. A separate experimental run showed the `Follow the plane` objective, hull, blue water and bridge at frame 1680, then crashed at frame 1701 before controllable boat input. Correct ray-traced lighting and driving in that run remain unverified.

## Build and run with your own game files

Use Windows x64, Rust 1.98.1 (pinned in `rust-toolchain.toml`), and a Direct3D 12 GPU with shader model 6.6 and mesh-shader support. Supply your own game installation.

From the repository root, copy `victory.example.toml` to the ignored `victory.toml` and set `[paths].game_dir` to your game directory. Keep game files and derived output outside this source tree. Then build and launch:

```powershell
cargo build --locked --release -p victory
target\release\victory.exe run "<your game executable path>" --execute --console series-x
```

The default cache and persistent save-data directories are `%LOCALAPPDATA%\victory\cache` and `%LOCALAPPDATA%\victory\data`. Set `cache_dir` and `data_dir` in `victory.toml`, or use `VICTORY_CACHE_DIR` and `VICTORY_DATA_DIR`, to choose other per-user locations outside the repository.

Experimental renderer gates are off by default. The F895 run used its recorded profile; leave the optional title BVH builder disabled unless running a separate, explicitly labeled experiment.
