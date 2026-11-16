# Shipped zstd dictionaries

`mainnet.zdict` is compiled into `xkrd` (`xkr_storage::builtin_dictionary("mainnet")`,
`include_bytes!`). An empty file means "no dictionary"; that is the state until one is trained.

## Why it is shipped

Sealed chunks (64 blocks of records, `docs/storage.md`) are compressed with the dictionary in
`META_BLOBS["zstd_dictionary"]`. A fresh database installs the shipped dictionary before its
first chunk is sealed, so every node built from the same source writes byte-identical chunks
(zstd is deterministic for a given version, level and dictionary). Changing the shipped file
does not touch existing databases: they keep the dictionary they were written with until
`xkrd db train-dict --dict FILE` (or `--samples`) re-compresses them explicitly.

## Producing `mainnet.zdict`

On a fully synced mainnet database (node stopped):

```sh
xkrd db train-dict --data-dir ~/.xkr --samples 20000 --max-dict-bytes 110000 \
    --out crates/xkr-storage/dicts/mainnet.zdict
```

This trains over block records spread evenly over the chain (keys already stripped, the same
bytes chunks are made of), writes the dictionary to `--out`, installs it in that database and
re-compresses every sealed chunk with it. Rebuild `xkrd` afterwards so fresh nodes start with it.
Record the source height and the before/after `xkrd db stats` numbers in `docs/storage.md`.

## Result on mainnet (2026-11-13)

Trained on the fully synced mainnet database (height 2 618 387, 20 000 samples, 110 kB): the
dictionary shrank the compressed blocks and prefixes by 0.7 % (18 MB of 2.45 GB). The file is
kept empty on purpose: a shipped dictionary must stay byte-stable forever for chunks to be
identical across nodes, and 0.7 % does not pay for that. The 64-block chunks already give zstd
enough context. See `docs/storage.md`, "Measured on mainnet".
