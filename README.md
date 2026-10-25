# ReDive

Runs CrackProof-protected Windows games under Proton/Wine — and natively on Windows — by statically unpacking them ([Senbei](https://xn--ri8h.gitbook.io/crackproof-research/senbei)), hiding the unpacked files from DMM Game Player's verify pass (`amulet`), and loading native plugins (`prism`), all from an external folder outside the launcher's and the game's directories.

No registry edits, no Windows Image File Execution Options (DotLocal redirection), and no persistent files in managed game or launcher directories.

Inspired by GateJumper. Named after the mechanic that rewinds a timeline back to how it was while keeping everything that mattered: the original stays on disk as `X.old`, the unpacked copy runs as `X`.

## Why ReDive?

While [GateJumper](https://github.com/HachiFridge/GateJumper) dynamically intercepts the entry point in memory at runtime without touching game files, certain anti-cheat packer implementations install complex Structured Exception Handling (SEH) traps, Vectored Exception Handlers (VEH), and page-encryption watchdogs that fail under dynamic jump interception.

ReDive solves this with a complementary **static bypass** strategy:
1. **Static Restoration**: Game executables and assemblies are cleanly unwrapped on disk via Senbei, eliminating runtime packer faults entirely.
2. **Filesystem Masking**: `amulet.dll` is injected into DMM Game Player to intercept Win32 filesystem checks (`CreateFileW`, `DeleteFileW`, `GetFileAttributesW`, `MoveFileW`), redirecting verification reads back to the pristine `X.old` originals.
3. **Synchronous Game Interception**: When DMM Game Player starts the game, `amulet.dll` catches the spawn in `CreateProcessW`, queues `prism.dll` via early-bird APC while suspended, and resumes execution—guaranteeing 0ms startup delay.
4. **Isolated Plugin Loading**: `prism.dll` loads native mods from an external `plugins/` folder with automatic DLL search-path resolution (`SetDllDirectoryW`).

## Components

| Binary | Role | Description |
|---|---|---|
| `redive.exe` | CLI & Orchestrator | Embeds the Senbei static unpacker engine and recursive deep scanner (`redive scan`/`search`, `redive init`, `redive unpack`, `redive restore`) and launches games/launchers (`redive launch` or direct execution). |
| `amulet.dll` | Hide Layer & Interceptor | Injected into DMM Game Player. Hooks filesystem inspection APIs to redirect reads to `X.old` and intercepts `CreateProcessW` to synchronously catch game spawns and deliver `prism.dll` via early-bird APC. |
| `prism.dll` | Plugin Loader | Injected into the unpacked game process before engine initialization. Scans `<ReDive>/plugins/` and safely loads native mods with isolated DLL search path management. |

## Building

Targets `x86_64-pc-windows-msvc`. Binaries run on both Windows and Wine/Proton.

**On Windows:**
```sh
cargo build --release
```

**On Linux (cross-compilation via [cargo-xwin](https://github.com/rust-cross/cargo-xwin)):**
```sh
cargo xbuild
```

Outputs land in `target/x86_64-pc-windows-msvc/release/` (or `target/release/` for native builds).

**Unit tests:**
```sh
cargo test --workspace
```

## Usage

Complete installation, launch setup (Steam, non-Steam, DMM Game Player, DMM Fast Launcher), unpacking rotation, configuration reference, and logging are documented in **[USAGE.md](USAGE.md)**.

### Quick Start
1. Create an external folder (e.g. `C:\ReDive` or `/media/.../ReDive`) containing `redive.exe`, `amulet.dll`, and `prism.dll`.
2. Configure `redive.ini` next to `redive.exe`:
   ```ini
   [main]
   target = Game.exe
   dmm = true
   ```
3. Run the init step to unpack game binaries and generate `amulet.conf`:
   ```sh
   redive init
   ```
4. Place any native mod DLLs into `plugins/`.
5. Launch the game:
   ```sh
   redive
   ```

## Acknowledgements

- **[GateJumper](https://github.com/HachiFridge/GateJumper)** — The runtime injector architecture and out-of-tree launcher orchestration that inspired ReDive.
- **[Senbei](https://github.com/Momoko-Ayase/Senbei)** ([Docs](https://xn--ri8h.gitbook.io/crackproof-research/senbei)) — The static unpacker engine enabling clean, byte-accurate restoration of protected PE and ELF binaries.
- **[umahide](https://github.com/Fernsicles/umahide)** — The original file-verification redirection concept that inspired `amulet`'s hide layer.
- **[FunnyHoney](https://gitlab.com/LeadRDRK/FunnyHoney)** (LeadRDRK) & **[Cellar](https://github.com/Hachimi-Hachimi/Cellar)** — Foundational research on commercial anti-cheat stubs and Wine/Proton environment compatibility.
- **[MinHook](https://github.com/TsudaKageyu/minhook)** — Minimalistic x86/x64 API hooking library for Windows.

## License

AGPL-3.0 (see [LICENSE](LICENSE)) — required by Senbei, which this project integrates.
