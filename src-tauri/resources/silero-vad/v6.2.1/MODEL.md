# Silero VAD v6.2.1

- Project: Silero VAD
- Release: `v6.2.1`
- Release commit: `7e30209a3e901f9842f81b225f3e93d8199902b1`
- Official source:
  `https://github.com/snakers4/silero-vad/blob/v6.2.1/src/silero_vad/data/silero_vad.onnx`
- File: `silero_vad.onnx`
- SHA-256:
  `1a153a22f4509e292a94e67d6f9b85e8deb25b4988682b7e174c65279d8788e3`
- License: MIT; the adjacent `LICENSE` is copied from the same official tag.
- ONNX IR version: 8
- ONNX opset: 16

Inspected model signature:

- `input`: float32 `[batch, samples]`
- `state`: float32 `[2, batch, 128]`
- `sr`: int64 scalar
- `output`: float32 `[batch, 1]`
- `stateN`: float32 `[dynamic, dynamic, dynamic]`

At 16 kHz, the official wrapper accepts 512 new PCM samples per call, prepends
64 samples of context, and carries `stateN` into the next `state` input.

The Rust module embeds this artifact at compile time with `include_bytes!`.
There is no model download at runtime and no Tauri filesystem permission is
needed for inference.
