# Silero VAD test fixtures

`pt-BR-sim.wav` and `pt-BR-sim-nao-sete-nove.wav` were generated specifically
for this test suite with the Windows 11 SAPI voice
`Microsoft Maria Desktop - Portuguese(Brazil)`. They contain only the synthetic
words `sim` and `sim nao sete nove`, respectively, encoded as mono 16-bit PCM
at 16 kHz. They do not contain a real person, customer, or call.

Silence, deterministic low white noise, an isolated impulse, a pure tone, and
a two-tone Windows-style chime are generated in memory by the Rust tests. Their
formulas and fixed seeds live next to the relevant tests, so they remain small,
repeatable, and free of external licensing requirements.
