# ReDive Usage

Installation, launch setups, unpacking, configuration, plugin loading, and logging. Project overview and building: [README.md](README.md).

---

## Zero-Touch Philosophy

> **Important**: DMM-managed games require authentication tokens passed by DMM Game Player at launch. Launching the game executable directly from its directory without an active session will fail. Furthermore, DMM updates and file verification passes can wipe unexpected files from both the game folder and the launcher directory.

ReDive solves this with a completely **out-of-tree / zero-touch** setup:
- The entire ReDive toolkit (`redive.exe`, `amulet.dll`, `prism.dll`, `senbei.exe`, `redive.ini`, `amulet.conf`, and `plugins/`) lives in a standalone folder outside the game and launcher trees (e.g. `C:\ReDive` on Windows or `/media/.../ReDive` on Linux).
- No game files are corrupted or permanently modified without backups (`X.old` originals are preserved).
- No Windows registry keys or Image File Execution Options (DotLocal redirection) are touched, ensuring 100% compatibility with Wine/Proton.

---

## Installation

### 1. Dedicated ReDive Folder
Create a standalone directory anywhere on your system:
- **Windows**: `C:\ReDive` (or any convenient drive/folder)
- **Linux / Proton**: `~/.local/share/redive` or inside/accessible by your Wine prefix

Copy the following files into that folder:
```
ReDive/
├── redive.exe          (CLI orchestrator & embedded Senbei unpacker)
├── amulet.dll          (DMM hide layer & process interceptor)
├── prism.dll           (Game plugin loader)
├── redive.ini          (Configuration file)
└── plugins/            (Folder for native mod DLLs)
```

`amulet.dll` and `prism.dll` must sit beside `redive.exe`. The Senbei static unpacker engine is embedded directly into `redive.exe` (a separate `senbei.exe` is not required, though an external binary can still be specified if desired).

---

## Inspection Phase (`redive scan` / `redive search`)

Before unpacking or when diagnosing protected installations, you can probe files and scan directory trees recursively:

```sh
# Scan entire game directory across all subfolders (alias: 'search')
redive scan "C:\Games\ExampleGame"
redive search "C:\Games\ExampleGame"

# Deep scan (probes all candidate files regardless of minimum size)
redive scan "C:\Games\ExampleGame" --all
redive search "C:\Games\ExampleGame" --all

# Inspect a single binary or file
redive scan "C:\Games\ExampleGame\ExampleGame.exe"
```

The scanner traverses all nested subfolders (e.g. `Game_Data/Plugins/x86_64/`), detects protected PE binaries, reports key header magic, links auxiliary companion payloads (`.exe._`), and flags obfuscated IL2CPP metadata without modifying any files.

---

## Initialization & Unpacking (`redive init` & `redive unpack`)

### First-Time Setup & Game Updates (`redive init`)
Before launching a protected game for the first time, or after a publisher game update that rewrites game files, run `redive init`. This unpacks the game tree, swaps unpacked files into place (originals kept as `X.old`), and initializes the `amulet.conf` hide manifest:

```sh
redive init
# Or explicitly pass the path:
redive init "C:\Games\ExampleGame"
```

### Static Unpacking Only (`redive unpack`)
To simply unpack protected binaries into `.unpack.` files (e.g. for inspection, modding, or analysis) without modifying any game files or touching `amulet.conf`:

```sh
# Unpack all protected binaries into an "out" folder inside the game directory
redive unpack "C:\Games\ExampleGame"

# Unpack a single file into "out"
redive unpack "C:\Games\ExampleGame\GameAssembly.dll"

# Specify an explicit custom output directory
redive unpack "C:\Games\ExampleGame" "C:\Output"
```

All unpacked files (e.g. `GameAssembly.unpack.dll`, `global-metadata.unpack.dat`) are placed flat into the output directory regardless of their internal subfolder depth. Original game files remain completely untouched and no `.old` files are created.

### Restoring Original Files (`redive restore`)
To completely revert the game tree back to its original protected state:

```sh
redive restore
# Or explicitly pass the path:
redive restore "C:\Games\ExampleGame"
```

This recursively finds all `.old` backup files, restores them back to their original names, removes the unpacked files, and cleans up `amulet.conf`. It does not launch the game.

### Auto-Detection & Options
If `redive.ini` contains `target = ...`, both `redive init` and `redive unpack` automatically use that path with the embedded unpacker.

