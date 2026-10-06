# Fixtures

Synthetic SteamPipe data for the `cdn` tests: chunks in each compression
wrapper (VZip/LZMA, VZstd, zip), encrypted the way Steam does it, and a v5
manifest with encrypted filenames. The key is bytes `00..1f`.

They're made by `gen.py` with tools independent of fumes (`openssl enc`,
Python's lzma, zipfile and compression.zstd) so the decoder isn't only
checked against itself. Regenerate with `python3 tests/fixtures/gen.py`.
