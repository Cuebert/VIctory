# Bridge rendering experiments

These two switches default to off. Set the one you want to evaluate before starting the runtime.

## Batch buffer uploads and hashes

Set `VICTORY_CARD_SYNC_BATCH=1` to group uploads and hash dispatches for independent buffers on the DIRECT queue. A list qualifies when at least four distinct dirty buffers already have card hashes and need nonempty copy ranges. If two entries reference the same shadow buffer, the list uses the original ordering. The grouped recording path is adapted from KyleS98’s [upstream pull request #1227](https://github.com/fxceless/victory-fork/pull/1227).

A stationary bridge comparison in the development build used the same camera and restored the original setting for a third measurement. Each setting contributed 120 completed guest presents to the GPU timing.

| Measurement | Stock | Adaptive batching | Restored stock |
| --- | ---: | ---: | ---: |
| DIRECT upload and hash time, ms per guest present | 31.125 | 6.794 | 30.882 |
| Guest presents per second | 10.237 | 10.015 | 10.040 |

Batching saved 24.21 ms on the DIRECT queue per guest present against the average of the two controls. The run remained near 10 guest presents per second in all three intervals.

## Translate the alpha-transmission prefilter on Wave32

Set `VICTORY_ALPHA_TRANSMISSION_WAVE32=1` to enable a targeted translation for the verified compute program on a Wave32 host. The selector checks the container digest, program size, wave requirement and threadgroup dimensions. The translator verifies the early exit, group barrier and shared-memory handoff before emitting the repaired program.

A live comparison in the development build held the boat and camera stationary. The stock interval recorded eight rejected dispatches. After enabling the repair, six immediate samples and seven settled samples matched the prefilter output to the first Gaussian pass on the same resource and mip. In the settled samples the prefilter changed 955–1,009 texels and the Gaussian pass changed 812–912 texels within their respective 1,024-texel samples. The independent color-grading correction stayed enabled throughout. The bright glint remained visible and the water away from the glint stayed smooth in both settings. The cause of the missing surface ripples remains open.

The Wave32 work was first qualified against the [DexNom source revision](https://github.com/DexNom/VIctory/commit/2e01ab4760db91820ef5f9a9f85761f83cd6b5d0) and ported to the [maintained renderer](https://github.com/fxceless/victory-fork/commit/bc6ee68ea17030fa9531837e395404cfd1856656). Offline validation compares the prefilter and two neighboring Gaussian programs under three conditions. The Wave32 switch-off, Wave32 switch-on and Wave64 control outputs matched the corresponding qualified DXIL byte for byte in all nine comparisons. The F895 port also reproduced all four qualified outputs for the color-grading program and its lookup-table producer with the grade switch both off and on.