To override with an external unpacker executable:
```sh
redive init "C:\Games\ExampleGame" "C:\Tools\senbei.exe"
```

### How the Unpack & Swap Works
1. **Static Analysis**: Senbei scans the game directory and unpacks protected PE executables and assemblies into a clean staging area. Bulk asset files are never opened.
2. **Swap Contract (`init`)**:
   - `X` on disk &rarr; The clean, unpacked runnable file that executes.
   - `X.old` on disk &rarr; The untouched original protected file that DMM's verify pass inspects.
3. **Hide Manifest Generation (`amulet.conf`, created during `redive init`)**:
   - ReDive automatically discovers all swapped pairs (`*.old`) and writes their basenames into `amulet.conf`.
   - Seeded mod files (such as `dxgi.dll`, `BepInEx/`, `doorstop_config.ini`, `global-metadata.dat`) are automatically included in `amulet.conf` so DMM's verify pass never detects or deletes them.

---

## Configuration (`redive.ini`)

Create `redive.ini` beside `redive.exe`. All settings live under the `[main]` section:

```ini
[main]
# Path to the game directory or game executable.
# Accepts Windows absolute paths (C:\...), relative paths, or Unix paths (/media/...).
target = "C:\Games\ExampleGame\Game.exe"

# Set to true to route the launch through DMM Game Player.
dmm = true

# Optional: Explicit path to DMM Game Player or a community fast launcher.
# (If omitted with dmm = true, standard installation locations are auto-detected).
# launcher = "C:\Program Files\DMMGamePlayer\DMMGamePlayer.exe"

# Optional: Enable or disable plugin loading (default: true if plugins/ exists).
# plugins = true

# Optional: Custom plugin directory (default: <ReDive>/plugins).
# plugins_dir = "C:\ReDive\plugins"

# Optional: Explicit path to an external senbei.exe (default: uses embedded unpacker).
# senbei = "C:\Tools\senbei.exe"
```

### Path Resolution
`target`, `launcher`, `plugins_dir`, and `senbei` accept:
1. **Windows absolute paths**: `C:\Games\Game.exe`, `D:\...`.
2. **Relative paths**: Anchored to `redive.ini`'s own folder.
3. **Wine / Proton Unix paths**: `/media/games/...` &rarr; automatically resolved through Wine's `Z:` mount or the prefix's `drive_<letter>` drive mappings.

---

## Launch Setups

### 1. DMM Game Player (Standard)
1. In `redive.ini`, set `dmm = true` and point `target` at your game.
2. Run `redive` (or double-click `redive.exe`).
3. ReDive starts `DMMGamePlayer.exe` suspended and injects `amulet.dll` via early-bird APC before resuming it.
4. When you click **Start** in DMM Game Player:
   - DMM's verify pass queries file attributes and hashes: `amulet.dll` redirects all checks to `X.old`, keeping unpacked files safe and passing integrity checks.
   - When DMM spawns the game process, `amulet.dll` intercepts `CreateProcessW` synchronously, spawns the game suspended, queues `prism.dll` via APC, and resumes the game.
   - The game launches cleanly without SEH exceptions or watchdog faults.

