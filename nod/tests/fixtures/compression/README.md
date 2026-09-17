These reference streams were produced by the C implementations, independently of
nod's Rust encoders. The uncompressed bytes are `b"nod pure Rust compression fixture\n"`
repeated 1024 times, followed by bytes 0 through 255 repeated 256 times.

- `reference.zlib`: Python `zlib.compress(data, level=6)`.

The fixtures contain generated test data only. Native tools are not required to
run the tests.
