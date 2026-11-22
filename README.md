<div align="center">

<img src=".github/branding/logo-128.png" width="128" height="128" alt="DivineMC Logo" />

# Linear Converter

Converts Minecraft region files between Anvil, DivineMC's Linear format and the older Linear layouts — fast, in parallel, without losing a chunk.

[![discord](https://cdn.jsdelivr.net/npm/@intergrav/devins-badges@3/assets/cozy/social/discord-plural_vector.svg)](https://discord.gg/qNyybSSPm5)
[![github](https://cdn.jsdelivr.net/npm/@intergrav/devins-badges@3/assets/cozy/available/github_vector.svg)](https://github.com/BX-Team/linear-converter)
[![documentation](https://cdn.jsdelivr.net/npm/@intergrav/devins-badges@3/assets/cozy/documentation/generic_vector.svg)](https://bxteam.org/docs/divinemc)

</div>

## ✨ Features

- **Linear format 2** — reads and writes [DivineMC](https://github.com/BX-Team/DivineMC)'s Linear format: an append-only log of zstd records with shipped dictionaries, about half the size of Anvil; small chunks (void and superflat worlds, empty entity chunks) share packs. Format 1 files read as they are.
- **Both directions** — Anvil to Linear and back, so a world is never locked into one format.
- **Every older layout** — classic Linear v1/v2 (xymb, LinearPurpur, Kaiiju, Leaves), linear v3, DivineMC Linear V1/V2/V3 and Buffered `.b_linear`, including a `.swp` file a crash left behind.
- **Whole worlds** — `region`, `entities` and `poi` of every dimension, Bukkit layouts like `world_nether/DIM-1` included, and Anvil's external `.mcc` chunks.
- **Safe** — damaged chunks are skipped and reported instead of written as garbage; output is written to temporary files and moved into place.
- **Fast** — all CPU threads, bounded memory and IO, a live progress line.
- **Checked against the spec** — the same golden files DivineMC's own reader is tested with.

## 📦 Installation

Every release has the binaries on the [Releases page](https://github.com/BX-Team/linear-converter/releases/latest):

| System | File |
| --- | --- |
| Linux | `linear-converter-x86_64-linux.tar.gz` / `linear-converter-aarch64-linux.tar.gz` |
| Debian, Ubuntu | `linear-converter-amd64.deb` / `linear-converter-arm64.deb` |
| Fedora, openSUSE | `linear-converter-x86_64.rpm` / `linear-converter-aarch64.rpm` |
| Windows | `linear-converter-x86_64-windows.zip` |
| macOS 11+ (Apple silicon and Intel) | `linear-converter-macos.tar.gz` |

Or build it with Cargo:

```bash
cargo install --git https://github.com/BX-Team/linear-converter --locked
```

## ⌨️ Usage

Stop the server first: the converter reads the files as they are on disk.

```bash
linear-converter convert world --to linear -o converted   # Anvil (or an older layout) → Linear
linear-converter convert world --to mca -o converted      # back to Anvil
linear-converter info world                               # formats, chunks and damage, nothing written
```

The output keeps the world's folder structure under `-o`; move it into place once the run finished without errors. The exit code is non-zero when a region failed or a chunk had to be skipped.

| Option | Meaning |
| --- | --- |
| `-t, --to <FORMAT>` | `mca`, `linear`, `linear-legacy` (linear v3, for servers that do not read the current format), `blinear-v2`, `blinear-v3` |
| `-o, --output <DIR>` | where the converted save goes |
| `--from <FORMAT>` | read every input as this format instead of detecting it |
| `-j, --threads <N>` | worker threads (default: all CPU threads) |
| `-l, --level <LEVEL>` | zlib 0–9 for `mca`, zstd 1–22 otherwise (default: 6) |
| `--profile` | where the time went |

Inputs can be world directories, region directories or single region files, several at once.

### Shell completion

The completions ask `linear-converter` itself, so they always match the installed version. The `.deb` and `.rpm` packages set them up; otherwise add one line to your shell's startup file:

```bash
source <(COMPLETE=bash linear-converter)        # ~/.bashrc
source <(COMPLETE=zsh linear-converter)         # ~/.zshrc
COMPLETE=fish linear-converter | source         # ~/.config/fish/config.fish
```

PowerShell: `$env:COMPLETE = "powershell"; linear-converter | Out-String | Invoke-Expression; Remove-Item Env:\COMPLETE` in your profile.

## 📐 Linear format

The format is specified in [`spec/SPEC.md`](spec/SPEC.md), with its zstd dictionaries and golden files. `spec/` is a copy of DivineMC's [`linear-format/`](https://github.com/BX-Team/DivineMC/tree/ver/26.3/linear-format); `scripts/sync-spec.sh <DivineMC checkout>` refreshes it, and `cargo test` checks this implementation against the golden files.

## 🔨 Build from source

It needs the toolchain pinned in `rust-toolchain.toml`.

```bash
git clone https://github.com/BX-Team/linear-converter.git
cd linear-converter
cargo build --release          # target/release/linear-converter
cargo test
```

Release binaries target a baseline CPU. For the fastest conversions build on the machine that runs them with `RUSTFLAGS="-C target-cpu=native" cargo build --release`.

## 🤝 Contributing

Ideas, bug reports and pull requests are welcome in [Issues](https://github.com/BX-Team/linear-converter/issues) or on [Discord](https://discord.gg/qNyybSSPm5).

## ⚖️ License

This project is licensed under the MIT License — see the [LICENSE](LICENSE) file for details.

## 💛 Credits

- [MenthaMC/region_converter](https://github.com/MenthaMC/region_converter) by LuminolMC: the converter this one started from — Anvil, classic Linear and Buffered Linear support and the parallel pipeline.
- [facebook/zstd](https://github.com/facebook/zstd): the compression and dictionaries behind Linear.