### 2. DMM Fast Launcher
Unlike the official DMM Game Player, [DMM Fast Launcher](https://github.com/fa0311/DMMGamePlayerFastLauncher) operates with key architectural differences:
- **No GUI Launch Button**: The Fast Launcher GUI is for configuration and shortcut creation only. Games are launched by **shortcut ID** (e.g. `Priconne`, corresponding to `data/shortcut/<id>.json`).
- **No File Verification Pass**: Fast Launcher queries DMM APIs for session tokens and launches the game directly without hashing or wiping unrecognized files.
- **PyInstaller & Command Carrier Chain**: Fast Launcher is a PyInstaller single-file bundle that re-executes itself (`is_own_image`), then spawns `cmd.exe /c ...` (`is_carrier_exe`), which finally spawns the target game executable.

`amulet.dll` automatically propagates through the PyInstaller child and arms `cmd.exe` via early-bird APC. When `cmd.exe` spawns the game, `amulet.dll` intercepts the creation and injects `prism.dll`.

#### Configuration via `redive.ini`:
```ini
[main]
target = "C:\Games\ExampleGame\Game.exe"
launcher = "C:\Users\<user>\AppData\Roaming\DMMGamePlayerFastLauncher\DMMGamePlayerFastLauncher.exe"
args = "Priconne"    # The shortcut id in Fast Launcher (without .json)
plugins = true
```

#### Command-Line Execution:
You can also pass the shortcut ID directly via CLI:
```cmd
redive.exe Priconne
```
Or with explicit paths:
```cmd
redive.exe "C:\Users\<user>\AppData\Roaming\DMMGamePlayerFastLauncher\DMMGamePlayerFastLauncher.exe" Priconne
```

> **Note on Admin Elevation**: If the game requires administrator rights, ensure the shortcut or ReDive is run as administrator so Windows UAC does not detach the process from the hook chain.

### 3. Steam / Proton Launch Options
When launching via Steam on Linux / Proton:
1. Keep the ReDive directory accessible to the Steam prefix.
2. In the game's Steam properties, set the **Launch Options** (replacing `Game.exe` with the actual executable name):
   ```sh
   eval $(echo "%command%" | sed 's/Game\.exe/redive\.exe/')
   ```
3. Launch normally via Steam.

### 4. Direct / Standalone Execution
For titles or configurations that run without launcher authentication:
```sh
redive "C:\Games\MyGame\Game.exe"
```
Or simply run `redive` with `target = Game.exe` in `redive.ini` (without `dmm = true`). The game is spawned suspended and `prism.dll` is queued via early-bird APC immediately.

---

## Plugins & Mods (`prism`)

Native DLL plugins live in the `plugins/` directory next to `prism.dll` (`<ReDive>/plugins/`).

```
ReDive/
├── redive.exe
├── amulet.dll
├── prism.dll
└── plugins/
    ├── Plugin.dll         # Native mod DLL
    ├── config.json        # Mod configuration
    └── assets/            # Companion mod assets
```

### Dependency Isolation
Before loading each candidate DLL with `LoadLibraryW`, `prism` sets `SetDllDirectoryW` to the plugin's folder. Sibling dependencies (DLLs located next to the mod) resolve naturally without needing to be copied into the game or `System32` directory.

### Environment Overrides
| Environment Variable | Description | Default |
|---|---|---|
| `REDIVE_LOAD_PLUGINS` | Force enable (`1`, `true`, `on`) or disable (`0`, `false`, `off`) plugin loading. | `true` if `plugins/` exists |
| `REDIVE_PLUGINS_DIR` | Override directory where plugins are scanned. | `<ReDive>/plugins` |
| `REDIVE_PLUGIN_ALLOWLIST` | Comma-separated list of DLL filenames to load (all others skipped). | Empty (load all candidates) |

---

## Handling Game Updates

When the publisher pushes an update through DMM Game Player:
1. DMM updates or rewrites the game files.
2. Run `redive init`:
   - ReDive detects that `X` has been updated with a new protected binary from DMM.
   - It rotates `X` &rarr; `X.old` (replacing the old backup with the newly updated original).
   - Senbei unpacks the new binary and installs it as `X`.
   - `amulet.conf` is updated.
3. Launch normally via `redive`.

---

## Command-Line Reference

| Command | Aliases | Description |
|---|---|---|
| `redive [launch] [exe] [args...]` | | Launch game or launcher with amulet/prism injection (default action when double-clicked or passed an executable). |
| `redive init [game_dir] [senbei_exe] [staging_dir]` | | Unpack binaries, swap into place (`X` & `X.old`), and generate `amulet.conf`. |
| `redive unpack [path_or_dir] [out_dir] [senbei_exe]` | | Statically unpack binaries into flat `.unpack.` files (defaults to `out/` folder) without touching game files. |
| `redive restore [game_dir]` | | Revert game tree back to original protected binaries (`X.old` → `X`) and remove `amulet.conf`. |
| `redive scan [path] [--all]` | `search`, `probe`, `check` | Probe binaries, auxiliary companions (`.exe._`), and obfuscated metadata. |
| `redive -h` / `--help` | `help` | Show command usage and options. |
| `redive -V` / `--version` | `version` | Show ReDive version. |

> **Note**: Any unrecognized subcommands or invalid syntax will immediately error out with an exit code rather than defaulting to launch.

---

## Logs

ReDive writes operational logs next to `redive.exe`:
- **`redive.log`**: Launch resolution, target selection, process spawning, and `prism` plugin loader status.
- **`amulet.log`**: DMM file redirection events (`CreateFileW`, `GetFileAttributesW`, etc.) and `CreateProcessW` game interception events.
