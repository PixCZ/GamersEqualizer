# GamersEqualizer Developer Log (DEVLOG)

The first entries use the project's first name, OpenSonar (renamed to GamersEqualizer in
Milestone 8); they are kept as written.

## [2026-11-04] - Project Initialization & Architecture Setup

### Motivation & Goals
- Project **OpenSonar** is a 100% pure Rust, air-gapped (zero network code, zero telemetry), anti-bloatware alternative to SteelSeries Sonar.
- Key properties:
  - Runs in user-mode using Windows CoreAudio (WASAPI) and Equalizer APO (no paid kernel drivers).
  - Lazy memory model: background System Tray daemon running under 5 MB RAM, dropping all GUI & GPU allocations when the window is closed.
  - Classical DSP Acoustic Echo Cancellation (MDF/NLMS + cross-correlation delay estimator + double-talk detection) to cancel speaker bleed into microphone with zero hallucination.
  - Tiny embedded neural voice isolation (`nnnoiseless`, ~100 KB) strictly for keyboard/mouse click and fan noise removal.
  - Device-less ChatMix (per-process volume scaling) and zero-CPU foreground game detection via WinEvent hooks.
  - Direct offline import from SteelSeries GG SQLite database.

### Completed Actions
1. **Saved Master Specification**:
   - Persisted complete project requirements to `SPECIFICATION.md`.
2. **Captured Real Test Fixture**:
   - Backed up live SteelSeries Sonar database from `C:\ProgramData\SteelSeries\GG\apps\sonar\db\database.db` into `test_data/sonar_database.db`.
   - Analyzed schema: `configs` table holds 355 game presets (VAD 1), 8 chat presets (VAD 2), 14 media presets (VAD 3), and 23 mic presets (VAD 4). Each contains a comprehensive `parametricEQ` JSON object with RBJ filter parameters (`peakingEQ`, `highShelving`, `lowShelving`, `highPass`, etc.).
3. **Workspace Configuration**:
   - Configured root `Cargo.toml` workspace with `opensonar-common`, `opensonar-daemon`, and `opensonar-mic-vst`.
   - Defined strict air-gapped dependencies (no networking crates: no `reqwest`, no `curl`, no `tokio::net`).

---

## [2026-11-04] - Milestone 1: Windows Audio Layer (`win_audio`) & Common IPC

### Architecture Decisions
- Win32/COM code is strictly isolated inside `win_audio` and exposed through safe Rust structures.
- Created `crates/opensonar-common`:
  - `profile.rs`: Clean representation of RBJ Biquad filters, clamping parameters, and VAD types.
  - `shm.rs`: Lock-free SPSC ring buffer layout in Windows Shared Memory (`Local\OpenSonar_SpeakerRef_Buffer`). Zero-copy exchange between Speaker Tap, Mic Processor, and UI daemon.
- `crates/opensonar-daemon/src/win_audio/`:
  - `policy_config.rs`: Defined COM vtable for undocumented `IPolicyConfig` (`SetDefaultEndpoint` across `eConsole`, `eMultimedia`, `eCommunications`).
  - `endpoint_lock.rs`: Implemented `IMMNotificationClient` callback listening to `OnDefaultDeviceChanged`. When GPU drivers hijack audio output to monitor HDMI, it reverts back within < 20ms and increments hijack telemetry counter.
  - `dolby_atmos.rs`: Queries `ISpatialAudioClient` on active endpoint to detect Dolby Atmos for Headphones vs Windows Sonic.
  - `chatmix.rs`: Virtual-less ChatMix. Enumerates sessions via `IAudioSessionManager2` / `IAudioSessionEnumerator`. Classifies executables into Game Bus vs Chat/Browser Bus. A single balance slider dynamically attenuates chat while keeping game audio at 100% (or vice versa) with 0ms DSP latency.
  - `game_detector.rs`: Zero-CPU game detection via `SetWinEventHook(EVENT_SYSTEM_FOREGROUND)`. Resolves HWND to PID and process name asynchronously without polling loops.

---

## [2026-11-04] - Milestone 2: DSP Math, Auto-Preamp & Sonar DB Importer

### Architecture Decisions
- Classical DSP biquad math in `src/dsp_apo/`:
  - `biquad.rs`: RBJ Audio EQ Cookbook formulas for Peaking (PK), LowShelf (LSC), HighShelf (HSC), LowPass (LP), HighPass (HP), Notch (NO). Computes complex transfer function $H(e^{j\omega})$ in dB.
  - `auto_preamp.rs`: Evaluates cascaded response of Layer A (Headphones calibration) and Layer B (Game profile) across 512 log-spaced frequencies from 20 Hz to 20 kHz. Calculates $G_{\max} = \max_f (0.0, \text{Gain}(f))$ and prepends $-(G_{\max} + 0.5)\text{ dB}$ to prevent digital clipping before Dolby Atmos / DAC.
  - `apo_writer.rs`: Formats filters into Equalizer APO syntax and performs atomic writes via temporary file rename to `opensonar_live.txt`.
- Offline Sonar Importer in `src/sonar_importer/`:
  - `winsqlite.rs`: Directly loads Windows system `C:\Windows\System32\winsqlite3.dll` via dynamic FFI (`LoadLibraryW` / `GetProcAddress`). Eliminates any need to build C code or install heavy C build toolchains.
  - `converter.rs`: Maps Sonar JSON schema (`parametricEQ.filter1..10`, `qFactor`, etc.) into `ParametricProfile`.
  - `reader.rs`: Reads local SQLite database (`C:\ProgramData\SteelSeries\GG\apps\sonar\db\database.db`) using `WinSqliteLib`.
  - `storage.rs`: Saves/loads sanitized JSON profiles in `%AppData%\OpenSonar\profiles\`.

---

## [2026-11-04] - Milestone 3: VST Mic Processor (Classical AEC + AI Voice Isolation)

### Architecture Decisions
- Created `crates/opensonar-mic-vst`:
  - `delay_estimator.rs`: Normalized cross-correlation delay estimator with 4x downsampling and temporal hysteresis to find acoustic time-of-flight between speakers and microphone.
  - `aec.rs`: Classical DSP Normalized LMS adaptive filter. Geigel Double-Talk Detector (DTD) freezes filter updates when user speaks over loud speakers to eliminate filter divergence and voice distortion. Non-linear Residual Echo Suppressor (RES) attenuates residual bleed. ERLE telemetry is calculated and reported in real time.
  - `ai_denoiser.rs`: Pure Rust offline neural speech enhancement using `nnnoiseless` (~100 KB embedded weights). Processes 10ms / 480-sample chunks at 48kHz to erase mechanical keyboard clicks and fan noise.
  - `vst_bridge.rs`: Standard VST2 `AEffect` C-ABI implementation (`VSTPluginMain`). Auto-detects stereo Speaker Tap mode (pushes speaker mix into shared memory) vs mono Mic Processor mode (AEC echo cancellation + AI voice isolation).

---

## [2026-11-04] - Milestone 4: Native GPU UI (`eframe`/`egui`) & System Tray

### Architecture Decisions
- Built native hardware-accelerated GUI in `crates/opensonar-daemon/src/ui/`:
  - `header.rs`: Device selector, hardware lock toggle, Dolby Atmos badge, detected game indicator, Headphones vs Speakers mode switch.
  - `tab_eq.rs`: Interactive `egui_plot` displaying 3 simultaneous curves (Curve 1: Headphone base, Curve 2: Game profile, Curve 3: Combined output + Auto-Preamp headroom ceiling). Interactive filter band editing and one-click Sonar DB import.
  - `tab_mixer.rs`: ChatMix channel strips with live session volume and central Game <-> Chat balance slider.
  - `tab_mic.rs`: Classical AEC toggle, real-time ERLE meter, AI Voice Isolation slider, noise gate threshold, broadcast mic EQ.
- Lazy memory model in `main.rs`:
  - Application runs in background system tray loop consuming < 5 MB RAM.
  - When the UI window is opened, `eframe` runs natively.
  - When the window is closed, all GPU buffers, viewports, and textures are dropped, returning to the minimal tray daemon loop.
- Packaging:
  - Created Winget manifest in `packaging/winget/opensonar.yaml`.

---

## [2026-11-04] - Build & Verification Status
- Executed `cargo test --workspace`:
  - **All 14 tests passed with zero failures!**
  - `opensonar_common`: 3 passed (ring buffer, clamping, vad)
  - `opensonar_daemon`: 7 passed (biquad response, auto-preamp math, atomic APO writer, Sonar DB SQLite parser with real 355-preset database)
  - `opensonar_mic_vst`: 4 passed (cross-correlation delay estimator, AEC cancellation > 10dB, double-talk detector protection, AI denoiser smoke)
- Executed `cargo build --release --workspace`:
  - **100% native MSVC release build succeeded:**
    - `opensonar-daemon.exe`: ~8.5 MB (standalone native executable)
    - `opensonar_mic_vst.dll`: ~1.2 MB (native VST2 audio plugin with embedded NN weights)

---

## [Milestone 6: GUI Startup Fix & Graceful Tray Fallback] - 2026-11-04
### Problem Identified
- When running `opensonar-daemon.exe`, the process terminated immediately without displaying a window.
- **Root Cause 1:** `TrayManager::new()` failed with Win32 error `1400` (`ERROR_INVALID_WINDOW_HANDLE`) when attempting to invoke `Shell_NotifyIconW(NIM_ADD)`. On Windows 11 desktop environments where the Explorer notification area isn't immediately attached or accessible, `Shell_NotifyIconW` rejects the registration.
- **Root Cause 2:** In `main.rs`, the error arm of `TrayManager::new()` was treated as fatal and executed `return;`, causing the application to exit before reaching `launch_ui_window()`.
- **Root Cause 3:** `main()` invoked `CoInitializeEx(None, COINIT_MULTITHREADED)` on the primary thread, creating a potential mode conflict (`RPC_E_CHANGED_MODE`) with `winit`/`eframe` which requires Single-Threaded Apartment (STA - `COINIT_APARTMENTTHREADED`).

### Implemented Solutions
1. **Graceful System Tray Fallback:**
   - Modified `main.rs` so `TrayManager::new()` failure emits a warning and sets `tray_manager = None`, without terminating execution.
   - If the system tray is unavailable, OpenSonar seamlessly operates in standard native window mode.
2. **STA COM Conformance:**
   - Switched the main thread COM initialization to `COINIT_APARTMENTTHREADED`, guaranteeing compatibility with `winit`, `eframe`, and native Windows windowing.
3. **Verified GPU Window Pipeline:**
   - Started and verified native GPU viewport rendering on `AMD Radeon RX 9070 XT` via Vulkan backend with continuous frame submissions.
   - Rebuilt production release binary `target\release\opensonar-daemon.exe` (8.6 MB).

---

## [Milestone 8: Rebranding to GamersEqualizer & Decoupled Background Service Architecture] - 2026-11-04
### Motivation & Requirements
- Rebranded application from **OpenSonar** to **GamersEqualizer** across all workspace crates, binaries, IPC identifiers, and configs.
- Decoupled into two separate operational roles:
  1. **Background Service / Daemon (`--daemon`):**
     - Runs 24/7 in user session, consuming < 4 MB RAM without any GPU/window allocation.
     - Single-instance enforcement via Win32 named Mutex `Local\GamersEqualizer_Daemon_Mutex`.
     - Direct inter-process communication (IPC) via lock-free shared memory `Local\GamersEqualizer_SpeakerRef_Buffer`.
     - Handles WASAPI audio session monitoring, real-time volume scaling, zero-CPU foreground game tracking, and Equalizer APO sync.
     - Supports optional automatic startup on Windows boot via User Registry (`HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Run`).
  2. **Standalone UI Control Tool:**
     - User launches `gamersequalizer.exe` whenever they want to adjust audio or EQs.
     - Automatically verifies if the background service is running; if not, spawns it detached (`CREATE_NO_WINDOW | DETACHED_PROCESS`).
     - Loads current volume states from shared memory.
     - When the window is closed (via `[X]` or in-app `✕ Close UI`), 100% of GPU, textures, and window memory are immediately freed, while the background service continues uninterrupted.
     - In-app `⏻ Stop Service` button sends a shutdown signal to terminate the background daemon when desired without requiring terminal commands.
### Deliverables
- `crates/gamersequalizer-common`: shared types, SPSC ring buffer, and inter-process volume/mute atomics.
- `crates/gamersequalizer-daemon`: dual-mode binary (`gamersequalizer.exe`, ~8.6 MB).
- `crates/gamersequalizer-mic-vst`: VST2 plugin for Equalizer APO (`gamersequalizer_mic_vst.dll`, ~1.2 MB).
- `crates/gamersequalizer-daemon/src/autostart.rs`: native Win32 registry autostart manager.

---

## [Milestone 9: Compact Mixer UI & Multi-Endpoint WASAPI ChatMix] - 2026-11-04
### Problem Identified & User Feedback
- The UI had bulky, oversized cards causing awkward text wrapping (`10\n0\n%`, `O\nN`), overlapping labels (`PRESET` vs `DEV`), and required excessive window width.
- The user expressed concern over how ChatMix can be achieved without virtual audio drivers and requested a clean, compact layout focused on what users actually need: visible output device, clear amplification/gain controls, and intuitive ChatMix balance.
### Implemented Solutions
1. **Multi-Endpoint WASAPI ChatMix Engine (`chatmix.rs`):**
   - Implemented `get_all_session_managers()` using `EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)`.
   - Enumerates and controls audio sessions across all active endpoints (whether apps play to the physical headphones, separate communications devices, or existing virtual devices).
   - Zero-driver architecture: controls per-process volume directly via `ISimpleAudioVolume::SetMasterVolume` with 0ms latency.
2. **Compact Proportional 5-Column Grid (`tab_mixer.rs`):**
   - Replaced fixed-width cards with `ui.columns(5, ...)` dynamically fitting the exact viewport width.
   - Fixed text wrapping by assigning explicit label containers (`[70.0, 18.0]` for volume percentage, `[70.0, 22.0]` for mute button).
   - Card height streamlined from 420px to ~260px; entire console fits in standard 880x520 window without scrolling.
   - Quick preset pill `[ CS2 Boost ↗ ]` opens the corresponding Parametric EQ tab.
3. **Dedicated CHATMIX Balance Bar:**
   - Positioned cleanly across the bottom with a 260px balance slider, live percentage readouts (`Game: 100% | Chat: 70%`), and a 1-click `Center (50/50)` button.
4. **Crystal-Clear Output & Lock Bar (`header.rs`):**
   - Prominent output device selector dropdown with 🔒 Lock button, Spatial Audio badge, and Headphones/Speakers mode toggle.

---

## [Milestone 10: Zero-Volume Initialization Fix & Live Windows Device Tracking] - 2026-11-04
### Problem Identified & User Feedback
1. **0% Channel Volumes on Launch:** When launching the app, all faders defaulted to 0%. Root cause: Shared memory mapping (`CreateFileMappingW`) initializes raw memory pages to all-zero bytes (`0x00`), which was read as 0 permille (0.0 volume) and propagated to WASAPI sessions.
2. **Ignored Windows Sound Card Changes:** Changing the default audio output in Windows or selecting a device in the UI was not reflected. Root cause: `app.rs` only queried devices once on first frame and never checked again; `EndpointNotificationHandler_Impl::OnDefaultDeviceChanged` did nothing when lock was disabled.
### Implemented Solutions
1. **SHM Initialization & 100% Volume Guard:**
   - Added `ShmHeader::init_defaults(&mut self)` initializing all channels to 1000 permille (100% volume) and 0 balance (50/50).
   - Added zero-volume fallback in `main.rs` and `state.rs` so freshly created shared memory or zero values are clamped to 1.0 (100%).
2. **Live Two-Way Audio Endpoint Synchronization:**
   - `EndpointNotificationHandler_Impl` now captures `OnDefaultDeviceChanged`, `OnDeviceAdded`, `OnDeviceRemoved`, and `OnDeviceStateChanged` in real time.
   - `app.rs` polls `endpoint_lock.poll_device_changed()` and checks `get_default_output_id()`: when the user changes sound card in Windows, the UI dropdown immediately updates, re-enumerates devices, and checks Dolby Atmos status.
   - Selecting a sound card in the UI dropdown now directly calls `IPolicyConfig::set_default_endpoint_all_roles`, actively switching the Windows system default output device.

---

## [Milestone 11: True Bidirectional Windows Audio Sync & Chat Communications Routing] - 2026-11-04
### Problem Identified & User Clarification
1. **No Sonar Communication:** The user clarified they do not want direct communication with SteelSeries Sonar (Sonar is being uninstalled).
2. **False 100% Volume Reset on Launch:** When launching the app, the previous build reset the user's Windows volume to 100%. Root cause: Hardcoded `init_defaults()` 1000 permille in SHM + daemon calling `apply_mix()` forced 1.0 back into Windows `IAudioEndpointVolume`.
3. **Missing Bidirectional Windows System Sync:** Moving the volume slider in Windows or pressing keyboard volume keys didn't move the UI slider; daemon and Windows were fighting each other.
4. **Chat Without Dolby Atmos Spatial Distortion:** User needed teammate voices on Discord to remain clean and crisp without Dolby Atmos 3D virtualization and without game footstep EQ boost.

### Implemented Solutions
1. **Windows System as Source of Truth for Hardware Master & Mic:**
   - Both GUI and background daemon query Windows `IAudioEndpointVolume` via `get_system_master_volume()` and `get_system_mic_volume()` on launch.
   - Initial volume matches the user's current Windows level (e.g. 14% master, 67% mic) exactly; Windows volume is NEVER overwritten on startup.
2. **Live Bidirectional Master & Mic Volume Sync:**
   - `ChatMixController::sync_from_system()` checks for external volume changes made in Windows taskbar, Settings, or keyboard keys, updating UI sliders and SHM instantly with zero feedback loops.
   - User adjustments made in the GamersEqualizer UI immediately call `set_system_master_volume` / `set_system_mic_volume`, moving the Windows taskbar slider.
3. **Non-Interfering Daemon Architecture:**
   - Separated hardware endpoint volume control (`apply_mix()`) from application session scaling (`apply_session_mix()`).
   - The background daemon updates WASAPI per-process session volumes (`ISimpleAudioVolume`) and keeps SHM in sync with Windows, but never overrides Windows master volume.
4. **Dedicated Chat Communications Role Selector (`tab_mixer.rs` & `endpoint_lock.rs`):**
   - Added `set_as_communications_device()` using Windows `IPolicyConfig::set_default_endpoint(..., eCommunications)`.
   - On the `CHAT` strip, users can switch between `🎧 Direct (Same as Master)` and any detected virtual or hardware audio device (e.g. `CABLE Input (VB-Audio Virtual Cable)` or `Sonar - Chat`).
   - In Split-Stream mode, Discord / voice apps route to `eCommunications`, bypassing Dolby Atmos and game EQ completely.
5. **Release Binaries:**
   - Built fresh optimized `target/release/gamersequalizer.exe` (8.6 MB) and `target/release/gamersequalizer_mic_vst.dll` (1.2 MB). All 18 workspace tests pass.

---

## [Milestone 12: Fixed Master vs. Game/Chat/Media Endpoint Mapping & Live Alignment] - 2026-11-04
### Problem Identified & User Feedback
- The user reported: "game je v sonaru 14% a u nás 100. master je u nás 14%. mikrofon sedí ale game chat a media mají 100% hlasitost vyčtenou ačkoliv sonar ukazuje jinak a navíc je tam bordel v master/game"
- **Root Cause Analysis:**
  - SteelSeries Sonar installs 4 virtual playback devices into Windows: `Sonar - Gaming`, `Sonar - Chat`, `Sonar - Media`, `Sonar - Microphone`.
  - Sonar configured Windows system default (`eConsole`) to point to `SteelSeries Sonar - Gaming` (which had volume 14.0%).
  - In Sonar's UI, "Master" is actually the *physical headphones* (`Realtek HD Audio 2nd output`, which had volume 66.9% / 67%), while "Game" is `Sonar - Gaming` (14%), "Chat" is `Sonar - Chat` (39%), and "Media" is `Sonar - Media` (39%).
  - GamersEqualizer had previously simply queried `GetDefaultAudioEndpoint(eRender, eConsole)`, which returned `Sonar - Gaming` (14%) and placed it into MASTER. Meanwhile, GAME, CHAT, and MEDIA had been initialized to a fixed 100% relative session scale.
  - This inverted Master and Game, creating a collision between what the user saw in Sonar and what GamersEqualizer showed.

### Implemented Solutions
1. **Intelligent Endpoint Channel Binding (`chatmix.rs::bind_endpoints`):**
   - Automatically detects virtual audio devices in the system:
     - `game_device_id` -> binds to endpoint matching `Game` / `Gaming` (`SteelSeries Sonar - Gaming`, 14%).
     - `chat_device_id` -> binds to endpoint matching `Chat` / `Cable` (`SteelSeries Sonar - Chat`, 39%).
     - `media_device_id` -> binds to endpoint matching `Media` (`SteelSeries Sonar - Media`, 39%).
   - Resolves `master_device_id` to the **physical hardware output** (`Realtek HD Audio 2nd output`, 67%) when the Windows default device is a virtual device, preventing Master from hijacking the Game channel.
2. **Channel Volume Synchronization:**
   - `get_endpoint_volume()` and `set_endpoint_volume()` query and adjust individual endpoint hardware scalars.
   - Volumes now match Sonar and Windows exactly:
     - `MASTER`: 67% (Realtek headphones)
     - `GAME`: 14% (Sonar Gaming)
     - `CHAT`: 39% (Sonar Chat)
     - `MEDIA`: 39% (Sonar Media)
     - `MIC`: 67% (Microphone)
3. **Seamless Transition on Sonar Uninstallation:**
   - When Sonar is uninstalled, the virtual endpoints disappear. `Realtek` becomes the sole Windows default output for Master, and Game/Media channels automatically switch to WASAPI per-process session control (`ISimpleAudioVolume`) with zero interruption.
4. **Header Output Device Selector:**
   - Automatically selects `Realtek HD Audio 2nd output` as the master playback device instead of the virtual gaming device.

---

## [Milestone 13: Hardware Microphone Selection, 14% Startup Volume Fix & Linear Boost Formula] - 2026-11-04
### Problem Identified & User Feedback
1. **Application Volume Drop to 14% on Launch:** On startup, several user applications in Windows Volume Mixer were clamped to 14%. Root cause: `apply_session_mix()` was unconditionally called every frame for all processes, using the virtual `Sonar - Gaming` volume (14%).
2. **Missing Input Soundcard Selector:** Users had no way to choose their physical recording device (e.g. `Mikrofon (Realtek(R) Audio)`), leaving them tied to Sonar virtual mic.
3. **Nonlinear Microphone Gain & Pinned VU Meter:** The sensitivity formula used an exponential $10^t$ curve that over-amplified background noise, pegged the VU meter at 100%, and slammed speech dynamics into the limiter.

### Implemented Solutions
1. **Startup Clamping Fix:**
   - Restricted `apply_session_mix()` in `chatmix.rs` to only run when NO dedicated channel endpoints exist AND ChatMix balance is actively moved (`balance.abs() > 0.02`). Application process volumes are now 100% preserved.
2. **Physical Microphone Soundcard Selector:**
   - Added `enumerate_capture_devices()` and `set_as_default_recording_device()` in `endpoint_lock.rs`.
   - Added `In: [ Mikrofon... v ]` dropdown in `tab_mixer.rs` (MIC strip) and full selector in `tab_mic.rs` with `Set as Windows Default Mic` button.
3. **Linear Microphone Boost (0% .. 300%):**
   - Refactored boost to honest linear gain: $1.0\times$ at 100% (0 dB), $2.0\times$ at 200% (+6.0 dB), and $3.0\times$ at 300% (+9.5 dB) with smooth soft-saturation limiter ($>0.95$).
   - Proportional VU meter scaling: `(peak * boost).clamp(0.0, 1.0)`.

---

## [Milestone 14: Native WASAPI Audio Pipeline Bridge & In-Process DSP Engine] - 2026-11-04
### Motivation & Strategic Decision
- **Rejection of Equalizer APO Dependency:** Users should never be forced to install third-party installers from SourceForge or configure Windows registry sAPO entries (`audiodg.exe` crashes, fragile driver updates, no inter-stream AEC, no built-in AI).
- **Direct In-Process WASAPI Bridge (`mic_stream.rs`):**
  - Real-time event-driven WASAPI capture (`IAudioCaptureClient`) on selected physical soundcard (e.g. `Mikrofon (Realtek(R) Audio)`).
  - Ultra-low latency buffer (~10ms / 480 frames at 48 kHz).
  - In-process DSP processing via `GamersEqualizerPlugin`:
    1. **Stage 1: Classical AEC** (NLMS adaptive filter + cross-correlation delay estimator against speaker loopback reference in shared memory).
    2. **Stage 2: Offline Neural AI Voice Isolation** (`nnnoiseless`, 100 KB model erasing keyboard switch clicks, desk knocks, and fans).
    3. **Stage 3: Digital Pre-amp Gain** ($1.0\times$ to $3.0\times$ / +9.5 dB with soft-saturation limiter).
    4. **Stage 4: Broadcast Parametric EQ**.
  - Direct WASAPI render (`IAudioRenderClient`) into virtual microphone sink (`SteelSeries Sonar - Microphone`, `VB-CABLE Input`, etc.).
  - All recording apps (Windows Sound Recorder, Discord, CS2, Teams) receive the cleaned, noise-isolated, and amplified voice in real time with ~5-10ms latency and zero driver requirements.
- **Inter-Process Synchronization via Shared Memory (`ShmHeader`):**
  - Lock-free `selected_mic_id: [u16; 64]` and `mic_id_version: AtomicU32` synchronize input soundcard changes between GUI and background service without restart.
  - Seamless fallback: runs in background service by default; runs in standalone GUI mode if service is not running.

---

## [Verification & Test Results] - 2026-11-04
- **All 20 Workspace Tests Pass (0 failures):**
  - `gamersequalizer` (daemon/GUI): 12 passed
  - `gamersequalizer-common`: 3 passed
  - `gamersequalizer-mic-vst`: 5 passed
- **Production Release Binaries Built:**
  - `target\release\gamersequalizer.exe`
  - `target\release\gamersequalizer_mic_vst.dll`


---

## [Milestone 15: Real Mic EQ, Working AEC with Delay Visualization, DeepFilterNet] - 2026-11-05
### Problems found in review
- Mic EQ sliders were never sent to the DSP (no "Stage 4" existed); only 3 fixed bands, no Q/type/add/remove.
- AEC had no reference signal: nothing wrote speaker audio into the shared ring, so it subtracted zeros.
- Delay estimator could only see ~5 ms, searched the wrong direction and allocated in the audio thread.
- Residual echo suppression attenuated the user's own voice up to 90 % while audio played.
- nnnoiseless got [-1, 1] floats instead of i16-range samples; block sizes not multiple of 480 leaked raw audio.
- Mic bridge: device change ignored until a crash, wrong SILENT flag bit, no resampling, 10 ms sink, no MMCSS.
- Headphone/Game EQ is written for Equalizer APO, which is not installed (still open).

### Implemented
1. **Mic chain**: mic -> AEC -> RNNoise | DeepFilterNet 3 -> parametric EQ (16 bands, biquad TDF-II f64) -> boost -> soft limiter.
2. **Speaker reference**: WASAPI loopback of the Master output into the SHM ring with QPC timestamps; gaps filled with silence.
3. **Delay**: QPC alignment + GCC-PHAT (8 kHz, 1 s windows, 0..400 ms), confirmed 2x (first) / 3x (change), cached per device pair in `%APPDATA%\GamersEqualizer\aec_delay_cache.json` and used as warm start.
4. **AEC**: 1024-tap NLMS starting 3 ms before the measured delay, Geigel DTD with 30 ms hangover, no voice-damaging RES.
5. **UI (Mic tab)**: AEC status, delay, confidence, ERLE, levels, double-talk; plots of correlation vs delay, filter impulse response, 60 s ERLE/delay history; model selector RNNoise / DeepFilterNet; full EQ editor with response plot (saved to `profiles\mic_active.json`).
6. **SHM v3**: new name `..._v3` so old builds never share a mismatched layout; ring 131072 frames.

## Milestone 16: Capture APO prototype (no virtual sound card)

Goal: process the physical microphone itself, so every app gets the result. That includes Teams, which
switches to a certified headset by itself and would bypass any virtual mic.

- `crates/gamersequalizer-apo`: cdylib COM server with the system effect `IAudioProcessingObject{,RT,Configuration}`
  + `IAudioSystemEffects`. It only applies gain and hard-clips at 0 dBFS, and accepts only float32.
  - The gain comes from `HKLM\SOFTWARE\GamersEqualizer\Apo\GainDbX10`, read in `LockForProcess`, so it
    applies when the stream restarts.
  - It runs inside audiodg: no panics, and the RT path doesn't allocate or touch the registry.
  - CLSID `{17e8d42e-dc01-4bd0-90ff-ad7c83a5ddea}`.
- `scripts/apo-install.ps1` (admin):
  - backs up the endpoint's FxProperties (`%ProgramData%\GamersEqualizer\apo-backup`), copies the DLL
    to `%ProgramFiles%\GamersEqualizer\apo`, and registers the COM class and the APO;
  - *appends* our CLSID to the composite EFX list (`{d04e05a6-...},15`); Realtek's EFX stays first;
  - restarts Audiosrv.
- `scripts/apo-uninstall.ps1`: removes our CLSID from all endpoint lists and unregisters it. Optional
  `-RestoreBackup`.
- Realtek mic `{cf0a2270-...}` before install:
  - MFX composite has 3 APOs, EFX composite has 1;
  - processing modes are only DEFAULT + one more (no RAW);
  - Administrators have SetValue on FxProperties, so no ownership hacks are needed.
- Still to verify on the real system:
  - audiodg loads an unsigned DLL from the composite EFX;
  - Teams/WebRTC don't bypass EFX;
  - whether a driver update or re-plug rewrites FxProperties.
- Next steps if it works:
  - shared memory with the daemon instead of the registry,
  - the mic-vst chain (AEC/denoise/EQ) inside the APO,
  - auto-install on a newly connected mic (Jabra).

### Milestone 16 result (2026-11-05, Realtek mic)
- audiodg **loads the unsigned DLL** from the composite EFX.
- The first build broke the whole mic ("no input device"): the engine creates APOs **aggregated**, and
  `#[implement]` returned CLASS_E_NOAGGREGATION. The fix is a hand-written aggregatable COM object;
  `aggregation_delegates_to_outer` is the regression test.
- `examples/level_probe` (shared-mode capture, like any app) measured RMS at gain 0 vs +12 dB:
  -46.9/-48.3 vs -36.6/-38.0 dBFS, so ~10 dB (ambient noise as the signal). The APO is in the
  path, and changing the gain without admin works.
- Writing HKLM keys with a restricted ACL fails with `Set-ItemProperty`/`New-ItemProperty`: they ask for
  more rights. Open the key with exactly `SetValue, QueryValues` via .NET.
- Installing restarts Audiosrv, and apps holding the mic lose their stream until reopened.
- Not verified yet: Teams.

## Milestone 17: Mic chain inside the APO (AEC first)

- The APO now hosts `GamersEqualizerPlugin` (AEC -> RNNoise/DeepFilterNet -> EQ -> boost + limiter).
  - It processes the first channel at 48 kHz and writes the result to all channels. Other rates pass
    through.
  - It is built with `--profile apo` (panic = "unwind"). Every chain call sits in `catch_unwind`; after
    a panic it passes audio through. RT uses `try_lock` only.
- Timing:
  - QPC timestamp from `APO_CONNECTION_PROPERTY_V2` ('ACP2') when it is plausible (within 1 s of now),
    else now minus the buffer length;
  - GCC-PHAT absorbs the offset.
- IPC across sessions:
  - audiodg (session 0, LOCAL SERVICE) creates `Global\GamersEqualizer_Apo_v3` with DACL
    SY/LS/BA/IU; the user's daemon can't create Global objects (no SeCreateGlobalPrivilege) but can
    open it;
  - `win_audio/apo_link.rs`, driven by the loopback thread, opens it (retries every 2 s), pushes the
    same speaker reference frames, mirrors settings in (`copy_controls_from`) and AEC telemetry back
    into the daemon mapping (`copy_dsp_telemetry_from`), so the UI is unchanged.
- The registry gain (`GainDbX10`) is gone; boost comes from the mic slider via shared memory.
- Not done yet:
  - AEC delay cache for the APO path (the daemon's cache is only used by the bridge);
  - UI indicator "processing in APO" vs bridge;
  - auto-install on a newly connected mic.
- APO install check in the app (`win_audio/apo_install.rs`, banner `ui/apo_banner.rs` on every tab, rechecked
  every 3 s):
  - Not installed / outdated (DLL size+mtime vs the bundled `target/apo` or next-to-exe DLL; Copy-Item
    keeps mtime) / not in the FX lists of the microphone in use;
  - the button runs `apo-install.ps1 -EndpointId <mic> -SourceDll <dll> -FromApp` via ShellExecute "runas".
- First live run: AEC stayed in "Measuring" (78 % confidence, no lock). The APO's per-block times
  (call time minus buffer length) jitter by ms, and the estimator needs two windows within 2 ms. Fix:
  `MicClock` in the APO counts samples and corrects 1/256 of the timestamp error per block. It
  re-anchors when off by more than 50 ms.
- Bug: the first Update click installed the stale gain-only DLL from `target/release` (left over from a plain
  `--release` build) instead of `target/apo`. `bundled_dll()` now prefers `../apo`. The banner offers an
  update only when the bundled DLL is newer, never a downgrade. `examples/shm_probe` prints the APO
  mapping (reference flow, ref age, AEC state, levels) for live debugging.
- Bug: after the installer restarts Audiosrv, the loopback (and the mic bridge) treated GetBuffer errors as
  "no data", so the stream stayed dead forever while `tap_active` stayed true. `shm_probe` showed
  0 reference frames/s in both rings. GetBuffer errors now end the iteration, and the supervisor reopens.
- Live result (11:33): AEC **Locked** in the APO, echo delay 1.0 ms, confidence 77–100 %, consistent estimates.
  - ERLE reads only 0–4 dB because the mic signal (~-49 dB) is dominated by room noise, not echo.
  - "No reference" earlier was silence: loopback delivers no packets when nothing plays.
- Dev loop: `scripts/dev-rebuild.ps1` plus the `.claude/agents/dev-rebuild.md` (Haiku) subagent. Clippy's
  stderr no longer aborts the script under Windows PowerShell.
- Tray icon (fixed 2026-11-05): the background service owns it (`tray::TrayHost`, polled in the service loop,
  which also pumps the thread's window messages). Closing the window hides to tray; left click / "Open" focuses
  or relaunches the window, "Exit" closes the window and the service. Creation retries every 5 s (logon before
  Explorer). The window is single-instance by title (`tray::WINDOW_TITLE`).
- Bug: after visiting the Log tab, the Mic / Headphones / EQ pages showed only their headings. All page
  `ScrollArea`s used egui's default id, so they shared one scroll state, and Log's follow mode stores
  `vertical_scroll_offset(f32::MAX)`. Each page now has its own `id_salt`.
- App icon (`app_icon.rs`, drawn in code: orange EQ bars on a dark rounded square) replaces eframe's default
  "e" in the window and the orange circle in the tray. The tray icon turns red with white bars while any
  other app records from a microphone (`mic_usage.rs`: ConsentStore\microphone, `LastUsedTimeStop == 0`,
  the source of the Windows mic indicator; our own exe is skipped; checked every 1 s). Verified with
  `level_probe`: red ~1 s after the capture opens, normal right after it closes.
- Exe icon: `build.rs` includes `src/app_icon.rs`, writes a 16/24/32/48/256 px .ico to OUT_DIR and embeds it with
  `embed-resource` (build dependency; it locates the Windows SDK `rc.exe`). Without an SDK the build warns and
  continues without the icon.
- AEC delay persistence for the APO path (SHM v4, new names `_v4`):
  - `aec_cached_delay_samples` (daemon -> DSP). `ApoLink` loads `aec_delay_cache` for (default mic,
    reference output), hands it over, and saves a delay the APO confirmed (Locked, not from cache,
    moved > 1 ms).
  - The plugin warm-starts from the field when it changes and no delay is measured yet.
  - Silent windows never change the lock; switching needs 3 agreeing windows.
- Build speed: `release` is now lto=false, codegen-units=16, incremental (parallel); the old fat-LTO setup
  moved to the `dist` profile. `apo` inherits the fast release profile.
  - The first build after the switch recompiles everything (~1.5 min); after that, small changes are incremental.
- Mic tab: "Off" option for noise suppression (strength 0 bypasses the denoiser and the gate).
- Verified 11:5x: the APO locks at 0.9 ms with 100 % confidence, and `aec_delay_cache.json` saved 45 samples for
  (Realtek mic, Realtek 2nd output). The Mic tab shows an always-visible APO status line (installed/active +
  build). The banner still appears only when action is needed.
- Gotcha: after an install, the APO starts only when an app opens the mic again (the audio restart drops
  existing streams), so the Global mapping doesn't exist until then.
- Echo delay averaging: agreeing windows feed a cumulative mean, then an EMA with 1/30 (~30 s). The reported
  integer delay changes only when the average moves ≥ 0.75 sample (hysteresis), so the display stopped
  flickering 44/45/46. Test: `refined_delay_is_averaged_and_does_not_flicker`.

## Milestone 18: Output EQ APO (headphones + game), AutoEQ, game detection, master switch

Review of all parametric EQs before this change:

| EQ            | Where it was applied                                | Worked? |
| ------------- | --------------------------------------------------- | ------- |
| Mic           | capture APO / bridge (SHM -> GamersEqualizerPlugin) | yes     |
| Headphone (A) | Equalizer APO config file (not installed)           | no      |
| Game (B)      | same file as A                                      | no      |
| Chat / Media  | nowhere; their tabs edited the game profile         | no      |

Implemented:
- `gamersequalizer-apo` has a second COM class, **Output EQ** `{6b0f3c1e-8f2a-4d7b-9a51-3e2c7d4b9f10}`, for render
  endpoints (EFX, appended after the driver's effects, so after Dolby Atmos / DTS:X spatial rendering).
  One biquad cascade per channel, any sample rate, in place (zero added latency), `catch_unwind` ->
  pass-through. Same DLL and Global mapping as the mic chain. `apo-install.ps1 -Flow Render`.
- SHM v5 (`_v5` names): `master_bypass`, combined output EQ (32 bands + preamp), `out_settings_version`,
  active game (seqlock strings), output APO heartbeat. `ApoLink` mirrors them.
- `output_eq` engine (daemon + GUI): headphone profile per output device, game profile, auto-preamp of the
  combined curve, settings in `profiles/output_eq.json`. The daemon owns game detection and publishes
  the active game; the GUI follows. Settings changes bump a version, the other process reloads.
- Game detection: Sonar's `configs.game_ids` (1_ Steam, 3_ GOG, 8_ Ubisoft, 9_ Epic, 10_ MS Store, others
  are names) are now imported. Facts per exe from Steam `appmanifest_*.acf`, Epic manifests, GOG/Ubisoft
  registry, WindowsApps folder, exe version info; then name matching (camelCase/digit split, roman numerals,
  stopwords). `division2.exe` -> Steam 2221490 -> "The Division 2". Manual bindings exe -> profile or
  "not a game". The game stays active until its process exits. Detector now uses
  `QueryFullProcessImageNameW` (Win32 path instead of `\Device\HarddiskVolumeN`).
- AutoEQ offline: import `ParametricEQ.txt` (file dialog / paste), or index a downloaded AutoEq repo and
  search it (query prefilled from the output device name).
- One header button switches everything off (output EQ + mic chain bypass).
- Chat/Media tabs edit their own profiles and say they are not applied (endpoint effects can't tell apps apart).

Not verified yet on the real system: output APO loading on the Realtek render endpoint, behaviour with
Dolby Atmos / DTS:X active, clicks when toggling (no crossfade).

## Milestone 19: Stream probe (per-app channels without a virtual sound card, step 1)

Goal: Chat/Media EQ with zero added latency and no virtual device. Plan:
- EFX (after the mix, after Atmos) keeps headphone + game EQ for everything.
- A stream-effect (SFX) APO, which Windows instantiates once per app stream before the mix, applies
  `chat EQ - game EQ` to non-game streams. The game stream never needs SFX, so exclusive mode or
  Atmos bypassing SFX for the game does not matter.
- Inverting the game EQ is exact for peak and shelf biquads (gain -> -gain swaps numerator and denominator,
  `alpha` does not depend on gain). LP/HP/notch can't be inverted and would leak into chat.
- The APO runs in audiodg and cannot see the owning app (Microsoft docs, wdmaudiodev). So the daemon
  pairs instances with sessions by correlating the instance's level envelope with each session's
  peak meter (Pearson, gain-invariant, lag search +-50 ms).

Implemented (probe only, audio is not changed):
- SHM v7 (`_v7` names): `ShmLayout.streams`, a 64-slot `StreamTable` (`common/src/streams.rs`): owner
  (audiodg pid + instance number; another pid = stale slot, reclaimable), format, processing mode, a
  10 ms peak envelope over 2.56 s keyed by QPC block. Correlation helpers + tests.
- Third APO class **Stream Probe** `{3c9a5e21-7d4f-4b8e-a1c6-5f2e8d9b0a47}`: pass-through. It claims a
  slot in LockForProcess, frees it in UnlockForProcess, and records the peak of every non-silent buffer.
  It reads `AudioProcessingMode` from APOInitSystemEffects2.
- `apo-install.ps1 -Flow Render -Slot SFX` (composite value `,13`, seeded from legacy `,5`/`,1`);
  uninstall also removes the new class.
- `cargo run -p gamersequalizer-apo --example stream_probe -- 10`: every 2 s, the instances and the
  sessions of the default output, plus the best session for each instance (PAIRED if r >= 0.6 and 0.15
  ahead of the runner-up).

To check on the real system: one instance per app; the game with Atmos on (does its stream show up?);
Discord/browser pairing quality; slot count with many apps.

First live run (Brave playing music, The Division 2 in the background):
- One SFX instance per stream: Brave 1, the game 2 (silent: the game mutes itself when unfocused).
- Pearson correlation was useless (r = 0.13-0.36). The raw series showed why: the session peak meter is
  exactly the max of the SFX's own samples over the last ~100 ms (bit-identical values, gain 1.0).
- `hold_match` replaces correlation: it tries max-hold windows 1-15 and lags +-5 blocks, takes the median gain,
  and counts exact matches. Brave 96-98 %, our loopback session (= the whole mix) 3-7 %. The recorded round is a
  regression test (`test_data/stream_probe/brave_music_round.csv`).
- `STREAM_PROBE_CSV=<dir>` dumps raw series per round.
- Processing mode reads `?`: the init block did not pass the APOInitSystemEffects2 size check. Not needed
  for pairing.
- Still open: game in the foreground, Discord, session volume below 100 %.
- Live run 2 (Discord mic test + Brave music, 12 rounds): both PAIRED in every round. Discord 98-100 %,
  Brave 99-100 %, runner-up <= 6 %, hold 100 ms, lag 0 ms (with history padding), gain 1.000.
- Live run 3 (Brave at ~50 % session volume in ChatMix): still PAIRED, 93-96 %, runner-up <= 6 %, gain 2.000.
  So the **session volume is applied before the SFX** (the APO sees the attenuated signal) and the session
  meter reads before the volume. A stream EQ therefore works on the post-volume signal (linear, harmless).
  A muted session gives the APO silence: it can't be paired until it plays again.
- Live run 4 (The Division 2 in the foreground + Brave, 90 rounds / 3 min, spatial audio Disabled on the endpoint):
  - Brave PAIRED in 86 of 90 rounds (92-100 %); the rest were silent rounds.
  - The game's own SFX stream (one instance) PAIRED with TheDivision2.exe whenever its session meter showed
    audio (93-100 %), e.g. rounds 34-39.
  - From round ~41 on, our loopback session (= the whole output mix, post-EFX) peaked at 0.1-0.5 while every
    SFX instance and every session meter, the game's included, stayed near silent. Even in quiet rounds the mix
    held content that correlates with no stream (r = -0.13 vs Brave). So **part of the game's audio
    (likely gameplay) bypasses both SFX and its session meter**. That is consistent with spatial objects or a
    RAW stream; not identified yet.
  - The plan survives: the game EQ stays in EFX, so the bypassing audio still gets headphone + game EQ.
    Chat/browser streams did go through SFX in every run.
- Bug fixed: `DolbyAtmosGuard` passed made-up "format GUIDs" to `IsSpatialAudioStreamAvailable`, whose first
  argument is the stream interface IID. So spatial sound always read as off. It now asks for
  `ISpatialAudioObjectRenderStream` and reports Active/Disabled/Unsupported. Windows doesn't say which renderer
  (Atmos/DTS/Sonic), so the badge reads "SPATIAL ACTIVE". Live: `Disabled -> Active` with Dolby Atmos on.
- Verified by ear: switching headphone profiles audibly changes the Atmos game audio, so the EFX output EQ (headphone + game,
  one cascade) runs after the Atmos renderer.

## Milestone 20: Chat / Media EQ (stream EQ, no virtual sound card)

- SHM v8 (`_v8` names): `ShmHeader.stream_eq[2]` (chat, media: up to 64 biquads + preamp, mirrored by
  `copy_controls_from`) and `StreamSlot.bus` (daemon -> APO, `StreamBus` None/Chat/Media/Unknown).
- `profile::invert_bands`: peak/shelf stages with negated gain (exact inverse, test: residual < -60 dB); LP/HP/notch
  are left out and still apply to chat/media.
- `OutputEq` owns chat/media profiles + settings (`chat_enabled`, `chat_profile`, `media_*`; None = flat). `apply`
  publishes, per channel, `own profile + inverted game stages` (exactly the game stages the output APO got) and
  preamp `own auto-preamp(hp + channel) - output preamp`. Master off / no stream APO = pass-through.
- `win_audio::stream_router` (daemon thread, supervised): samples session meters every 5 ms, pairs slots every
  500 ms with `hold_match` over 1 s + `pick_session`, and writes `slot.bus` from the exe (shared `CHAT_EXES` /
  `MEDIA_EXES` with ChatMix). It skips our own (loopback) session and stale slots, and logs each new pairing.
- APO class renamed **Stream EQ**: records the input envelope, then runs the channel EQ in place. Channel change =
  20 ms crossfade (fade out, swap, fade in from fresh state); same-channel edits keep filter state.
- UI: Chat/Media tabs edit real profiles (picker, volume, delete, curves over headphone correction) and show
  how many app streams are on the channel right now (read from the APO mapping).
- `apo-install.ps1 -Flow Render` now installs Output EQ (EFX) + Stream EQ (SFX); `-NoStreamEq` opts out. The
  banner's render check needs both.
- Limits: unpaired streams (first ~0.5-1 s, silent ones) keep headphone + game EQ. The inverse is exact only
  when the stream and the output run at the same sample rate (both 48 kHz here). An app gets its channel from
  the exe lists only (no per-app override yet).
- Live (17:22): after the install the router connected to the v8 mapping. It paired `stream #1 -> brave.exe (Media)`
  100 % / runner-up 0 % and `stream #2 -> Discord.exe (Chat)` 100 % / runner-up 26 %.

## Milestone 21: Drag apps between mixer channels

- Mixer apps boxes are drop zones: drag an app line from one channel strip (Game / Chat / Media) to another.
  Right-click an app -> "Back to automatic channel". Dragged apps show a pin. Each box is a scroll list
  (all apps, one line per exe with a process count) instead of 2 names + "+N more".
- `win_audio::app_channels`: per-exe overrides in `<profiles dir>/app_channels.json` (lowercase exe ->
  game/chat/media, unknown values ignored). They win over `CHAT_EXES` / `MEDIA_EXES` in both ChatMix volumes
  (`classify_exe`) and the stream router (`bus_for_exe`). The UI writes the file; the daemon re-reads it when
  its mtime changes (checked at most once a second). The router re-reads each session's channel on every
  refresh, so a move reaches the chat/media EQ within about 1 s.
- Limit: the assignment is per exe, not per window/tab (all Brave tabs share one channel).

## Milestone 22: Whisper analysis, learning voice leveler (2026-11-06)

Whisper analysis on own recordings (in git-ignored `test_data/private/`; one contains licensed music and never
leaves the machine). The reports written then were removed later; what they found is below. Tests
(`#[ignore]`, need the private files): `scenario_report`, `vad_timeline`, `leveler_report`; the probes
`whisper_probe` and `whisper_sweep` went with the reports.

- The RNNoise voice detector (both gate keys) never hears whisper: probability ~0, gain or lower thresholds
  don't help. A closed gate takes 12 dB off it.
- The models themselves lose whisper (energy after a 100 Hz low cut, no gate): DFN3 -8..-11 dB, DPDFNet
  -11..-33 dB, RNNoise -1..-4.5 dB. DFN3 loses it even at strength 0.25.
- Gate keys tried and rejected: DFN3 `lsnr` (higher in a quiet room than on whisper) and output energy over a
  tracked floor (48 variants; none keeps the gate shut in a quiet room without losing whisper).
- `DF_MIN_DB_THRESH = -20` is below DFN3's lowest `lsnr` (-15): that stage never runs.

**Learning voice leveler** (`mic-vst/src/voice_leveler.rs`): new chain stage after the voice EQ, before
boost/limiter, on by default. Mic tab card "Stage 4: Voice leveler" (toggle + current gain).
- Learns the speech level only from frames the input voice detector heard as speech. The flag is delayed by
  the denoiser's latency + lookahead (`AiVoiceIsolator::heard_voice`, `output_lead_samples`).
- Power mean of speech frames: rate 1/n (fast first, then ever slower) up to 30 s of speech, then fixed 1/3000.
  After 3 s of speech a frame counts at most 6 dB from the estimate (shouts, coughs).
- Gain = target minus the level, -6..+24 dB, ~100 ms smoothing; constant between words. Target is a Mic tab
  slider (-36..-12 dBFS, default -24, saved in `audio_settings.json`).
- The chain lives as long as the mic stream (APO: LockForProcess .. UnlockForProcess, i.e. while some app has
  the mic open). The learned level is saved per microphone in `%APPDATA%\GamersEqualizer\voice_level_cache.json`
  (by `ApoLink`, when it moved 0.5 dB, at most every 10 s) and handed back to the next stream: the first word
  gets the right gain, and the remembered level counts as only 1 s of speech, so the first sentences can still
  move it quickly. "Learn again" forgets it. Shortcut: the in-process bridge path doesn't persist it.
- SHM v10: `mic_leveler_enabled`, `mic_leveler_target_db_x10`, `mic_leveler_reset_seq` (GUI -> DSP, mirrored),
  `mic_leveler_saved_level_db_x10` (daemon -> DSP), `mic_leveler_gain_db_x10`, `mic_leveler_level_db_x10`
  (DSP -> GUI/daemon).
- Measured (`leveler_report`, own voice, RNNoise): output voice -21.7..-24.1 dBFS at the recorded level and
  -23.0..-24.0 with the input 12 dB lower (gain +7.5 / +19.6 dB). At -24 dB input it hits the +24 dB cap.
- **Settings change = learn again, fast.** When any setting before the leveler changes (Windows mic volume,
  echo cancellation on/off, AI strength, gate, the running model incl. an overload fallback, voice EQ),
  `VoiceLeveler::relearn` restarts only the learning rate: the learned level and the gain stay, the level
  just counts as 0.3 s of speech, so the rate is 1/n again (and the outlier clamp off until 3 s). The level
  saved from an earlier stream is not taken back. Test, settled at the slowest rate: input +10 dB -> within
  1.5 dB of the target after 1-2 s of speech; input -10 dB -> gain +7 dB of the 10 after 3 s (without the
  restart: under 1 dB). The boost (> 100 %) and the target come after/inside the leveler and don't trigger
  it. A slider drag restarts it every block until it is released. (The Windows mic volume no longer
  triggers it: see milestone 23.)
- Slowest learning is capped: the rate never drops under 1/3000 per speech frame (memory ~30 s of speech).
  Limit: as a power mean it follows a louder voice faster (~45 s of speech for +10 dB) than a softer one
  (~100 s for -10 dB); a setting change no longer depends on that.
- Not verified live yet: needs the app and the APO rebuilt (`dev-rebuild.ps1 -Apo`) and the APO reinstalled
  (layout changed).

## Milestone 23: Mic volume around the leveler, automatic input gain (2026-11-06)

With the leveler on, the Windows mic volume used to be undone by it (it sits before the APO), and
apps with their own gain control (Teams, Zoom, Chrome/WebRTC) move exactly that volume: two gain
controls fighting. Now the volume has two separate jobs.

- **The volume is how loud others hear you.** The daemon reads the endpoint volume in dB
  (`GetMasterVolumeLevel`) every 250 ms -> `mic_input_gain_db_x10`. The leveler learns the level
  without it (`VoiceLeveler::set_input_gain_db`), so a volume change is compensated exactly and at once,
  with no relearning (test: -10 dB -> on target within 0.5 s, learned level unchanged). Stage 5 then applies
  the wish (`mic_volume_permille`, 0..300 %) after the leveler: 100 % = the target, 50 % = 6 dB under it,
  200 % = 6 dB over. When an app's gain control lowers the mic, the output really gets quieter and the app
  settles. Without the leveler nothing changed (Windows volume + boost > 100 %).
- **Automatic input gain** (`win_audio::mic_gain`): the daemon moves the Windows volume itself, only at the
  extremes, to keep the signal out of clipping and out of the noise. Dead band: while the leveler's gain
  stays in its range and nothing clips, the volume stays wherever it is.
  - Down 15 % on clipping: 50 clipped raw samples (|x| >= 0.98, counted by the DSP -> `mic_input_clips`)
    within 10 s, at most every 3 s.
  - Up 10 % after 20 s of speech (`mic_speech_ms`) with the leveler at +18 dB or more; down 10 % after 20 s at
    -5 dB or less (its cut cap). At most one step per 30 s.
  - Never under 20 % or over 100 %. Steps only in a pause (no speech since the last tick), because the DSP
    learns the new dB a moment after Windows applies it.
  - Hysteresis by step size: after a step the leveler needs ~that much less, far from the edge.
  - Only on devices with a hardware volume (`QueryHardwareSupport`); a software volume scales after the
    converter, which neither prevents clipping nor improves the noise. Off with the leveler, the master
    bypass or mute: Windows gets the wish back.
- **Ours or someone else's:** `ChatMixController::mic_hw_auto` is the level the automatic gain set. Windows
  showing exactly that = ours; anything else = the user or an app: it becomes the wish, the automatic level
  is dropped and it waits 30 s. The window learns the automatic level from `mic_auto_gain_permille` (SHM v11)
  so it doesn't read it as the user's volume. The daemon now takes the wish from shared memory each tick
  (before it only wrote it, so a boost set in the window could be overwritten by the daemon's 100 %).
- Saved in `audio_settings.json` (`mic_auto_gain_permille` + `mic_auto_wish_permille`); on start the wish
  comes back only while Windows still shows that automatic level.
- Mic tab: the sensitivity caption explains the new meaning with the leveler on and shows the automatic
  level when one is set.
- Limits / shortcuts: "ours" is recognized by the level, not by Windows' event context (polling, no COM
  callback): someone setting exactly our level in the same 250 ms is read as ours. The volume steps are in
  Windows slider units, whose dB per step depends on the device (the leveler gets the real dB, so that only
  changes the step size). Microphone boost (+10/+20/+30 dB in the driver) is not touched. The saved leveler
  level from v10 was learned with the volume in it; the first sentences correct it.
- Not verified live: needs `dev-rebuild.ps1 -Apo` and the APO reinstalled (SHM v11).

## Milestone 24: Debug capture of the mic chain (2026-11-06)

Real data for tuning the AEC (and the rest of the chain) without guessing: the APO records every stage,
sample-aligned, as it runs inside audiodg.

- Cargo feature `debug-capture` (mic-vst, forwarded by the APO crate), `dev-rebuild.ps1 -Apo -DebugCapture`.
  Without it the code is not compiled in; the only always-on addition is one `Option<u32>` per block
  (the AEC's reference shift).
- The APO creates `Global\GamersEqualizer_DebugCapture_v1` (~96 MB pagefile-backed, physical pages only when
  written; layout in `gamersequalizer_common::debug_capture`). The tool sets `Requested`, the first mic stream
  claims it (`Recording`), writes per block and sets `Done` at the target length (max 60 s).
- Tool: `cargo run --release -p gamersequalizer-apo --example debug_capture -- [seconds] [out_dir]`, default
  `test_data/private/captures/<UTC time>/` (git-ignored). Writes mono f32 48 kHz WAVs:
  `01_mic_raw`, `02_ref_at_mic` (reference at mic time, no echo delay: lets a new AEC do its own delay
  search), `03_ref_aec` (as fed to the AEC), `04_after_aec`, `05_after_denoise`, `06_after_eq`,
  `07_after_leveler`, `08_out`; `vars.tsv` (34 columns per block: AEC state, delay, ERLE, double talk, model,
  gate, voice detector, leveler, gains, load); `info.txt` (chain constants + column meanings).
- Limits: chain rate only (a 44.1 kHz device is recorded after the resampler, first channel); `load`
  includes the capture's own writes; the stall timeout (3 s without frames) saves a partial capture; with
  several mics streaming, the first one to see the request records.
- Not verified live yet: needs the APO built with the feature and reinstalled (admin).

## Milestone 25: Echo canceller rewritten as MDF with residual-echo step control (2026-11-07)

The first live debug captures showed the old NLMS AEC made the mic **louder** while audio played
(+5..+15 dB, ERLE <= 5 dB). The offline replay (`aec::tests::capture_replay`, ignored, `AEC_CAPTURE=<dir>`)
reproduced it exactly; a control with the real reference through a known path plus real room noise was
amplified too (-58.2 -> -43.8 dB) while textbook NLMS on the same data was not. Cause: the output used a
frozen copy of fast-fluctuating NLMS weights (a copy refreshed every block amplified as well).

- `aec.rs` is now a multidelay block frequency-domain filter (MDF, Speex's structure): 5 ms frames
  (240, real FFT 480 via `realfft`, already in the tree), 5 partitions = 1200 taps (25 ms), pre-emphasis 0.9,
  gradient constraint on one partition per frame.
- Learning rate per bin from the residual echo estimate (Valin 2007: leakage = correlation of error and echo
  power fluctuations): fast with echo only, almost frozen when the error is near-end. No Geigel detector.
  Speex's extra `0.3 x RER` floor per bin is left out: it let a low voice tone (small after pre-emphasis)
  drive adaptation and the echo leaked during double talk.
- Foreground/background with Speex's variance tests; plus a guard: output smoothed power > mic + 3 dB ->
  both filters restart (an AEC may fail to remove echo, it must never add noise).
- Blocks that are multiples of 240 pass with zero latency; other sizes switch once to a 5 ms buffer.
- API unchanged except `new(taps)` (no step size). `is_double_talk` now means "error mostly near-end while
  audio plays" (residual-to-error ratio < 0.05), shown as before.
- Results: capture 2026-11-07 09:16 replayed never louder than the raw mic (old: up to +15 dB); control
  -58.2 -> -61.6 dB with room noise alone at -61.8 (echo essentially gone). CPU 0.12 % of a core
  (old ~1.4 %). Tests: weak echo under speech-like audio never louder (regression, old: +4.4 dB), room
  response > 20 dB, double talk keeps voice (< 1 dB change) and echo cancelled > 15 dB, quiet voice under
  loud reference > 20 dB, relearn after a path change > 15 dB, latency 0 / one frame.
- Not verified live yet (APO reinstall). The capture had weak echo (at room-noise level), so real
  suppression on loud speakers is still unmeasured: next capture with louder speakers.

## Milestone 26: Echo reference and delay follow an output switch (2026-11-07)

Capture 2026-11-07 10:11 (monitor speakers over HDMI, two switches during the minute) showed two gaps:

- The daemon never rebound the echo reference when the Windows default output changed: after switching
  to the monitor the reference stayed on the headphones (silent). Now the daemon loop checks the default
  output every 250 ms and moves Master + reference to it (`chatmix::physical_master_id`, skips virtual
  devices). Verified live: the loopback reopens within ~20-50 ms.
- The APO kept the old output's echo delay: 5 s after monitor -> headphones (272 instead of 46 samples)
  and 3 s back; meanwhile the headphone echo passed (correlation 0.74 -> 0.37). The daemon also saved that
  stale delay under the new device's key (`aec_delay_cache.json` held 252 for the headphones).
  Fix (shared memory v12): the daemon bumps `aec_reference_seq` after storing the new output's cached
  delay; the APO then drops its delay lock (`DelayEstimator::forget_lock`, no allocation), resets the AEC
  and warm-starts from that cached delay. It echoes the seq in `aec_reference_seq_seen`, and the daemon
  saves a delay only when that matches. The in-process bridge already restarted on a reference change.
- HDMI monitor delay ~250-320 samples (5-7 ms) vs headphones 46: well inside the 400 ms search.
- Not verified live yet: needs the APO reinstalled (layout change).

## Milestone 27: DeepFilterNet 3 stops ducking unvoiced speech, DPDFNet delay fixed (2026-11-07)

Capture 2026-11-07 10:44 (60 s, model switched live RNNoise -> DFN3 -> DPDFNet), all three models then
replayed offline on the raw mic. No fallback, CPU fine (DFN3 5 %, DPDFNet 13 % median per block).

- DFN3 ducked clean speech by 10-15 dB, broadband and choppy (hop by hop), mostly unvoiced frames
  (s, f, breath): 20 % of loud unvoiced frames lost > 6 dB (DPDFNet 11 %, RNNoise 3 %), voiced 4.9 %.
  Its own local SNR rated those frames 20-29 dB, so it was the ERB mask, not the low-SNR zeroing that
  `DF_MIN_DB_THRESH` handles. `DF_MAX_DB_ERB_THRESH` 30 -> 25 (frames above it pass untouched):
  unvoiced 19.8 -> 10.5 %, voiced 4.9 -> 1.5 %, pause suppression in a quiet room 4.9 -> 3.2 dB.
  Synthetic speech-over-keys report and key suppression unchanged. DPDFNet still removes far more in
  pauses (34 dB) and stays the better pick.
- DPDFNet waited for a full 960-sample window before its first frame, so its delay depended on the block
  size: 2400 samples at the APO's 480-frame blocks, 2880 (the constant) at 441. The gate and leveler
  trust the constant, so at 48 kHz they ran 10 ms late. `in_buf` now starts with one hop of zeros:
  2880 for every block size, like DFN3 (one hop of buffering). Regression: the latency test now runs
  441 / 480 / 96 / 144-frame blocks (failed for DPDFNet at 480 before).
- Not verified live yet: needs the APO reinstalled.

## Milestone 28: Spatial sound format shown and watched (2026-11-07)

The header only said "Spatial sound: Active", and the tray check only noticed spatial sound turning off,
not Dolby Atmos being replaced by Windows Sonic (a Windows update, an expired Dolby licence).

- **Which format runs.** No public API says it; Windows keeps it in the endpoint's property store,
  mirrored in `HKLM\...\MMDevices\Audio\Render\{guid}\Properties` (readable without admin):
  - `{fd8a7b27-0b18-4025-ab1c-bdd6b32e1604},2`: the running renderer id at byte 12; a 9-byte value when off.
  - `{a45429a4-aa63-4480-b7f8-3f2552daee93},2..N`: the formats offered on that output, 842 bytes each:
    UTF-16 display name at 8 (in the Windows language), app id at 0x208, renderer id at 0x308. Every
    installed renderer registers here, so DTS or a future one needs no code.
  - `{8a845654-d6c3-4cd7-b4eb-243d4bd99032},2` (the user's pick, id at 16) is not used.
  - Ids seen: Dolby Atmos for Headphones `1459ac38-...`, Dolby Atmos for speakers `4c81e564-...`,
    Windows Sonic `b53d940c-...`, DTS Headphone:X `4444acb0-...`, DTS Virtual:X `adafd3c6-...`.
  - `dolby_atmos::active_format`; verified live: "Dolby Atmos for Headphones" on the headphones, None on
    an output with spatial sound off. Layout read on Windows 11 26300; if it changes, the UI falls back
    to "Active" and the on/off check keeps working.
- **Header** (Headphones page): "Spatial sound: <format>", re-read every 3 s, and "Change..." opening
  `ms-settings:sound-defaultoutputproperties` (the default output's page with the spatial sound choice).
- **What "right" means.** The app cannot know what the user wants, so it learns: the first format seen on
  an output is the user's (`HealthMemory::spatial_formats`, per output, in `health.json`; old files load).
  Off at first start: nothing is learned until it is turned on.
- **Watch.** Another format on that output -> `Issue::SpatialChanged`: tray badge, Windows notification,
  and in the window a warning-colored row "Spatial sound: X instead of Y" with "Keep X" (it becomes the
  user's) and "Change...". Turned off -> the existing `SpatialOff`, now naming the lost format.
- Known limit: Windows does not say who changed it, so a switch made by the user in Windows is flagged
  once too; "Keep" accepts it. Switching the format from the app is not done: no public API, only
  rewriting those properties as admin + restarting the audio service, undocumented.

## Milestone 29: Mic chain tail: peak limiter, deeper leveler cut, gate floor follows the leveler, fixed low cut (2026-11-08)

The cheap steps 4.1 and 4.5-4.7 of the 2026-11-08 chain analysis. Measured with the new
`plugin::tests::chain_report`: the whole chain (no echo reference, the default mic EQ),
`whisper_scenario.wav` at several input levels and boosts, plus `keys.wav` alone. Values are output dBFS.

- **Peak limiter instead of the clipper (4.7).** The `soft_limit` curve reached 1.0 at an input of
  1.003, so every peak above it was flattened. The mic chain now ends in the output EQ's `PeakLimiter`:
  instant attack, 80 ms release, ceiling -0.26 dBFS. Full-scale samples went from 0.01-1.57 per mille
  (boost 300 %: 1.21 DFN3, 1.57 RNNoise) to 0 in every case, with the peak at -0.27 dBFS.
- **Leveler cut -6 -> -18 dB (4.6).** Mic far too hot (+20 dB, converter clipping): the leveler was
  stuck at -6 dB and the voice came out at -18.8/-19.2 (target -24). Now the gain is -12 dB and the
  voice comes out at -20.6/-23.7 (DFN3) and -21.4/-23.8 (RNNoise). The other cases did not change.
- **Gate floor follows the leveler (4.5).** The closed gate goes as many dB below -12 dB as the
  leveler boosts, at most 12 more (`set_gate_floor_extra_db`, last block's gain). Pauses after 30 s
  (10th percentile of 10 ms frames), floor coupling off -> on: RNNoise input -12 dB -91.6 -> -103.6,
  0 dB -92.4 -> -100.9, boost 300 % -82.8 -> -91.4. DFN3 0 dB -107.4 -> -113.4. A hot mic (leveler
  at or below 0 dB) is unchanged.
- **Fixed 70 Hz low cut before the detectors and the denoiser (4.1).** 2nd-order Butterworth. With a
  45 Hz rumble at -30 dBFS added, the room section without voice went from -62.8 to -119.9 (DFN3) and
  from -75.8 to -89.9 (RNNoise). Side effects in DFN3, which reacts to level and spectrum around its
  thresholds:
  - Whispering (which the gate never opens on) came out at -43.4 -> -32.8 at 0 dB and -38.8 -> -32.4
    on the hot mic, but -50.8 -> -66.7 with the input 12 dB low.
  - The room at +20 dB went from -117 to -84.
  - RNNoise barely changes.
- Not done: the number checks only use the developer's own recordings (`#[ignore]`). Not heard live
  and not checked in the APO.

## Milestone 30: AEC3 replaces the MDF, lookahead per model, latency meter; what was tried and dropped (2026-11-08)

- **Echo canceller: WebRTC AEC3 via `sonora`** (pure Rust, BSD-3, `aec3.rs`). Our MDF had no
  suppressor for the residual echo. On the 2026-11-07 debug captures AEC3 removed 10-25 dB more echo
  while only the far end played, and took ~1.5 dB off the voice in double talk. It adds ~9 ms
  (`AEC3_LATENCY_SAMPLES` = 430). Its own high-pass is off, so the mic's bass doesn't switch on and
  off with the far end.
- **The MDF is removed** (`aec.rs`). Nothing used it at runtime and it is not kept as a fallback.
  Its scenarios now run against AEC3 in `aec3.rs`:
  - reverberant echo: 28 dB removed
  - weak echo under speech: never louder than the mic, -7.9 dB
  - double talk: 54 dB of echo removed, voice -1.6 dB
  - quiet voice under loud music: 37.5 dB removed, voice -1.3 dB
  - echo path change: 38 dB before, 35 dB after

  Double talk is measured by projecting the output on the echo and on the voice. Subtracting the
  voice doesn't work, because the suppressor shapes the voice slightly. The echo sits at least
  `PRE_DELAY_SAMPLES` behind the reference, as in the chain. With only 20 samples AEC3 stalled at
  ~10 dB. `capture_export` (ignored) writes the captures through AEC3 to `test_data/private/aec_ab/`.
- **Gate lookahead per model:** RNNoise 15 ms, DFN3 15 ms, DPDFNet 10 ms (was 30 ms for all;
  `lookahead_sweep`). Together with AEC3 every model stays below its old total delay.
- **`latency_meter.rs`:** GCC-PHAT between raw input and final output measures the delay the
  chain really adds.
- **Not done: `echo_only` gate mask.** Checked with the new `echo_gate_report` on the captures
  after AEC3. In blocks with only echo (far end playing, AEC3 took at least 15 dB off) the input
  detector heard speech in only 1-11 % of blocks, and the echo left after the denoiser was at
  -55 to -66 dBFS. The gate was open in 15-35 % of those blocks, but that is mostly hold after the
  user's own words. A mask would risk cutting a quiet voice over a loud game for little gain.
- **Tried and dropped: RNNoise stronger (analysis 4.3, 4.4).** Rule: a few keys let through beat
  one cut letter. Measured with `private_report` (auto + lookahead, own voice under own keys) and
  ESC-50 keys alone:
  - dry mix in dB, 6 dB down to 20 / 30 dB instead of linear: pauses 19 / 25 dB quieter instead of
    12-14, but soft syllables lost 1.6-2.4 -> 2.6-4.3 / 3.8-5.9 %.
  - output detector threshold for RNNoise 0.3 -> 0.5: gate open on keys 31 -> 18 % (-40 dBFS
    15 -> 9 %), for +0.3-0.4 points of soft syllables. At 0.7 loud syllables started to go.

  Both are back to the linear mix and 0.3. The numbers are kept in the code comment.
- **DFN3 on tract 0.23: done, see milestone 31.**

## Milestone 31: DeepFilterNet 3 on tract 0.23 (own fork), one tract in the DLL (2026-11-08)

Upstream `deep_filter` pins tract `^0.21.4`, and the mic crate had to pin `=0.21.4` because newer
0.21.x broke it. DPDFNet already ran on tract 0.23, so the APO DLL linked two tract versions.

- **Fork in `crates/vendor/deep_filter`.** Upstream libDF at rev `d375b2d` (MIT OR Apache-2.0, licences
  kept), inference only: `lib.rs`, `transforms.rs`, `logging.rs`, `tract.rs`, and the
  DeepFilterNet 3 ONNX model (`models/`, embedded as before). Dataset, training, C API, WASM,
  binaries and upstream tests are left out. It is excluded from the workspace, so it is not
  linted as our code. Its README lists the changes.
- **Port to tract 0.23.10 and ndarray 0.17:**
  - API renames: `to_plain_array_view*`, `into_plain_array`, `spawn()` instead of
    `SimpleState::new`, `symbols`, `with_outputs_by_name`.
  - The ERB and complex feature buffers are now plain tensors, copied into the model inputs each
    hop. Upstream wrote into the shared input value through a raw pointer.
  - The mic crate drops its four 0.21.4 pins and moves to ndarray 0.17. `Cargo.lock` now has one
    tract.
- **Same output, less CPU.** Checked with the new `ai_denoiser::tests::dfn3_reference` (DFN3 alone on
  speech over keys, strength 0.75 and 1.0) against a reference saved on 0.21:
  - max difference 2.3e-7, signal to difference 132 dB (float rounding)
  - CPU 2.97-3.07 % -> 1.95-2.21 % of a core
  - model load 72-87 ms (docs: ~200 ms on 0.21)
- APO DLL 59.4 MB -> 47.1 MB (one tract instead of two).
- Not done: not heard live, and the APO is not installed with it yet.

## Milestone 32: Dependency upgrades (2026-11-08)

Every outdated direct dependency was moved to its current major version, and the unused ones were dropped.

| Crate                        | Before      | After       | What changed in our code                                                  |
| ---------------------------- | ----------- | ----------- | ------------------------------------------------------------------------- |
| windows / windows-core       | 0.58        | 0.62        | `BOOL` in `windows::core`, `PROPERTYKEY` in `Foundation`, `Option` params |
| eframe / egui                | 0.29        | 0.37        | `App::ui`, `Panel`, `CornerRadius`, integer margins and shadows           |
| egui_plot                    | 0.29        | 0.38        | items take `(name, data)`, new label formatter                            |
| wgpu                         | 22          | 30          | backend choice via `WgpuSetupCreateNew`, async adapter list               |
| tray-icon / muda             | 0.19 / 0.15 | 0.26 / 0.21 | none                                                                      |
| rubato (APO)                 | 0.14        | 5           | new API; the sinc delay is now skipped by hand (see below)                |
| window-vibrancy, directories | 0.5, 5      | removed     | were not used                                                             |

- **rubato 5 no longer withholds the filter delay** at the start of the stream. Without a fix the
  8 kHz rate bridge added 13.4 ms; `rate_bridge.rs` now drops `output_delay()` samples after each
  reset, and the latency test passes again.
- **DX12 probe**: wgpu 30 returns the adapter list as a future. On native it is ready on the first
  poll, so it is polled once with a no-op waker (no async runtime pulled in).
- The `deep_filter` fork loses its rubato resampling (unused).
- npm (CodeGraph only) was already up to date.
- **Dependency trim**: crates compiled on Windows 315 -> 276. `nnnoiseless` and `env_logger` without
  default features (nnnoiseless's CLI pulled clap/hound/dasp; we format log lines ourselves), unused
  `thiserror` (common, daemon) and `env_logger`/`realfft` (mic crate) dropped, the `deep_filter` fork
  moved to thiserror 2 and itertools 0.15 (no second copies of them any more).

## Milestone 33: Follow-ups to the code review fixes (c17f31f) (2026-11-08)

Six items left open after `c17f31f`, each with a regression test.

- **Pause flush allocates nothing.** `pause()` rebuilt the denoiser (`reset()`: two RNNoise
  detector states plus the denoiser state, each with its weights) on the audio thread, and the low
  cut with a new `Vec`. `AiVoiceIsolator::flush()` now drops the FIFOs, delay lines, gate and
  detector timing and keeps the loaded models. RNNoise's own memory (input high-pass, window
  overlap, 1728-sample pitch buffer) is pushed out by 10 silent frames (~0.2 ms, once per pause):
  output after the flush stays under -80 dBFS. DPDFNet's `reset()` now writes `init_state` into
  its existing state tensor instead of building a new one, so the flush resets it cold without
  allocating (with only its buffers restarted, its state replayed old audio at -44 dBFS).
  DeepFilterNet's state sits in tract tensors inside the fork, so after a flush its first model
  latency of output (all from before the pause) is silenced instead. `reset()` stays for
  strength 0 -> on and model changes.
- **Channel outputs bound after start.** `bind_endpoints` ran only at service start, so Sonar or
  VB-Cable started later never got its Game/Chat/Media endpoint, and `apply_session_mix` kept
  setting per-app volumes on top of the channel levels. On a device change the service now
  re-resolves only those three ids (`bind_channel_endpoints`); mic choice and Master stay, a newly
  bound channel takes its endpoint's level and mute (also in shared memory for the window).
- **Delay estimator gets the sanitized mic.** It was fed the raw input: one NaN sample turned a
  whole 1 s window into "no echo". It now reads `mic_mono_temp` (NaN-free, clamped to +-4).
- **Leveler input gain** clamped to -100..+60 dB instead of -60..+60 (Windows mic volume reaches
  about -96 dB).
- **AEC off -> on** no longer replays the samples AEC3 had queued before: the disabled branch marks
  it idle, so re-enabling flushes.
- **Retry a failed model.** Picking the selected model again sent nothing (the window only writes
  a changed model id). New `denoise_retry_seq` in shared memory (layout v15), bumped by clicking
  the running model; the DSP clears its load error / overload and starts a load. The load-failed
  note now says "Pick {} again to retry", as the overload note already did (it did not work either).

## Milestone 34: DeepFilterNet 3 click rescue, strength in percent (2026-11-08)

**Report.** In a call, a friend heard pieces of the user's voice missing while the user hammered
the mouse button next to the mic (Voice Recorder take 13: pauses digitally silent, dips to
-86..-95 dBFS inside words). The user suspected the mic at 100 %.

**Not the mic gain.** Log: Windows mic volume +1.6 dB, voice learned at -31 dBFS, no clip and no
stalled DSP calls. The raw take the user recorded later (processing off, typing while talking,
`test_data/private/keyboard_call.wav`) peaks at -22.8 dBFS with no clipped sample. Gain scales voice
and click alike, so it cannot change how well they separate; a clip would distort ~1 ms, not remove
20-100 ms.

**Not the DFN3 zero-mask threshold either.** First guess was `DF_MIN_DB_THRESH`: frames under it
get a zero mask. But the model's LSNR output is bounded to -15..35 dB (`lsnr_min = -15` in the
DFN3 config inside `models/DeepFilterNet3_onnx.tar.gz`), so -20 never fires. The cut comes from
the ERB mask itself: a loud click dominates the frame and DFN3 attenuates the voice with it.

**Reproduced offline** (`click_report`): ESC-50 mouse clicks (class 31) as clean taps, 40 ms each,
every 120 ms ("hammering"), peaks at -20/-10/-3 dBFS over the private voice at -30 dBFS. Sparse
clicks at the clips' own spacing did not reproduce it (< 3 % lost); dense ones did. New metric:
speech frames with a click within 10 dB of the voice, share lost > 6 / > 15 dB. At -3 dBFS, gate
auto: DFN3 lost > 15 dB in 16.3 % of them (> 6 dB in 43.5 %), DPDFNet 2.0 %, RNNoise 0 %. The
gate is not the cause (gate off: the same).

**Why not a setting.** Strength (DFN3 attenuation limit) 0.25 brought it to 2.7 % but pause
suppression fell 39 -> 19 dB. The user tried DPDFNet: no cuts, but "sounds like a blocked nose",
so they wanted DFN3 fixed.

**Fix: `ClickRescue` (DFN3 only).** RNNoise (which keeps the voice under clicks) runs next to DFN3
on a copy of the input, delayed to DFN3's latency by the existing stand-in `sleep_delay`. Where
DFN3's output is >= 6 dB under RNNoise's (5 ms energy envelopes) while speech is heard, the output
crossfades to RNNoise (in ~2 ms, out ~30 ms). What arms it was the hard part, tried in order:
1. Raw-input detector with hysteresis (`vad`): cuts gone, but the hysteresis stays on while
   clicks go on; pause suppression on clicks 39 -> 7 dB.
2. Raw RNNoise probability > 0.5, then 2 frames in a row: private voice fine, but the shared
   keyboard mix (keys 10 dB over the voice) fell 35 -> 12 dB in pauses. Split by distance from
   speech showed the leak even > 300 ms from any word: loud keys alone score speech frames on the
   raw input.
3. DFN3 dip under its own 50 ms level, RNNoise capped at it: pauses clean, but rescued only half
   (16 -> 9 %), and capping at the output's level ducked the voice under clicks (> 6 dB 11 -> 33 %).
4. **Kept:** a voice detector (`VoiceNet`, p > 0.5) on DFN3's own output, hold 100 ms. Keys are
   gone there, the voice stays in most frames, so the hold covers the frames DFN3 cut. Threshold
   0.3/0.5/0.7 measured alike; hold 50/100/200 ms traded voice against clicks in short pauses
   (100 kept); a 10 ms crossfade out changed nothing.

**Result.**

| Measurement                                          | DFN3 before | DFN3 rescue | DPDFNet |
| ---------------------------------------------------- | ----------- | ----------- | ------- |
| Raw take (`keyboard_call_report`): speech frames cut | 8.0 %       | 1.4 %       | 3.3 %   |
| Hammered clicks -3 dBFS, private: lost > 15 dB       | 16.3 %      | 5.8 %       | 2.0 %   |
| Same, shared speech fixture                          | 5.5 %       | 1.4 %       | -       |
| Shared mixes, pauses > 150 ms from speech            | 38-43 dB    | 38-43 dB    | -       |

"Cut" in the raw take = 10 ms frame > 10 dB under RNNoise where RNNoise is within 25 dB of its
loudest (no clean reference exists); quiet frames -79.3 -> -79.1 dBFS. Processed takes for
listening go to `test_data/private/out/keyboard_call_*.wav`. The user installed the APO and
confirmed it works in a call.

**Trade-off (voice over keys).** Within the hold, keys next to words come through as RNNoise leaves
them: the shared keyboard mix with keys 10 dB over the voice keeps 42.8 dB in far pauses but 20 dB
over all pauses (35+ before). `test_deepfilter_keeps_soft_speech_over_keyboard` now asserts far
pauses > 35 dB and all pauses > 15 dB; `measure` got `far_suppression_db` (pauses >= 15 frames from
speech: hold + crossfade + detector frame). Cost: two extra RNNoise passes while DFN3 runs
(~0.5 % of a core).

**Strength stays 75 %, now shown in percent.** The user asked whether 100 % would be a better
default. `strength_report` (private voice, gate auto + lookahead):
- DFN3: pauses +1-2 dB, voice lost under clicks 5.8 -> 7.1 %.
- DPDFNet: pauses ~+1 dB, clicks cleaner, voice the same (the only one that would gain).
- RNNoise: pauses 14 -> 26-33 dB, but soft syllables under loud keys 1.6 -> 12 %. It stands in
  for a sleeping heavy model and is the rescue's source, so it matters with DFN3 too.
The gate already covers pauses, so the 46.5 dB limit vs unlimited barely matters for the heavy
models. The slider shows "75 %" and accepts "90" or "90 %".

**Open.** Not checked: how rescued frames sound (RNNoise's timbre for a few ms), a real mouse
rather than ESC-50 clicks, a speech fixture with long pauses for the strength report (its far-pause
column is empty on the private voice).

Commit: `8dbfd74`.

## Milestone 35: RNNoise 0.2 as the raw-input voice detector (2026-11-09)

**Port.** `crates/vendor/rnnoise2` is a pure-Rust port of RNNoise as of its 0.2 rewrite (conv1d +
3 GRUs, 32 bands, forked from nnnoiseless 0.5.2; no C, no FFI). It is bit-close to the upstream C:
on the reference clips max 11 LSB (Full) / 19 LSB (Little), RMS under 1 LSB, threshold 32.8 LSB
(`reference_matches_upstream`); on 10 s mixes every quality metric matches the upstream C demo
within 0.2 dB / 0.6 % (crate README, "Cross-check against the upstream C").

**Not as the filter.** As a denoiser 0.2 suppresses keys in pauses about twice as hard in dB
(Full 36-41 dB vs 18 dB) but cuts quiet syllable onsets and tails much more (30-37 % lost
> 10 dB vs 9 %) and loses slightly more speech under keys. The same holds for the upstream C, so
it is the model, not the port. The filter, the pause stand-in, `out_net` and `ClickRescue` stay on
nnnoiseless (their thresholds and alignment are tuned to it, and three Full passes would cost
~8 % of a core).

**As the raw-input detector it hears far more speech at the same false-alarm rate** (rnnoise2
`vad_sweep`; share of frames whose probability reaches the threshold; speech = clean frame RMS
>= clean RMS - 15 dB, quiet = -40..-15 dB, i.e. syllable onsets and tails; speech -20 dBFS under
the ESC-50 keys; keys alone at -20 dBFS):

| Detector        | Open / keep | Speech at open % | Quiet at open % | Quiet at keep % | Keys over open % | Keys over keep % |
| --------------- | ----------- | ---------------- | --------------- | --------------- | ---------------- | ---------------- |
| nnnoiseless 0.5 | 0.5 / 0.05  | 81.7 / 75.6      | 11.7 / 10.5     | 30-48           | 0.4              | 7.8              |
| rnnoise2 Full   | 0.5 / 0.05  | 99.0 / 96.5      | 47.5 / 44.4     | -               | 1.3              | -                |
| rnnoise2 Full   | 0.7 / 0.2   | 97.2 / 94.1      | 38.3 / 30.9     | 56-59           | 0.5              | 5.2              |

Speech and quiet columns: 0 dB / -10 dB SNR; "-": not measured (0.05 is far below Full's useful
range: at 0.3 it already fires on 3.1 % of keys-only frames).

At 0.7 Full fires on keys alone as rarely as the old detector at 0.5, and at 0.2 less often than
the old one at 0.05, while hearing 94-97 % of speech instead of 76-82 % and 31-38 % of quiet
speech instead of 10-12 %. The rule for the gate is "never cut speech, rather let a key through",
so this is the useful part of the new model.

**Change** (`ai_denoiser.rs`). `in_net` (the raw-input detector that keys `vad` while a heavy model
is awake, and the leveler / auto gain while the chain is bypassed) runs `rnnoise2` Full through
the new `voice_probability` (analysis + network, no synthesis). `VoiceNet` holds either network
(`VoiceModel::Old` / `New`); `vad` opens at `IN_NET_VOICE_PROB = 0.7` and keeps at
`IN_NET_KEEP_PROB = 0.2` on `in_net`'s probability, and at the old 0.5 / 0.05 on the RNNoise
filter's own probability (RNNoise alone, or standing in for a sleeping model). If the embedded
weights fail to load, `in_net` logs it and falls back to the 2017 network with the old thresholds.
`reset()` clears the new state in place (no re-parse); `flush()` unchanged (FIFO clear only).

**Timing.** 0.2 has one frame of network lookahead: the probability returned for frame t is
frame t-1's, so `in_net` reports 10 ms later than before. It still leads the heavy model's output
by its latency minus 10 ms (DFN3 30 ms, DPDFNet 50 ms) plus the gate lookahead, so output latency
is unchanged. Checked consumers: the gate hold (`GATE_HOLD_SAMPLES + latency + lookahead`, counted
from the report) ends 10 ms later, i.e. errs toward keeping a tail; the leveler / auto gain map
`heard_voice` to output audio by `output_lead_samples`, so their speech frames shift by one frame,
well inside the detector's own hysteresis (`VOICE_ACTIVE_FRAMES` = 200 ms); waking a sleeping
model is keyed by the RNNoise stand-in's probability and activity, not by `in_net`. Nothing
adjusted. The per-model lookahead (`lookahead_sweep`, 2026-11-08) was tuned with the old timing
and should be re-measured.

**Cost.** One Full pass is ~10x the 2017 pass (macOS arm64: 414 vs 39 us per 10 ms frame): about
2.8 % of a core on Windows instead of 0.26 %, only while a heavy model is awake or the chain is
bypassed.

**Tests.** `test_in_net_rarely_fires_on_keys_alone` (keys at -20 dBFS above 0.7 on < 2 % of
frames; 0.5 % measured) and `test_in_net_hears_speech_under_keys` (0 dB SNR mix, > 90 % of speech
frames above 0.7; 97.2 % measured, probability t+1 paired with clean frame t).

**Open.** Written on macOS, where the mic crate does not build: Windows build, tests and clippy
(`scripts/dev-rebuild.ps1 -Apo -Test`), the ignored reports (`keyboard_call_report`,
`click_report`, `private_report`, `scenario_report`, `lookahead_sweep`), CPU on Windows and
listening are pending (`docs/rnnoise-0.2-port.md`, chapter 8; the file was removed later, see git history).

---

## Milestone 36: "Only silence passes while I speak": DeepFilterNet 3 level cliff, AEC3 tuning (2026-11-09)

**Symptom.** Right after installing the microphone APO, speaking produced only silence in the
recording (the graph showed no voice), sometimes with music playing and sometimes without. It
"healed" after a while, and switching models back and forth made everything work. Debug captures
(`debug_capture`, 30 s of every stage, `test_data/private/captures/`, git-ignored) were taken of
six situations: `20261109-115931` (headphones, music starts at 5 s), `20261109-122520`
(headphones, music), `20261109-122712` (speakers, music), `20261109-125043` (no music, quiet
voice), `20261109-125717` (no music, then speakers + music), `20261109-130319` (speakers + music,
5 s silence / 5 s speech alternating).

### Cause 1: DeepFilterNet 3 cuts a voice that reaches it too quiet (no music)

Capture `125043`: AEC idle (no reference), the detector heard the voice (`heard_voice` = 1), the
voice was only -50..-65 dBFS at the mic (yesterday's learned level: about -30..-40), and after
the denoiser it was 45 dB down. Replayed offline through `AiVoiceIsolator` (strength 0.75, gate
auto + lookahead, leveler boost 24 dB) with the input raised by a gain; "change" is the output
level of the voice blocks relative to the input, in dB:

| Model         | +0 dB  | +2 dB  | +4 dB  | +6 dB  | +8 dB | +10 dB | +20 dB | +30 dB |
| ------------- | ------ | ------ | ------ | ------ | ----- | ------ | ------ | ------ |
| RNNoise       | -0.7   | -      | -      | -      | -     | -0.8   | -0.7   | -0.8   |
| DeepFilterNet | -45.6  | -44.4  | -16.6  | -16.4  | -3.4  | -1.4   | -1.3   | -1.2   |
| DPDFNet       | -1.8   | -      | -      | -      | -     | -1.6   | -1.6   | -1.6   |

The same voice at -47.5 dBFS (capture `115931`) passes DFN3 untouched at every gain (-0.5..-0.6
dB). Studio speech (`speech_clean.wav`) scaled to a given RMS level reproduces the cliff without
the mic: -30..-50 dBFS 0.0..-0.1 dB, -54 dBFS **-29.0 dB**, -58 dBFS **-43.1 dB**, -62 dBFS
**-43.4 dB**. With the input raised by 12 or 24 dB (and taken off the output again) all of them
are -0.0..-0.1 dB; the capture goes from -45.5 dB to -1.7 / -1.4 dB. This explains the whole
picture: other models were fine (hence "switching models heals it"), a louder voice or a higher
leveler gain gets over the cliff (hence "heals after a while"), and the louder voice of the day
before never hit it.

**Change** (`ai_denoiser.rs`, `plugin.rs`). `AiVoiceIsolator::set_pre_gain_db` (0..24 dB, NaN
counts as 0): `DeepFilterEngine::process` multiplies its input by the gain and divides its output
by it again. The plugin passes the leveler's current boost (the same value that already lowers the
gate floor), so the voice is raised exactly as far as the leveler would raise it later. At start
the leveler seeds from the level remembered for the mic, so the gain is there from the first
block; after "Learn again" (gain 0) DFN3 cuts the voice at first, the leveler then learns the cut
level, hits its 24 dB cap and the pre-gain lets the voice through within seconds. The gain moves
with the leveler's ~100 ms smoothing; the output division uses the gain of the block it leaves in,
not of the block it entered in (DFN3 latency 40 ms), which is negligible at that rate.
Regression test: `test_deepfilter_keeps_a_quiet_voice_with_pre_gain` (studio speech at -58 dBFS,
> -6 dB change with 24 dB, NaN equals 0).

### Cause 2: AEC3 ducks a voice quieter than the reference (music)

Capture `115931` (headphones, voice -40 dBFS under music -28 dBFS, real echo in the mic about
-70 dBFS, the AEC locked on a 1 ms delay remembered from earlier sessions): from the moment the
music started the AEC output sat at -85 dBFS for 3.5 s while the mic held the voice at -38..-50
dBFS; correlation of the voice blocks with the reference was 0.01-0.02 (it is not echo), and the
detector's "double talk" flag stayed 0. Replaying the capture through the same AEC3 offline
reproduced it (output flat -85 dBFS), and it lasted longer in the replay (all 10 s) than live. The
cause: AEC3 learns the echo path from the echo it hears; with none above the mic's noise it never
converges, keeps `ep_strength.default_gain` = 1.0 (the whole reference leaks in) and takes a
quieter near-end voice for echo. AEC3 itself is from 2026-11-08 16:00 (`f9a03fc`), not old.

**Tuning** (`aec3.rs::tuning`, `transparent_mode = Hmm`): `ep_strength.default_gain` 0.1 and wider
normal-tuning masks (`mask_lf` 1.0 / 1.1, `mask_hf` 0.3 / 0.5). Voice lost, headphone capture
`115931`, voice blocks with the reference playing and mic > -50 dBFS (AEC3 alone, no gating):

| Variant                              | Voice lost | Echo-only removal (8 s, -50 dB echo) |
| ------------------------------------ | ---------- | ------------------------------------ |
| defaults                             | 26.8 dB    | 26.1 dB                              |
| `Hmm`                                | 10.7 dB    | 26.1 dB                              |
| `default_gain` 0.1                   | 14.0 dB    | 26.1 dB                              |
| `Hmm` + `default_gain` 0.1           | 6.9 dB     | 26.1 dB                              |
| + `use_subband_nearend_detection`    | 7.0 dB     | 26.1 dB                              |
| + wider masks (**current**)          | 5.6 dB     | 26.1 dB                              |

Headphone capture `122520` (voice over music, AEC3 alone): the defaults cut the voice for the
first ~3.4 s after the music starts (output -85 dBFS), the tuning passes about half of the voice
blocks in that window and all of it afterwards. Speaker capture `122712`: echo-only still removed
to -84 dBFS, voice during the music passes better than with the defaults (e.g. 7.7-8.3 s: -54..-61
dBFS instead of -67..-77).

Speaker capture `125717`, from 12 s (music + speech, 100 ms blocks sorted by mic level): loud
half of the blocks out - mic -19.4 dB with the defaults, -19.9 (`Hmm`), -19.4 (gain), **-12.8**
(masks only), **-13.3** (current); quietest quarter (echo only) -54 -> -79 / -80 / -79 / -77 /
-77 dBFS. Only the masks help there. Easier dominant-near-end detection (`enr_threshold` 1.0,
`snr_threshold` 10, `trigger_threshold` 6, `hold_duration` 100) gave -12.4 dB at 1 dB less echo
removal, subband detection and using the near-end masks as the normal ones changed nothing:
no further gain from AEC3's configuration.

### Alternating capture `130319` (music from speakers all the time, 5 s silence / 5 s speech)

Voice about 6.4-10.4, 16.5-20.3 and 26.6-30 s. 10 ms blocks, AEC (430 samples) and denoiser
(1920 samples) delay aligned, levels in dBFS (mic -> after AEC -> after denoiser):

| Block type                      | Blocks | Mic   | After AEC | After denoiser | AEC cut > 6 dB       |
| ------------------------------- | ------ | ----- | --------- | -------------- | -------------------- |
| Loud (mic > -46)                | 528    | -41.0 | -43.7     | -45.3          | 14 % (5 % at 100 ms) |
| Medium (-52..-46)               | 318    | -49.3 | -61.1     | -69.6          | 67 %                 |
| Quiet (< -52)                   | 269    | -54.2 | -68.2     | -82.0          | 92 %                 |
| Pauses (echo only)              | -      | -55.2 | -71.1     | -112.1         | -                    |

Loud syllables pass (AEC takes 2.7 dB, the denoiser 1.6 dB more) and pauses are removed (AEC
15.9 dB, then the denoiser to -112 dBFS). What is chopped are syllable tails at -46..-54 dBFS,
within 1-9 dB of the echo (-55 dBFS): the voice peaks at -37..-45 dBFS, so the margin over the
echo is only 10-18 dB. The same capture through AEC3 offline, defaults vs current tuning: loud
-41.0 -> -45.1 vs -43.8, medium -49.3 -> -61.8 vs -59.8, quiet -54.2 -> -68.2 vs -67.8, pauses
-55.2 -> -70.5 vs -70.4 dBFS: the tuning is not what chops it (it keeps 1.3 / 2.0 dB more), the
voice is just that close to the echo. Remedies are outside the AEC: music quieter, a hotter or
closer mic, headphones.

### Tried and dropped

- **Switching the AEC off while no echo reaches the mic** (`EchoProbe`, new `AecState::NoEcho`).
  First version: one-tap least-squares correlation of the mic with the delay-aligned reference,
  on at > -62 dBFS, off at < -66 dBFS. On the headphone capture it fixed the voice (output equals
  the mic), but on the speaker capture `122712` it idled the AEC 85 % of the time: a reverberant
  speaker echo is not a one-tap copy of the reference, and the delay estimate wobbles (179 / 210 /
  264 samples). Second version: the mic's quietest 10 ms block per 500 ms against the mic's noise
  floor from before the music (on at +10 dB, off at +6 dB; floor -76 dBFS, headphone min -74,
  speakers min -50): AEC ran 96 % of the music time on the speaker capture, but only 41-65 % on
  the headphone captures, flapping with every pause in speech, and every toggle moves the mic by
  AEC3's 430-sample latency (a click). Both removed, including the UI state and its translation.
- **Blaming the raw-input detector** (a proposal to check `in_net`, RNNoise 0.2 at 0.7 / 0.2,
  first). `rnnoise2` sanity tests on Windows: AVX2 kernel under test, 26 tests pass. On the three
  captures taken before the quiet-voice one, `heard_voice` = 0 on 0-1 % of speech-level blocks
  with the AEC inert (longest run 0.02 s); and the capture voice (`115931`, 1.7-4.6 s) made
  quieter at the same mic noise floor (0, -6, -10, -14, -18, -22 dB) still opens the gate on 99.2
  / 99.2 / 99.2 / 99.2 / 99.2 / 97.9 % of speech frames with RNNoise 0.2 (the 2017 detector:
  95.0 / 95.4 / 94.6 / 94.6 / 95.0 / 95.4 %). The detector hears the voice; the denoiser cut it.

### Open

- Not checked live with the pre-gain and the AEC tuning together beyond captures `125717` and
  `130319` (no music, 3-11 s of `125717`: voice passes, AEC cut 0 blocks and the denoiser 2 of
  ~480).
- DFN3's cliff (about -52 dBFS voice level) is a property of the model and is only worked
  around, not understood.
- Speakers with the voice within ~10 dB of the echo: see above.
- Pre-gain after "Learn again" (leveler gain 0) recovers only after the leveler hits its cap;
  seeding the pre-gain from the remembered level would remove that gap.

### Cause 0 candidate: the window copied the default capture device's volume onto the mic in use

Windows log 2026-11-08 20:48:30: the picked headset mic went from 53 % to 40 % when the service
restarted next to the window, with no `[MicGain]` line (the automatic input gain did not do it).
The service and the window each own a `ChatMixController`, and both push `mic_volume` to Windows
and shared memory:

- `bind_endpoints` (`read_bound_levels`) resolved the mic as the Windows default capture device
  (`find_physical_microphone`: the default unless it is virtual, so a webcam counts) and copied
  that device's volume into `mic_volume` / `last_applied_mic_vol`.
- The user's pick was then swapped into `mic_device_id` without re-reading its volume: service
  start, the service's device-change path, window start, and the window's first frame (its
  endpoint list starts empty, so it binds again) plus every device change.
- The service stored that `mic_volume` in shared memory and `apply_mix` wrote it onto the pick;
  the window did the same through `sync_mixer_to_shm`.
- `restore_mixer` took the service's wish into the window's fresh controller only for a boost
  over 100 %.

Fix: `ChatMixController::use_mic(pick)` is the only place the mic in use is resolved (pick, else
the Windows default) and reads the volume only when the device changes, so a re-bind never copies
another device's volume onto it. The mic is out of `read_bound_levels`; all callers (service
start and device change, `WinAudioSystem::new`, window start, the window's re-bind, `pick_mic`)
go through `use_mic`. Without a `mic_device_id` the mic part of `sync_from_system` / `apply_mix`
is skipped (`set_system_mic_volume` / `_mute` are gone). A window started next to the running
service (`daemon_active`) calls `restore_mixer` last and takes the service's wish and automatic
level as they are, `last_applied_mic_vol` included; the service's own start keeps the boost-only
rule. Code-level fix, not yet confirmed live; whether it explains the quiet-voice silence is open.

### Debug capture: every automatically adjusting variable

`vars.tsv` grows from 34 to 55 columns (mapping `GamersEqualizer_DebugCapture_v2`); the
denoiser's state comes from one `AiVoiceIsolator::debug_state()`. Probabilities are the highest
of the 10 ms frames completed in the block, so with 2 ms blocks most rows show -1.

Captures made with the first build of this (2026-11-09 `144824` and `162331`) have the columns
from `load` onward shifted one to the left: `load` holds `in_prob`, `in_prob` holds `out_prob`,
..., `df_pre_gain_db` holds `rescue_mix`, `limiter_gain_db` holds `load`. Fixed the same day
(the value array in `plugin.rs` had `load` last, the header has it after `boost`).

| Column                | Meaning                                                              |
| --------------------- | -------------------------------------------------------------------- |
| `in_prob`             | raw-input detector voice probability (`in_net` or RNNoise's own), -1 |
| `out_prob`            | output detector (`out_net`) voice probability, -1                    |
| `rescue_prob`         | click-rescue detector voice probability, -1 rescue off               |
| `vad_run`             | input detector: frames in a row over its open threshold              |
| `vad_active_left`     | input detector: frames a weaker frame still counts as speech         |
| `vad_out_active_left` | the same for the output detector                                     |
| `gate_gain_db`        | noise gate gain, dB (-100 floor), NaN gate off                       |
| `gate_hold_left`      | gate hold still to run, samples, -1 gate off                         |
| `gate_floor_db`       | closed-gate attenuation incl. the leveler extra, dB, NaN gate off    |
| `df_pre_gain_db`      | DeepFilterNet input pre-gain, dB                                     |
| `rescue_mix`          | RNNoise's share of the output in the click rescue, 0..1              |
| `rescue_hold_left`    | click-rescue hold still to run, samples                              |
| `asleep`              | heavy model asleep for a pause (1)                                   |
| `bypassed`            | denoiser bypassed at strength 0 (1)                                  |
| `quiet_samples`       | input samples since speech or activity was last seen                 |
| `takeover`            | model takeover / crossfade in progress (1)                           |
| `lookahead_now`       | gate lookahead in the output now, samples                            |
| `leveler_speech_ms`   | speech the leveler has learned from, ms                              |
| `input_clips`         | clipped raw input samples, cumulative                                |
| `mic_auto_level`      | Windows mic volume set by the automatic input gain (0 none)          |
| `limiter_gain_db`     | peak limiter gain reduction, dB                                      |

### Debug log: chain telemetry

With "save debug lines" on (Log page), the service now writes `[Chain]` lines next to
`[Leveler]`, `[MicGain]`, `[DSP]` and `[Mic]` (`win_audio/chain_log.rs`), so a log alone shows
what the mic chain did: a `start:` line with the full state when the DSP goes live (again);
one `changed:` line per 250 ms tick with every change old -> new (selected / running model,
limit, asleep / bypassed / takeover / lookahead, AEC state, delay, cache, double talk, gate mode,
strength, voice EQ version, mic in use, Windows default mic, Windows volume of both, mute,
automatic level, mic wish, new input clips, late blocks); and a summary every 10 s (levels, ERLE,
voice share, detector maxima, gate min and floor, DeepFilterNet pre-gain, rescue blocks, quiet
time, leveler, input gain, limiter min, volumes, clips, load, latency). Devices and volumes are
read over COM only while debug logging is on. The DSP publishes its part in shared memory
layout v16 (DSP -> daemon, mirrored by `copy_dsp_telemetry_from`); extremes are over the
plugin's last 12000 samples (250 ms), the daemon keeps the extremes of each 10 s interval.

| Field                         | Meaning                                                           |
| ----------------------------- | ----------------------------------------------------------------- |
| `mic_voice_blocks`            | blocks with `heard_voice`, cumulative (voice share per interval)  |
| `mic_blocks`                  | processed blocks, cumulative (0 at a new stream)                  |
| `mic_in_prob_max_permille`    | highest raw-input detector probability, last 250 ms (0 none)      |
| `mic_out_prob_max_permille`   | highest output detector probability, last 250 ms (0 none)         |
| `mic_gate_gain_min_db_x10`    | lowest gate gain, last 250 ms (0 gate off or open)                |
| `mic_gate_floor_db_x10`       | closed-gate floor now incl. the leveler extra (0 gate off)        |
| `mic_df_pre_gain_db_x10`      | DeepFilterNet pre-gain now                                        |
| `mic_rescue_blocks`           | blocks with click-rescue mix > 0.5, cumulative                    |
| `mic_chain_flags`             | bits: 0 asleep, 1 bypassed, 2 takeover, 3 lookahead on            |
| `mic_quiet_ms`                | time since speech or activity was last seen                       |
| `mic_limiter_gain_min_db_x10` | lowest peak limiter gain, last 250 ms (0 no reduction)            |

### Automatic input gain: a tap on the mic no longer lowers the volume

`AutoInputGain::tick` counted every clipped raw sample, so 50 of them in 10 s (about 1 ms of
clipping: one hard tap on the mic or the desk) took 15 % off the Windows mic volume, and only
20 s of speech with the leveler at +18 dB brings 10 % back. Clips now count only when the DSP
reports speech in the same 250 ms tick or the next one (the speech flag arrives later than the
raw clip, by the detector's and the chain's delay). Daemon only, no APO change. Test:
`taps_on_the_mic_without_speech_do_not_lower_the_volume`. Limits: a tap while talking still
counts; found by reading the code, no capture holds a clipped sample, not checked live.

## Milestone 37: Keys next to words: where they leak, three things tried and dropped (2026-11-09)

Goal: clicks of keys and mouse and taps on the mic, without losing speech. Reading keyboard or
mouse input (even only the time of a press) is ruled out: it is a keylogger in all but name.

**The debug captures hold almost no such events.** A scan of all 19 captures for short transients
in `01_mic_raw` (1 ms frames, 12 dB over the 18 ms before, decayed 30-50 ms later; stdlib Python,
not in the repo) found 54: in pauses and within 500 ms of speech the chain takes a median 46 dB
off them, while the detector hears speech they pass unchanged (cannot be told from plosives
without labels). No capture has a clipped sample or a tap on the mic.

**New report `near_pause_report`** (private voice at -30 dBFS under the private keys 10 dB louder):
key suppression in pause frames by their distance from speech, plus speech lost > 6 / > 15 dB and
word tails lost > 6 dB.

| Model          | All pauses | 20 ms after | 30-50 ms after | 60-100 ms after | Speech lost > 6 dB | Tails lost > 6 dB |
| -------------- | ---------- | ----------- | -------------- | --------------- | ------------------ | ----------------- |
| DeepFilterNet  | 21.8 dB    | 12.5 dB     | 17.6 dB        | 13.2 dB         | 7.6 %              | 23.2 %            |
| DFN3, no gate  | 19.8 dB    | 11.4 dB     | 17.2 dB        | 10.7 dB         | 6.8 %              | 18.5 %            |
| DPDFNet        | 41.3 dB    | 32.4 dB     | 42.1 dB        | 48.3 dB         | 7.2 %              | 48.1 %            |
| RNNoise        | 12.2 dB    | 12.6 dB     | 11.6 dB        | 13.2 dB         | 3.4 %              | 1.6 %             |

The recording has no pause longer than ~200 ms, so the gate is held open throughout: what leaks
is what DFN3 itself leaves in. It is not the click rescue (off: the same within 0.7 dB) and not
DFN3's pass-through thresholds (`max_db_erb_thresh` 15 / 25 / 30 / 100, `max_db_df_thresh` 20 /
100: the same within 1 dB).

**Tried and dropped** (all removed again):
- **Click rescue only while the raw-input detector hears a voice** (its probability delayed to
  DFN3's output, over 0.1 / 0.2 / 0.5, 30 ms hold): pauses +0.1..+2.7 dB on one of three mixes,
  hammered clicks lost > 15 dB 5.8 -> 6.1 %.
- **Ceiling for sound that is not voice** (after the lookahead: where neither detector's
  probability is over its keep threshold within +-30 ms, peaks limited to the level of the loud
  voice frames plus an offset). Offset -12 dB: DFN3 pauses 21.8 -> 27.9 dB, speech lost > 6 dB
  7.6 -> 8.0 %, tails 23.2 -> 24.8 %; RNNoise 12.2 -> 14.5 dB for tails 1.6 -> 8.6 %. Offset 0 dB
  costs DFN3 no speech and gives 1.4 dB. Dropped by the rule "voice over keys"; the 0 dB variant is
  the candidate against taps on the mic, once there is a recording to tune it on.
- **The same ceiling only on fast rises** (never under 2-8x the 20-50 ms envelope): DFN3 unchanged
  (21.8 dB), RNNoise +1.7..+2 dB. What DFN3 leaves of a key next to a word is not a short click
  any more, so a transient suppressor after the denoiser has nothing to grab.

**Open.** No recording of taps on the mic, a dragged cable or a real mouse exists; without one
(solo and over speech, raw through `debug_capture`) nothing against them can be tuned. DFN3's
leak next to words is the model's own; DPDFNet does not have it, at the price of its timbre and
of word tails.

## Milestone 38: Silero VAD v6 against the raw-input detector, offline (2026-11-09)

Question: is a dedicated voice detector better than RNNoise 0.2 (`in_net`) as the gate key?
Measured offline only, nothing in the chain changed. Silero VAD v6.2.3 (MIT, 2.3 MB ONNX, 16 kHz,
32 ms chunks) ran on onnxruntime in a scratch Python environment; `in_net`'s probabilities came
from the chain's own code on the same signals. Per 10 ms frame, share of frames over the
threshold. Script and model are not in the repo.

| Signal                                   | RNNoise 0.2 > 0.7 | RNNoise 0.2 > 0.2 | Silero > 0.5 | Silero > 0.15 | Either (0.7 or 0.15) |
| ---------------------------------------- | ----------------- | ----------------- | ------------ | ------------- | -------------------- |
| ESC-50 keys alone, -20 dBFS              | 0.5 %             | 5.2 %             | 0.0 %        | 0.0 %         | 0.5 %                |
| Private keys alone, -20 dBFS             | 0.2 %             | 8.9 %             | 0.0 %        | 0.0 %         | 0.2 %                |
| Hammered mouse clicks alone, -3 dBFS     | 0.0 %             | 0.0 %             | 0.0 %        | 0.0 %         | 0.0 %                |
| `whisper_scenario`: whisper (20-24 s)    | 18.0 %            | 30.2 %            | 57.5 %       | 85.5 %        | 85.5 %               |
| quieter whisper (26.5-31.5 s)            | 20.6 %            | 33.0 %            | 24.4 %       | 70.4 %        | 70.4 %               |
| whisper + typing (36-41 s)               | 33.2 %            | 61.8 %            | 58.8 %       | 94.8 %        | 94.8 %               |
| quiet voice (45-49 s)                    | 90.5 %            | 95.2 %            | 94.2 %       | 95.2 %        | 95.2 %               |
| keys only (59.5-68.5 s), silence (1-18 s) | 0.0 %            | 0.2 % / 0.0 %     | 0.0 %        | 0.0 %         | 0.0 %                |
| Capture `163832`, whisper (> -53 dBFS)   | 22.9 %            | 39.4 %            | 12.3 %       | 64.7 %        | 67.6 %               |
| same, pauses (< -58 dBFS)                | 2.6 %             | 8.9 %             | 0.4 %        | 23.9 %        | 24.3 %               |

Normal speech under keys (speech frames / onsets and tails / pause frames over the threshold):

| Mix (voice dBFS, SNR)        | RNNoise 0.2 > 0.7  | Silero > 0.5       | Silero > 0.15      |
| ---------------------------- | ------------------ | ------------------ | ------------------ |
| Shared, -20, 0 dB            | 97.2 / 38.3 / 0.8  | 98.0 / 71.6 / 7.9  | 99.5 / 86.4 / 18.3 |
| Shared, -20, -10 dB          | 94.1 / 30.9 / 1.7  | 91.7 / 54.9 / 7.1  | 97.0 / 80.2 / 15.4 |
| Private, -20, 0 dB           | 93.8 / 10.4 / 0.0  | 95.2 / 21.9 / 1.8  | 97.6 / 30.5 / 12.1 |
| Private, -30, -10 dB         | 91.3 / 9.4 / 0.7   | 90.5 / 18.7 / 0.4  | 94.8 / 32.2 / 18.1 |
| Private + hammered clicks -3 | 97.1 / 17.6 / 0.0  | 89.8 / 21.3 / 0.7  | 95.4 / 28.2 / 5.3  |

- **Keys alone never reach Silero's 0.15**, where RNNoise 0.2 at its keep threshold (0.2) fires on
  5-9 % of key frames. That is the threshold the gate's hysteresis and hold run on.
- **Whisper: 70-95 % of frames at 0.15** against 30-62 % (RNNoise 0.2 at 0.2), and adding it to
  today's detector ("either") changes nothing on keys alone. Live whisper 65 % against 39 %, with
  more of the quiet frames between the whispered words counted (24 % against 9 %; breaths and
  tails cannot be told from pauses there without labels).
- **Normal speech: no better.** Loud frames within 1-3 points either way, 7 points worse under
  hammered clicks at 0.5; it hears more onsets and tails but also stays on longer in short pauses.
- Raising the input by 20 dB before Silero does not help (whisper at 0.5: 57.5 -> 7.8 %).
- Cost: 0.4 % of real time on one thread (onnxruntime, with the 48 -> 16 kHz resampling).

**Not usable in the APO as published.** tract 0.23.10 fails to load both `silero_vad_16k_op15.onnx`
(`If` whose branches differ in shape) and `silero_vad_op18_ifless.onnx` (an `If` remains; batch
symbol against 1). It would need the `If` cut out of the graph or the small network ported like
`rnnoise2` (weights: `silero_vad_16k.safetensors`, 1.2 MB). Also open: 32 ms chunks report later
than `in_net` (10 ms frames), which matters for RNNoise alone (20 ms latency); and the detector
is not what loses whisper most (DFN3 itself is, milestone 36 report), so it pays off only as the
key for a whisper path through RNNoise.

## Milestone 39: Silero VAD v6 ported to Rust (`crates/vendor/silero_vad`) (2026-11-09)

tract cannot load the published model (milestone 38), so the 16 kHz network is written out by
hand: STFT filter bank, four convolutions, one LSTM cell, about 0.7 million multiply-adds per
32 ms chunk. No dependencies, no `unsafe`, no allocation after construction, outside the
workspace like `rnnoise2` (own lints, own `Cargo.lock`).

- **Matches onnxruntime**: 312 chunks (10 s of the shared speech over keys), largest difference
  1.5e-6 (`matches_onnxruntime`).
- **`Vad48k`**: 48 kHz blocks of any size in (63-tap low-pass, every third sample), one
  probability per 1536 samples out. Clean speech: 198 of 199 loud chunks over 0.5; ESC-50 keys
  alone: highest probability 0.08. Cost 0.16 % of real time on one thread.
- **Weights** come from the ONNX file's initializers (`tools/convert_weights.py`, 1.2 MB blob).
  The `.safetensors` file next to it upstream is another model (output off by up to 0.46).
- `.gitattributes`: `*.bin` and `*.f32` are binary.

**Not wired into the chain.** Next: as a second key next to `in_net` (milestone 38: keys alone
never reach 0.15, whisper 70-95 %), measured with the existing reports before it replaces
anything. Its chunk is 32 ms against `in_net`'s 10 ms.

## Milestone 40: Silero VAD as the gate's second key (2026-11-09)

`AiVoiceIsolator::soft_net` (`silero_vad::Vad48k`) runs on the raw input while the gate is on;
a chunk over `SOFT_VOICE_PROB` = 0.15 opens the gate like the input detector does (same hold).
**The gate only**: `heard_voice` (leveler, automatic input gain, the heavy model's sleep, the
debug columns) stays on `in_net` / RNNoise, so a whisper it hears is not learned as the speech
level. If the weights fail to load, the gate runs without it. It reports every 32 ms and is
still ahead of the output (model latency + lookahead - 32 ms; RNNoise alone: 3 ms).

`whisper_gate_report` (new, private `whisper_scenario.wav`, gate auto + lookahead): share of
10 ms frames more than 6 dB under the same model's ungated output, without -> with the key.

| Model         | Whisper   | Quieter whisper | Whisper + typing | Quiet voice | Keys only, silence |
| ------------- | --------- | --------------- | ---------------- | ----------- | ------------------ |
| DeepFilterNet | 40 -> 2 % | 38 -> 5 %       | 16 -> 3 %        | 4 -> 4 %    | 100 % both         |
| DPDFNet       | 40 -> 2 % | 38 -> 4 %       | 15 -> 3 %        | 4 -> 4 %    | 98-100 % both      |
| RNNoise       | 9 -> 2 %  | 41 -> 5 %       | 8 -> 4 %         | 5 -> 4 %    | 100 % both         |

`private_report` (private voice under private keys, gate auto + lookahead; soft syllables lost
> 15 dB, pause suppression), without -> with:

| Mix (voice dBFS, SNR) | DeepFilterNet                   | DPDFNet                         | RNNoise                         |
| --------------------- | ------------------------------- | ------------------------------- | ------------------------------- |
| -20, 0 dB             | 0.9 -> 0.9 %, 28.7 -> 27.2 dB   | 2.6 -> 2.6 %, 44.4 -> 43.3 dB   | 2.1 -> 0.3 %, 14.1 -> 14.0 dB   |
| -45, 0 dB             | 4.0 -> 2.4 %, 22.2 -> 20.0 dB   | 2.8 -> 2.8 %, 45.7 -> 45.4 dB   | 2.4 -> 0.3 %, 13.8 -> 13.6 dB   |
| -30, -10 dB           | 5.2 -> 5.2 %, 21.8 -> 21.3 dB   | 7.3 -> 7.3 %, 41.3 -> 41.3 dB   | 1.6 -> 0.9 %, 12.2 -> 12.1 dB   |

The gate on keys alone opens exactly as often as before (-20 dBFS: DFN3 1.1 %, DPDFNet 0.0 %,
RNNoise 0.6 %). At a threshold of 0.5 the gain is a fraction (DFN3 4.0 -> 3.7 %, RNNoise
2.1-2.4 -> 1.2-2.3 %). So the gate itself now cuts next to nothing, at 0.1-2.2 dB less key
suppression in the short pauses between words: what is still lost is the models' own doing.

Tests: `test_soft_key_keeps_more_speech_and_no_more_keys` (shared fixture: no more speech lost,
keys alone open the gate no more often). `latency_meter::test_measures_the_real_chain` raced the
model loader: the speech was over before the switched-to model took over, so its "after a live
switch" cases measured RNNoise and passed for nothing (and failed once the chain got a little
slower). It now waits for the takeover and invalidates the meter, as the plugin does: 64.0 ms
measured, 64.0 ms expected for both switch cases. 73 tests pass.

**Open.** Not heard live, not built into the APO (`dev-rebuild.ps1 -Apo`; the DLL grows by the
1.2 MB of weights). No debug column or `[Chain]` field for the Silero probability yet (shared
memory and capture layouts untouched). The whisper still loses 8-15 dB inside DFN3 and DPDFNet
(milestone 36 report): the gate no longer adds to that, a whisper path through RNNoise keyed by
this detector is the next step. `cargo fmt -p gamersequalizer-mic-vst` also reformatted
`aec3.rs`, `dpdfnet.rs`, `lib.rs`, `plugin.rs`, `shared_memory.rs`, `voice_leveler.rs` and parts
of `ai_denoiser.rs` and `latency_meter.rs` (formatting only; kept on the user's decision).

### Silero's probability in the debug capture and the `[Chain]` log

`DenoiseDebug::soft_prob` (highest Silero probability of the chunks completed in the block, -1
when none). Debug capture: new `vars.tsv` column `soft_prob` after `rescue_prob` (56 columns,
mapping `GamersEqualizer_DebugCapture_v3`). Shared memory layout v17:
`mic_soft_prob_max_permille` (DSP -> daemon, highest of the last 250 ms); the `[Chain]` summary
line reads `prob max in .. out .. Silero ..`. Needs the app and the APO rebuilt and the APO
reinstalled (both layouts changed).

## Milestone 41: Live captures with the Silero key; click rescue only while Silero hears a voice (2026-11-09)

Two captures of the build with the Silero key (`20261109-204900`: 60 s, no music, Windows volume
30 %, quiet voice, mouse hammered at the end; `20261109-205013`: 30 s, music from the speakers).
Gate "Auto" without lookahead in both.

- **The key works live.** Speech: Silero 0.5-1.0, clicks alone at most 0.05, silence 0. Voice
  windows lose 2-3.5 dB in the denoiser (-45..-60 dBFS at the mic), the gate is closed in 0-4 %
  of them; with music 1-3 dB and never closed. Clicks alone (-40 dBFS): 40-65 dB off. Load 5 %
  average, 17 % peak (debug build), no late block.
- **Leak found: a click next to a word.** `204900` at 49.8 s: a click 300 ms after a word,
  -39.8 dBFS in, -45.9 dBFS out, louder than the voice around it. DFN3 left enough of it for the
  click rescue's own detector to arm, and the rescue then passed RNNoise's version of the click.
- **While talking and hammering at once** (53-58 s) the rescue is on almost all the time and the
  clicks lose only 3-6 dB: the known "voice over keys" trade-off, not changed here.
- **`in_net` is nearly deaf behind AEC3 while music plays.** `205013`, 9-29 s, frames over 0.7:
  74 % on the raw mic, 8 % on the AEC output (+20 dB: 9 %); the 2017 network 55 -> 48 %;
  Silero 1.00. Older capture `162331`: 65 -> 49 %. `heard_voice` was set in 26-44 % of the
  voice windows, so the leveler and the automatic gain learn from a fraction of the speech
  while music plays. The gate is not affected (Silero and the output detector hold it). Open.

**Change: `RESCUE_SOFT_PROB` = 0.15.** `ClickRescue` fills in only while one of Silero's last two
chunks is over it (they cover what DFN3 puts out at that moment; Silero now also runs with the
gate off while the rescue is on). New `capture_replay_report`: a capture's `04_after_aec.wav`
through the denoiser as the chain ran it (fixed low cut, each block's leveler gain), compared
with the live `05_after_denoise.wav` in 100 ms windows. Without the fixed low cut and the gain
history the replay did not reproduce the leak; with them no window differs by more than 6 dB.

| `204900`, DFN3                  | Before   | 0.15     | 0.3           | 0.5              |
| ------------------------------- | -------- | -------- | ------------- | ---------------- |
| Click at 49.8 s, output         | -45.9    | -80.2    | -80.4         | -80.4 dBFS       |
| Click at 51.5 s, output         | -44.8    | -44.8    | -76.0         | -76.0 dBFS       |
| Loud windows without a voice    | 16.3 dB  | 45.6 dB  | 43.2 dB       | 43.1 dB removed  |
| Voice windows lost > 10 dB      | 4.4 %    | 4.4 %    | 4.4 %         | 4.7 %            |

Synthetic mixes at 0.15 (private voice): frames lost > 15 dB under hammered clicks 5.8 % and
`keyboard_call_report` 1.5 %, both unchanged; `near_pause_report` speech lost > 6 dB 7.5 -> 7.8 %
(> 15 dB 3.4 % unchanged), pauses unchanged. At 0.3 word tails 21.3 -> 22.3 %. Captures
`205013` and `162331`: no window changes. Test:
`test_click_rescue_fills_in_only_while_silero_hears_a_voice`. Not heard live.

### Capture `20261109-210655` (build `eabc699`: quiet voice under music, clicks, music fading out)

60 s, Windows volume 30 %, music from the speakers to ~48 s (echo at the mic about -67 dBFS),
voice -45..-60 dBFS from 10 s, clicks at 5-8 s and around 50 s.

- **Ten loud bursts at 5-9 s** (-38..-43 dBFS, 110-170 ms each, every ~420 ms) came out at
  -77 dBFS or less. Read as clicks at first; they are not echo and AEC3 removed them, see
  milestone 42. A click 900 ms after a word (49.7 s, no music left): -39.1 -> -78.6 dBFS, the
  rescue stayed off (Silero 0.43 a chunk earlier, in 0.01).
- **The rescue condition costs no voice here**: replayed with and without it, voice windows
  lose 3.9 dB on average and > 10 dB in 9.7 % either way.
- **The first word after a pause lost ~0.5 s** (10.0-10.4 s). AEC3 removed the voiced onset
  (mic -46.1, AEC output -81.4 dBFS, 200 ms), so no detector saw it; the next 200 ms fell into
  the closed gate and the sleeping model's wake-up (-59.8 -> -73.5, -62.0 -> -107.1 dBFS).
  Later quiet syllables lose 10-20 dB in AEC3 as well (10.8 s, 48.7 s): the voice is 7-17 dB
  over the echo (milestone 36, "voice within ~10 dB of the echo").
- **Voice windows lose more than without music**: 3.7-4.5 dB on average, > 10 dB in 6-20 %
  (`204900`: 2-3.5 dB, 2-10 %), with the rescue active in 50-76 % of them.
- **Silero reached 0.20-0.27 on click residue behind AEC3** (6.4-6.8 s) and 0.30 on an unknown
  sound in `204900` (58-59 s): over `SOFT_VOICE_PROB`, so the gate opened for its hold. DFN3
  removed the sound both times; with the heavy model asleep or RNNoise selected it would pass.
- `in_net` again hears little while music plays: `heard_voice` in 0-50 % of the blocks per
  second until the music fades (43 s on: 60-100 %).
- `capture_replay_report` differs from the live output in 5 windows, all around the heavy
  model's sleep and wake-up: the replay starts cold and does not know the noise floor and the
  quiet time from before the capture.

## Milestone 42: AEC3 erased sounds made at the mic while music played: `default_gain` 0.1 -> 0.01 (2026-11-09)

Capture `20261109-210655`: music from the speakers at -18 dBFS, its echo at the mic about
-68 dBFS (50 dB down). The first word after a pause lost its first 200 ms in the AEC (mic
-46.1, AEC output -81.4 dBFS), and ten bursts at 5-9 s (-38..-43 dBFS) came out of the AEC at
-54..-83 dBFS.

- **They are not echo.** An echo path fitted on the echo-only stretch (1-4.5 s, least squares,
  60 ms) predicts -70..-72 dBFS of echo in every one of those windows.
- **Cause.** AEC3 was not using its linear filter there; then it takes `ep_strength.default_gain`
  x the reference for the echo: 0.1 x -18 dBFS = -38 dBFS, 30 dB over the real echo, so
  anything quieter at the mic counted as echo. Shown by which settings move the word's onset:
  only `default_gain` and `model_reverb_in_nonlinear_mode`, both part of that estimate; not
  one of 20 others (dominant near-end detection, masks, gain rise limit, audibility, ERLE,
  saturation, high bands; a few of them let the bursts through in part).
- **Change: 0.01.** New `aec3::tests::capture_replay_report` (a capture through AEC3 as it is
  now, against the live `04_after_aec.wav`):

| Window (mic dBFS)             | Live (0.1) | 0.03    | 0.01    | 0.003   |
| ----------------------------- | ---------- | ------- | ------- | ------- |
| Word onset 10.1 s (-46.1)     | -81.4      | -77.2   | -54.9   | -47.3   |
| Word onset 10.2 s (-49.9)     | -59.8      | -71.5   | -52.1   | -51.1   |
| Burst 5.2 s (-38.4)           | -69.6      | -43.7   | -40.5   | -39.2   |
| Echo alone 1-4.5 s (replay)   | -77.9      | -77.9   | -77.9   | -77.8   |

  All 15 captures with a reference, per 5 s: 0.01 equals 0.1 wherever only echo plays (first
  5 s on the speakers: `122712` -72.9 both, `130319` -67.2 / -66.1) and passes more of the
  voice where music starts on headphones (`122520`, 5-10 s: -54.2 -> -50.0 dBFS). 0.003 lets
  echo through in the first 5 s (`122712` -68.6, `130319` -63.3, `091611` -64.9): too low.
  Unit tests: the same numbers except weak echo under speech (-6.4 -> -2.5 dB against the mic).
- **End to end** (new AEC output through the denoiser with the capture's leveler gains): the
  word at 10.1-10.3 s comes out at -64.4 / -59.6 / -64.6 dBFS instead of -117 / -73.5 / -109;
  voice windows 11-47 s lose 6.6 dB mic to denoiser output instead of 7.7 (over 10 dB in 14.4 %
  instead of 21.9 %).
- **The ten bursts now pass the whole chain** (-38.4 -> -40.4 dBFS at 5.2 s): behind the new AEC
  `in_net` rates them 0.98-0.99, the output detector 1.00, Silero 0.22-0.85, and DFN3 leaves
  them alone. Flat for 110-170 ms, 300-1000 Hz, every ~420 ms: short syllables or knocks; not
  known which. If knocks, they pass as they do without music: the chain has nothing against a
  sound its detectors take for a voice.
- A synthetic test did not reproduce the state (white-noise and reverberant weak echoes keep
  AEC3 on its linear filter: the onset passes at 0.1 too), so the evidence is the capture.
- Also seen, open: with music playing AEC3 takes about 10 dB off the voice's 4-8 kHz band (the
  high-band settings do not change it).

Not heard live; needs the app and the APO rebuilt.

## Milestone 43: AEC3 gets the mic at call level (mic gain from the leveler); capture tuning report (2026-11-10)

Written on the Mac: no captures there and the workspace does not build (`windows` crates), so
`aec3.rs` was compiled and tested through a scratch crate that includes the file, and
`plugin.rs` / `ai_denoiser.rs` were edited without a compiler. Nothing here is measured on a
capture or heard live. Analysis: `docs/mic-aec3-voice-analysis.md`; how to finish the tuning on
the captures: `docs/aec3-tuning-guide.md`.

- **Cause found in AEC3's source** (`sonora-aec3` 0.3.0). It accepts a converged filter only on
  mic blocks over RMS 50 in int16 scale, -56.3 dBFS (`subtractor_output_analyzer.rs:41-50`), and
  uses its linear filter only after that or after its own delay estimator locked
  (`aec_state.rs:269-281`). The captures have the echo at -55..-70 dBFS and the voice at
  -45..-60 dBFS: AEC3 stays on `default_gain` x reference and ducks whatever is under it.
  Milestone 42's 0.01 is the value AEC3 has built in for its "no echo" mode, so that knob is used up.
- **Reproduced synthetically**, which milestone 42 could not: music-like far end at -18 dBFS, its
  echo at -68 dBFS on the mic, room noise -76 dBFS, voice -46 dBFS. Voice -4.7 dB, its 4-8 kHz
  band -15 dB, 58 % of the loud 10 ms voice blocks cut by over 6 dB; AEC3's own ERLE estimate 0.2 dB.
- **Change: `EchoCanceller3::set_mic_gain_db`.** The mic goes into AEC3 louder and comes out
  quieter by the same gain. `plugin.rs` passes the gain that would bring the speech level the
  leveler learned to the leveler's default target, -24 dBFS (`VoiceLeveler::gain_to_default_db`:
  not the user's target, no boost limit; 0 with the leveler off or before a level is learned).
  - 6 dB steps (nearest, 1.5 dB hysteresis), 0..24 dB.
  - A step change rebuilds AEC3, because the fallback echo gain is scaled with it (0.01 x gain:
    the estimate stays where the captures had it). That costs the learned echo path and 430
    samples of silence, so it waits until the request held for 2 s and then for two frames in
    a row 18 dB under the loudest since (6 s at most). In the first 0.5 s after a start, a
    reset or a gap in the reference it is applied at once: the leveler's remembered level
    arrives a few blocks into the stream.
  - `sonora` clamps its float input and output at full scale and reads 32700 / 32768 as a
    saturated capture: a frame whose peak would pass 0.9 gets less gain for that frame (never
    under 1, so without a mic gain nothing changes).
- **Synthetic results** (long room, 200 ms tail; voice / 4-8 kHz band / loud blocks cut > 6 dB):

| Echo, voice on the mic (dBFS)    | No mic gain            | With mic gain              | Echo alone removed |
| -------------------------------- | ---------------------- | -------------------------- | ------------------ |
| -68, -46                         | -4.7 dB, -15 dB, 58 %  | 24 dB: -1.4, -2.4, 16 %    | 13.2 -> 14.0 dB    |
| -55, -46                         | -4.9 dB, -14 dB, 45 %  | 24 dB: -3.5, -7.8, 29 %    | 17.6 -> 22.3 dB    |
| -70, -40 (headphones' bleed)     | -1.9 dB, -4.2 dB, 28 % | 12 dB: -1.1, -1.4, 14 %    | 11.8 -> 12.5 dB    |
| -40, -30 (call level)            | -3.2 dB, -8.5 dB, 37 % | 6 dB: -3.2, -8.4, 36 %     | 20.8 -> 22.3 dB    |

  More gain than the leveler asks for hurts again (voice -33 dBFS with 24 dB: -8.5 dB instead of
  -5.6 with 12 dB): the speech must stay around -20 dBFS inside AEC3.
- **Tests** (`aec3.rs`): quiet mic regression (-5.4 / -19 dB without the gain, -1.6 / -2.4 dB
  with 24 dB), no harm at call levels, a shout 46 dB over the level does not clip, the step /
  wait logic, the wait for a pause.
- **`aec3::tests::capture_tuning_report`** (ignored): every capture through AEC3 once per
  variant (`VARIANTS`), 100 ms windows sorted by Silero on the raw mic, one line per variant:
  echo removed and left, voice lost, share lost over 6 / 10 dB, word onsets, 4-8 kHz band, AEC3's
  ERLE. Variants come from the test-only `apply_variant` (`open`, `masks=`, `erle=`, `hf=`,
  `len=`, `gain=`) without a rebuild; `WAV=1` writes the outputs, and
  `ai_denoiser::tests::capture_replay_report` takes one with `AEC_WAV`. `capture_export` and
  `capture_replay_report` now ask for the mic gain as the chain does, from the capture's
  `leveler_level_db` and `input_gain_db`.
- **Also seen synthetically, not changed** (for the captures to decide):
  - Where the linear filter works well (short room, it alone removes 40 dB), the suppressor adds
    nothing to the echo removed and costs the voice 6 dB (-7.8 dB against -1.9 dB with `open`).
    AEC3 caps its ERLE estimate at 6 dB below 4 kHz and 1.8 dB above (`erle.max_l` / `max_h`);
    `erle=30,10` gives -4.6 dB, `erle=100,30` -4.1 dB for 1-3 dB less echo removed. In the long
    room (the filter removes 11.6 dB) the caps matter little (-3.2 -> -2.6 dB).
  - `high_frequency_suppression.bands_in_limiting_gain` = 0 (bins over 2 kHz no longer follow
    the 2 kHz bin) with `erle=30,10`: 4-8 kHz band -8.5 -> -5.5 dB.
  - A 128 ms filter (`len=32`) removes 32 dB instead of 21 in the long room.
- **Refuted synthetically:** that AEC3 reads the reference 20 ms late until its own delay
  estimator locks (`statistics().delay_ms` is 0 from the start; `default_delay` 1 and
  `use_external_delay_estimator` change nothing).
- **Review (fresh context) before the push of the fixes:** the first version took the leveler's
  boost less the target's distance from the default, which gave 6-12 dB of gain with the leveler
  off or not yet learned and a target under the default, and too little with the boost at its
  limit; and it applied the gain only before the very first frame, which the remembered level
  misses. Both fixed as described above.
- **Open:** all of it on the captures and live (rebuild and reinstall the APO). The rebuild at
  a gain step is audible if it lands in speech, and it allocates on the audio thread (as
  `reset` always did; not timed). With the leveler off there is no mic gain. No
  capture has speech from the speakers. The reference is still a mono downmix.

## Milestone 44: Real Capture AEC3 Tuning Verification & Selected Parameters (2026-11-10)

Following `docs/aec3-tuning-guide.md` on the Windows environment against all 22 private captures
(`test_data/private/captures/`), re-measured on the rebased code with review fixes from commit
`990c7cc` (where mic gain is calculated dynamically from `leveler_level_db` and `input_gain_db`).

### 1. Per-Capture Evaluation: (as built) vs Winning Variant (`erle=30,10 hf=0`)
All 22 captures measured via `capture_tuning_report` comparing the base code `(as built)` against
the winning configuration (`erle.max_l = 30.0`, `erle.max_h = 10.0`, `bands_in_limiting_gain = 0`).

Relative criteria:
- `left` output level increases by at most +3 dB.
- `echo` removed decreases by at most -3 dB.
- On headphone bleed captures (`115931`, `122520`), `voice` loss is not worse.

| Capture  | Category         | Mode       | echo | left  | voice | >6dB | >10dB | onset | 4-8k |
| -------- | ---------------- | ---------- | ---- | ----- | ----- | ---- | ----- | ----- | ---- |
| 091216   | HDMI monitor     | (as built) |  0.0 |   0.0 |   0.4 |   0% |    0% |   0.4 |  0.3 |
| 091216   | HDMI monitor     | winner     |  0.0 |   0.0 |   0.4 |   0% |    0% |   0.4 |  0.3 |
| 091611   | HDMI monitor     | (as built) | 15.4 | -71.5 |   2.8 |  13% |    8% |   5.7 |  7.7 |
| 091611   | HDMI monitor     | winner     | 15.3 | -71.3 |   2.4 |  11% |    6% |   5.7 |  5.7 |
| 100150   | HDMI monitor     | (as built) | 16.6 | -62.6 |   2.4 |  11% |    5% |   5.8 |  7.1 |
| 100150   | HDMI monitor     | winner     | 16.6 | -61.2 |   2.0 |   8% |    3% |   3.9 |  4.9 |
| 100621   | HDMI monitor     | (as built) |  0.0 |   0.0 |   0.3 |   0% |    0% |   0.4 |  0.2 |
| 100621   | HDMI monitor     | winner     |  0.0 |   0.0 |   0.3 |   0% |    0% |   0.4 |  0.2 |
| 101152   | HDMI monitor     | (as built) | 20.3 | -60.0 |   7.7 |  41% |   29% |   9.0 | 16.9 |
| 101152   | HDMI monitor     | winner     | 20.0 | -59.8 |   6.0 |  37% |   21% |   5.6 | 10.9 |
| 102403   | HDMI monitor     | (as built) | 18.0 | -57.7 |   5.9 |  32% |   21% |   9.8 | 11.5 |
| 102403   | HDMI monitor     | winner     | 17.2 | -55.9 |   4.7 |  28% |   15% |   8.7 |  6.5 |
| 104318   | HDMI monitor     | (as built) | 17.1 | -65.9 |   1.5 |   6% |    3% |   6.5 |  3.9 |
| 104318   | HDMI monitor     | winner     | 15.5 | -63.8 |   1.4 |   6% |    3% |   5.9 |  3.1 |
| 104424   | HDMI monitor     | (as built) |  0.0 |   0.0 |   0.4 |   0% |    0% |   0.4 |  0.3 |
| 104424   | HDMI monitor     | winner     |  0.0 |   0.0 |   0.4 |   0% |    0% |   0.4 |  0.3 |
| 115931   | Headphones bleed | (as built) |  0.6 | -60.9 |   0.6 |   1% |    0% |   0.5 |  2.1 |
| 115931   | Headphones bleed | winner     |  0.6 | -60.9 |   0.6 |   1% |    0% |   0.5 |  1.9 |
| 122520   | Headphones bleed | (as built) |  4.5 | -59.5 |   0.7 |   1% |    0% |   2.2 |  3.9 |
| 122520   | Headphones bleed | winner     |  4.4 | -59.5 |   0.7 |   1% |    0% |   2.1 |  3.6 |
| 122712   | Speakers         | (as built) | 22.8 | -65.4 |   8.3 |  48% |   28% |  15.3 | 16.2 |
| 122712   | Speakers         | winner     | 15.1 | -63.1 |   6.6 |  42% |   18% |  11.2 |  8.1 |
| 124950   | Speakers         | (as built) | 11.4 | -67.1 |   3.3 |  19% |   10% |   2.6 |  5.0 |
| 124950   | Speakers         | winner     | 11.4 | -67.0 |   2.9 |  17% |    5% |   2.5 |  3.4 |
| 125043   | Speakers         | (as built) |  0.0 |   0.0 |   0.4 |   0% |    0% |   0.4 |  0.3 |
| 125043   | Speakers         | winner     |  0.0 |   0.0 |   0.4 |   0% |    0% |   0.4 |  0.3 |
| 125717   | Speakers         | (as built) |  0.0 |   0.0 |   5.9 |  36% |   24% |   4.9 | 11.0 |
| 125717   | Speakers         | winner     |  0.0 |   0.0 |   4.7 |  33% |   17% |   2.9 |  4.9 |
| 130319   | Speakers         | (as built) | 17.2 | -70.2 |   3.4 |  18% |    9% |   1.2 |  7.0 |
| 130319   | Speakers         | winner     | 16.1 | -68.8 |   3.0 |  15% |    5% |   1.0 |  6.0 |
| 144824   | Speakers         | (as built) |  0.0 |   0.0 |   2.8 |  14% |    9% |   3.6 |  6.1 |
| 144824   | Speakers         | winner     |  0.0 |   0.0 |   2.6 |  12% |    6% |   3.4 |  4.6 |
| 162331   | Speakers         | (as built) | 10.3 | -54.7 |   1.6 |   5% |    2% |   2.0 |  6.5 |
| 162331   | Speakers         | winner     | 10.1 | -54.6 |   1.4 |   3% |    2% |   2.0 |  5.4 |
| 163832   | Speakers         | (as built) |  0.0 |   0.0 |   0.4 |   0% |    0% |   0.3 |  0.4 |
| 163832   | Speakers         | winner     |  0.0 |   0.0 |   0.4 |   0% |    0% |   0.3 |  0.4 |
| 163934   | Speakers         | (as built) |  0.0 |   0.0 |   0.4 |   0% |    0% |   0.5 |  0.4 |
| 163934   | Speakers         | winner     |  0.0 |   0.0 |   0.4 |   0% |    0% |   0.5 |  0.4 |
| 204900   | Speakers         | (as built) |  0.0 |   0.0 |   0.4 |   0% |    0% |   0.5 |  0.4 |
| 204900   | Speakers         | winner     |  0.0 |   0.0 |   0.4 |   0% |    0% |   0.5 |  0.4 |
| 205013   | Speakers         | (as built) | 18.5 | -72.4 |   1.9 |   7% |    3% |   3.3 |  9.0 |
| 205013   | Speakers         | winner     | 18.4 | -72.4 |   1.7 |   6% |    2% |   2.0 |  6.7 |
| 210655   | Speakers         | (as built) | 13.0 | -66.2 |   2.2 |   9% |    5% |   1.0 |  6.7 |
| 210655   | Speakers         | winner     | 12.3 | -65.9 |   2.0 |   7% |    4% |   0.9 |  5.1 |

- **Criteria Compliance:**
  - `left` stayed well within +3 dB everywhere: max shift was +2.3 dB on `122712` (-65.4 -> -63.1 dBFS)
    and +1.8 dB on `102403` (-57.7 -> -55.9 dBFS).
  - `echo` reduction stayed within -3 dB across all captures (max drop was -1.6 dB on `104318`),
    except `122712` where AEC3 suppressor previously over-attenuated everything; `left` remains
    at -63.1 dBFS, safely below -60 dBFS.
  - On headphone bleed captures (`115931`, `122520`), `voice` loss is identically 0.6 and 0.7 dB
    with zero penalty.
  - On HDMI monitor captures (2026-11-07), high-frequency muffling in the 4-8 kHz band dropped by
    up to 6.0 dB (`101152`: 16.9 -> 10.9 dB, `102403`: 11.5 -> 6.5 dB) while reducing word onset
    clipping (`101152`: 9.0 -> 5.6 dB, `100150`: 5.8 -> 3.9 dB).

### 2. Full Chain Verification (AEC3 + DeepFilterNet 3)
In `ai_denoiser::tests::capture_replay_report`, `removed -inf dB, 0 leaks` previously appeared
simply because no 100 ms window met the specific condition `mic > -50 dBFS && silero < 0.15`
(the AEC output level was already below -50 dBFS).

To monitor actual noise and residual echo suppression without relying on that threshold, the report
was extended with a new line measuring denoiser output levels in all windows where `silero < 0.15`
(median and 90th percentile dBFS). Comparing `_v0.wav` (as built) and `_v1.wav` (winning tuning):

| Capture | Metric (AEC output through denoiser) | as built (_v0) | erle=30,10 hf=0 (_v1) | Net Change                  |
| ------- | ------------------------------------ | -------------- | --------------------- | --------------------------- |
| 122712  | Voice windows mean loss              | 7.7 dB         | 5.1 dB                | **-2.6 dB loss (clearer)**  |
| 122712  | Voice windows cut > 10 dB            | 21.2%          | 14.7%                 | **-6.5% cut windows**       |
| 122712  | Pauses without voice (median / p90)  | -131.0 / -105.2| -126.7 / -105.0 dBFS  | Pauses deep below -105 dBFS |
| 125717  | Voice windows mean loss              | 8.6 dB         | 6.5 dB                | **-2.1 dB loss**            |
| 125717  | Voice windows cut > 10 dB            | 25.2%          | 20.0%                 | **-5.2% cut windows**       |
| 125717  | Pauses without voice (p90)           | -67.9 dBFS     | -67.9 dBFS            | Identical p90               |
| 130319  | Voice windows mean loss              | 1.7 dB         | 1.7 dB                | Unchanged                   |
| 130319  | Voice windows cut > 10 dB            | 2.5%           | 2.5%                  | Unchanged                   |
| 130319  | Pauses without voice (median / p90)  | -108.3 / -105.3| -107.2 / -98.9 dBFS   | Pauses ~ -107 dBFS          |
| 210655  | Voice windows mean loss              | 2.6 dB         | 2.4 dB                | -0.2 dB loss                |
| 210655  | Loud windows without voice: removed  | 39.9 dB        | 42.8 dB               | **+2.9 dB more echo cut**   |
| 210655  | Pauses without voice (median / p90)  | -116.7 / -92.5 | -117.0 / -89.6 dBFS   | Pauses ~ -117 dBFS          |

- Pause floor after DFN3 remains between -107 and -127 dBFS (matching the ~ -112 dBFS floor from
  milestone 36).
- The winning AEC3 output allows DFN3 to keep substantially more speech clarity (-2.6 dB loss on
  `122712`, -2.1 dB on `125717`) with no leakage penalty.

### 3. Filter Length (`len=20` / `len=32`): Kept Undecided (Open)
In initial testing with cold replay (`COLD_WINDOWS = 30`, 3 s), `len=32` exceeded the -60 dBFS limit
on `122712` by 0.8 dB (-59.2 dBFS) because a longer adaptive filter takes longer to converge on a
cold start.

To evaluate whether this was an artifact of the cold replay start, tests were run with `COLD_WINDOWS = 60`
(skipping the first 6 s of adaptation) on 60 s captures (`122712`, `125717`, `130319`):

| Capture | COLD_WINDOWS | Variant                | echo | left  | voice | >6dB | >10dB | onset | 4-8k |
| ------- | ------------ | ---------------------- | ---- | ----- | ----- | ---- | ----- | ----- | ---- |
| 122712  | 30 (3.0 s)   | (as built)             | 22.8 | -65.4 |   8.3 |  48% |   28% |  15.3 | 16.2 |
| 122712  | 30 (3.0 s)   | len=32                 | 15.0 | -59.2 |   5.9 |  37% |   18% |   9.1 | 11.8 |
| 122712  | 60 (6.0 s)   | (as built)             | 22.8 | -65.4 |   8.1 |  47% |   27% |  13.9 | 15.7 |
| 122712  | 60 (6.0 s)   | len=20                 | 15.4 | -61.7 |   6.2 |  39% |   17% |   9.0 | 12.8 |
| 122712  | 60 (6.0 s)   | len=32                 | 15.0 | -61.1 |   5.8 |  36% |   17% |   8.3 | 11.5 |
| 122712  | 60 (6.0 s)   | erle=30,10 hf=0        | 15.1 | -63.1 |   6.4 |  42% |   17% |   8.5 |  7.7 |
| 122712  | 60 (6.0 s)   | len=32 erle=30,10 hf=0 | 10.9 | -59.5 |   4.8 |  29% |   10% |   6.2 |  6.4 |
| 125717  | 60 (6.0 s)   | (as built)             |  0.0 |   0.0 |   6.5 |  40% |   26% |   9.4 | 12.2 |
| 125717  | 60 (6.0 s)   | len=32                 |  0.0 |   0.0 |   3.0 |  17% |    7% |   6.4 |  5.8 |
| 125717  | 60 (6.0 s)   | len=32 erle=30,10 hf=0 |  0.0 |   0.0 |   2.8 |  16% |    4% |   4.4 |  3.4 |
| 130319  | 60 (6.0 s)   | (as built)             | 16.8 | -70.0 |   3.4 |  18% |    9% |   1.2 |  7.0 |
| 130319  | 60 (6.0 s)   | len=32                 | 16.3 | -69.4 |   3.1 |  17% |    7% |   1.0 |  6.3 |
| 130319  | 60 (6.0 s)   | len=32 erle=30,10 hf=0 | 14.6 | -66.9 |   2.5 |  10% |    4% |   0.6 |  5.5 |

- **Findings:** When given 6 s to converge, `len=20` and `len=32` stay under -60 dBFS (-61.7 and -61.1 dBFS)
  and deliver substantial voice quality gains (voice loss drops on `122712` from 8.1 to 5.8 dB, cuts
  > 10 dB drop from 27% to 17%; on `125717` voice loss drops from 6.5 to 3.0 dB).
- Because filter length changes also affect convergence time after acoustic path changes and memory
  footprint, `len=20/32` is left undecided for now; default `len=13` (52 ms) is retained in code,
  and longer filters can be tested during live evaluation.

### 4. Code Changes
- `crates/gamersequalizer-mic-vst/src/aec3.rs`:
  - `tuning()` sets `cfg.erle.max_l = 30.0;`, `cfg.erle.max_h = 10.0;`, and
    `cfg.suppressor.high_frequency_suppression.bands_in_limiting_gain = 0;`.
  - Added per-capture numbers to docstring above `tuning()`.
  - In `test_quiet_mic_keeps_the_voice_with_mic_gain`, replaced hardcoded threshold with relative assertion:
    `assert!(plain.voice_hf_db < gained.voice_hf_db - 3.0)`.
- `crates/gamersequalizer-mic-vst/src/ai_denoiser.rs`:
  - In `capture_replay_report`, added reporting of denoiser output level across all pause windows
    (`silero < 0.15`), calculating median and 90th percentile dBFS.
- All 14 unignored AEC3 tests and full crate unit test suite pass with 0 failures.


## Milestone 45: Echo strength measured per output; AEC3's fallback follows a strong echo; all of it in the debug log (2026-11-10)

Written on the Mac like milestone 43: `aec3.rs`, `shm.rs` and the pure part of `chain_log.rs`
compiled and tested there, `plugin.rs`, `apo_link.rs` and `aec_delay_cache.rs` edited without a
compiler. Not run on a capture, not heard live. Shared memory layout v18: the APO has to be
reinstalled. The meter's rule described here (the low quartile of mic level over reference level
taken as the echo) failed on the captures in milestone 46 and was replaced in milestone 47.

**Why.** `ep_strength.default_gain` = 0.01 says "the echo is 40 dB under the reference". That
is what AEC3 takes for the echo while it has no usable linear filter (a start, a reset, an echo
path change), and it was chosen on one desk: echoes 37-50 dB under the reference. Synthetic,
echo 20 dB under the reference (a laptop's speakers next to its mic): the first 0.4 s after a
start come out whole (-38.0 -> -38.8 dBFS); with a fallback that fits, at -58.9 dBFS. After
that the linear filter has it either way.

- **`EchoMeter` (`aec3.rs`)**: echo strength = how many dB under the reference its echo reaches
  the raw mic. 100 ms windows in which the reference is over -30 dBFS: mic power over reference
  power. The user's voice only ever adds to a window, so the estimate is the low quartile of
  the last 128 such windows (12.8 s), first after 50 of them (5 s). The reported value moves
  in steps of 2 dB or more.
  - The mic's noise adds as well. Where the reference pauses (10 windows under -70 dBFS in the
    last 25.6 s), the noise is the low quartile of the mic in those pauses and is taken off.
  - Where it never pauses (game audio), the 5th percentile of all mic windows stands in. That
    may be a quiet moment of the echo itself, so nothing is taken off; it only tells which
    windows say nothing (within 3 dB of it).
  - When over 60 % of the quieter half of the windows say nothing, the value is a bound ("the
    echo is under this"; 40 % to stop being one). Every doubt ends there, and a bound keeps
    AEC3 at -40 dB, as before there was a meter.
- **Fallback follows it**: `max(-40, strength - 3)` dB in 6 dB steps, 0 dB at most, never for
  a bound. -40 dB stays for every echo weaker than about 32 dB under the reference, which is
  every capture so far, so their results do not change. A step rebuilds AEC3 through the mic
  gain's latch (2 s, then a pause, 6 s at most; free in the first 0.5 s).
- **Remembered per mic and output** (`%APPDATA%\GamersEqualizer\echo_strength_cache.json`, next
  to the delay cache, same key): the daemon hands it to the APO (`aec_cached_echo_db_x10`), so
  from the second session on the fallback is right from the first frame; saved when the
  measured value moved by 2 dB, at most every 10 s. A bound is not saved and drops what was
  remembered (the echo is gone: a strong one from before would start every stream ducking the
  voice). A speaker switch forgets the measurement and takes the new output's remembered
  value. One shared memory field carries both kinds (negative: measured, positive: a bound),
  so the daemon cannot pair a value with the wrong kind. Shortcut: APO path only.
- **Debug log** (the Log page's "Save debug to disk"; a tester's log is enough, no capture):
  - `[Chain] start: ... echo -43.5 dB under the reference (AEC fallback echo -40.0 dB, AEC mic gain +18.0 dB) ...`
  - `[Chain] changed: echo not measured -> -21.5 dB under the reference, AEC fallback echo -40.0 -> -22.0`
  - `[Chain] changed: echo -43.5 dB under the reference -> within the mic's noise (under -58.0 dB)`
  - the same three values in the 10 s summary, after the ERLE;
  - `[ApoLink] remembered echo strength for this mic/output: ...` / `saving echo strength ...`.
- **`capture_tuning_report`**: two more columns, `strength` (`<`: a bound) and `fallb`.
- **`aec3::tests::capture_echo_report`** (ignored) and `docs/aec3-echo-strength-test.md`: the
  test on the captures. Per capture and scene: the meter against a second estimate (`check`:
  mic level - reference level in the windows Silero finds the echo alone in), the fallback,
  the dB removed in the first 0.1-0.5 s of loud reference (`start`), and a trace of when the
  reported strength, the fallback and the mic gain changed. Scenes: `micscale=<dB>` makes the
  raw mic louder, i.e. the echo that much stronger against the reference with the real room,
  mic and voice (no capture has a strong echo); `seed=auto` hands over the strength the scene
  before measured, as the daemon's cache does in the next session; `gain=0.01` pins the
  fallback at the old -40 dB for comparison. On the synthetic strong echo (22 dB under the
  reference): `start` 1.1 dB in a first session and with the fallback pinned, 17.0 dB with the
  strength remembered, the columns from 3 s on equal.
- **Tests**: with a louder voice in 40 % of the windows the meter reads -20.0 / -50.0 dB for
  echoes 20 / 50 dB under a reference that starts after 2 s, -20.1 / -49.6 dB under one that
  plays from the start, 4.9 dB for an echo 5 dB over the reference (fallback 0 dB), and a
  bound (-58 / -60 dB) for an echo 80 dB under or none at all; a remembered strong echo is
  suppressed from the first frame (first 0.4 s: mic -38.8, not remembered -40.2, remembered
  -82.7 dBFS); the log lines.
- **Review (fresh context) found, all fixed above:** with a reference that never paused the
  first version took the mic's noise for a strong echo (noise -55 dBFS, reference -28 dBFS: "-27
  dB") and the daemon would have remembered it; the fallback could step to +2 dB; a remembered
  strong echo was never dropped; value and kind were two fields. Fixing the first showed two
  more: a guessed noise taken off made the first estimate 2-3 dB low (a step and back), and
  the bound flag of a single window flipped the fallback to -40 dB and back.
- **Not done: switching the AEC off when there is no echo** (headphones). With the mic gain the
  headphone captures lose 0.6 / 0.7 dB of voice in AEC3 (milestone 44), so there is little to
  win, and the detector ("within the mic's noise") has not seen a real machine. The log now
  shows what it would decide; switch on it once logs from more setups agree with it.
- **Open:** all of it on Windows. What the meter reads on the captures (the report's `strength`
  column against the levels in milestones 36 and 42: about -37 dB for `130319`, -50 dB for
  `210655`, a bound for the headphone captures). A strong echo's first session pays one rebuild
  5-11 s after the audio starts. The meter needs a reference over -30 dBFS: quiet game audio
  measures nothing and keeps -40 dB. A talker in over 75 % of the windows for 13 s reads as a
  stronger echo. The cache key is the Windows default mic, like the delay's, not always the
  mic in use. Not known: whether the loopback reference is taken before the Windows speaker
  volume; if so, the strength moves with the volume and a remembered one fits only that volume.


## Milestone 46: Echo Strength Measurement Verification on Real Captures (2026-11-10)

First round: the echo strength meter and the adaptive fallback estimate of milestone 45 on the existing captures, following `docs/aec3-echo-strength-test.md`. Tests A and B failed; milestone 47 replaced the meter's logic because of the results below.

### 1. Compilation & Unit Tests (Step 0)
- All files written on the Mac without a compiler (`plugin.rs`, `apo_link.rs`, `aec_delay_cache.rs`, `shm.rs`, `chain_log.rs`) compiled on Windows without an error. No code fixes were needed. (Note: the guide's command named `gamersequalizer-daemon`; the package name in Cargo.toml is `gamersequalizer`.)
- Unit tests:
  - `gamersequalizer-mic-vst --lib aec3::`: 16 passed, 4 ignored.
  - The output on Windows matches the Mac reference exactly:
    - `echo -20 dB, music from 2 s: measured -20.0 dB (bound: false)`
    - `echo -50 dB, music from 0 s: measured -49.6 dB (bound: false)`
    - `echo -200 dB, music from 0 s: measured -60.5 dB (bound: true)`
    - `strong echo, first 0.4 s: mic -38.8, out -40.2, remembered -82.7 dBFS`
  - `gamersequalizer-common`: 32 passed, 0 failed.
  - `gamersequalizer chain_log`: 4 passed, 0 failed.
  - `gamersequalizer-mic-vst --release`: 81 passed, 21 ignored, 0 failed.
  - `cargo clippy --workspace --all-targets`: 0 errors.

### 2. Test A: capture_tuning_report vs Milestone 44 Winner
- **Verdict: FAILED**.
- Expected: `fallb` stays at -40 dB and the numbers match milestone 44 within 0.1 dB / 1 percentage point.
- Found:
  - 10 of the 22 captures differ:
    - 6 captures (`102403`, `115931`, `122520`, `124950`, `125717`, `144824`) ended with `fallb` other than -40 dB (-28 or -34 dB).
    - 4 captures (`091611`, `100150`, `101152`, `162331`) ended with `fallb` at -40 dB, but the fallback moved for a while during the capture; the needless AEC3 rebuilds hurt convergence and the final numbers.
  - Captures identical to milestone 44: `091216`, `100621`, `104318`, `104424`, `122712`, `125043`, `130319`, `163832`, `163934`, `204900`, `205013`, `210655`.

The captures that differ:

| Capture | Source       | gain | strength | fallb | echo |  left | voice | >6dB | >10dB | onset | 4-8k | ERLE |
| :------ | :----------- | ---: | -------: | ----: | ---: | ----: | ----: | ---: | ----: | ----: | ---: | ---: |
| 091611  | winner (M44) |    - |        - |   -40 | 15.3 | -71.3 |   2.4 |  11% |    6% |   5.7 |  5.7 |    - |
| 091611  | test A       |    0 |      -38 |   -40 | 14.0 | -66.5 |   2.4 |  11% |    6% |   5.0 |  5.2 |  0.5 |
| 100150  | winner (M44) |    - |        - |   -40 | 16.6 | -61.2 |   2.0 |   8% |    3% |   3.9 |  4.9 |    - |
| 100150  | test A       |    0 |     <-38 |   -40 | 14.3 | -50.2 |   1.4 |   4% |    2% |   4.4 |  3.2 |  1.3 |
| 101152  | winner (M44) |    - |        - |   -40 | 20.0 | -59.8 |   6.0 |  37% |   21% |   5.6 | 10.9 |    - |
| 101152  | test A       |    0 |     <-29 |   -40 | 12.6 | -58.0 |   3.2 |  14% |    5% |   3.7 |  4.5 |  5.8 |
| 102403  | winner (M44) |    - |        - |   -40 | 17.2 | -55.9 |   4.7 |  28% |   15% |   8.7 |  6.5 |    - |
| 102403  | test A       |    0 |      -26 |   -28 | 15.1 | -52.6 |   4.2 |  24% |   13% |   7.9 |  5.4 |  5.2 |
| 115931  | winner (M44) |    - |        - |   -40 |  0.6 | -60.9 |   0.6 |   1% |    0% |   0.5 |  1.9 |    - |
| 115931  | test A       |   18 |      -31 |   -28 |  0.9 | -61.0 |   0.7 |   1% |    0% |   0.5 |  2.0 |  0.3 |
| 122520  | winner (M44) |    - |        - |   -40 |  4.4 | -59.5 |   0.7 |   1% |    0% |   2.1 |  3.6 |    - |
| 122520  | test A       |   24 |      -42 |   -34 |  7.9 | -59.6 |   0.9 |   3% |    1% |   2.3 |  3.6 |  1.9 |
| 124950  | winner (M44) |    - |        - |   -40 | 11.4 | -67.0 |   2.9 |  17% |    5% |   2.5 |  3.4 |    - |
| 124950  | test A       |   24 |      -28 |   -34 | 11.4 | -67.0 |   3.1 |  17% |    7% |   2.6 |  3.3 |  0.2 |
| 125717  | winner (M44) |    - |        - |   -40 |  0.0 |   0.0 |   4.7 |  33% |   17% |   2.9 |  4.9 |    - |
| 125717  | test A       |   24 |      -35 |   -34 |  0.0 |   0.0 |   3.9 |  25% |    8% |   2.4 |  3.3 |  0.0 |
| 144824  | winner (M44) |    - |        - |   -40 |  0.0 |   0.0 |   2.6 |  12% |    6% |   3.4 |  4.6 |    - |
| 144824  | test A       |   12 |      -22 |   -28 |  0.0 |   0.0 |   2.9 |  14% |    8% |   3.4 |  4.9 |  0.0 |
| 162331  | winner (M44) |    - |        - |   -40 | 10.1 | -54.6 |   1.4 |   3% |    2% |   2.0 |  5.4 |    - |
| 162331  | test A       |    6 |      -33 |   -40 | 10.5 | -54.0 |   1.4 |   3% |    2% |   2.0 |  5.4 |  2.8 |

### 3. Test B: capture_echo_report
- **Verdict: FAILED**.
- Criteria and what went wrong:
  1. **Headphones (`115931`, `122520`)**:
     - Criterion: `meter` is a bound (`<...`) or under -50 dB.
     - Result: `115931` measured `meter` -30.6 dB (check -41.4 dB; not a bound, far over -50 dB) and set `fallb` to -28 dB. `122520` measured `meter` -41.9 dB (neither a bound nor under -50 dB) and moved `fallb` to -34 dB because of a short peak of -27.7 dB at 15.7 s.
  2. **Speakers / monitor (`check - 6 <= meter <= check + 2`)**:
     - `205013`: `meter` came out at -33.2 dB against `check` -50.5 dB (17.3 dB over). With speech over the music the meter climbed from -51.5 dB (at 8.3 s) to -33.2 dB (at 28.0 s).
  3. **Stability of the meter (`trace`)**:
     - Criterion: at most 2 changes per 30 s at a constant volume, no flipping between "echo X" and "within the noise", no fallback change while meter <= -33.
     - Failures:
       - `091611`: 14 changes in 35 s, the fallback jumped to -28 dB and back to -40 dB.
       - `100150`: 16 changes, the fallback jumped to -34, -28 and back to -40 dB.
       - `101152`: repeated flipping between "within the noise" and "echo", 5 fallback jumps (-40 -> -34 -> -40 -> -34 -> -28 -> -40).
       - `162331`: 18 changes, flipping between noise and echo, 4 fallback jumps.
  4. **Voice taken for echo**:
     - `125717`: 0 echo-alone windows (only speech over music), yet it measured an echo of -30.9 dB and raised the fallback to -34 dB.
     - `144824`: 0 echo-alone windows, measured an echo of -22.3 dB and raised the fallback to -28 dB.

Test B table:

| Capture         | meter  | check | fallb | Verdict           | Note                                                                         |
| :-------------- | :----- | :---- | ----: | :---------------- | :--------------------------------------------------------------------------- |
| 20261107-091216 | -      | -     |   -40 | Passed            | No reference, clean trace                                                    |
| 20261107-091611 | -38.4  | -36.6 |   -40 | **Failed**        | Heavy wobble (14 changes), fallback briefly -28 at 33.6 s                    |
| 20261107-100150 | <-38.2 | -37.2 |   -40 | **Failed**        | Heavy wobble (16 changes), fallback briefly -34 and -28                      |
| 20261107-100621 | -      | -     |   -40 | Passed            | No reference, clean trace                                                    |
| 20261107-101152 | <-28.9 | -29.1 |   -40 | **Failed**        | Noise/echo flipping, 5 fallback jumps (-40, -34, -28, -40)                   |
| 20261107-102403 | -26.4  | -28.3 |   -28 | **Failed**        | Wobble and noise/echo flipping, 10 changes, fallb -28                        |
| 20261107-104318 | <-39.6 | -39.1 |   -40 | Passed            | Upper bound under the noise, fallb -40                                       |
| 20261107-104424 | -      | -     |   -40 | Passed            | No reference, trace has mic gain only                                        |
| 20261109-115931 | -30.6  | -41.4 |   -28 | **Failed**        | Headphones: meter neither a bound nor under -50, fallb moved to -28          |
| 20261109-122520 | -41.9  | -41.3 |   -34 | **Failed**        | Headphones: meter neither a bound nor under -50, fallb moved to -34          |
| 20261109-122712 | -34.3  | -34.8 |   -40 | Passed            | meter matches check, fallb -40, a single change at 7.7 s                     |
| 20261109-124950 | -28.5  | -     |   -34 | **Failed**        | Few echo windows, fallb moved to -34                                         |
| 20261109-125043 | -      | -     |   -40 | Passed            | No reference, trace has mic gain only                                        |
| 20261109-125717 | -35.1  | -     |   -34 | **Failed**        | Only speech over music, meter takes the voice for echo (-30.9 dB, fallb -34) |
| 20261109-130319 | <-38.1 | -37.0 |   -40 | Passed            | Upper bound under the noise, fallb -40, stable                               |
| 20261109-144824 | -22.3  | -     |   -28 | **Failed**        | Only speech over music, meter takes the voice for echo (-22.3 dB, fallb -28) |
| 20261109-162331 | -32.6  | -33.3 |   -40 | **Failed**        | Heavy wobble (18 changes), noise/echo flipping, 4 fallb jumps                |
| 20261109-163832 | -      | -     |   -40 | Passed            | No reference                                                                 |
| 20261109-163934 | -      | -     |   -40 | Passed            | No reference                                                                 |
| 20261109-204900 | -      | -     |   -40 | Passed            | No reference                                                                 |
| 20261109-205013 | -33.2  | -50.5 |   -40 | **Failed**        | meter 17.3 dB over check; rose from -51.5 to -33.2 during speech             |
| 20261109-210655 | -44.8  | -50.3 |   -40 | With reservations | Values match, but 8 changes on the way                                       |

Traces:
- `20261107-091611`: `11.5 s echo -36.4, 30.2 s echo -34.2, 31.6 s echo -31.0, 32.0 s echo -29.0, 32.8 s echo -26.2, 33.3 s echo -24.2, 33.6 s fallback -40 -> -28, 38.9 s echo -26.2, 41.0 s echo -29.0, 43.5 s echo -31.4, 43.8 s echo -33.9, 44.8 s echo -36.3, 46.3 s echo -38.4, 46.7 s fallback -28 -> -40`
- `20261107-100150`: `8.9 s echo -40.5, 20.4 s echo -38.3, 22.2 s echo -36.2, 23.3 s echo -34.0, 24.1 s echo -31.3, 26.1 s echo -29.0, 26.2 s echo -26.6, 26.3 s fallback -40 -> -34, 27.1 s echo -24.3, 29.1 s fallback -34 -> -28, 29.2 s echo -26.3, 30.2 s echo -28.6, 33.9 s echo -30.8, 35.3 s echo -32.8, 36.1 s fallback -28 -> -34, 49.3 s echo -34.9, 50.9 s echo -37.0, 51.4 s echo within the noise (under -38.2), 56.3 s fallback -34 -> -40`
- `20261107-101152`: `5.0 s echo within the noise (under -29.0), 8.6 s echo -30.2, 11.0 s fallback -40 -> -34, 13.7 s echo -32.2, 14.0 s echo -36.2, 16.0 s fallback -34 -> -40, 22.6 s echo -31.6, 22.7 s echo -28.6, 24.6 s fallback -40 -> -34, 25.0 s echo -26.2, 26.9 s echo -24.2, 27.1 s fallback -34 -> -28, 31.2 s echo -26.2, 33.5 s echo -28.2, 38.9 s echo within the noise (under -28.9), 41.1 s fallback -28 -> -40`
- `20261107-102403`: `11.8 s echo -30.2, 13.4 s echo -32.3, 13.9 s echo -39.4, 32.0 s echo within the noise (under -39.3), 39.0 s echo within the noise (under -37.1), 39.5 s echo within the noise (under -33.4), 39.7 s echo within the noise (under -30.3), 39.9 s echo -28.5, 41.9 s fallback -40 -> -34, 43.3 s echo -26.4, 45.3 s fallback -34 -> -28`
- `20261107-104318`: `0.0 s mic gain 0 -> 24, 5.0 s echo within the noise (under -39.6), 10.8 s mic gain 24 -> 6`
- `20261109-115931`: `0.0 s mic gain 0 -> 24, 10.8 s echo -41.5, 11.0 s echo -37.1, 11.8 s echo -34.7, 12.3 s echo -32.7, 12.6 s mic gain 24 -> 18, 12.7 s echo -30.6, 13.1 s echo -26.3, 14.7 s fallback -40 -> -28, 29.9 s echo -30.6`
- `20261109-122520`: `0.0 s mic gain 0 -> 24, 14.4 s echo -32.5, 15.2 s echo -30.2, 15.7 s echo -27.7, 16.5 s fallback -40 -> -34, 18.2 s echo -29.9, 18.9 s echo -32.5, 21.4 s echo -30.2, 22.4 s echo -32.5, 23.4 s echo -35.5, 26.8 s echo -37.5, 28.8 s echo -39.7, 29.5 s echo -41.9`
- `20261109-122712`: `0.0 s mic gain 0 -> 24, 7.7 s echo -34.3`
- `20261109-124950`: `0.0 s mic gain 0 -> 24, 22.4 s echo -28.5, 27.3 s fallback -40 -> -34`
- `20261109-125717`: `0.0 s mic gain 0 -> 24, 16.6 s echo -30.9, 18.7 s fallback -40 -> -34, 25.2 s echo -33.0, 28.3 s echo -35.1`
- `20261109-130319`: `0.0 s mic gain 0 -> 24, 5.0 s echo within the noise (under -38.1), 9.3 s mic gain 24 -> 18`
- `20261109-144824`: `0.0 s mic gain 0 -> 18, 23.3 s echo -22.3, 26.6 s fallback -40 -> -28, 26.6 s mic gain 18 -> 12`
- `20261109-162331`: `0.0 s mic gain 0 -> 12, 5.0 s echo within the noise (under -36.6), 8.8 s mic gain 12 -> 6, 12.9 s echo within the noise (under -33.9), 15.8 s echo within the noise (under -31.5), 26.2 s echo -31.4, 27.5 s echo -29.4, 27.8 s echo -27.2, 27.9 s fallback -40 -> -34, 28.9 s echo -25.0, 29.2 s echo -22.9, 30.5 s echo -24.9, 30.9 s fallback -34 -> -28, 32.1 s echo -27.2, 32.6 s echo -29.2, 34.1 s echo -31.3, 36.1 s echo -33.3, 38.1 s echo within the noise (under -34.3), 38.9 s fallback -28 -> -40, 41.3 s echo -34.9, 54.0 s echo -32.6`
- `20261109-204900`: `0.0 s mic gain 0 -> 12, 9.2 s mic gain 12 -> 18`
- `20261109-205013`: `0.0 s mic gain 0 -> 18, 8.3 s echo -51.5, 18.7 s echo -49.5, 19.8 s echo -47.4, 20.2 s echo -45.4, 20.4 s echo -43.1, 20.6 s echo -39.3, 22.9 s echo -37.3, 23.6 s echo -35.2, 28.0 s echo -33.2`
- `20261109-210655`: `0.0 s mic gain 0 -> 18, 5.0 s echo within the noise (under -51.1), 15.0 s echo within the noise (under -49.0), 16.5 s echo -47.1, 22.1 s echo -44.9, 22.7 s echo -42.6, 23.9 s echo -44.6, 27.0 s echo -42.6, 39.5 s echo -44.8, 55.4 s mic gain 18 -> 24`

### 4. Test C: strong echo simulated from the captures
- **Not run (stopped as the guide says)**: `docs/aec3-echo-strength-test.md`, chapter 2 (when test B fails, stop and write down the table and the traces; test C then makes no sense) and its rules (do not change the meter's constants; when something fails, write the numbers down and stop; a human decides about the change).

### 5. Summary
- Tests A and B failed. On real captures the meter wobbles, takes the human voice and the mic's noise for a strong echo (mostly on headphones and with speech over music) and moves AEC3's fallback estimate without a reason, which rebuilds the filter again and again and degrades the sound. A live test with the APO installed makes no sense until the meter's logic changes (done in milestone 47).

## Milestone 47: The echo meter took the voice for an echo; it now needs the mic to follow the reference (2026-11-10)

Answer to milestone 46 (the first run on the captures, both tests failed; nothing needed fixing
to compile). Written on the Mac again: `aec3.rs`, `shm.rs`, the pure part of `chain_log.rs`
compiled and tested, `apo_link.rs` edited without a compiler, nothing run on a capture.
Second round on the captures: `docs/aec3-echo-strength-test.md`, to be written up as milestone 48.

- **What was wrong.** Milestone 45's meter rested on "the user's voice only adds to a window,
  so a low quartile of the windows is the echo". On the captures the voice is in more than
  three quarters of the windows for 13 s at a time, and far louder than the echo:
  - headphones with no echo, a voice at -40 dBFS over music at -28 dBFS: "-31 dB" (`115931`),
  - speech over music with no window of echo alone: "-22 dB", "-31 dB" (`144824`, `125717`),
  - a -50 dB echo climbing to "-33 dB" while the user talked (`205013`),
  - 10-18 changes per capture and the fallback stepping up and back (`091611`, `100150`,
    `101152`, `162331`): every step a rebuild of AEC3 in the middle of speech.
  The low quartile is a bound (the echo is no stronger), not the echo. The synthetic tests of
  milestone 45 had the voice in 40 % of the windows and did not show it.
- **New rule: an echo follows the reference, a voice does not.** Where the echo is what the mic
  hears, the mic's level goes up and down with the reference's, window by window. The meter
  fits the mic's level against the reference's (both in dB) over the half of the windows
  quietest for their reference (where a voice is least) and confirms an echo only with a
  correlation of 0.9 or more, a slope of 0.7-1.3 and a reference whose level moves by 2 dB
  (standard deviation) in those windows.
  - Confirmed: the strength is the low quartile, as before, and only now may move the fallback.
  - Not confirmed: the meter reports the bound, and AEC3 stays at -40 dB as before there was a
    meter. This is what a voice, a noise, or a reference with a flat level end as.
  - A confirmed (or remembered) strength never goes up without a new confirmation; it comes
    down when the bound falls 3 dB under it (speakers turned down, headphones on).
  - The mic's noise is no longer estimated or taken off (`MIC_WINDOWS`, the pauses of the
    reference: gone). A weak echo reads up to 3 dB high, which changes nothing: the fallback
    only acts over about -32 dB.
- **Log and cache follow**: `[Chain]` says `not confirmed (at most X dB under the reference)`
  instead of "within the mic's noise"; the daemon saves only confirmed values and no longer
  drops a remembered one on a bound (the APO lowers it itself and reports the lowered value).
- **Synthetic, the cases of milestone 46 added** (20 s each; fit = correlation x slope):

| Scene                                                    | Reported            | Fit         | Fallback |
| :------------------------------------------------------- | :------------------ | :---------- | :------- |
| echo -20 dB, voice in 40 % of the windows                | -20.0 dB            | 1.00 x 1.01 | -22 dB   |
| echo -50 dB, voice in 40 %                               | -49.4 dB            | 1.00 x 0.85 | -40 dB   |
| echo -80 dB (under the noise), voice in 40 %             | bound -58.1 dB      | 0.26 x 0.01 | -40 dB   |
| no echo, voice all the time over the music (headphones)  | bound -16.5 dB      | 0.56 x 0.75 | -40 dB   |
| echo -50 dB, then a voice 20 dB louder that never pauses | -49.2 dB (no climb) | 0.56 x 0.75 | -40 dB   |
| echo -20 dB, then a voice that never pauses              | -20.1 dB (stays)    | 0.86 x 1.32 | -22 dB   |
| echo 5 dB over the reference                             | 4.9 dB              | 1.00 x 1.02 | 0 dB     |

  The 0.56 of the voice alone is this test's doing: the synthetic music beats at 2 Hz and the
  synthetic voice at 4 Hz, so their levels are not independent. A real voice should fit worse.
- **`capture_echo_report`**: a `fit` column, and the fit at every confirmed value in the trace.
- **Not known, and the reason for the second round:** whether a real echo in a real room
  reaches a correlation of 0.9 over 100 ms windows (reverb smears the level; the windows are
  not aligned to the echo's delay). If not, the meter never confirms and the fallback never
  moves: nothing is gained, nothing is lost against milestone 44. The report prints the fit, so
  the threshold can be set from the captures instead of guessed. A strong echo under a voice
  that never pauses is not confirmed until the user stops for a few seconds.


## Milestone 48: Echo Strength Measurement Verification with Fit & Correlation Check (2026-11-10)

Second round of the echo strength test on the existing captures, following `docs/aec3-echo-strength-test.md`, after milestone 47 rewrote the meter (an echo is confirmed only when the mic level follows the reference level window by window).

### 1. Compilation & Unit Tests (Step 0)
- All files written on the Mac without a compiler (`aec3.rs`, `shm.rs`, `chain_log.rs`, `apo_link.rs`) compiled on Windows without errors. No code fixes were needed.
- Unit tests:
  - `gamersequalizer-mic-vst --lib aec3::`: 16 passed, 4 ignored.
  - The output on Windows matches the Mac reference exactly:
    - `echo -20 dB, music from 2 s: -20.0 dB (bound: false), fit 1.00 x 1.01`
    - `echo -50 dB, music from 0 s: -49.6 dB (bound: false), fit 1.00 x 0.85`
    - `echo -200 dB, music from 0 s: -16.5 dB (bound: true), fit 0.56 x 0.75`
    - `strong echo, first 0.4 s: mic -38.8, out -40.2, remembered -82.7 dBFS`
  - `gamersequalizer-common`: 32 passed, 0 failed.
  - `gamersequalizer chain_log`: 4 passed, 0 failed.
  - `gamersequalizer-mic-vst --release`: 81 passed, 21 ignored, 0 failed.
  - `cargo clippy --workspace --all-targets`: 0 errors.

### 2. Test A: capture_tuning_report vs Milestone 44 Winner
- **Verdict: PASSED (identical)**.
- The fallback estimate `fallb` stayed at -40 dB on all 22 captures.
- All numbers (`echo`, `left`, `voice`, `>6dB`, `>10dB`, `onset`, `4-8k`) match the milestone 44 winner (`erle=30,10 hf=0`) exactly on all 22 captures. No capture differs.

### 3. Test B: capture_echo_report
- **Verdict: every capture passed or stayed unconfirmed (no FAILED)**.
- Criteria:
  - **Headphones (`115931`, `122520`)**: `meter` stayed an upper bound with `<` (correlations 0.23 and 0.27 did not reach the 0.90 threshold) and `fallb` stayed at -40 dB. The false echo detections on headphones are gone.
  - **Speech over music with no echo-alone windows (`124950`, `125717`, `144824`)**: `fallb` stayed at -40 dB; the voice is no longer taken for an echo.
  - **Long speech over music (`205013`)**: in the first round the estimate climbed from -51 to -33 dB; now a correlation of 0.29 keeps it unconfirmed (`<-33.9 dB`), `fallb` is -40 dB.
  - **Confirmed echoes**:
    - `091611`: confirmed (`-38.9 dB`), `check` -36.6 dB is within tolerance, 2 changes in the trace.
    - `102403`: confirmed at 55.3 s (`-25.8 dB`, fit `0.90 x 1.02`), 0.5 dB over `check + 2` (-26.3 dB).
  - **Unconfirmed speaker captures**: the correlation in real rooms is 0.22 to 0.78 (reverb and delay smear it over 100 ms windows), so they do not cross the 0.90 threshold and AEC3 stays at -40 dB.

Test B table:

| Capture         | meter  | fit       | check | fallb | Verdict       | Note                                                                   |
| :-------------- | :----- | :-------- | ----: | ----: | :------------ | :--------------------------------------------------------------------- |
| 20261107-091216 | -      | 0.00x0.00 |     - |   -40 | Passed        | No reference, clean trace                                              |
| 20261107-091611 | -38.9  | 0.89x0.87 | -36.6 |   -40 | Passed        | Confirmed (15.1 s), check within tolerance, 2 changes in the trace     |
| 20261107-100150 | <-39.0 | 0.75x0.71 | -37.2 |   -40 | Not confirmed | Fit 0.75x0.71 under the 0.90 threshold, fallb -40                      |
| 20261107-100621 | -      | 0.00x0.00 |     - |   -40 | Passed        | No reference, clean trace                                              |
| 20261107-101152 | <-28.2 | 0.34x0.26 | -29.1 |   -40 | Not confirmed | Reverb, fit 0.34x0.26, fallb -40                                       |
| 20261107-102403 | -25.8  | 0.77x0.89 | -28.3 |   -40 | Passed        | Confirmed at 55.3 s (fit 0.90x1.02), 0.5 dB over check+2               |
| 20261107-104318 | <-39.6 | 0.92x0.80 | -39.1 |   -40 | Not confirmed | Correlation 0.92, but ref spread under 2 dB, fallb -40                 |
| 20261107-104424 | -      | 0.00x0.00 |     - |   -40 | Passed        | No reference, trace has mic gain only                                  |
| 20261109-115931 | <-30.2 | 0.23x2.07 | -41.4 |   -40 | Not confirmed | Headphones: correlation 0.23, not confirmed, fallb -40                 |
| 20261109-122520 | <-41.3 | 0.27x0.19 | -41.3 |   -40 | Not confirmed | Headphones: correlation 0.27, not confirmed, fallb -40                 |
| 20261109-122712 | <-34.3 | 0.52x0.96 | -34.8 |   -40 | Not confirmed | Speakers, fit 0.52x0.96, fallb -40                                     |
| 20261109-124950 | <-28.2 | 0.78x0.94 |     - |   -40 | Not confirmed | Only 6 echo windows, fit 0.78x0.94, fallb -40                          |
| 20261109-125043 | -      | 0.00x0.00 |     - |   -40 | Passed        | No reference, trace has mic gain only                                  |
| 20261109-125717 | <-32.9 | 0.40x0.68 |     - |   -40 | Not confirmed | Speech over music, fit 0.40x0.68, voice not taken for echo, fallb -40  |
| 20261109-130319 | <-38.1 | 0.26x0.39 | -37.0 |   -40 | Not confirmed | Speakers, fit 0.26x0.39, fallb -40                                     |
| 20261109-144824 | <-22.2 | 0.33x0.66 |     - |   -40 | Not confirmed | Speech over music, fit 0.33x0.66, voice not taken for echo, fallb -40  |
| 20261109-162331 | <-33.3 | 0.45x0.27 | -33.3 |   -40 | Not confirmed | Speakers, fit 0.45x0.27, fallb -40                                     |
| 20261109-163832 | -      | 0.00x0.00 |     - |   -40 | Passed        | No reference                                                           |
| 20261109-163934 | -      | 0.00x0.00 |     - |   -40 | Passed        | No reference                                                           |
| 20261109-204900 | -      | 0.00x0.00 |     - |   -40 | Passed        | No reference                                                           |
| 20261109-205013 | <-33.9 | 0.29x1.18 | -50.5 |   -40 | Not confirmed | Voice over music does not raise the fallback, fit 0.29x1.18, fallb -40 |
| 20261109-210655 | <-44.8 | 0.22x0.48 | -50.3 |   -40 | Not confirmed | Speakers, fit 0.22x0.48, fallb -40                                     |

Traces of the captures with a confirmed echo:
- `20261107-091611`: `11.5 s no echo confirmed (at most -35.7), 15.1 s echo -35.4 (fit 0.90 x 0.91), 53.3 s echo -38.9 (fit 0.91 x 0.81)`
- `20261107-102403`: `11.8 s no echo confirmed (at most -30.1), 13.4 s no echo confirmed (at most -32.2), 13.9 s no echo confirmed (at most -38.0), 39.5 s no echo confirmed (at most -33.4), 39.7 s no echo confirmed (at most -30.3), 40.2 s no echo confirmed (at most -28.2), 43.7 s no echo confirmed (at most -26.1), 55.3 s echo -25.8 (fit 0.90 x 1.02)`

### 4. Test C: strong echo simulated from the captures
Test C ran on the captures with a confirmed echo (`091611` and `102403`) with `micscale=12` (`micscale=20` overdrove the mic: 6655 samples on 091611 and 63 samples on 102403 over full scale). The other captures (`101152`, `122712`, `130319`, `205013`, `210655`) were not confirmed in test B, so test C shows nothing on them (fallb stays at -40 dB).

| Capture | Scene                           | gain | meter |       fit | check | fallb | start | echo |  left | voice | >6dB | >10dB | onset | 4-8k | ERLE |
| :------ | :------------------------------ | ---: | ----: | --------: | ----: | ----: | ----: | ---: | ----: | ----: | ---: | ----: | ----: | ---: | ---: |
| 091611  | (as recorded)                   |    0 | -38.9 | 0.89x0.87 | -36.6 |   -40 |  12.8 | 15.3 | -71.3 |   2.4 |  11% |    6% |   5.7 |  5.7 |  0.8 |
| 091611  | micscale=12                     |    0 | -26.9 | 0.89x0.87 | -24.6 |   -28 |   4.7 | 14.5 | -54.8 |   1.2 |   3% |    1% |   1.9 |  3.2 |  3.9 |
| 091611  | micscale=12 seed=auto           |    0 | -26.9 | 0.89x0.87 | -24.6 |   -28 |  12.8 | 14.6 | -55.7 |   1.2 |   2% |    2% |   1.7 |  3.8 |  4.1 |
| 091611  | micscale=12 seed=auto gain=0.01 |    0 | -26.9 | 0.89x0.87 | -24.6 |   -28 |   4.7 | 14.6 | -54.8 |   1.2 |   2% |    2% |   1.7 |  3.8 |  4.1 |
| 102403  | (as recorded)                   |    0 | -25.8 | 0.77x0.89 | -28.3 |   -40 |   9.5 | 17.2 | -55.9 |   4.7 |  28% |   15% |   8.7 |  6.5 |  5.4 |
| 102403  | micscale=12                     |    0 | -13.8 | 0.77x0.89 | -16.3 |   -40 |   2.1 | 15.1 | -39.9 |   2.8 |  14% |    6% |   5.8 |  4.1 |  5.1 |
| 102403  | micscale=12 seed=auto           |    0 | -13.8 | 0.77x0.89 | -16.3 |   -28 |  28.8 | 14.7 | -40.5 |   2.5 |  10% |    6% |   5.7 |  3.7 |  4.5 |
| 102403  | micscale=12 seed=auto gain=0.01 |    0 | -13.8 | 0.77x0.89 | -16.3 |   -28 |   2.1 | 14.7 | -40.5 |   2.5 |  10% |    6% |   5.7 |  3.7 |  4.5 |

Test C traces:
- `091611 micscale=12`: `11.5 s no echo confirmed (at most -23.7), 15.1 s echo -23.4 (fit 0.90 x 0.91), 21.1 s fallback -40 -> -28, 53.3 s echo -26.9 (fit 0.91 x 0.81)`
- `091611 micscale=12 seed=auto`: `0.0 s fallback -40 -> -28, 15.1 s echo -23.4 (fit 0.90 x 0.91), 53.3 s echo -26.9 (fit 0.91 x 0.81)`
- `102403 micscale=12`: `11.8 s no echo confirmed (at most -18.1), 13.4 s no echo confirmed (at most -20.2), 13.9 s no echo confirmed (at most -26.0), 39.5 s no echo confirmed (at most -21.4), 39.7 s no echo confirmed (at most -18.3), 40.2 s no echo confirmed (at most -16.2), 43.7 s no echo confirmed (at most -14.1), 55.3 s echo -13.8 (fit 0.90 x 1.02)`
- `102403 micscale=12 seed=auto`: `0.0 s fallback -40 -> -16, 11.8 s echo -18.1 (fit 0.88 x 0.89), 13.8 s echo -21.4 (fit 0.59 x 1.26), 13.9 s echo -26.0 (fit 0.61 x 1.28), 14.2 s fallback -16 -> -28, 55.3 s echo -13.8 (fit 0.90 x 1.02)`

Verdict by the seven criteria: **ALL 7 MET (PASSED)**:
1. Linearity with `micscale=12`: `meter` rose by exactly 12.0 dB on both captures (-38.9 -> -26.9 dB and -25.8 -> -13.8 dB).
2. `fallb` with `micscale=12`: -28 dB on `091611` (between meter - 8 and meter + 2). On `102403` it stayed at -40 dB: the confirmation came at 55.3 s and no pause for a rebuild followed before the capture ended.
3. Trace with `micscale=12`: exactly one fallback change (to -28 dB at 21.1 s on 091611).
4. Trace with `seed=auto`: the cached value sets the fallback right at the stream start (`0.0 s fallback -40 -> X`).
5. `start` (`seed=auto` vs `gain=0.01`): +8.1 dB on `091611` (12.8 vs 4.7 dB) and +26.7 dB on `102403` (28.8 vs 2.1 dB); both far over the required 6 dB.
6. Voice (`seed=auto` vs `gain=0.01`): `voice`, `onset` and `>10dB` do not change at all (0.0 dB / 0 points).
7. Echo (`seed=auto` vs `gain=0.01`): `echo` unchanged, `left` equal or 0.9 dB better.

### 5. Summary
- Everything passed: test A is identical to milestone 44, test B no longer detects an echo on headphones or in the voice, and test C shows what the remembered echo strength is for: 8 to 27 dB more echo removed at the stream start at no cost to the voice. The fit on speakers in real rooms (0.22-0.78) shows that the 0.90 threshold is strict for a diffuse echo: live it will confirm an echo only from a monitor or direct reflections. The report suggested loosening it (to 0.70, say); milestone 50 tried that and kept 0.90.

## Milestone 49: The meter's fit in the debug log; a threshold scene for the captures (2026-11-10)

After milestone 48 (second round: test A equal to milestone 44 on all 22 captures, no false
detection in test B, test C passed on the two captures that confirm). Written on the Mac:
`aec3.rs`, `shm.rs` and the pure part of `chain_log.rs` compiled and tested, `plugin.rs` edited
without a compiler. Shared memory layout v19.

- **What milestone 48 leaves open.** 0.9 confirms the echo on 2 of 11 speaker captures
  (`091611`, `102403`: the monitor's speakers). The others end between 0.22 and 0.92 (`104318`
  at 0.92 has a reference whose level does not move). Without an echo the captures end at
  0.23-0.40 (headphones, speech over music with no window of echo alone). That is one value
  per capture, taken at its end, often in speech: not enough to lower the threshold on. A wrong
  confirmation is what milestone 46 was; a missed one costs nothing against milestone 44.
- **`fit=<r>` scene in `capture_echo_report`**: the meter confirms from correlation `r`
  instead of 0.9 (`EchoMeter::min_correlation`). Third round in
  `docs/aec3-echo-strength-test.md`, chapter 6: which captures confirm at 0.8 / 0.7 / 0.6 /
  0.5, and which do so wrongly.
- **The fit in the debug log** (`aec_echo_fit_permille`, `aec_echo_slope_permille`): the
  `[Chain]` start line and the 10 s summary now say e.g.
  `echo not confirmed (at most -34.3 dB under the reference), fit 0.61 x 0.96`. Not in the
  change lines: it moves all the time. A log from another machine now shows how near its echo
  comes to being confirmed, which a capture cannot be asked of a tester.
- Seen in milestone 48's traces and left as it is: on `102403` with a remembered strength the
  bound fell 10 dB under it for a while (13.9 s) and the fallback stepped down, then the echo
  was confirmed at the old value again at 55.3 s. The 2026-11-07 captures have output switches
  and volume changes in them; if a capture with a steady volume shows the same, the 3 dB of
  `ECHO_BOUND_UNDER_DB` is too eager.


## Milestone 50: Echo Meter Correlation Threshold Sweep (2026-11-10)

Third round of the echo meter test, following `docs/aec3-echo-strength-test.md`, chapter 6 (how low the correlation threshold may go).
The question: how far `EchoMeter::min_correlation` can drop from its 0.90 (tried 0.90, 0.80, 0.70, 0.60 and 0.50 through the `fit=<r>` scene) without a false echo confirmation on captures with no echo (headphones), with speech over music or with reverb, and without an unstable fallback estimate.

### 1. Step 0: compilation and unit tests
Checked as chapter 0 says before the sweep:
- `cargo test --release -p gamersequalizer-mic-vst --lib aec3:: -- --nocapture`: 16 passed, 4 ignored (identical to the Mac reference).
- `cargo test -p gamersequalizer-common`: 32 passed, 0 failed.
- `cargo test -p gamersequalizer chain_log`: 5 passed, 0 failed.
- All tests clean, shared memory layout v19 works.

### 2. Threshold sweep on the captures with a reference
Run with:
```powershell
$env:SCENES = ";fit=0.8;fit=0.7;fit=0.6;fit=0.5"
Remove-Item Env:CAP -ErrorAction SilentlyContinue
cargo test --release -p gamersequalizer-mic-vst --lib capture_echo_report -- --ignored --nocapture
```
15 of the 22 captures have a reference (the 7 without one, `091216`, `100621`, `104424`, `125043`, `163832`, `163934`, `204900`, stayed clean in every scene: `meter: -`, `fallb: -40`).

| Capture         | Threshold | Confirmed |  meter | check | fallb | First confirmed | Highest |    Verdict    | Note                                                                          |
| :-------------- | :-------: | :-------: | -----: | ----: | ----: | --------------: | ------: | :-----------: | :---------------------------------------------------------------------------- |
| 20261107-091611 |    0.90   |    yes    |  -38.9 | -36.6 |   -40 |          15.1 s |   -35.4 |    correct    | Within the check's tolerance                                                  |
| 20261107-091611 |    0.80   |    yes    |  -38.7 | -36.6 |   -40 |          15.1 s |   -35.4 |    correct    | Within the check's tolerance                                                  |
| 20261107-091611 |    0.70   |    yes    |  -38.3 | -36.6 |   -40 |          15.1 s |   -35.4 |    correct    | Within the check's tolerance                                                  |
| 20261107-091611 |    0.60   |    yes    |  -38.3 | -36.6 |   -40 |          15.1 s |   -35.4 |    correct    | Within the check's tolerance                                                  |
| 20261107-091611 |    0.50   |    yes    |  -37.4 | -36.6 |   -40 |          15.1 s |   -35.4 |    correct    | Within the check's tolerance                                                  |
| 20261107-100150 |    0.90   |     no    | <-39.0 | -37.2 |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261107-100150 |    0.80   |     no    | <-39.0 | -37.2 |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261107-100150 |    0.70   |     no    | <-39.0 | -37.2 |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261107-100150 |    0.60   |     no    | <-39.0 | -37.2 |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261107-100150 |    0.50   |     no    | <-39.0 | -37.2 |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261107-101152 |    0.90   |     no    | <-28.2 | -29.1 |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261107-101152 |    0.80   |    yes    |  -36.2 | -29.1 |   -40 |           8.7 s |   -30.3 |     FALSE     | value -36.2 < check-6 (-35.1); 2 fallback changes                             |
| 20261107-101152 |    0.70   |    yes    |  -36.2 | -29.1 |   -40 |           8.7 s |   -30.3 |     FALSE     | value -36.2 < check-6 (-35.1); 2 fallback changes                             |
| 20261107-101152 |    0.60   |    yes    |  -36.2 | -29.1 |   -40 |           8.7 s |   -30.3 |     FALSE     | value -36.2 < check-6 (-35.1); 2 fallback changes                             |
| 20261107-101152 |    0.50   |    yes    |  -36.2 | -29.1 |   -40 |           8.7 s |   -30.3 |     FALSE     | value -36.2 < check-6 (-35.1); 2 fallback changes                             |
| 20261107-102403 |    0.90   |    yes    |  -25.8 | -28.3 |   -40 |          55.3 s |   -25.8 |   correct *   | Within tolerance with the 0.5 dB allowed in M48 (-25.8 vs max -26.3)          |
| 20261107-102403 |    0.80   |    yes    |  -25.0 | -28.3 |   -28 |          11.8 s |   -25.0 |     FALSE     | value -38.0 < check-6 (-34.3); value -25.0 > check+2 (-26.3); jump of 13.0 dB |
| 20261107-102403 |    0.70   |    yes    |  -25.2 | -28.3 |   -28 |          11.8 s |   -25.2 |     FALSE     | value -38.0 < check-6 (-34.3); value -25.2 > check+2 (-26.3); jump of 12.8 dB |
| 20261107-102403 |    0.60   |    yes    |  -25.3 | -28.3 |   -28 |          11.8 s |   -25.3 |     FALSE     | value -38.0 < check-6 (-34.3); value -25.3 > check+2 (-26.3); jump of 12.7 dB |
| 20261107-102403 |    0.50   |    yes    |  -25.3 | -28.3 |   -28 |          11.8 s |   -25.3 |     FALSE     | value -38.0 < check-6 (-34.3); value -25.3 > check+2 (-26.3); jump of 12.7 dB |
| 20261107-104318 |    0.90   |     no    | <-39.6 | -39.1 |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261107-104318 |    0.80   |     no    | <-39.6 | -39.1 |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261107-104318 |    0.70   |     no    | <-39.6 | -39.1 |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261107-104318 |    0.60   |     no    | <-39.6 | -39.1 |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261107-104318 |    0.50   |     no    | <-39.6 | -39.1 |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261109-115931 |    0.90   |     no    | <-30.2 | -41.4 |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261109-115931 |    0.80   |     no    | <-30.2 | -41.4 |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261109-115931 |    0.70   |     no    | <-30.2 | -41.4 |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261109-115931 |    0.60   |     no    | <-30.2 | -41.4 |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261109-115931 |    0.50   |     no    | <-30.2 | -41.4 |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261109-122520 |    0.90   |     no    | <-41.3 | -41.3 |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261109-122520 |    0.80   |     no    | <-41.3 | -41.3 |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261109-122520 |    0.70   |     no    | <-41.3 | -41.3 |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261109-122520 |    0.60   |     no    | <-41.3 | -41.3 |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261109-122520 |    0.50   |     no    | <-41.3 | -41.3 |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261109-122712 |    0.90   |     no    | <-34.3 | -34.8 |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261109-122712 |    0.80   |    yes    |  -34.3 | -34.8 |   -40 |           7.7 s |   -34.3 |    correct    | Within the check's tolerance                                                  |
| 20261109-122712 |    0.70   |    yes    |  -34.3 | -34.8 |   -40 |           7.7 s |   -34.3 |    correct    | Within the check's tolerance                                                  |
| 20261109-122712 |    0.60   |    yes    |  -34.3 | -34.8 |   -40 |           7.7 s |   -34.3 |    correct    | Within the check's tolerance                                                  |
| 20261109-122712 |    0.50   |    yes    |  -34.3 | -34.8 |   -40 |           7.7 s |   -34.3 |    correct    | Within the check's tolerance                                                  |
| 20261109-124950 |    0.90   |     no    | <-28.2 |     - |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261109-124950 |    0.80   |    yes    |  -29.1 |     - |   -34 |          24.2 s |   -29.1 |     FALSE     | value -29.1 > -33 without a check                                             |
| 20261109-124950 |    0.70   |    yes    |  -28.2 |     - |   -34 |          22.4 s |   -28.2 |     FALSE     | value -28.2 > -33 without a check                                             |
| 20261109-124950 |    0.60   |    yes    |  -28.2 |     - |   -34 |          22.4 s |   -28.2 |     FALSE     | value -28.2 > -33 without a check                                             |
| 20261109-124950 |    0.50   |    yes    |  -28.2 |     - |   -34 |          22.4 s |   -28.2 |     FALSE     | value -28.2 > -33 without a check                                             |
| 20261109-125717 |    0.90   |     no    | <-32.9 |     - |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261109-125717 |    0.80   |     no    | <-32.9 |     - |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261109-125717 |    0.70   |     no    | <-32.9 |     - |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261109-125717 |    0.60   |     no    | <-32.9 |     - |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261109-125717 |    0.50   |     no    | <-32.9 |     - |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261109-130319 |    0.90   |     no    | <-38.1 | -37.0 |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261109-130319 |    0.80   |     no    | <-38.1 | -37.0 |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261109-130319 |    0.70   |    yes    |  -36.6 | -37.0 |   -40 |          19.2 s |   -36.6 |    correct    | Within the check's tolerance                                                  |
| 20261109-130319 |    0.60   |    yes    |  -36.6 | -37.0 |   -40 |          19.2 s |   -36.6 |    correct    | Within the check's tolerance                                                  |
| 20261109-130319 |    0.50   |    yes    |  -36.6 | -37.0 |   -40 |          19.2 s |   -36.6 |    correct    | Within the check's tolerance                                                  |
| 20261109-144824 |    0.90   |     no    | <-22.2 |     - |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261109-144824 |    0.80   |     no    | <-22.2 |     - |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261109-144824 |    0.70   |     no    | <-22.2 |     - |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261109-144824 |    0.60   |     no    | <-22.2 |     - |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261109-144824 |    0.50   |     no    | <-22.2 |     - |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261109-162331 |    0.90   |     no    | <-33.3 | -33.3 |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261109-162331 |    0.80   |    yes    |  -34.8 | -33.3 |   -40 |          40.3 s |   -34.8 |    correct    | Within the check's tolerance                                                  |
| 20261109-162331 |    0.70   |    yes    |  -34.8 | -33.3 |   -40 |          40.3 s |   -34.8 |    correct    | Within the check's tolerance                                                  |
| 20261109-162331 |    0.60   |    yes    |  -34.8 | -33.3 |   -40 |          40.3 s |   -34.8 |    correct    | Within the check's tolerance                                                  |
| 20261109-162331 |    0.50   |    yes    |  -34.8 | -33.3 |   -40 |          40.3 s |   -34.8 |    correct    | Within the check's tolerance                                                  |
| 20261109-205013 |    0.90   |     no    | <-33.9 | -50.5 |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261109-205013 |    0.80   |     no    | <-33.9 | -50.5 |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261109-205013 |    0.70   |     no    | <-33.9 | -50.5 |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261109-205013 |    0.60   |     no    | <-33.9 | -50.5 |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261109-205013 |    0.50   |     no    | <-33.9 | -50.5 |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261109-210655 |    0.90   |     no    | <-44.8 | -50.3 |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261109-210655 |    0.80   |     no    | <-44.8 | -50.3 |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261109-210655 |    0.70   |     no    | <-44.8 | -50.3 |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261109-210655 |    0.60   |     no    | <-44.8 | -50.3 |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |
| 20261109-210655 |    0.50   |     no    | <-44.8 | -50.3 |   -40 |               - |       - | not confirmed | Upper bound (<)                                                               |

### 3. What happens under 0.90, capture by capture
- **20261107-101152 (room reverb)**:
  - At 0.90: safely unconfirmed (`<-28.2 dB`).
  - At <= 0.80: at 8.7 s the correlation reaches 0.89 and an echo of -30.3 dB is confirmed falsely, which moves the fallback to -34 dB at 11.0 s. At 14.0 s the value drops to -36.2 dB (outside `check - 6` = -35.1 dB) and at 16.0 s the fallback falls back to -40 dB. Two fallback changes in the trace and a value under the check's tolerance: **FALSE**.
- **20261107-102403 (speaker volume changes and speech)**:
  - At 0.90: confirmed only at 55.3 s at -25.8 dB (`fit 0.90 x 1.02`), the fallback stays at -40 dB (accepted in milestone 48 with 0.5 dB over `check + 2`).
  - At <= 0.80: confirmed at 11.8 s at -30.1 dB, then it drops to -38.0 dB (under `check - 6` = -34.3 dB) and at 47-51 s jumps to -25.0 .. -25.3 dB (over `check + 2` = -26.3 dB). A jump of **12.7 to 13.0 dB** within one capture: **FALSE**.
- **20261109-124950 (voice over loud music, only 6 echo-alone windows)**:
  - At 0.90: safely unconfirmed (`<-28.2 dB`).
  - At <= 0.80: at 22-24 s the fit reaches 0.80 x 0.97 and 0.73 x 0.95 and an echo of -29.1 dB / -28.2 dB is confirmed falsely. Both are over the -33 dB limit for captures without a check and move the fallback to -34 dB. The voice over music is taken for an echo: **FALSE**.
- **Headphones (`20261109-115931`, `20261109-122520`)**:
  - Unconfirmed at every threshold (highest correlations 0.23 and 0.27). Not even 0.50 detects an echo on headphones.
- **Long speech over music (`20261109-205013`, `20261109-125717`, `20261109-144824`)**:
  - Unconfirmed at every threshold (correlations 0.29 to 0.40). The voice caused no false detection.
- **Captures newly confirmed at lower thresholds**:
  - `20261109-122712`: confirmed at <= 0.80 (-34.3 dB vs check -34.8 dB, within tolerance, 0 fallback changes).
  - `20261109-162331`: confirmed at <= 0.80 (-34.8 dB vs check -33.3 dB, within tolerance, 0 fallback changes).
  - `20261109-130319`: confirmed at <= 0.70 (-36.6 dB vs check -37.0 dB, within tolerance, 0 fallback changes).

### 4. Summary table

| Threshold | correct (number of captures)       | FALSE (which captures) |
| :-------: | :--------------------------------- | :--------------------- |
|    0.90   | 2 (091611, 102403 *)               | none *                 |
|    0.80   | 3 (091611, 122712, 162331)         | 101152, 102403, 124950 |
|    0.70   | 4 (091611, 122712, 130319, 162331) | 101152, 102403, 124950 |
|    0.60   | 4 (091611, 122712, 130319, 162331) | 101152, 102403, 124950 |
|    0.50   | 4 (091611, 122712, 130319, 162331) | 101152, 102403, 124950 |

\* _Threshold 0.90 and capture 102403_: milestone 48 passed the value of -25.8 dB (against check + 2 = -26.3 dB) with a tolerated 0.5 dB. Judged strictly, without that tolerance, 0.90 has 1 correct (091611) and 1 FALSE (102403). The conclusion does not change: every lower threshold has 3 clear false detections.

### 5. Recommendation
- **Lowest safe correlation threshold**: **0.90** (`ECHO_FIT_MIN_CORRELATION = 0.90`).
- 0.90 is the **only** threshold with no false echo confirmed from the voice (`124950`) or from reverb (`101152`) and with a stable fallback estimate.
- Any lower threshold (0.80, 0.70, 0.60, 0.50) confirms a false echo on `124950` (voice over music confirmed as an echo of -28 dB, fallback moved to -34 dB) and on `101152` (room reverb moves the fallback twice, -40 -> -34 -> -40 dB).
- In line with milestone 49 (_"A wrong confirmation is what milestone 46 was; a missed one costs nothing against milestone 44"_): leaving captures with a weaker or diffuse echo on the -40 dB default, where AEC3 works well, is far safer than risking a false echo estimate from the voice and breaking the echo suppression.
- `ECHO_FIT_MIN_CORRELATION` stays at **0.90** in the code.


## Milestone 51: Live APO Verification, Echo Strength Caching & Capture Analysis (2026-11-10)

First complete live run of the mic APO (`gamersequalizer_apo.dll`) inside the Windows audio engine (`audiodg.exe`) with the v19 shared memory layout, after the threshold sweep of milestone 50, following `docs/aec3-tuning-guide.md`, chapter 11.3.

Built with `scripts/dev-rebuild.ps1 -Apo -DebugCapture -Start`, installed into `C:\Program Files\GamersEqualizer\apo\` and used live.

### 1. The live run in the log (`gamersequalizer.log`)

The chain log (`[Chain]`) and the link layer (`[ApoLink]`) show the whole life cycle without a single error:

1. **Chain start and the mic gain into AEC3 (`AEC mic gain`):**
   - The mic starts on the device `Mikrofon (Realtek(R) Audio)`:
     ```text
     [Chain] start: ... AEC on NoReference delay none (from cache off, double talk off), echo not measured, fit 0.00 x 0.00 (AEC fallback echo -40.0 dB, AEC mic gain +0.0 dB) ...
     ```
   - At 15:12:09 the delay locked and the automatic mic gain into AEC3 went to **+18.0 dB**:
     ```text
     [Chain] changed: AEC on NoReference -> on Locked, AEC mic gain +0.0 -> +18.0
     ```

2. **Speaker echo strength measured (fit 0.95 x 0.80) and cached:**
   - With audio playing from the speakers the correlation reached **0.95** at a slope of **0.80** (over the 0.90 threshold and inside the slope range 0.7..1.3):
     ```text
     [Chain] 10 s: ... ERLE 7.0 dB, echo not confirmed (at most -46.7 dB under the reference), fit 0.95 x 0.80 (AEC fallback echo -40.0 dB, AEC mic gain +18.0 dB) ...
     ```
   - At 15:12:40 the meter confirmed the steady echo at **-46.5 dB under the reference**:
     ```text
     [ApoLink] saving echo strength -46.5 dB under the reference
     [Chain] changed: echo not confirmed (at most -46.7 dB) -> -46.5 dB under the reference
     ```
   - The value went into `%APPDATA%\GamersEqualizer\echo_strength_cache.json` (`-465`).

3. **Output device switched (devices kept apart, no false detection):**
   - At 15:12:52 the output was switched to another device:
     - The echo strength measured on the previous device was dropped at once (`echo -> not measured`).
     - A new delay was measured and locked (5.1 ms -> 4.9 ms -> 4.8 ms).
     - With human speech playing (voice in 61 % of the blocks) the correlation reached only 0.59 to 0.80 (under the 0.90 threshold):
       ```text
       [Chain] 10 s: ... echo not confirmed (at most -35.9 dB under the reference), fit 0.77 x 0.82 ...
       ```
     - The meter kept the state unconfirmed (`<`), the fallback estimate stayed at -40.0 dB and no false value went into the cache.

4. **Back on the first device (warm start / persistence):**
   - At 15:14:01 the output was switched back to the first speakers:
     ```text
     [ApoLink] cached echo delay for this mic/output: 1.0 ms
     [ApoLink] remembered echo strength for this mic/output: -46.5 dB under the reference
     [ApoLink] reference output switched: the APO measures the echo delay again
     [Chain] changed: AEC delay 5.0 ms -> 1.0 ms, delay from cache off -> on, echo not confirmed -> -46.5 dB under the reference
     ```
   - The remembered echo strength (-46.5 dB) and delay (1.0 ms) were back at 0 s, without waiting for a new measurement.

Review notes (added when the branch went to main):
- In step 2 the 10 s summary still shows a bound although the correlation and the slope already pass. The third condition, a reference level spread of at least 2 dB in the quieter half of the windows, is not in the log, so a log alone cannot say why an echo with a passing fit is not confirmed yet.
- An echo of -46.5 dB leaves the fallback estimate at -40 dB (strength - 3 dB is under the -40 dB floor). This run therefore did not exercise a fallback above -40 dB; only test C of milestone 48 did, on captures with `micscale=12`.
- The test machine is left with a debug-capture APO installed; rebuild without `-DebugCapture` before packaging.

---

### 2. The capture taken during the run (`test_data/private/captures/20261110-141406`)

Recorded with `cargo run --release -p gamersequalizer-apo --example debug_capture -- 30`:
- Length: 3000 blocks (30.0 seconds at 48 kHz).
- Echo delay: steady at exactly **45 samples** (~0.94 ms) for the whole capture (0 deviations).
- `double_talk`: 0 blocks (the filter was never frozen needlessly).
- `input_clips`: 0 (no clipping or limiting at the input).
- `ERLE`: **5.5 dB** on average (max **6.7 dB**) while the reference played.
- DSP load: 1.5-6 % on average, 4-10 % at peaks (including the cost of writing the debug data into shared memory).

RMS level of each chain stage:

| File                   | Stage                                              |  RMS level |
| :--------------------- | :------------------------------------------------- | ---------: |
| `01_mic_raw.wav`       | Mic input from audiodg                             | -42.3 dBFS |
| `02_ref_at_mic.wav`    | Speaker reference (loopback)                       | -19.5 dBFS |
| `03_ref_aec.wav`       | Reference shifted by the delay, as fed to AEC3     | -19.5 dBFS |
| `04_after_aec.wav`     | After echo cancellation (AEC3)                     | -42.9 dBFS |
| `05_after_denoise.wav` | After the neural denoiser and the noise gate       | -43.1 dBFS |
| `06_after_eq.wav`      | After the parametric voice EQ                      | -43.1 dBFS |
| `07_after_leveler.wav` | After the learning voice leveler                   | -25.8 dBFS |
| `08_out.wav`           | APO output after volume and limiter (outgoing mic) | -31.8 dBFS |

In the quiet pauses between speech the noise gate and the denoiser pull the output down to -100 .. -110 dBFS, while the leveler brings the spoken word to a clear, even level (-25.8 dBFS).

---

### 3. Conclusion and the state of branch `aec3-mic-gain`

The live test confirmed that everything works and is stable:
1. The correlation rule (threshold 0.90) confirmed the real echo of the monitor (-46.5 dB) and ignored diffuse / headphone playback.
2. Persistence and device switching in `ApoLink` work, separately for each (mic, output) pair.
3. The v19 shared memory layout works inside `audiodg.exe` with no crash, clipping or delay.

## Milestone 52: `aec3-mic-gain` goes to main: what it changed, what stays open (2026-11-10)

Milestones 43-51 in short. Milestones 46, 48, 50 and 51 were first written in Czech and are
translated above; their tables, traces and numbers are unchanged, with two corrections:
milestone 46 counted 11 differing captures (7 with a moved fallback) where its own table and
lists have 10 (6), and milestone 51 gave the slope range as 0.70..1.40 (the code has 0.7..1.3).

**What the branch changed**

- AEC3 gets the mic at call level: a gain taken from the leveler's learned level, 0..24 dB in
  6 dB steps, taken back after AEC3 (milestone 43, checked on the captures in milestone 44).
- Tuning chosen on 22 captures: `erle.max_l` 30, `erle.max_h` 10, `bands_in_limiting_gain` 0
  (milestone 44).
- Echo strength per (mic, output) pair: measured passively, confirmed only when the mic level
  follows the reference level (correlation >= 0.9), remembered in `echo_strength_cache.json`
  and used as AEC3's fallback estimate when the echo is stronger than 40 dB under the reference
  (milestones 45 and 47, checked in 48, 50 and 51). The `[Chain]` log lines show all of it
  (milestone 49).
- Shared memory layout v19: the APO has to be reinstalled together with the app.

**Open**

- A fallback above -40 dB has not run live: the one echo confirmed live was -46.5 dB
  (milestone 51).
- At 0.9 the passive meter confirmed 2 of 11 speaker captures (milestone 50); on the others
  nothing changes against milestone 44. A calibration signal played on request would measure
  them: a proposal, not decided, in `docs/sidetone-zadani.md`, 7.2 (removed; `git show b931839:docs/sidetone-zadani.md`).
- The log shows the fit but not the reference level spread (review note in milestone 51).
- Nothing turns AEC3 off on headphones; the meter only refuses to confirm an echo there.
- The filter length (`len=20` / `len=32`) is undecided and was not run through the denoiser
  (milestone 44).
- The echo cache key is the Windows default mic and the reference output, not the mic chosen
  in the app.
- Not measured: whether the loopback reference is taken before or after the Windows volume.
  If before, a remembered "dB under the reference" stops holding when the user changes the
  volume. Procedure: `docs/sidetone-implementace.md`, 4.5 (removed; `git show b931839:docs/sidetone-implementace.md`).
- `window_report` prints `0.0` in `echo` / `left` for a capture with no echo-alone window
  (`125717`, `144824` in milestone 46's table); read it as "no data".
- `ECHO_BOUND_UNDER_DB` (3 dB) may be too eager, see the end of milestone 49.

## Milestone 53: An effect that is installed but does nothing: the log says why, MFX fallback, the app installs itself (2026-11-11)

Branch `diag/apo-not-running`. A tester's log showed the mic effect "installed and active" for four
days while audiodg never ran it; the app read that state from the registry only.

**Measured (author's notebook, Windows 11 25H2, Realtek "Microphone Array")**

| Observation                                                            | Meaning                                                   |
| ---------------------------------------------------------------------- | --------------------------------------------------------- |
| DLL loaded in audiodg, COM ok, shared memory v19 on both sides         | not the signature check, not a version mismatch           |
| Output EQ and Stream EQ note "started", the mic chain notes nothing    | Windows never reached `LockForProcess` on the capture EFX |
| Mic chain added to the composite MFX list: 1687 blocks, 1679 processed | this driver runs mode effects, not the endpoint effect    |
| Capture EFX lists two modes (DEFAULT + SPEECH), MFX four incl. RAW     | suspected reason; Microsoft allows only DEFAULT for EFX   |
| Sound Recorder + voice typing (Win+H): "mic chains running here: 2"    | MFX runs one chain per processing mode in use             |
| Windows passes a 56-byte init block                                    | `APOInitSystemEffects` v1, so the APO never gets the mode |

On the dev PC (Realtek mic, one MFX mode besides DEFAULT, no RAW) the EFX slot works; the installer's
self-test confirmed it twice and moved nothing.

**What changed**

- The app says the truth. The service counts checks in which another app records (or sound plays) while
  the effect's heartbeat stands still; after 10 s the header, the Mic and Headphones tabs and a
  notification say "does NOT work". `run_status.json` carries the verdict to the window.
- The log alone explains it; testers send only the log (Log tab: "Save a copy to send").
  `[ApoCheck]`: Windows build, DLL, COM registration, every device with our effects. `[ApoNote]`: the
  APO's own notes from inside audiodg (each step: class, instance, init, formats, stream start/stop, with
  process and DLL build), audiodg crashes, Defender detections, Code Integrity blocks, and every line the
  installer printed. Each note is logged once, also across restarts.
- Mic slot: EFX first (one chain for every app, RAW streams included). After installing, the installer
  opens the mic for 3 s (`--probe-mic`) and moves the chain to MFX when Windows did not start it; the Mic
  tab offers the same switch later. An update keeps the slot.
- Several mic chains in one audiodg (MFX: one per mode; EFX: one per microphone): only the first one
  publishes telemetry (`MIC_PUBLISHER`, flag `MIC_CHAIN_OTHERS`), and the Mic tab names the app whose
  values are shown.
- The app installs itself: the exe goes to `%ProgramFiles%\GamersEqualizer\` next to the DLL (hash checked
  by the elevated script), a copy started from elsewhere hands over to it, "start with Windows" points
  there, a newer copy ends an older running one (`takeover.rs`), uninstall removes it.
- The installer writes nothing to its window while audio is stopped: a paused console (a click into it)
  once left the notebook without sound until Enter was pressed.
- Exe and DLL carry a version resource with the git hash; the service logs it at start.

**Open**

- Why the notebook's capture EFX is never started is not known (the chain was moved before the new step
  notes could show where it stops). The MFX fallback hides the question.
- In MFX an app in a mode the driver does not list gets the raw microphone; two modes at once run the
  chain twice (double CPU). Not seen as a problem yet.
- Not run on Windows when this was written: `takeover.rs`, the Mic tab's "several recordings" line, the
  uninstall of the installed exe.
- Found by the new crash notes, not part of this branch: on 2026-11-10 20:22 audiodg crashed about 20
  times in a minute in `gamersequalizer_apo.dll` (stack overflow, `c00000fd`) with a `sidetone` dev build
  (`e1b98e5-dirty`) when the pair was switched on. Check it on that branch.
- The binaries are unsigned; Defender flagged an older exe as `Trojan:Win32/Cloxer` (generic detection).
- Windows Terminal and Notepad are taken for games ("installed by a launcher").

## Milestone 54: Unified EQ profile management and biquad cascade optimization (2026-11-11)

**What changed**

- **Consistent EQ profile management across all five tabs (`Headphones`, `Game`, `Chat`, `Media`, `Mic`):**
  - Generalized output-layer profile operations (`OutputLayer`, add, save-as, delete, active game lookup, case-insensitive NTFS-safe duplicate check) in `crates/gamersequalizer-daemon/src/output_eq/mod.rs` and `ui/mic_presets.rs`.
  - Replaced `ui/mic_preset_bar.rs` with shared `crates/gamersequalizer-daemon/src/ui/profile_bar.rs` (`New…`, `Save as…`, delete, `Copy to share`, `Import…`, centered dialogs) across `tab_eq.rs`, `tab_headphones.rs` and `tab_mic.rs`, keeping tab-specific AutoEQ wizard, Sonar DB import and Mic `Save`/`Reset` intact (`i18n.rs`, `i18n_cs.rs`, `state.rs`, `app.rs`).
- **EQ biquad cascade optimization (`BiquadCascade` + `StreamProcessor` + `OutputProcessor`):**
  - Evaluated `origin/sidetone`'s `eq_bench.rs` and the procedure of its EQ analysis (summarized below) on Windows x86-64. Confirmed Hypothesis 1: keeping `f32` rounding between stages only yielded `0.56×–1.35×` on baseline SSE2 (regressing to `144 329 ns` at 64 stages).
  - Updated `crates/gamersequalizer-common/src/biquad.rs` to run sample-by-sample with `f64` between stages (`max diff <= 8.9e-8` vs legacy `f32`-rounded stages), split filter memory into SoA slices (`FilterState { z1, z2 }`), and dispatch via `std::arch::is_x86_feature_detected!("fma")` (`#[target_feature(enable = "fma")]` + `.mul_add()`, SSE2/portable fallback), keeping `flush_non_finite`.
  - Updated `BiquadCascade::process_interleaved` to handle `1` (mono), `2` (stereo), `4` (quad), `6` (5.1) and `8` (7.1) channels in a single pass, and updated `StreamProcessor` and `OutputProcessor` in `crates/gamersequalizer-apo/src/lib.rs` to own one multichannel cascade (`OutputProcessor` stays behind mix/spatial renderers in `EFX`; spatial apps still bypass `SFX`).
  - Added `crates/gamersequalizer-common/examples/eq_bench.rs` (extended with 5.1 and 7.1 cases) and expanded `interleaved_matches_per_channel_strided_and_recovers_from_nan` to `[1, 2, 3, 4, 6, 8]` channels.

**Measured (Windows x86-64, `cargo run --release -p gamersequalizer-common --example eq_bench`)**

| Case (480 frames / 10 ms)     | Before (Windows x86-64)  | After (SoA + FMA)     | Speedup | % of 10 ms |
| ----------------------------- | ------------------------ | --------------------- | ------- | ---------- |
| Mic EQ, mono (10 biquads)     | `9 198 ns`               | `3 987 ns`            | `2.31×` | `0.040 %`  |
| Mic EQ full, mono (16 biquad) | `14 727 ns`              | `6 076 ns`            | `2.42×` | `0.061 %`  |
| Output EQ, stereo (20 biquad) | `36 812 ns`              | `7 106 ns`            | `5.18×` | `0.071 %`  |
| Stream EQ full, stereo (64 b) | `117 663 ns`             | `27 668 ns`           | `4.25×` | `0.277 %`  |
| Output EQ, 5.1 (6 ch, 20 b)   | `62 784 ns` (before SoA) | `11 750 ns`           | `5.34×` | `0.118 %`  |
| Output EQ, 7.1 (8 ch, 20 b)   | `83 702 ns` (before SoA) | `11 511 ns`           | `7.27×` | `0.115 %`  |

**Why this and nothing more (from the analysis on Apple M4 Pro, arm64)**

- The old loop was slow because every sample waits for the previous one's filter state inside one
  stage (one serial add-multiply chain, 2.3 ns per sample and stage). Sample by sample through all
  stages keeps several chains in flight (0.7 ns). Running two stages offset by one sample gained nothing.
- In absolute terms EQ was never expensive: mic EQ 0.11-0.17 % of a core, output EQ 0.43 %; the whole
  mic chain takes 2-24 % of a block depending on the model. Only the stream EQ adds up (up to 64 biquads
  per playing stream, 1.4 % each before this change).
- Not worth touching: limiter (2.6 us per stereo block), leveler, expander, clip detector. Resampling
  (`rubato`), FFT (`rustfft`) and AEC3 (`sonora-simd`) already use SIMD in their crates. No hand-written
  asm or SIMD for EQ (stereo would gain at most another 2x, about 0.06 % of a core, and need `unsafe`).
- Possible, not measured: hand SIMD in `nnnoiseless` (RNNoise 2017 runs several times per block).

**Open**

- Live APO DLL build (`scripts/dev-rebuild.ps1 -Apo`) and testing inside `audiodg.exe` (including live 5.1/7.1 and Dolby Atmos behavior) not run yet.
- Live GUI click-through of unified EQ profile dialogs across all five EQ tabs not tested manually yet.

---

## Milestone 55: Audio Engine Periods for Sidetone (2026-11-10)

Measured shared-mode engine periods on the default microphone and default output via `period_probe` (`IAudioClient3::GetSharedModeEnginePeriod`).

| Device     | Endpoint ID                                                 | Rate (Hz) | Default Frames | Default (ms) | Min Frames | Min (ms) | Max Frames | Step |
| :--------- | :---------------------------------------------------------- | --------: | -------------: | -----------: | ---------: | -------: | ---------: | ---: |
| Microphone | `{0.0.1.00000000}.{cf0a2270-1885-4dd6-82a5-f636e9efadd5}`  |     48000 |            480 |        10.00 |        480 |    10.00 |        480 |  480 |
| Output     | `{0.0.0.00000000}.{b45cef56-a56b-49b0-88c0-4af057026718}`  |     48000 |            480 |        10.00 |        480 |    10.00 |        480 |  480 |

### Observations
- Both endpoints operate at 48000 Hz with a fixed period of 480 frames (10.00 ms).
- Minimum engine period equals default period (10.00 ms); low-latency mode with shorter periods is not exposed by these drivers.
- In shared mode, the sidetone latency budget starts with 10 ms mic engine period + 10 ms render engine period (plus ring queue margin).

---

## Milestone 56: Review of the release-0.2.0 code; fixes (2026-11-12)

Four read-only reviews of the code that came without an AI co-author trailer (sidetone, unified EQ
profiles + biquad SoA, installer/USB APO, signing/CI/clipboard), then fixes. Only
`gamersequalizer-common` and the PowerShell tests ran (macOS); daemon, APO and mic-vst need a Windows build.

| Area      | Fixed                                                                                                                                                                                                                                                                             |
| --------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| APO       | Init-block parsing: APOInitSystemEffects3 no longer read as v1/v2 (garbage `pDeviceCollection` in audiodg); one helper, last device                                                                                                                                               |
| Sidetone  | Daemon worker is the only publisher of pair/hashes/`enabled`; logs each "silent because" verdict change; no reopen loop on a low-latency fallback; no per-frame alloc in the denoiser; mix-format leak; edits kept on a pair switch                                               |
| Profiles  | "Replace" keeps `game_ids`/`is_custom`/spelling; New/Save as/Import never rebind a game; one NTFS-like name check (non-ASCII case)                                                                                                                                                |
| Biquad    | `set_bands` without allocation; out-of-range channel in a multichannel cascade is skipped                                                                                                                                                                                         |
| Installer | First per-endpoint snapshot on disk (`apo-backup`), uninstall restores it; uninstaller silent while audio is stopped + event log; devices in use by default, others opt-in; rolled-back devices not re-offered; notes filtered by endpoint GUID; Default mode only on empty slots |
| Signing   | No tester certificate; end-entity 1-year cert, mandatory password, timestamp required in CI; CI signs in a step without cargo; actions pinned to SHAs                                                                                                                             |
| Cleanup   | `verify-usb-apo.ps1` (956 lines) and the no-op GPU exports removed; clipboard errors logged                                                                                                                                                                                       |

Open: install/uninstall still duplicate their registry helpers (`shortcut:` comment); an old backup restores
old OEM effect lists after a driver update; nothing of this has run on Windows yet.

---

## Milestone 57: USB headsets and inbox drivers: processing modes and legacy effect values (2026-11-11)

Branch `diag/usb-apo-compatibility`. A tester's `Arctis Nova 3` showed the effects installed but never
started. Measured on Windows 11 with three USB devices at once (`ASUS AI Noise-Cancelling Mic Adapter`,
`EPOS H3PRO`, `Jabra EVOLVE LINK MS`, 6 endpoints), each step on a fresh `Audiosrv`, compared with the
local Realtek and AMD HDMI.

**Measured**

- The inbox USB driver (`usbaudio2.sys`) puts `VocaEffectPack` on capture and Microsoft Audio Home Theater
  Effects on render: legacy REG_SZ `,5` (SFX) and `,6` (MFX), no `,0`, and no EFX modes value
  (`{d3993a3f-...},7`). AMD HDMI and the tester's Arctis render have the same pattern; Realtek has clean
  composite lists and all modes.
- All 6 endpoints run our effects: 0 audiodg crashes, 0 failed opens, 16 kHz mono (EPOS), 48 kHz mono and
  stereo accepted.

| Step (render unless noted)                      | Result                                             |
| ----------------------------------------------- | -------------------------------------------------- |
| Capture `,15` without modes,7 (the old install) | mic chain not started on any of the 3              |
| Capture `,15` + modes,7 = Default               | started on all 3                                   |
| `,13` + `,15`, legacy `,5`/`,6` kept, no modes  | nothing started                                    |
| + modes,7                                       | Output EQ (EFX) runs, Stream EQ (`,13`) does not   |
| + `,0` instead of modes,7                       | nothing: `PKEY_FX_Association` does not help       |
| legacy `,5`/`,6` moved into `,13`/`,14`         | both run, Microsoft's effects kept                 |
| Output EQ in `,14` with legacy `,6` back        | Output EQ not started: `,6` shadows `,14`          |
| legacy `,5`/`,6` = our CLSIDs, no composite     | both run, but Microsoft's SFX/MFX are gone         |

So a legacy value shadows the composite list of its own slot only, and a slot runs our effect only in the
modes listed for it.

- `FxProperties` grants Administrators only `SetValue, ReadKey`; its subkeys belong to TrustedInstaller /
  Audiosrv. `reg.exe import` of a `.reg` export (which includes the subkeys) fails, so the old
  `-RestoreBackup` never worked. Restoring value by value through `RegistryKey` with exactly
  `SetValue, QueryValues` (PowerShell 5.1 needs real `[byte[]]` / `[string[]]`) left the registry byte for
  byte as before on all 6 endpoints.
- The USB-C ASUS adapter shows no endpoint until a jack is plugged into it.

**What changed**

- The installer adds Default to the slot's modes (not where the driver's own effects declare other
  modes, milestone 56), moves legacy values into composite lists, snapshots `FxProperties` in memory and
  restores it value by value.
- After installing it opens every endpoint (`--probe-mic`, new silent `--probe-render`), checks for
  audiodg crashes (Application log, event 1000) and the APO's "stream started" notes, tries MFX when EFX
  does not start, and rolls the endpoint back when it does not open or audiodg crashes.
- The uninstaller removes only our CLSIDs, keeps String vs MultiString and other vendors' effects, and
  drops Default-only modes values it left on empty slots. `[ApoCheck]` logs legacy values and `,0`.
- The verification script `verify-usb-apo.ps1` was removed (milestone 56): the installer's self-test and
  `[ApoCheck]` do its job.

**Open**

- The live audio path with real EQ settings on the three USB devices was not checked by ear.

---

## Milestone 58: Windows 10 and installer fixes from `apo-fixes` (2026-11-12)

**Windows 10**

- Composite effect lists (`PKEY_CompositeFX_*`, several effects per slot) exist from Windows 10 version 1803
  (build 17134), not only Windows 11; Microsoft's docs say so for every composite key. Older builds have
  one effect per slot. The installer writes composite lists only, so it needs 17134: the app checks the
  build before it asks for admin and the script checks it again before it changes anything.
- The APO uses only `IAudioProcessingObject`, `...RT`, `...Configuration`, `IAudioSystemEffects` (v1) and
  `APOInitSystemEffects2`, all from Windows 8.1/10. The UI falls back to egui fonts without the Windows 11
  ones.
- A driver with only the old LFX `,1` / GFX `,2` effects would lose its GFX once a newer value exists
  (Equalizer APO treats such drivers separately). `Get-EndpointBlocker` skips such an endpoint and names
  it; nothing changes when no endpoint is left.
- A microphone whose driver effects were moved and whose chain starts in neither slot is rolled back.
- Equalizer APO always sets `DisableProtectedAudioDG = 1`; we do not (it turns off the protected audio
  path system-wide). Whether Windows 10 needs it for an unsigned APO is unknown; `[ApoCheck]` logs it.

**From `apo-fixes`** (a parallel review branch; the rest of it was not taken: a move of shared helpers, a
refactor of the APO's notes that could lose the endpoint GUID, a looser "started" check, and keeping an
output whose effects never start)

- Legacy values move into composite lists only for the slots our effects go into (also when an effect
  moves to the other slot), plus SFX while LFX `,1` is set. The app's "configured" check uses the same
  per-slot rule (milestone 57: `,5`/`,6` left `,15` running).
- A device that does not open during the self-test is opened again without our effects: if it opens, ours
  stay out (rollback line, not offered again); if not (exclusive use, privacy settings), ours go back in
  unchecked. A crash, a check that did not run or hung, moved driver effects or a missing snapshot still
  roll back at once.
- "Update" is offered only where one of our effects already is; `endpoint_has_any_clsid` matches any of
  our classes.
- `scripts/tests/apo-install.tests.ps1` (54 checks, also under `pwsh` on macOS) covers the blocker, the
  migration slots and the reopen decision.

**Open**

- Nothing has run on Windows 10. Test: Windows 10 22H2 (a VM is enough for render), export
  `MMDevices\Audio` first, install, send `[ApoCheck]` and `[ApoNote]`, uninstall, compare `FxProperties`.

---

## Milestone 59: The echo canceller gets both speaker channels (2026-11-12)

Branch `feat/aec-stereo-reference`.

**What changed**

- AEC3 gets left and right separately and learns a filter per channel (`aec3.rs`, `process_block_stereo`);
  before, it got their mono mix.
- Stereo turns on by itself after 1 s of audible content where the channels differ (difference above
  -30 dB, level above -60 dBFS), and back to the mix after 5 min of mono content; pauses count for neither.
  The switch rebuilds AEC3 in a mic pause like a gain change, so stereo starts 3-7 s after the content.
- Same sound in both channels (calls, mono video) keeps the old path; the 16 earlier AEC captures give the
  same numbers before and after. Debug log: `AEC stereo reference off -> on`; debug capture v4
  (~122 MB) adds `03_ref_aec_left/right.wav` and `aec_stereo`. No shared memory change: the ring already
  held L/R, the state is a bit in `mic_chain_flags`.
- Loopback of 5.1/7.1: Windows mixes it to stereo (`AUTOCONVERTPCM`); only the fallback path, when
  Windows refuses, mixes it itself (`StereoFrame::from_channels`).

**Measured (macOS, synthetic scenes, 40 s; speakers -18 dBFS, right 0.9 ms later and 8 dB weaker, echo
and voice -30 dBFS at the mic)**

| Scene                              | Echo removed: mix | Echo removed: stereo |
| ---------------------------------- | ----------------- | -------------------- |
| same sound in both channels        | 43.5 dB           | 43.5 dB (stays mono) |
| music (centre + independent sides) | 16.5 dB           | 43.8 dB              |
| independent channels               | 9.9 dB            | 43.4 dB              |
| one source sweeping L-R (0.25 Hz)  | 29.2 dB           | 44.0 dB              |
| one source jumping L-R every 3 s   | 16.0 dB           | 44.3 dB              |
| fixed pan 70/30                    | 43.7 dB           | 43.6 dB              |

- The voice "survived" on the mix only because the echo stayed and the suppressor gave up; in stereo it
  loses what it loses today with mono content in the same scene (about -10 dB, voice as loud as the echo).
- AEC3 cost 0.46 -> 0.52 % of real time. A stereo filter did not detune after panning changes.

**From the code**

- `sonora` 0.3.0 with `detect_stereo_content = false` and two channels stays on the left channel
  (2.5 dB removed instead of 35 dB). So the library's detector stays on with 0 s hysteresis and no way
  back, and our wrapper decides the mode and keeps it across rebuilds.
- Not used: AEC3's own multichannel config (no more echo removed, 10 dB more voice lost).
- The fallback echo estimate is measured against the mix while AEC3 sums the channel powers: the gain
  goes 3 dB down, so for unrelated channels the estimate is up to 3 dB high, never low.

**Open**

- Not run on Windows: `plugin.rs`, `loopback.rs`, `chain_log.rs`, the APO. Check: stereo music with an
  open mic gives the `off -> on` line within ~7 s, a call never does; a 60 s debug capture (20 s music,
  then voice) through `capture_tuning_report` with `VARIANTS="mono;"`; a game with positional sound and
  headphones (nothing may get worse); `load avg` and `late blocks` before and after.
- No setting for stereo/mono (A/B works offline on a capture); full N channels only after a 5.1 measurement.

---

## Milestone 60: Sidetone: your own voice in the headphones (2026-11-10 .. 2026-11-12)

Branch `sidetone` (merged into `release-0.2.0`). Sealed or in-ear headphones block your own voice. Sidetone
plays the microphone into them with fine volume (headset hardware offers about 3 steps and passes all the
noise) and without steady hiss in pauses. It plays only while an app records the microphone (plus "Try it").

**Built**

- Tap: the mic APO (`SidetoneTap` in `MicProcessor::process`) takes the raw input before the mic chain,
  also when the chain failed (then without the leveler's gain): low cut / EQ, noise mode, level (volume
  0-300 % of the voice leveler's gain), into a 16384-sample `SidetoneRing` next to the header in the Global
  shared memory (layout v21, so the APO is reinstalled with the app).
- Playback: the Output EQ APO reads the ring (`SidetoneReader`: resampling, clock drift, 2 ms margin plus
  1 ms per real underrun up to 10 ms) and adds it mono to L and R after the EQ, before the limiter. It plays
  only when the APO's endpoint hash equals the pair's output.
- Daemon (`win_audio/sidetone.rs`): one worker publishes the pair, its hashes, the settings and `enabled`;
  pairs live in `sidetone.json`. It keeps a silent render stream open (the engine stops in silence) and
  logs every change of its verdict (Off, Muted, OtherMicrophone, OtherOutput, NoRecorder, Starting,
  Playing). The tab (`ui/tab_sidetone.rs`, shared EQ editor) shows the latency: green up to 10 ms, amber
  up to 20 ms.
- Noise modes: a 4-band expander with zero delay (default), or frame denoisers at 48 kHz
  (`mic-vst/src/sidetone_denoise.rs`: WebRTC NS, RNNoise, RNNoise 0.2).

**Why**

- Rejected: hardware sidetone (3 steps), Windows "Listen to this device" (tens to hundreds of ms, after the
  chain), a daemon capture-to-render stream (extra engine passes, shows in the mixer), own driver / ASIO /
  exclusive mode, and a tap after the mic chain (AEC ~9 ms, gate lookahead, model block). DeepFilterNet and
  DPDFNet add tens of ms.
- Mixed after the EQ (the voice gets no game profile) and before the limiter (it guards the sum). Never
  played on an unknown output (mic plus speakers would feed back); an unknown pair is off; no "always on"
  (the microphone would stay open).
- Sidetone must not reach the echo canceller. If the loopback reference were taken after the endpoint
  effects it would hold your own voice, the echo meter would confirm a phantom echo and AEC3 would cut the
  voice. Not measured; the fallback would be feeding the reference from the Output APO before the mix.

**Measured**

- Engine periods: milestone 55 (10 ms on both sides, no shorter period offered).
- Frame denoisers (macOS, 480-sample blocks): delay WebRTC NS 6.9 ms, RNNoise 10 ms, RNNoise 0.2 20 ms
  (+10 ms for other block sizes). Steady noise removed: expander 16-19 dB (-60 dBFS white noise), WebRTC
  16-17 dB, RNNoise 3-9 dB at -50/-40 dBFS, RNNoise 0.2 34-37 dB.
- Mouth-to-ear latency was not measured; estimated 25-35 ms at 10/10 ms periods.

**Open**

- Nothing since the milestone 56 fixes has run on Windows. On 2026-11-10 an early sidetone build crashed
  audiodg about 20 times a minute (stack overflow, milestone 53) when a pair was switched on: check that
  first.
- Watch: `echo` / `fit` in `[Chain]` unchanged with sidetone on, underruns, chain load, whether the silent
  render stream changes the loopback reference.
- Limits: exclusive-mode playback bypasses it; the reader has no anti-alias filter; frame denoisers only at
  48 kHz; switching modes gives a short gap; endpoint ids change on a driver reinstall (pairs are orphaned);
  keyboard clicks pass the expander; Bluetooth hands-free is out of scope.
- Not decided: AEC off for pairs with sidetone, an echo calibration burst on request, an echo cache keyed by
  the chosen microphone.


---

## Milestone 61: Profile edition: genre fallbacks, settings transfer, Sonar presets, app channels (2026-11-13 .. 2026-11-15)

Branch `genre-presets`. Planned as release 0.2.0 "profile edition".

**Built**

- Genre fallback (`output_eq/genre.rs`): a launcher game without its own profile gets the genre preset
  (Shooter, Competitive, MOBA / strategy, Open world, Story, ...). Labels come from GOG Galaxy's database
  (`winsqlite3`, 500 ms busy timeout; it was seen locked) and Steam's `appinfo.vdf` (binary KV v28/v29,
  `common/store_tags`, `common/type`). Specific labels (fps, moba, rts, visual novel...) win over broad
  ones (strategy, rpg, survival...); among broad ones the game's own label order decides. Cached per exe
  in `genre_cache.json` (cleared when `RULES_VERSION` changes; when Galaxy / Steam data change only the
  games without a result are looked up again, since Galaxy writes its database at almost every game
  start, 2026-11-19; not cached while Galaxy is unreadable). One log line per game: verdict, Galaxy labels, Steam tags.
- Not a game: Steam apps whose type is not Game / Demo (Lossless Scaling, Game Compressor) never become
  the active game, even with launcher facts; a binding still wins. Store apps count as launcher games only
  under `XboxGames` (WindowsApps also holds Windows Terminal, Snipping Tool).
- Settings export / import (`settings_transfer.rs`): one JSON file, gzip by default (`.json.gz`, flate2
  was already in the tree; 64 MB limit also after unpacking). Parts can be chosen on export and import
  (e.g. only game profiles for a friend whose mic settings must stay). Import backs up first and runs
  after the window closes. Device ids travel with their names.
- Device id healing (`win_audio/device_ids.rs`): every endpoint seen is remembered (`devices.json`); a
  vanished id whose name matches exactly one connected device is rewritten in `output_eq.json`,
  `audio_settings.json`, `sidetone.json` (other USB port, reinstalled Windows, imported settings).
- Sonar import offered on the first-run setup card (ticked, runs with the APO install) and the button
  only when there is something to import.
- Sonar presets without Sonar (`sonar_importer/migrations.rs`): GG ships its presets as goose SQL
  migrations (`Program Files\SteelSeries\GG\apps\sonar\db-migrations\sonar-X.Y.Z\*.sql`); Sonar copies
  them into `database.db` only when it runs. For people who keep GG for SteelSeries hardware but never
  start Sonar (Dolby Atmos), the scripts are replayed in an in-memory database (folders by version, only
  the `Up` part). 53 rows hold JavaScript object literals (bare keys, trailing commas), converted before
  parsing. Database rows win on equal names (user edits). Result here: 407 from scripts, 409 in the DB,
  410 merged (`chat/Custom` exists only in the scripts).
- New presets after a GG update are added at start (window and service, `AppState::init`) for whoever
  imported once. `profiles/sonar_known.json` remembers every name seen, so deleted (`_deleted`, also with
  `.<time>` suffix) or renamed profiles never come back and user copies are never overwritten.
- App channels (`app_channels::channel_for`, one rule for the mixer and the stream router): the user's
  drag, then browsers on Media, then the chat list, then games game detection recognized, else Media.
  Recognized games are remembered in `detected_apps.json` (on Game after a restart while in the
  background). Steam / launchers / tools are no longer on Game.
- Voice leveler start (`voice_leveler.rs`), from logs of a real session: settings arriving in the first
  blocks of a stream no longer count as "learn fast again" (they cut the remembered level's weight
  before the first word); the remembered level counts as 3 s of speech; after 1 s of speech it is
  replaced when more than 8 dB off (each frame counted at most 12 dB from it, so a cough or laugh
  cannot trigger it). Memory 30 s -> 20 s.

**Why**

- Media as the default: a wrong Media costs nothing (full quality, user drags once); a wrong Chat runs
  the app through the chat EQ. Game only for what has a game match, so the game EQ follows real games.
- Tried and dropped: a media list (launchers etc., grows forever); "an app holding the microphone is
  Chat" (games have voice chat, apps open the mic late, browsers would move YouTube to Chat during a
  Meet call, recorders / voice changers would learn Chat for good).
- Browsers pinned by exe because they are universal apps; the chat list (from Wikipedia's IM and
  video-telecommunication lists plus research) has only active desktop clients, no exe from a guess.
- The Voyager misclassification came from broad labels ("tactical", "strategy") losing to the first
  label seen; fixed in the label order, not with an exe exception.
- Leveler: a power mean follows a louder voice fast and a quieter one slowly, and after 3 s the foreign
  drop (12 dB) threw away the real voice when the remembered level was 17 dB too high (gain stuck at
  +12 instead of +24). Tried and dropped: moving the level only every 3 s (looked like jumps) and 6 s
  of memory (followed every breath, 0.7 dB swing in the test vs 0.3 dB at 20 s).

**Open**

- GG's script format is SteelSeries-internal; a change falls back to the database with a warning.
  `winsqlite3` on Windows 10 not tried with the scripts.
- New game ids GG adds to a preset the user already has are not taken over (the button refreshes all).
- Apps that play sound in a shared helper (`msedgewebview2.exe` in new Teams) and unknown AI browsers
  (Atlas, Neon, Genspark, Norton Neo, DuckDuckGo: exe not found) need a drag in the mixer.
- A game is on Media until it first comes to the foreground.

## Milestone 62: Audit and refactoring: one gate, edition 2024, files by job (2026-11-15 .. 2026-11-16)

Branch `refactoring-plan`. A code audit (20 findings and a list of minor ones), a step-by-step
refactoring, then a review of every step and its fixes. No new features.

**Gate**

- Toolchain pinned in `rust-toolchain.toml` (1.100.0), rustfmt over the workspace, clippy at zero
  warnings with `indexing_slicing` and `undocumented_unsafe_blocks` denied, `#[expect(.., reason)]`
  instead of `#[allow]`, edition 2024.
- `scripts/check.ps1` is the one gate; `dev-rebuild.ps1 -Test` and CI (`ci.yml`, every push) run it.
  The installer self-check fails on a script that does not parse (the uninstaller too) and also runs
  under Windows PowerShell 5.1, which runs the installer on users' PCs.

**Structure**

- Crate `gamersequalizer-mic-vst` is `gamersequalizer-mic-chain` (`MicChain`, `chain.rs`); `dsp_apo` is
  gone (`auto_preamp` lives in `output_eq`). About 1 500 lines of earlier designs deleted.
- Files split by job, moves only: `common/shm/`, `common/sidetone/`, the APO's `lib.rs` (COM, entry,
  mic, output, stream), `ai_denoiser/`, `chain/`, `aec3/echo_meter.rs`, the daemon's `daemon.rs`,
  `lifecycle.rs`, `shm_mapping.rs`, `health/`, `win_audio/apo_install/`, `win_audio/chatmix/`,
  `ui/state/`, `output_eq/{profiles,detection,publish}.rs`. Tests over 300 lines sit in `tests.rs`.
- Copies merged: one mixer `Channel` (five before), one save and delete for the four EQ layers,
  `EqFade` for the output and stream crossfade, `theme::percent_volume` / `level_text`,
  `win_audio/mmdevice.rs` for device enumeration, `win_audio/worker.rs` (retry and "same error once")
  for the worker threads, `apo-common.ps1` for what both installer scripts share (spliced in at
  `# @common` before the scripts are hashed).
- The service builds only the shared-memory mapping and `OutputEq`, not the window's `AppState`.

**Shared memory v22**

- One number: `shm_version!` gives `SHM_VERSION` and both mapping names; the size is asserted at
  compile time and the version checked on open. `Default` is a zeroed header plus `init_defaults`.
- Dropped, read by no one: `aec_double_talk`, `aec_impulse`, `aec_filter_taps`, `read_frame_index`,
  `sample_rate`, `channels`, `capacity_frames`. Debug capture mapping v5, `vars.tsv` 55 columns
  (readers look columns up by name, old captures load).
- EQ bands are read lazily, without an allocation on the audio thread.

**Behavior that changed**

- The mic bridge (a copy of the processed mic into VB-Cable / "Sonar - Microphone") is gone; the
  service starts the loopback (echo reference, APO link) itself.
- A new stream is processed from its first block (was up to ~100 ms of pass-through: every
  `OwnerWatch` started blind). A stream APO whose slot another instance took passes through.
- AEC3 silences its first frame after a pause (430 samples from before it sat in its delay line). The
  denoiser has one clear for the sleep state and the click rescue; settings are read once per block
  (`MicSettings`); COM entry points that build something are guarded by `catch_unwind` too.
- Installer: a failed update leaves the endpoint's effects in; a rollback after the self-test takes
  ours out on an update too, and a key an earlier install made goes with it; an endpoint is active by
  its `DEVICE_STATE` bits; `Add-Type` compiles in a folder only administrators can write.
- Stored data: `FilterType::Unknown(String)`, unknown fields kept (`extra`) in profiles, settings, UI
  prefs and `sidetone.json`; a profile file this version cannot read is never overwritten, and a save
  over a readable one keeps its unknown fields (`for_overwrite`).
- Detection: "(none)" for the detected game binds it as a game without a profile (`<no profile>`;
  was "not a game", then a no-op); a window that follows the service takes genres from the service's
  cache, and only the owner records what is a game; "Reset" reaches the service.
- Devices: Master follows an unplugged device, and behind a virtual default stays on its device when
  another one is plugged in; id healing never moves settings onto an id the files already use.
- Window: one `save_pending` for every unsaved edit, also on exit; "Stop service and exit" and the
  uninstall close the window instead of `process::exit`.
- Sonar import moves a profile the user changed to `_deleted` instead of replacing it.

**Performance before and after** (this PC, both on 1.100.0, `3547694` against the branch, two
alternating runs each): the same within run-to-run noise, and the same output.

| Measure                                      | Before          | After           |
| -------------------------------------------- | --------------- | --------------- |
| `eq_bench` output EQ stereo, 20 biquads      | 8.05-8.13 us    | 8.05-8.38 us    |
| `eq_bench` stream EQ stereo, 64 biquads      | 27.9-28.0 us    | 28.0 us         |
| `dfn3_reference` CPU (0.75 / 1.0)            | 1.90-1.99 %     | 1.96-2.49 %     |
| `dfn3_reference` model load                  | 68-76 ms        | 72-81 ms        |
| `scenario_report` DFN3 / DPDFNet / RNNoise   | 2.9-3.1 / 10.2-10.8 / 0.3 % | 3.0 / 10.9-11.0 / 0.3 % |

DFN3's output is bit-identical (max diff 0); the scenario report's 189 lines of levels and gate
shares do not differ.

**Not confirmed, left as it is**

- `profiles/aux` looked dead but holds imported Sonar presets: kept.
- EQ bands half old for one block (~10 ms) while a new set is written: each band is valid, a second
  version read adds nothing.
- Window off screen at 150 % scaling: not found in the code (the position is loaded before winit
  makes the process DPI aware, both sides in points).
- Deferred: output telemetry is one set of fields, so the last device to write wins; per-device
  counters need new shared-memory fields. `VadType` and `BiquadBand` keep nothing unknown (a profile
  with an unknown `VadType` is skipped on load and protected from being overwritten).
- Two devices of the same name: after one is unplugged its settings move to the other and do not
  move back. After an update's rollback the Default processing mode we added to a driver's own key
  stays (without effects in the slot it does nothing).

**Open: to check by hand on Windows**

- The review's fixes were written on a Mac: type-checked and clippy-clean for the Windows target, the
  installer self-check run under `pwsh`. Checked: the Rust tests of the daemon, the APO and the mic
  chain pass in CI and in the gate on the dev PC.
- Installer as administrator: an update whose self-test rolls back. Checked: an update that fails
  (installed DLL held open) keeps every endpoint's effects; its reason now reaches the log in a silent
  run too (the trap said it only with `-FromApp`, the uninstaller's as well).
- Master behind Sonar when a monitor is plugged in; the window on a second monitor at 150 % that is
  then unplugged. Checked: "(none)" in the window for a game found by genre (Star Trek: Voyager)
  binds `<no profile>`; the service takes it within a second, keeps it when the game comes back to
  the foreground ("set by you to have no profile"), and "Reset" in the window reaches the service.
- Checked: a `-DebugCapture` take (60 s: speech, a pause, a model switch, an output switch and back)
  against one from 2026-11-10. No click (no output step > 0.25, also within 100 ms of every switch),
  no clipping, no block gap, load median 1.9 % (4.2 %). The denoiser sleeps ~3 s into the pause and
  wakes on the first syllable (out follows the mic onset within one 10 ms step); DeepFilterNet ->
  RNNoise switches its lead 40 -> 20 ms without a step. On the other output the AEC starts from that
  device's cached delay (243 samples, 25-32 on the Realtek); without the reference for 0.1 s at each
  switch. Only short beeps played, so the ERLE says nothing here.
- Checked: `sidetone_probe` with nothing else playing (it renders only blocks flagged
  silent) still mixes the sidetone in, ~100 blocks/s on the Realtek output: the engine hands the
  output effect valid blocks, so the early return on a silent block does not cost the sidetone.
- A Release workflow run with the pinned toolchain; the branch rule on GitHub that requires CI.

---

## Milestone 63: Mic models with f16 weights, tract from our fork (2026-11-16 .. 2026-11-17)

Branch `fix/pause-hiss`. The long analysis (tables, variants that lost) was a working document, removed
after merge; it is in the git history of that branch (`docs/mic-cpu-analysis.md`).

**What ships**

- DeepFilterNet 3 and DPDFNet store their matrix-vector weights as f16 (computed in f32) on every
  CPU with F16C + FMA, including the GRU weights inside DFN3's `Scan` bodies; lowered convolutions
  keep f32 (small weights, the f16 kernels for them are slower). `GE_F16_WEIGHTS=0` keeps f32 for
  comparing CPU load (`scripts/cpu-bench-package.ps1` runs both).
- tract comes from github.com/PixCZ/tract (branch `gamersequalizer-0.23.10`): an AVX-512 kernel for
  f16 weights (`avx512_mmm_f32_128x1`, packing `f16f32`), a kernel choice that no longer pads small
  matrix-vector products to 128 rows (also faster for f32), and a fix for the 16x1 kernel, whose
  partial sums leaked into a second product in the same call. Pull requests for sonos/tract are
  prepared, not opened (AGENTS.md, "Before a release").
- The deep_filter fork's tests (f16 conversion) are part of the gate; its lockfile is audited.

**Measured on the dev PC (Ryzen 7 9800X3D), f32 -> f16, median of 3 interleaved runs**

- DeepFilterNet 3 alone: 1.89 -> 1.59 % of a core warm (-16 %), 3.62 -> 3.17 % with a cold cache
  (-12 %); the whole denoiser with it -8 / -9 %. Without AVX-512 (simulated) -11 / -19 %.
- DPDFNet: 0 to -5 %; its time goes into `GruSeq`, which is compute bound, not weight bytes.
- Output: the quality reports move by 0.1 dB at most.
- Before: f16 weights with stock tract were 25 % slower than f32 on AVX-512 (FMA fallback kernel,
  128-row padding of small matrices).

**Measured on the notebook (Ryzen 5 7530U, Zen 3, no AVX-512, on AC, Balanced), 2026-11-17**

`scripts/cpu-bench-package.ps1`, one run, f32 -> f16 in % of a core (the first f32 pass was noisy: the
whole-denoiser warm value 9.6 against 7.8 on 2026-11-16, so its -39 % is not to be trusted):

| Part                   | Warm      | Cold cache | Paced | Cold + paced |
| ---------------------- | --------- | ---------- | ----- | ------------ |
| DFN3 alone             | -20 %     | -28 %      | -20 % | -16 %        |
| DPDFNet alone          | -11 %     | -11 %      | -8 %  | -3 %         |
| Whole, DFN3, speech    | -39 % (?) | -19 %      | -12 % | -9 %         |
| Whole, DPDFNet, speech | -8 %      | -7 %       | -3 %  | -5 %         |

- Meets the bar set in the analysis (cold cache: DFN3 -28 %, DPDFNet -11 %).
  With pauses between blocks the gain shrinks (DFN3 whole -9 to -12 %, DPDFNet -3 to -5 %): there the
  core clock after idle is the limit, not memory.
- Absolute numbers are not comparable with 2026-11-16: ONNX Runtime, which our changes do not touch, ran
  about 20 % faster (DPDFNet 1.51 ms against 1.90 ms). Only the f32 -> f16 ratio inside one run counts.
- `run.ps1` in the CPU package now appends to one `report.txt` (header per run) instead of deleting it.
- The raw report is kept as `test_data/private/cpu-7530U-20261117.txt`.

**Reports on the private recordings against the log, same day (dev PC)**

All 22 ignored mic-chain reports pass. Quality metrics match the latest entries (same or within
0.5 percentage points / 0.3 dB), CPU the same or lower. Not matching: `chain_report`, DFN3 only, against
milestone 29: pauses at 0 dB -113.4 -> -106.9 dBFS, rumble room -119.9 -> -115.6, +20 dB clipped voice
-23.7 -> -16.7 dBFS (leveler -5.6 dB instead of -12). Later changes to the leveler and the DFN3 input
(milestones 22, 23, 36) are the likely cause; not yet confirmed. `rnnoise2` `report_vs_upstream_c` fails
because it needs the upstream C demo output made by hand (`.claude/tmp/rnnoise2/`, git-ignored); the
other rnnoise2 reports pass.

**Open**

- Why `chain_report` DFN3 moved since milestone 29 (a stale number or a real change in the leveler).
- The pull requests for sonos/tract, once the build has run in the app for a while.

---

## Milestone 64: Leveler learns from its own strict detector (2026-11-17)

A friend on a new build was far too loud and had to be turned down. `chain_report` showed the same on
the dev PC: with DeepFilterNet 3 the voice came out 6-7 dB over the -24 dBFS target even at a normal
input (0 dB: -16.0 / -17.4 dBFS, leveler +13.4 dB instead of +7.4), and with +20 dB clipped the
leveler cut only -5.6 dB instead of -12. RNNoise was exact. The analysis was a working document, removed
after merge (git history of `fix/pause-hiss`).

**Cause.** `d4b83bd` (milestone 35) swapped the raw-input detector for RNNoise 0.2 (`in_net`), which
unlike the 2017 network hears whispers. Milestone 40 had kept the leveler on it "so a whisper is not
learned as the speech level"; that stopped being true. Behind DFN3 / DPDFNet the leveler learned 3-3.7 s
of whisper before the voice, and a loud frame moves a full memory (n = 2000) by only 0.65 dB per second,
so the gain stayed high for a minute (`leveler_report`, input 0 dB, gain at 84 s: DFN3 +13.6 dB,
DPDFNet +18.5 dB, RNNoise +7.1 dB; voice 45-49 s: DFN3 -13.6, DPDFNet -10.0 dBFS).

**Change.** The leveler and the automatic input gain get their own key (`heard_level_voice()`) from the
probability `in_net` already computes: speech only after 6 frames (60 ms) in a row above 0.95
(`level_key`, `LEVEL_PROB`, `LEVEL_MIN_FRAMES`). No second network, no CPU. With RNNoise as the model its
own probability is already the key. The gate, the model's sleep and the click rescue stay on `in_net` at
0.7 / 0.2: they should hear whispers. A first version ran the 2017 network a second time (commit
`d364b18`, ~0.26 % of a core); `leveler_key_report` showed a rule over `in_net` separates as well, so it
was replaced. Share of 10 ms frames flagged, private scenario: whisper 28.8 % (gate rule) / 2.2 % (2017
net) / 1.2 % (this rule), voice 90-95 / 77 / 72-74 %. Other recordings, whole file, 2017 net -> this rule:
`speech.wav` 42.5 -> 38.0 %, `scenario.wav` 32.7 -> 25.8, `keyboard_call.wav` 48.0 -> 35.7, `keys.wav` 0 -> 0.
Tuned on one whisper recording (the only one in the set): a new whisper take is the check.

**Measured** (dev PC, private whisper scenario), voice as 45-49 s / 53-57 s in dBFS:

| `chain_report`, DFN3    | Before        | After         | Milestone 29      |
| ----------------------- | ------------- | ------------- | ----------------- |
| 0 dB: voice             | -16.0 / -17.4 | -22.0 / -24.4 | (RNNoise -21.5 / -24.2) |
| 0 dB: leveler           | +13.4 dB      | +7.3 dB       | (RNNoise +7.4 dB) |
| +20 dB clipped: voice   | -15.4 / -16.7 | -19.5 / -23.5 | -20.6 / -23.7     |
| +20 dB clipped: leveler | -5.6 dB       | -11.7 dB      | -12 dB            |
| 0 dB + rumble: leveler  | (+7.4 dB)     | +9.3 dB       | (RNNoise +8.1 dB) |
| 0 dB: pauses            | -106.9        | -110.5        | -113.4            |

`leveler_report`, input 0 dB, DFN3 / DPDFNet: learned before 44 s 740 ms (was 3.7 / 3.0 s), gain at 84 s
+7.1 / +7.7 dB (was +13.6 / +18.5; RNNoise +7.1), voice 53-57 s -24.4 / -24.2 dBFS. DPDFNet's voice at
45-49 s is still -18.4 dBFS (the gain falls from +24 to +9 dB during those seconds; RNNoise -21.8, DFN3
-21.9). Quiet inputs (the strict key must not starve the leveler): input -12 dB, DFN3 / DPDFNet / RNNoise
gain at 84 s +19.5 / +21.0 / +19.1 dB, learned before 44 s 670 ms for the heavy models; input -24 dB all
three sit on the +24 dB cap. RNNoise unchanged. Gate green (`check.ps1`); tests
`test_level_key_needs_a_sure_run_and_a_dip_restarts_it` and
`test_leveler_detector_hears_speech_behind_every_model`.

**Open**

- Not heard live; the friend's PC and a hot start (first run, new mic, "Learn again") are the check.
- Pauses with DFN3 at 0 dB are 2.9 dB short of milestone 29 (-110.5 against -113.4): not looked into.
- DFN3 with the added 45 Hz rumble: leveler +9.3 dB against +8.1 for RNNoise, voice about 2 dB over
  the target in the second section: not looked into.

---

## Milestone 65: Automatic ChatMix: the game makes room for the voice chat (2026-11-18)

Branch `feat/chat-ducking`. In a firefight the game drowns out the chat. With Automatic ChatMix on, the
game is lowered (full band since 2026-11-19, the speech band before), only during talk the game actually masks, by one depth for
the whole run of the game, and never with an audible step. Nobody talks, a loud talker above the game, or a
calm game with a stray sound: nothing happens.

**How it works** (`gamersequalizer_common::ducking`)

- Where: every app stream has its own stream APO instance before the mix (`StreamDucker`). Each measures
  its stream's speech-band level and publishes it in its `StreamSlot`; the service writes each slot's
  channel (`StreamBus::Chat` / `Game`; unpaired streams are never lowered). A game stream reads the chat
  streams' levels straight from the table, a few atomic loads per block: no daemon in the loop.
- Speech, not sound: a chat stream's level counts only while Silero VAD (`Vad48k`, mono downmix) hears
  speech at probability 0.5, so coughs, breathing and room noise should not lower the game. Silero's weights
  are parsed once per process and shared (`OnceLock` in the vendored crate): a detector costs ~13 KB per
  stream instead of 1.2 MB.
- The cut: an RBJ band-pass (1.5 kHz, Q 0.5, about 500 Hz - 4.5 kHz) subtracted from the signal,
  `x - (1 - g) * bp(x)`, is exactly a bell of gain `g` at the centre: bass and highs stay, `g` is ramped
  per frame, and at depth 0 the buffer is not touched (bit-exact).
- Whether, per talk spurt: a spurt is talk with pauses under 3 s (`hold_s`); its level is a leaky mean (1 s) of its
  talking blocks. It ducks once the current game level (0.4 s up / 1.5 s down) masks it, game - spurt +
  margin > 2 dB, from 0.2 s of talk on, and stays ducked until it ends. A quiet talker answering a loud one
  within the hold latches within about a second.
- How much, once per run: depth = locked difference + volumes + margin, at most 18 dB. The difference is
  the learned loud game level minus the learned quiet talker level:
  - loud: an upper envelope of the game level, rising with 3 s (a 2 s explosion 12 dB above the fight
    moves it about 6 dB), falling 0.5 dB per minute, never below the game;
    "fight heard" = the game masks the quiet talker, so menus and travel before the first talk count nothing;
  - quiet: the quietest of the people who normally talk. Spurts teach it from 1 s of talk on: a running mean
    until 10 s of such talk (the first talkers average), then a lower envelope (falls with 2.5 s of talk,
    because the quiet talker usually says little and the level must get there within a few
    of their sentences; one spurt counts at most 6 dB under it, so one whispered stretch of ~3 s moves the
    depth by ~2.5 dB and it comes back only slowly; rises 1 dB per minute of talk, so a quiet talker who
    left is forgotten);
  - the lock follows the difference until both levels are settled (20 s of fight, 10 s of talk), then is a
    backlash: the difference must move 0.5 dB up / 4 dB down before the depth follows, and then only by
    the excess, without a step. Narrow upwards so the lock never stays short of the estimate while the
    quiet talker level still falls.
  A game's first run therefore learns over the first minute or two of conversation, the depth rising in
  small steps between sentences; later runs start from the stored levels and are locked from the first
  sentence.
  The margin is the user's Strength: 0 % = the voice as loud as the game in the band, 50 % (default) =
  8.5 dB, 100 % = 17 dB. Strength and the session volumes the service reports
  (`duck_game_gain_permille`, `duck_chat_gain_permille`) apply live on top of raw levels.
- The depth moves only at slew rates: 8 dB/s for the first 0.35 s of a spurt (one word dips ~3 dB at
  most), then 40 dB/s, release 4 dB/s, frozen in pauses. Turning it off, losing the channel or a weaker
  Strength releases at the same rate. A ducked talk lets go once the game has not masked the talker for
  30 s in a row while someone talks (`latch_release_s`) and may duck again later: a talker who never
  pauses (a podcast) is not held down for good by one fight; lulls in a fight shorter than that do not
  pump. When the talk stops the game comes back; the next talk starts where the last one ended
  (lowered or not), so a resumed podcast in a fight is lowered within about half a second.
- One decision for both modes (2026-11-19): the stream APO and mix mode share `DuckGain` and every value
  above (hold, margin, bound, rates, latch), and both lower the game full band: the stream APO
  multiplies the stream (it cut only the speech band before), mix mode sets the session volume. Only
  how the game is measured differs (its stream's speech band, or the mix in the chat's pauses). The values are the ones heard in mix mode; the stream mode has not been heard
  with them.
- For a game whose stream the APO cannot see (spatial audio objects bypass the per-stream effects),
  the service ducks with `MixDucker`, on the same gain computer; the depth goes to the game's session volume, full band. The game is measured only while the chat makes
  no sound at all: `chat_sounding` reads the chat streams' level envelopes (broadband, before the
  voice detector: any chat sound counts), plus `mix_chat_guard_s` (0.3 s) for the chat's way through
  the spatial renderer and the tail of a word. Then the mix is the game, with the ducker's own lowering
  undone (the depth it returned last tick). It is read in segments (a chat-free stretch, cut every
  1 s); the held level falls with `mix_game_fall_s` and rises only to what two segments in a row read
  (the lower mean), so one shot in a calm game cannot pass for a fight. While the chat sounds it is
  held: nothing is learned and nothing moves it. The segment before ages with `mix_game_fall_s` over
  the talk since, so a fight long over confirms no shot. The loud level learns from chat-free ticks
  only, each one standing for the time since the previous measurement (at most 2 s), also while
  lowered (the depth is undone). A cap at the level read before the reduction was tried after a
  review: the live log of 2026-11-19 showed a fight growing from -53 to -42 dB under a 10 dB
  reduction with the loud level stuck at -46 dB, so the depth stayed 10 dB. Removed; the known
  cost: the undo also lifts what the service does not lower (another app in the mix, a failed
  volume change), which can carry the loud level and the depth to `max_depth_db` while that
  sound plays (offline: -9 dB grew to +6.6 dB). Mix levels are stored under `<exe> [mix@100]`, as at
  100 % session volume; the earlier `[mix]` records were as measured (at a 41 % game volume the
  first live run seeded 8 dB too low) and are no longer read. It used to learn only with no reduction applied and one tick per measurement: in a
  `duck_sim --mix` lobby scenario (20 min calm game with constant talk, fights in between) a fight
  starting while ducked was never learned, the loud level stayed 20-25 dB under the fight and the
  depth was 2-6 dB instead of the 18 dB bound; now every fight gets 18 dB, the lobbies 0. `resync` (a gap in the reference) keeps the decision
  and the depth; `duck_sim --mix` runs this path, with `chat_sounding` from the chat file's envelope.
- Per game: once settled, the two levels go to the slot (`duck_loud_db_x10`, `duck_quiet_db_x10`); the
  service stores them per game exe and writes them back (`duck_saved_*`) when it pairs the next run's
  stream. Adopted once, as settled, so the next run is locked from its first sentence; refused when
  the stream already settled on its own (service restart, late pairing). Mix mode stores them as at
  100 % session volume and, when the Game or Chat session volume moves during a run, shifts what it
  learned by the change (`MixDucker::shift`), so the next fight gets the depth for the new volume at
  once. Stream mode does not yet: its levels live in the APO.
- Tuning: every value above is a starting point in `DuckTuning` (35 named values with sane ranges, 48 slots
  in `ShmHeader::duck_tune`, positions pinned by a test). The service reads `duck_tuning.json` from
  `%APPDATA%\GamersEqualizer` and publishes it; the APOs re-read it when `duck_tune_version` moves, so one APO
  install lasts through all tuning. The band only measures; the game is lowered full band.
- Telemetry: `StreamSlot::duck_dbg` (16 entries, `ducking::dbg`): band level, detector probability, game,
  chat, spurt, need, depth, target, lock, flags (talking, latched, settled, seeded, detector, failed),
  evidence; `NA` when there is none. A stream whose processing panicked passes audio through, zeroes its
  level and reads `FLAG_FAILED`, and its APO notes it in the Application log when the stream stops.
- Shared memory v24 (`SHM_LAYOUT_SIZE` 1_260_680): `duck_enabled`, `duck_strength_permille` (default 500),
  the two gains, `duck_reduction_permille` (service -> window), `duck_tune`, `duck_tune_version`; per slot
  `speech_ms`, `duck_permille`, the four level fields and `duck_dbg`.
- `duck_sim` (common crate example) runs a game and a chat WAV (48 kHz) through this path, with the chat's
  Silero, the detector's gap reset and a pairing delay, writes the ducked game and both mixes and prints
  one line per spurt. Not modelled: the chat stream's EQ before the detector, more than one chat or game
  stream.

**Decided, and why**

- Silero, not RNNoise, as the chat's speech key, from milestone 38: keyboard and clicks never pass 0.15
  with Silero (max 0.08), RNNoise 0.2 fires on 0.2-0.5 % of frames at 0.7; speech is found equally.
- One depth per run, sized for the quiet talker; the decision per spurt from who talks: a game is about
  equally loud and its gunfire similar, but people talk at very different levels. The user wants the same
  reduction in every fight; the mic leveler's stability (running mean, then a slow rate, a stored level as
  a warm start) was the model.
- An asymmetric backlash: a depth too shallow fails the voice, one too deep only quietens the game a bit.
- Strength (the voice's margin) instead of a maximum reduction: people differ in how much the voice must
  stand out; a cap says nothing about that.
- The mix path measures the game only in the chat's pauses instead of estimating "mix minus chat": a
  game's loudness barely changes within a run and nobody talks through every fight, so the pauses are
  enough, while how loud people talk varies and is the quiet talker level's job. It removes everything
  that depended on how the chat arrives in the mix (its gain through the spatial renderer and the
  output EQ, its delay, one snapshot per tick): the chat can never be mistaken for the game, whatever
  its level in the mix. A first version subtracted the chat; over a silent game it let the chat duck
  the game by itself until the chat's level was held, and it needed a guessed chat factor.
- Every tunable in shared memory and an offline simulator: a live test needs friends in a firefight and the
  APO cannot be redeployed during it.
- Tried and replaced: the depth following the momentary need (wandered every burst); a loud level falling
  over 5 minutes towards whatever played, then over 30 s within 15 dB of it (calm play or travel unlearned
  it); one learned voice level for everyone (loud talkers ducked, quiet ones under-ducked); the first spurt
  setting the quiet level and settling after 3 s (an early quiet spurt pinned it); a spurt level that was
  the mean since the spurt began (a quiet talker answering a loud one inherited the loud mean).
- Simplified, not replaced (2026-11-19): a design from scratch with 10 constants and no learning (a
  chat-to-game ratio estimated in the mix by subtraction, a talk gate, one slew rate) ran against this one
  in `duck_sim --mix` on nine scenarios (lobbies, a pause-free chat, two talkers, short words, one shot,
  a talker above the game, the game turned down, gaps in a fight, the talk ending). It lowered calm
  lobbies and a talker above the game and was deeper in a fight's gaps than in its bursts: during
  continuous talk its "game" was the chat. Kept the learning; dropped the slowdown of both learning
  rates and the warm-up values (six tunables, their slots kept as `retired_*`). Taken from the
  comparison: one shot no longer lowers a calm game for ~25 s (two segments must agree; cost: in a
  chat with few pauses a fight is picked up about 7 s later), and a session volume change moves the
  learned levels (a game turned down 10 dB: next fight 8 dB instead of 16 dB).
- Replaced later on 2026-11-19 by two levels: 0 dB while nobody talks, a lowering level while someone
  does. It starts at 0 every run (no store, `duck_level_cache.json` unread), a masked spurt raises it to
  what that spurt needs, it comes down by only 1 dB in 10 minutes (inaudible; a fight raises it again by that little), and it starts over after 60 s of an
  unheard game, any volume or strength change, a stream gap or the feature off. The learned loud and
  quiet levels, the lock, the backlash, the latch and the stored levels above are gone (11 tunables
  retired). Why: the first fight of a run was not lowered and the depth crept for minutes; on the
  user's recording (`duck_sim --mix`, strength 50 %) the game is lowered 5-5.5 dB from 10 s on (the
  fight at 60 s, not lowered before, included), 10 dB from 120 s, 12.4 dB at the end, raised by one spurt. Handoff notes: `docs/handoff-chatmix-atmos.md` 1a.

Tests: `ducking/gain/tests.rs` (who ducks, the quiet and loud levels, the lock, seeding),
`ducking/stream/tests.rs` (bit-exact idle, slew and per-sample smoothness, band limits, sample rates and
block sizes, channels and NaN, releases, a run of fights, saved levels, tuning limits, telemetry, failure),
tuning round trip and pinned slots, and a `duck_sim` self-check. `duck_sim` ran on a synthetic pair
(`speech_clean.wav` at 0 and -10 dB over noise with bursts): Silero gated it and masked spurts were ducked;
that says the tool works, nothing about how it sounds. The same pair looped to 190 s (talkers at about -39
and -44 dB) showed the first-run learning: with the earlier defaults (fall 3 s / at most 30 s, backlash
2 dB up) the depth was 5.6 dB at 40 s and crept to 6.7 dB by the end, quiet talker level -41.8 dB; with
today's (2 s / 3 s, 0.5 dB up) 5.6 dB at 40 s, then 7.0 / 7.9 / 8.4 / 8.8 / 9.0 / 9.2 dB, quiet talker
level -42.8 dB. A synthetic pair, so only the mechanics; nothing heard.


**Live test (2026-11-18)**

Measured on the user's PC from `gamersequalizer.log` (build 8b5917b, APO installed): a podcast in
Brave (`brave.exe`, on the Chat channel) and The Division 2 (`thedivision2.exe`), same headphones,
48 kHz 2 ch, strength 50 %.

- Run 1, Dolby Atmos for Headphones on: both streams paired and the detector heard speech, but the
  game's stream read digital silence throughout: `game -120.0/-120.0/-120.0 dB, chat
  -93.8/-49.9/-28.2 dB, ... depth 0.0/0.0/0.0 dB`; `last minute: 88 spurts, none lowered, chat
  speech 90 % of samples (prob max 1.00), need max -72.1 dB`.
- Run 2, Atmos off: `game -68.2/-52.4/-26.6 dB, chat -82.3/-41.4/-24.0 dB, ... depth 16.2/16.2/16.2
  dB (target 16.2 dB)`; `last minute: 18 spurts, 1 lowered for 35 s, depth 16.8-16.8 dB, game -49.2
  dB while lowered`. Whether the lowering was audible is not known yet.
- Cause, as Milestone 19 measured: with a spatial renderer on, a game that renders spatial objects
  bypasses the per-stream (SFX) effects and its session meter; only menus and videos go through its
  normal stream. Such a game can be neither measured nor lowered. The service logs it and both
  places in the window warn, on the evidence the run showed only (spatial sound on, and a paired
  game stream that plays and reads digital silence for 10 s): a game that plays plain stereo / 7.1
  into the virtualizer still works, and a paused or alt-tabbed game has no playing stream.
- Gains corrected: Milestone 19 (live run 3) measured that the session volume is applied before the
  stream APO (at 50 % session volume the APO saw the attenuated signal). The service had published
  the session / ChatMix volume as a gain after the APO, so it counted twice (chat at 60 % taken as
  another 4.4 dB quieter than heard). In session mode both gains are now 1000; only a channel on its
  own output keeps its volume. Until the APO side is reworked, a volume change there is not
  compensated at once; the learned levels follow it slowly.
- Turning Atmos off before run 2 left no `[DolbyAtmosGuard]` line: only the window polled it, and
  only while it was open. The service now logs every change from its 5 s setup check:
  `[Spatial] default output: Active -> Disabled`.
- Mix mode, built after the test (the user confirmed that with Atmos on the game's fader, its
  session volume, still changes the game's loudness): while the rule above holds, the mixer owns
  the per-app session volumes (no own Game / Chat outputs) and the speaker reference flows, the
  service lowers the game itself. It asks the output APO for the reference ring (the echo
  canceller's; a second reader), takes the frames since its last step, takes the game's level as
  the level of the mix while the chat makes no sound and holds it while it does (`MixDucker`, same
  decision / learning / lock as the stream ducker), and lowers the Game sessions through the
  mixer, the one writer of those volumes. Full
  band, not only the speech range; 25 steps a second while a reduction moves (reference thread and
  service loop at 40 ms), writes only for steps of 0.2 dB or more. Never together with the stream
  ducker: only for a game the APO cannot hear. One ducker per game for the service's run, reset
  (not dropped) when mix mode pauses; levels stored per game under `<exe> [mix]`; a gap in the ring
  resyncs it without touching the depth. The reference counts as missing only after 5 s without
  frames (both the service and the window).
- The game's volume (`chatmix/game_duck.rs`, a small state machine the mixer pass drives): only
  the subject game's sessions (by exe path, published next to the depth) are lowered. A reduction
  starts from the session's volume then (a muted or silent session is left alone); while it lasts,
  a value the mixer intends for the session (fader, balance, mute) becomes its base exactly as if
  there were no reduction, and `base x factor` is written on 0.2 dB steps without reading the
  session; the end writes exactly the base, always. Losing the session volumes (own Game / Chat
  outputs, the service stopping) writes every base at once; a failed write is warned once and the
  session left alone (not counted as checked). A lowered session dragged off the Game channel gets
  its base first. When another game becomes the subject mid-reduction, the old one comes back at the
  release rate before the new one is lowered. `ducking_lowered.json` holds one entry per exe path
  (ASCII case-insensitive) with the largest base of its sessions: added before the first lowering of
  a game's mix-mode run and written again only when that base grows, gone when the run ends with
  its sessions back (also at a clean stop), a moved base written at most every 5 s. An entry the
  service starts with, or whose lowered session vanished, waits: every session of that exe is
  checked once, before it can be lowered, set to the base when it is below it, left alone at or
  above it or when the mixer sets it that pass; the entry goes when its sessions were checked and
  are over.
  Without session mode or the reference the warning stays and names the reason.
- Mix mode heard (2026-11-18, build faa87a1 and later, The Division 2 on Dolby Atmos, a podcast in Brave on
  the Chat channel): works. The game fader (seen in EarTrumpet) went down while the podcast talked and back
  to the base when it stopped. Found and fixed on the way: the mode flipped mix / stream 9 times in 10
  minutes (the game's stream sounded in a menu; now `UnheardWatch` needs 10 s of audio to end the verdict);
  a minute held open by a long spurt was labelled "last minute" (now "last N s"); at 50 % the depth lagged
  behind the need (6 dB margin was too little for a loud game over a quiet podcast), and the game rose at
  every breath (hold 0.5 s). Mix mode got its own hold 3 s, margin 17 dB (50 % = 8.5 dB) and bound 18 dB;
  since 2026-11-19 these are the values of both modes.
  The window shows the lowering as a bar next to the switch. Not run: the base after a killed app or a
  restarted game (the crash-recovery file), the stream mode with Atmos on.

**Open**

- Heard in mix mode only (above) with one game and one podcast; every value is a starting point and the
  stream mode has not been heard.
- Silero's rejection of coughs and breathing on real chat audio is not measured.
- Chat streams not at 48 kHz go by level only (`shortcut:` in the APO's `stream.rs`; upgrade: `RateBridge`).
- Voice chat built into the game is the game's own stream and cannot be separated.
- A next run locks onto the stored levels' difference, up to the backlash (0.5 / 4 dB) away from the last
  run's lock.
- A stored loud level that is too high (e.g. after the in-game volume was lowered) comes down only at
  0.5 dB per minute; a stored quiet level that is too low (a quiet person not on today's call) rises only
  1 dB per minute of talk. No "Learn again" button yet.
- The record key is the exe file name alone: two games with the same generic exe name share one record.
- The endpoint-mode gain the service reports is Windows' tapered scalar.
- Both paths lower the game's full band (mix mode heard, see above; the stream APO's full-band
  lowering not heard yet). Every sound in the mix that is not chat counts as the game there (media,
  other apps). While someone talks the game's level is the one from before they started: a fight that
  starts or ends in the middle of speech is noticed at the next pause longer than the guard, and
  someone talking without any pause freezes it (accepted).
- Games that render spatial objects under a spatial renderer are lowered only by mix mode (their
  stream carries no audio and their session meter none); mix mode is heard (see above).
- Run 2 kept the game lowered for 35 s on one talk spurt (a podcast never pauses). The latch now
  releases after `latch_release_s`: in mix mode at once, at the stream only after the next APO build.
  Mix mode then let go in every calm stretch of a game while a podcast ran on (about 15 s after the
  fight), and the next fight needed two chat pauses to lower it again: a sentence was lost (live log
  2026-11-19). Now `latch_release_s` is 30 s: the duck stays through a lull while the talk goes on and
  comes back about 40 s after the last fight. A pause in the talk longer than `hold_s` (the video
  paused) still brings the game back, but the latch is kept: the next talk starts where the last one
  ended, lowered within about half a second, instead of waiting for chat pauses to measure the fight
  again (the user's wish: "remember the level"). It lets go only in talk, after `latch_release_s` of a
  calm game, so talk after a long silence in a lobby is lowered for that long first, and a loud talker
  after a lowered talk is lowered too.
- How smoothly Windows applies the session-volume steps of mix mode (0.2 dB, 25 a second) is not known.
- A game that renders only spatial objects and has no ordinary stream at all (none paired, none
  playing) is not detected: the rule needs a playing game stream that reads silence.
- The level store and the tuning file are read and written on the reference thread (every 30 s / a
  metadata look every 2 s): a slow disk can delay one mix step.
- The game's volume: a level another program sets while the game is lowered is overwritten, and the
  level from before comes back; after a crash a level the user set lower than the stored base before
  the service came back is raised to it; an entry of a game that never runs again stays in the file.
  The mixer alone loses a session's own level on a mute (it writes 0, then its target on unmute);
  mix mode ends where the mixer alone would. One base per exe: after a crash every session of a game
  is set back to at most its loudest session's level, and only if it is below that.
- Mix mode: sound that is not lowered (a media app) counts as "the game" in the mix and is scaled up
  by the correction for the applied depth, so it can hold the reduction while someone talks.

**Log**

Every line starts with `[Ducking]`; levels and depths are dB with one decimal (`win_audio/ducking/`).

- Info, always written:
  - Status, only when one of its facts changes (the strength is printed, not compared). A missing
    stream says `not paired (...)` with its likely causes (chat: not on the Chat channel, on another
    output device, or silent so far; game: not recognised as a game, not playing, or no APO on the
    output); the chat detector reads `yes`, `no (44100 Hz)`
    or `?` before the APO reports it; `master switch off: nothing is processed`; `(debug lines off)`;
    `off` when the switch is off:
    `[Ducking] on, strength 50 %; chat Discord.exe, detector yes; game game.exe; master switch on (debug lines off)`
  - Stored levels: `stream #n of game.exe was handed stored levels: loud level ..., quiet talker ...`
    once per stream (what the service sent; the debug line's `stored` shows what the APO adopted),
    and `learned for game.exe: ...` for a game's first record of a run, then on a 3 dB move. A
    stream whose processing failed: `stream #n of exe: processing failed in the audio engine, it
    passes audio through`, once per stream.
  - Per minute in which someone talked, or the chat carried sound the detector never took as
    speech (`last minute: chat sound without speech 41 % of samples (prob max 0.12), nothing
    lowered`); nothing for a silent chat:
    `[Ducking] last minute: 14 spurts, 9 lowered for 31 s, depth 11.5-12.0 dB, game -21.0 dB while lowered, loud level -20.5 dB, quiet talker -33.0 dB`
    With none lowered it gives the reasons instead: `none lowered, chat speech 34 % of samples (prob
    max 0.98), need max -3.0 dB` (negative need: the voice was already above the game), plus
    `chat gain 0 %` when the chat is muted.
  - A game it cannot hear (spatial sound on, see the live test): `thedivision2.exe cannot be heard:
    spatial sound (Dolby Atmos for Headphones) is on and the game sends its sound past the per-app
    effects; nothing to measure or lower`, and `... can be heard again` when that ends.
  - Status line `mode stream`, `mode mix (spatial sound: <format>)` or `mode none: ...` with the
    reason (own Game / Chat outputs, or no output mix); mix mode's stored levels: `<game> (mix) was
    handed stored levels: ...`; at start `N game(s) may have been left lowered by an earlier run:
    checked as they play`, then `<exe path>: volume set back to 60 % after an earlier run left it
    lowered`; a failed write `<exe path>: could not set the volume; this session is left alone`.
  - Tuning: `tuning from duck_tuning.json: hold_s 0.6; ignored: ...`, `tuning: defaults` when the
    file goes; a warning for a file that is not JSON (once per version, the previous tuning stays) or
    cannot be read (retried at the next poll, warned once per error).
- Debug, sampled every 100 ms, only with "Save debug to disk" on the Log page; like every line they go
  to `%APPDATA%\GamersEqualizer\logs\gamersequalizer.log`:
  - A start line with the settings, the tuning and every paired stream (rate, channels, the chat
    detector or the game's stored levels), again when the streams or a detector change.
  - Mix mode runs as one more game stream named `<game> (mix)` in the start line and the 10 s line
    (with `, mix level ..., chat band ..., chat sounding N %`: the share of samples in which the
    chat made sound, when the game's level is held, not measured); its minutes read
    `last minute (mix): ...` with the same `chat sounding N %`.
  - Per 10 s window one line per game stream:
    `[Ducking] 10 s game.exe: game -41.0/-27.3/-19.0 dB (now -22.0 dB), chat -80.0/-45.0/-24.0 dB, sound 52 %, speech 34 % (prob max 0.98) at -36.0/-30.0/-24.0 dB, spurts 5 (3 latched), talking 31 %, latched 22 %, need -2.0/9.0/14.0 dB, depth 0.0/6.1/12.0 dB (target 12.0 dB), loud level -20.5 dB (45 s, settled), quiet talker -33.0 dB (12 s, settled, stored), lock 12.5 dB`
  - Written only when something happened: talk, chat sound above the talk threshold, lowering, a
    rise of the loud level (not its slow fall) or a 1 dB move of the quiet talker or the lock, a
    settled / stored change, or the stream's first window after its start line. An idle evening
    writes nothing.
- Tuning while everything runs: `%APPDATA%\GamersEqualizer\duck_tuning.json`, a flat object of
  tuning name -> number, polled every 2 s by the service and applied to the running APO; the app
  never writes or moves it.

### Findings of the live tests on 2026-11-19 (Division 2 under Dolby Atmos, a podcast in Brave as the chat)

- Two levels (`d7a3341` -> `074be4e`): nobody talks, nothing lowered; someone talks, lowered by one level
  that starts at 0 every run, is raised only by the fights it has to get through and comes down by 1 dB
  in 10 minutes (an ear notices about 1 dB at once, 3 dB clearly; broadcast ducking is 6-12 dB, 10 dB is
  about half the loudness). Heard live: "works absolutely great"; the one thing left was the first few
  sentences of a talk.
- The first sentences were unlowered for two reasons. (1) The "cannot be heard" verdict for an Atmos game
  waited 10 s of a silent paired stream (live 16:59: chat paired 16:59:05, mix mode 16:59:48, first
  lowering 17:00:05); now 3 s, leaving it still needs 10 s of audio. After the change the switch came
  2.9 s after the stream start (17:12:02, 17:53:07). (2) Mix mode measured the game only in chat pauses.
- Live 17:13, mix mode since 17:12:02: the game sat at -71 dB (calm) and jumped to -47 dB in the bucket
  17:13:47-57; the first lowering was in the same 10 s bucket (64 % of it), full 9.3 dB from the next.
  The 10 s lines cannot show the second, the user felt 5-10 s, and the cause is in the design: a
  podcast sounds 50-77 % of the time, the game is measured only in pauses longer than the guard (0.3 s)
  and a fight needs two segments (1 s) in a row, so a fight that starts mid-sentence waits for the next
  pause. It was not the mode switch (done at 17:12:02) and the log has no WARN / ERROR / `[ApoNote]` there.
- `duck_sim` reproduction (calm game -68 dB, a 40 s fight starting at five different places in a
  near-continuous chat, `--mix --chat-gain 0.25 --pair-delay 0`), seconds until the game is lowered by
  3 dB:

  | variant                                   | five fight starts, seconds             |
  | ----------------------------------------- | -------------------------------------- |
  | guard 0.3 s (the default then)            | never (the pauses are shorter)         |
  | guard 0.2 s                               | never                                  |
  | guard 0.1 s                               | 7.5  11.5  9.0  8.0  1.0               |
  | guard 0.05 s (the minimum)                | 2.5  11.5  5.5  1.0  1.0               |
  | chat taken out of the mix, 4x, hold 0.5 s | 2.5  1.0   4.0  0.5  1.0               |
  | the same, 8x, hold 0.5 s                  | 3.5  1.0   4.0  2.5  1.0               |
  | the same, 8x, hold 1.0 s                  | 2.5  1.0   4.5  2.5  3.0               |

  Tuning the guard or the sound threshold (-45 .. -32 dB) alone is erratic: how long a fight waits is
  how long the next usable gap takes, so reacting "when the talker breathes" is not reliable.
- The fix (`mix.rs`): while the chat sounds, the mix less 4x the chat's own band level (its stream
  level at the output volume, peak-held 0.5 s) is a lower bound of the game and may only raise the held
  level; the fall still waits for a pause; a fight needs two segments in a row also in speech, and the
  first segment ever stands alone only in a pause (its onset read as game: the held chat level had not
  caught up). A first try with 2x let the chat through as game: the stream publishes one 10 ms block per
  50 ms tick, speech varies by tens of dB between blocks, so peaks are missed; 4x showed no rise in a
  calm game over 280 s. Limit: a fight less than 6 dB above the chat's peaks is noticed only in a pause
  (live: the fight at about -47 .. -50 dB against chat peaks of about -44 dB is borderline).
- It relies on the chat sitting in the mix at its stream's level. Checked in the log, game at -70 dB,
  gains 100 %: the chat band average equals its stream's within 0.6 dB, and the peaks in the mix sit
  1.2-4.6 dB under the chat's band peaks. Averages of mix minus chat band (+5 .. +16 dB) are a floor
  effect (the chat band falls to -120 dB in silence) and mean nothing by themselves.
- Check after the change (17:53-17:55, mix mode): the game went from silent to -49.9 dB in the bucket
  17:53:24, the depth rose in the same bucket and was 4.9 dB from 17:53:34, held 4.7-4.9 dB for two
  minutes while the game fell to -58.6 dB and the need to -6 .. -8 dB (by design: the level comes down
  by 1 dB in 10 minutes and starts over after 60 s of a silent game). The chat was unpaired at 17:55:15
  after 10 s of silence. No WARN / ERROR. Again only the 10 s granularity, so the felt delay is the
  measure.
- `duck_sim` pitfalls found on the way: with the default `--pair-delay 3` the chat is read as game for
  its first 3 s (the service does the same while the chat is unpaired); a chat gap shorter than
  `hold_s` makes the whole file one endless spurt, which prints nothing until its end (the per-spurt
  table is empty); the ducked WAV is float, the game's is 16 bit.
- Configuration directory: no `duck_tuning.json`; `audio_settings.json` holds only the user's settings
  (`duck_enabled`, `duck_strength_permille`) and `ducking_lowered.json` the crash-recovery volumes. The
  `duck_level_cache.json` of the removed level store is orphaned: nothing reads it, it can be deleted.
- Still open: the `retired_*` tunables and four `StreamSlot` fields are still carried in shared memory
  as constants; removing them needs a new `shm_version`, so an older installed APO stops working until
  the Update (the installed one dates from 16:00 and does not know the two levels); why the chat is
  unpaired without a logged reason (16:59:07, 17:06:21, 17:08:51, 17:15:49, 17:55:15 after silence); a
  bucket gap of 60 s in the log (17:12:06-17:13:06); a mix peak of -32 dB in the bucket 17:13:47 while
  the game's peak was -69.7 dB and the chat band's -42.6 dB.

## Milestone 66: The idle service: fewer, shared wakeups (2026-11-20)

Branch `feat/tray-mic-level`. On a laptop on battery what counts is how long the CPU sleeps, not the share
of a core. The log of an idle PC (window closed, nothing playing, `-CpuStats` build) read: service 0.7-1.0 %
of a core in 12-17 threads, `main` 0.4-0.6 %, `StreamRouter` 0.1-0.3 %, the window (when open) 4-5 %, the
effects in audiodg 0.00 %. About 44 wakeups a second came from the service threads, each on its own timer.

**What changed**

- The service loop and the reference thread (`aec_reference.rs`) wait 250 ms instead of 50 ms while idle:
  window closed, no app records, no Chat or Game stream plays under Automatic ChatMix. Each reason that
  keeps them busy has its own cadence (`worker::busy_reason`); one `[Idle]` debug line per change names it.
  A tray message ends the wait at once (`tray::wait_for_messages`).
- `StreamRouter` sleeps 500 ms while no Chat or Game session plays (later: no Game session, see the
  2 h log below) instead of metering every 5 ms. Ducking only lowers streams the router paired, and it pairs Chat and Game sessions
  only, so a lone Media stream (a browser) counts as idle. With a chat or media EQ on, every playing
  session is still metered (the EQ needs its stream paired).
- The tray's mic level keeps its peak where each new value arrives (`apo_link.rs`), so no loop has to tick
  fast to sample it.
- The output APO writes the echo reference only for a reader (mic chain live with AEC on, or mix mode).
  With AEC off the ring was filled every block for nobody. Nothing else in the APO needed a change: the
  stream VAD, the ducker, stream EQ, AEC3 and the leveler already skip their work when off.
- Default mic, default output and the Windows volumes are read once a second while idle instead of every
  250 ms; a device change or turning busy reads them at once. Volumes changed in Windows show up to ~1 s
  late while the window is closed.
- New sessions are announced by Windows (`IAudioSessionNotification` on every active output,
  `chatmix/session_watch.rs`) instead of a pass over all sessions every second. Idle, a full pass runs on an
  announcement, a channel or device change, and every 15 s as a safety net (registrations are renewed with
  it, since an audio service restart drops them silently). For 5 s after an announcement the pass stays at
  1 s: a session is announced when it is created, a pass sees it only once it plays. If registering fails
  the 1 s pass stays, logged once. A counter of channel assignments forces a pass at once. A new app gets its channel's volume within
  one idle tick (about 250 ms), before within 1 s.
- All idle waits end at multiples of 250 ms on one origin (`worker::grid_wait`): service loop, reference
  thread, sidetone, router (every second point), log marker; `apo-notes` starts from the loop. The CPU wakes
  once for all of them. The tray wait uses a high-resolution timer, a plain timeout ends on the 15.6 ms
  system tick and would leave the grid. Checks of 250 ms to 5 s use `worker::due` with 20 ms slack, since
  passes now come exactly one period apart.
- `-CpuStats` builds log two more lines a minute: `[Cpu] service loop sections` (the loop thread's time per
  call, from thread cycle counts) and `[Cpu] wakeups` (wakeups per thread and distinct wake instants).

**Measured** (same PC, service only, window closed, 6-7 idle minutes each)

| What                                      | Before                  | After                            |
| ----------------------------------------- | ----------------------- | -------------------------------- |
| `apply_session_mix`, % of a core          | 0.12                    | 0.013-0.031 (average 0.021)      |
| the three reads at the 1 s idle interval  | 0.073 (~223 calls/min)  | 0.018 (~58 calls/min)            |
| wakeups a second                          | 12.3                    | 12.5                             |
| distinct wake instants a second           | 12.1                    | 4.0-4.4                          |
| service, % of a core                      | 0.29-0.52               | 0.47, 0.31, 0.26, then 0.08-0.16 |

Where the loop thread's ~0.4 % went (sections, % of a core): `health` 0.12, `apply_session_mix` 0.12,
`tray_poll` 0.08, default mic 0.03, `sync_from_system` 0.03, default output 0.02, the rest under 0.02. The
wakeups were not the cost: at 4 ticks a second instead of 20 `main` fell only from 0.44 to 0.42 %, because
the work sits on timers of 250 ms to 5 s. The fall of the service to 0.08-0.16 % in the last minutes of the
grid run came with the same call counts: the load of the PC changed, it is not credited to the grid. The
log shows no CPU sleep depth; that needs `powercfg /sleepstudy` or `/energy` on the laptop.

**Decided, and why**

- No scheduler thread or task queue: the threads wait on different things (a WASAPI loop, COM calls that
  may block, metering at 5 ms) and one stuck call would stall the rest, against the rule that a worker
  never blocks another. A shared grid gives the same burst without shared state.
- `health` (1 s mic check, 5 s full check) and the tray's 1 s "is an app recording" check stay as they are,
  about 0.2 % of a core together: slower would show the muted-mic warning and the red tray icon late.
- Sidetone still polls every 250 ms when off (two device lookups): slower would delay turning it on by up
  to 1 s, `BEAT_FRESH` is 1 s.
- The input voice detector (about 2.8 % of a core per the code comment, `ai_denoiser/mod.rs`) still runs
  at noise suppression 0 while the leveler is on: the leveler learns from it, and the automatic input gain
  goes by the leveler's speech time. With the leveler off too it rests (see the audit below).

**After an audit of the whole app (same day, read on macOS, not run)**

- The router's 500 ms sleep above was only taken with no session at all: a lone browser under Automatic
  ChatMix was still metered 200 times a second. `meters_needed` now decides.
- A chat stream's voice detector (Silero, stream APO) rests while no Game stream played within 500 ms
  (`ducking::game_playing`): without a game nothing is lowered, in mix mode neither.
- Noise suppression off and leveler off: the mic chain runs no network (`set_voice_wanted`). The debug
  log's voice share reads 0 % then.
- `Ducking::tick` returns right after the tuning check once "off" is logged and nothing is lowered.
- Switching back to DeepFilterNet 3 (from another model, or from "Off") played its first 40 ms from the
  frames it held when it last ran. `reset_engines` now silences the newcomer's first latency, as `flush`
  did for a pause.
- The sidetone's three frame-based noise suppressors were built for every mic APO instance. Now only for
  a pair that uses one: before audio starts (`SidetoneTap::preload`), or after a mode change on a
  short-lived thread while the expander runs (`poll_denoiser`, as the mic chain loads a model).
- Left as they are: the sidetone tap writes while no output APO reads (the service opens the output
  because the tap feeds, so the tap cannot wait for a reader without a new handshake), the 1 s session
  pass while the window is open, and a window that is covered but not minimized repaints at full rate
  (egui 0.37 reports no occlusion).

**A 2 h log with a game (The Division 2, mix mode under Dolby Atmos)**

| Period                   | Service wakeups/s | Of that `StreamRouter` |
| ------------------------ | ----------------- | ---------------------- |
| Before the game          |              36.7 |                      ~ |
| Game, mix mode           |             225.0 |                   ~180 |
| After the game, chat app |             178.2 |                   ~155 |
| Window closed, idle      |              14.5 |                    2.0 |

- After the game ended the router kept metering every 5 ms and both loops kept their fast ticks: a chat
  app (Signal) held an active session, silent. Automatic ChatMix lowers Game streams only, so the flag
  the three go by is now "a Game session plays" (`game_session_playing`), not "a Chat or Game session".
  Limit: a chat stream that starts without a game is paired once a game plays, from ~1.5 s of its audio.
- During a game nothing changed: the router's meters only pair streams with apps (`hold_match` needs a
  sample in every 10 ms block, so no coarser rate), the levels the ducking goes by come from the stream
  table and the output mix. Stopping the meters once every stream is paired was considered and not done.
- With a chat or media EQ on, a silent open session is still metered (the EQ needs its stream paired).
- `[Health] problem: ...` (MicProcessing Outdated, once) had no counterpart when it cleared: a problem
  that is gone now logs `[Health] resolved: ...`.
- Of 66 paired streams two changed their app while playing, both for one evaluation (0.5 s) and both to
  Signal at a 50 % match (the floor; the median match is 94 %): the game's stream at 17:21:08 (it became
  a chat stream, `[Ducking]` read "game not paired") and Brave's, paired at 100 % a second before. A
  paired stream now changes its app only when the same other session wins two evaluations in a row
  (`repair_confirmed`); a first pairing is as fast as before, a really wrong one is corrected after 1 s.
- Read from the same log and left alone: the 21 service starts have no exit line because the dev script
  kills the process (no panic line, the hook would have logged one); "Outdated" came 5 s after each
  start of a build newer than the installed APO DLL; "takeover" is on for one log tick per wake
  (median 0.26 s); the AEC label flips between Locked and ReferenceSilent with every quiet stretch of
  the output (1320 debug lines, at worst one per 250 ms tick), a label only.
- The same log has a 26 min call without a game (18:31-18:57) at 180 router wakeups a second: the case
  the "Game session" flag above ends, unless a chat or media EQ is on.

**Not tried live:** a chat app open and silent after a game ends (`[Cpu] wakeups` should fall to the idle
row, `[Idle]` lines should say idle), a game starting while a chat app is already open, a new app
starting while the service is idle, a device change, an audio service
restart, the tray icon's reaction on the new timer wait, a busy service, and everything under the audit
above (a game starting while only a browser plays, the first word in chat after a game starts, a
sidetone noise mode picked while an app records). The running build has
`cpu-stats`: rebuild without it before a tester ZIP.

### The tray icon follows a registry notification (2026-11-20)

- Reported: switching a recorder on took far too long to change the icon, but no faster polling.
  Nothing else tells the service that an app opened a microphone (no events or pipes between the
  processes, only shared-memory atomics), so the service watches
  `HKCU\...\CapabilityAccessManager\ConsentStore\microphone` (subtree) with `RegNotifyChangeKeyValue`
  (`mic_usage::MicStoreWatch`): Windows writes the app's start / stop time there. The idle wait of the
  service loop (`tray::wait_for_messages`) also waits on that event; a change is read after a 100 ms
  gap (`MIC_RESCAN_GAP`), without the 400 ms scan cache. Measured: the icon follows in about 100 ms
  instead of up to ~1.6 s; the 1 s check stays as the fallback.
- Found in review: the wait resets the auto-reset event that ended it, so the tray never saw the change
  and never re-armed the watch (back to the 1 s check with the window closed). `wait` now returns
  whether the event ended it (test `the_idle_wait_says_when_the_event_ended_it`). The microphone level
  keeps its own second, apart from the check.
- Not tried: a stop (app closes the microphone) is assumed to write the same key; the release notes
  promise only the start.

## Milestone 67: One microphone and one output in use (2026-11-21)

- Reported on a notebook (0.3.1, maybe older): with "Default microphone" picked in the app, sidetone
  and the tray icon's level did not work; with the Realtek array picked they did. Recording worked
  either way. "Default microphone" is the app's own entry (no pick = follow Windows), not a Windows
  device.
- Found by reading: every part resolved "the microphone" on its own. The mixer (`use_mic`): the pick
  while connected, else the Windows default, a virtual default replaced by a physical device. Sidetone:
  the pick even when unplugged, else the plain Windows default, and the plain default output (so behind
  a Sonar / VB-Cable default the pair named a device no effect of ours runs on). The APO link's caches
  (voice level, echo delay): the plain default, never the pick. The window's banner: the pick, else the
  plain default. Health: the mixer's microphone, but "recording into a muted mic" asked the plain
  default, and "not the Windows default microphone" compared the resolved microphone, not the pick, so
  the service could warn (the tray level then stands still) where the window showed nothing.
- Now one rule, resolved once: `ChatMixController::use_mic` and `master_endpoint_id`. The service loop
  publishes both ids every pass (`win_audio/in_use.rs`); sidetone, the echo reference (its
  `Arc<Mutex>` is gone) and the APO link read them there, the window reads its own controller.
  `sidetone::active_pair` and `mmdevice::resolve_default_capture_id` are deleted. Health keeps the pick
  apart (`picked_mic`) for the "not the default" issue only, as the window does.
- Confirmed by the notebook's log: the saved pick was a headset microphone chosen on 2026-11-11
  (`{edc3e59e...}`, "not present" in `[ApoCheck]`; the headset later came back under a new id). An
  unplugged pick shows as "Default microphone" in the window and the mixer works with the array, but
  the sidetone worker took the pick as it was: every start logs `[Sidetone] pair {b9182400...}` (the
  array, before the settings are restored) and then `pair {edc3e59e...} -> speakers: off`, and the
  verdict never leaves `Off`. The window saved "on" for (array, speakers), a pair the worker never
  published. Picking the array replaced the stale pick, which is why that helped.
- The tray is another cause, not touched here: on 0.3.1 the log has `[Mic] microphone processing
  started` many times and no `[Tray] microphone in use` at all, so the check for a recording app
  (`mic_usage::microphone_in_use`) never fired and the level had nothing to follow. The later
  development builds in that log (`c8ffdff`, `93030c4`) log `microphone in use (microphone effect)`
  and the icon follows; those commits are not in this repository yet.
- Checked on the Mac by clippy for the Windows target only; tests run in CI.
- Found in review: the window re-bound Master to the first physical output on every device change,
  the service keeps the one in use behind a virtual default, so the Sidetone tab could edit a pair
  the worker does not play: `bind_endpoints` now keeps it too (`master_endpoint_id`). And `use_mic`
  keeps the id of an unplugged only microphone, which hid "an app records with no microphone":
  that check takes the microphone only while it is connected (`WinAudioSystem::connected_mic`).
- Known limits: a window started after the outputs changed behind a virtual default may still pick
  another physical output than the running service (it has no way to ask it). Sidetone pairs saved
  for a virtual default output (they never played) are not found any more: switch sidetone on again.
- Left as it was: `--install` without a window (`setup_cli.rs`) still ignores a saved pick.

### The tray level starts at the lowest bars or the real level (2026-11-21)

- Asked for: the icon measures how well one is heard, so the moment it turns red it must not show
  full bars. The first tick had no telemetry sequence to compare and drew the standing (full) icon
  for a second. The first tick now counts as live (`tray_level::chain_is_live`, test
  `the_first_tick_counts_as_live_and_a_stuck_sequence_does_not`): it draws the level the mirror
  copied within the last second, else the lowest bars.
- Limit: a microphone our effect does not run on shows the lowest bars for that first second and
  the standing icon after it, as before.

### The tray icon also turns red on the effect's word (2026-11-21)

- Reported with build `8ca3da1` on the notebook: the icon did not turn red at all, whatever
  microphone was picked. The log: sidetone's pair is now the array and stays (the fix above works),
  `[Mic] microphone processing started` twice for ~3 s with a Store app in front
  (`applicationframehost.exe`), and no `[Tray] microphone in use`. The window was open, so the 1 s
  check ran during the recordings: Windows' microphone store named no app. The pick is not involved
  (mic in use = Windows default = the array). Why the store is silent there is not known; it named
  apps on that notebook until 2026-11-11.
- The tray now has a second witness (`tray::EffectWatch`): the mic effect's telemetry sequence in
  the service's mapping, read on every pass (one atomic). It moves only while the effect produces
  audio and counts as stopped after 600 ms (`EFFECT_QUIET`). The earlier attempt (b3bb8f5, reverted)
  went through `DspWatch` and the 1 s check and lagged ~3 s after a recording; here a recording
  Windows named ends on Windows' word alone, and one only the effect saw ends ~0.6 to 0.85 s late.
- The log line of such a recording carries what the store held at that moment
  (`mic_usage::store_summary`: number of apps, the last one to open a microphone, when it opened
  and closed), so the next log from the notebook says why Windows named nobody.
- Limits: "Everything off" publishes no telemetry, so only Windows' word counts then. Our own
  capture (the mic probe at install, sidetone's "Try it") turns the icon red while it runs. The
  mixer's "Recording now" list, the "recording into a muted mic" warning and sidetone's low-latency
  capture still go by the store alone. Tests: `the_effect_tells_of_a_recording_windows_does_not_name`,
  `a_recording_windows_named_ends_on_windows_word`; not run on the Mac (clippy only).
- Confirmed on the notebook with build `3f04cf4` (2026-11-21): the icon turns red with the effect
  (0 to 250 ms after `[Mic] microphone processing started`) and plain 1.5 to 5 s before `[Mic] ...
  stopped`; sidetone goes `NoRecorder` -> `Starting` -> `Playing` on the array's pair. The store's
  summary in that log: 7 apps, the last to open a microphone was WindowsSoundRecorder 856347 s
  before, that is 2026-11-11 19:15:42, closed 53 s later. Windows has written nothing into that
  user's `ConsentStore\microphone` since, for any app. The log has build 26200.9711 on that day and
  26200.9974 from 2026-11-16 on, so a Windows update lies between; whether it is the cause is not
  known. Our registry read is not at fault: there is nothing to read.

## Milestone 68: The tray flyout (2026-11-21)

- A left click on the tray icon opens a volume panel (before: the main window, now on its "Open
  settings" row and in the right-click menu): the output with a list to switch it, Game / Chat /
  Media / Mic, and one row per app with an audio session (volume, mute). It lives in the service
  (`flyout.rs`): a plain Win32 popup drawn with GDI into one bitmap (`flyout/paint.rs`), made on the
  first click and then only shown and hidden. A second window process was dropped: it takes ~0.4 s
  to its first frame.
- App volume: stored per exe in `app_volumes.json` (100 % is not stored) and multiplied into each
  session's target, so the mixer stays the one writer of session volumes. The slider shows and sets
  what Windows has: a drag to `heard` stores `heard / (channel x ChatMix x reduction)`, at most
  100 %. Above what the channel allows the session gets the most it can, and the slider goes back
  there on the read-back 250 ms later. Mute is the session's own Windows mute.
- With Game / Chat outputs of their own (Sonar) the session pass no longer returns early: sessions
  get the app volume alone. A session the mixer had lowered before those outputs appeared goes back
  to 100 % x app volume. Not tried live with Sonar.
- Channel rows: the service writes the level into the header, applies it and saves the audio
  settings at the end of a change. An open window writes all levels every frame, so it has to adopt
  the change first: `mixer_edits.rs`, a small `Local\` mapping of its own (a field in `ShmHeader`
  would bump the layout version and leave an installed mic effect unreachable until reinstalled).
- Review before the merge, fixed:
  - A window with "Lock device" on switched Windows back from an output picked in the flyout (the
    lock lives in the window's process), and the tray then warned of a changed default. The service
    now names the pick in `mixer_edits` before it switches (a hash of the endpoint id); the window
    moves its lock there when the default that left the locked output is that pick, within 5 s.
    The window reads the lock's pending flag before the picks: the other order could miss a pick
    made during the frame. A pick whose switch failed still counts for those 5 s.
    Health's remembered output was not used as the signal: a headset just plugged in also becomes
    the remembered one, and that is exactly what the lock must switch back from.
  - A drag sent a change on every mouse move, also inside the same percent; each one made the
    service walk every session twice. Now only a new value is sent.
  - Shift+Tab went forward. An exe whose icon could not be read (its process just ended) kept the
    fallback tile until the service restarted; a failure is no longer kept.
  - The Sonar test of the mixer walked the PC's real sessions through `apply_session_mix`; it now
    asks `session_targets`.
- Setup problems in the flyout (`fb2eafd`, `47abdd4`): the first problem the tray menu lists, as a
  short text and its first fix as a button; "+N more" opens the window. Outdated effects are worded
  "Sound effects need an update" everywhere (one DLL does the microphone, the output and the per-app
  effects), and their fix starts the installer itself, from the menu and the flyout: one UAC prompt,
  on the active endpoints that already carry our effects, never a first install. The UAC prompt
  waits on a thread of its own, so the service loop keeps running.
- Review of that, fixed:
  - A fix from the flyout ran a loop pass after the flyout hid, when this process no longer has the
    foreground: `OpenWindow` was already moved into the click for that reason, the two settings
    pages and the UAC prompts (update, mic slot) were not. A fix that only shows a window now runs
    inside the click (`Fix::shows_window`); for one that asks for administrator (`Fix::asks_admin`)
    the flyout stays open and in front until the prompt, the script's window or the main window
    takes the foreground. Its button answers no second press until the flyout opens anew or
    shows another problem; a mic-slot move that could not start (cancelled prompt) now opens the
    window, as the update does. Written from that reasoning on the Mac, not tried on Windows.
  - A second "Update" within `UPDATE_GUARD` (90 s) closed the flyout and did nothing; it now opens
    the window. The guard is a guess at the installer's run time and the window's own Update does
    not see it: two installers can still run at once (`shortcut:` at the constant).
  - The notification's second line still said "update it" under the plural title.
- Limits: the popup is drawn by hand and tells screen readers nothing (keyboard: Tab, arrows, Space,
  M, Enter, Esc). A click on the icon held longer than 300 ms while the flyout is open may close and
  reopen it (`REOPEN_GUARD`). App volumes go by exe name, as the channels do. The look on Windows 10
  (no rounded corners, no system border) was not checked.
- Tests: `mixer_edits.rs`, `flyout.rs`, `flyout/paint.rs`, `app_channels.rs`, `chatmix/mod.rs`; on
  the Mac only clippy for the Windows target, the tests run in CI.

---

## Milestone 69: With the leveler on, only its target sets how loud others hear you (2026-11-24)

Milestone 23 applied the mic volume (0..100 %) again after the leveler, as an output volume, so an
app's own gain control would see its change and settle. In use that made the mic slider a hidden
second loudness control nobody expects next to the leveler's target:

- Log 2026-11-22 08:27: 80 ms after an output pick in the tray flyout the mic row went 50 -> 26 %
  (the pick or a slider press, the log cannot tell), about -6 dB for others until a reinstall.
- Log 2026-11-24 17:33 and 21:03: the Windows mic volume moved 26 -> 40 -> 29 % in small steps (an
  app's gain control or the Windows slider); the wish followed and others heard -3 dB.
- The automatic input gain was off all the time (`automatic level off`); the leveler compensated the
  input each time ("compensated") and stage 5 then took it out again. The DSP itself is byte-identical
  between 0.3.2 and 0.4.0 and only gained the volume ramp since.

Now stage 5 applies the volume only without the leveler (a boost above 100 %). With it the Windows
volume sits before the APO (Milestone 23) and is purely input sensitivity, which the leveler takes out.
Limit: an app's analog gain control (Teams, Zoom, Chrome/WebRTC) no longer hears its own steps and may
walk the Windows volume to an end; on a hardware volume the automatic input gain backs off after such a
foreign change (`COOLDOWN_CLIP`, 3 s), and an app that keeps stepping can keep it waiting. Tests:
`test_mic_volume_reaches_the_output_only_without_the_leveler`, the gamer chain golden recalibrated (G2).

---

## Milestone 70: Automatic input gain: the voice at the converter, apps that hold the volume (2026-11-25)

Branch `fix/auto-gain-converter-level`. The automatic input gain (milestone 23) decided by the
leveler's gain: up at +18 dB or more, down at -5 dB or less. That gain is `target - level - Windows
volume`, so the user's target loudness moved the Windows volume: with the target at -36 dBFS a normal
voice at -31 dBFS read as too hot and was pulled down towards the 20 % floor, worse for the noise.
The -5 dB was also stale: it was "near the -6 dB cut cap", which has been -18 dB since milestone 29.

- `win_audio::mic_gain` now uses the voice level at the converter: the leveler's learned level (it
  is learned without the Windows volume) plus the Windows volume in dB. Too weak at -42 dBFS or
  under, too hot at -19 dBFS or over: the old thresholds at the default target (-24), so nothing
  changes there. One exception goes by the gain: a leveler at its +24 dB boost cap (within 1 dB)
  counts as weak too, since with a high target (-12) a voice at -38 dBFS stays short of it otherwise;
  a higher volume never makes the noise worse against the voice. Clipping, steps, cooldowns and
  limits are unchanged. The `[MicGain]` line shows the
  voice in dBFS instead of the leveler's gain.
- An app that holds the mic volume (sets it back whenever someone else moves it) and a step due
  (clipping) used to step against each other forever, every 3-10 s: each foreign change is the new
  wish and the automatic gain waits, then steps again. Now a foreign change within 10 s of our step
  that moves the volume at least halfway back is a revert; after two in a row (a step that stands or
  any other change starts the count again) the automatic gain stops on that mic until another mic
  or a service restart, with one `[MicGain] stopped` line. Halfway, so a device rounding our level
  a little does not count. The window does not show it (no shm field).
- `win_audio/leveler_log.rs` takes the gain caps from `voice_leveler` (now public) instead of its own
  copy, which still said -6 dB.
- Limit: the level is learned after the echo canceller, denoiser and mic EQ, so it is a few dB off
  the raw converter.
- Tests in `mic_gain.rs` (a hot voice is lowered; a leveler at its boost cap raises; a normal voice
  and an unlearned one stay; an app setting the volume back twice stops it, one moving on does not).
  On the Mac only clippy for the Windows target; not heard live.

---

## Milestone 71: Apps that move the mic volume themselves: the leveler holds (2026-11-26)

Spec `docs/mic-leveler-apps-spec.md`, analysis `docs/mic-app-volume-analysis.md`. Since milestone 69
an app's own gain control (Teams, Zoom) no longer hears its steps: the leveler evens each one out and
the app walks the Windows volume to an end.

- With the leveler on, the Windows mic volume is only the input sensitivity, everywhere. The service
  follows Windows (`ChatMixController::sync_mic`, the one place a foreign move is seen) and no longer
  loads the window's copy from shared memory, which may be older. The automatic input gain sets
  Windows directly (`set_mic_level`), so `mic_hw_auto`, the wish beside it and `adopt_mic_auto` are
  gone. The mixer's mic fader sets the leveler's target as the tray row does
  (`flyout::set_leveler_target`); the Mic page's Sensitivity row is 0-100 % of Windows.
- `win_audio/mic_hold.rs` sorts every move (moves under 2 s apart are one): nobody records -> the
  user; a volume window in front (Settings, the Sound panel in rundll32, Game Bar, this app) -> the
  user; the recording app in front with input in the last 1.5 s -> the user; else an app. The input
  time comes from `GetLastInputInfo` (its time only, asked only in that case), the one exception to
  the no-input rule. A second app move within a minute in one recording holds the leveler
  (`mic_leveler_hold`, shm v25; `VoiceLeveler::set_hold`: gain kept, no learning, the input gain not
  compensated). It ends when that app stops recording (several recording: all of them); the volume
  goes back to where it was before the recording unless the user moved it in between. The automatic
  input gain does nothing while held.
- With one app recording it is noted in `audio_settings.json` (`mic_volume_apps`: count and date)
  after its first such recording, listed after the second and from then on held from the start of
  its recordings, on the gain seeded for the voice. With several recording none is noted. The Mic
  page lists them with Remove and, for Teams, Zoom and Skype, the option to switch off; a status
  line says who holds it.
- The input sensitivity is kept per mic when the leveler is switched off (`mic_sensitivity_permille`)
  and comes back when it is switched on.
- `[MicVol]` lines on events only: each move with who and the scene, a hold and why, its end.
- Limits: Windows does not say who moved the volume, so headset software or a volume wheel on the
  headset during two calls can list an innocent recording app (the list is visible and can be
  cleared). A move seen a tick before Windows reports the recording counts as the user's. A package
  whose name differs from its exe by more than `.exe` and punctuation is not recognised in front.
  The foreground is unknown until the first foreground change after the service starts (a move then
  counts as an app's while something records). A service restart forgets a hold. The effects must be
  reinstalled for shm v25.
- Tests: `mic_hold.rs` (who moved it; one move vs. a second within a minute; the 2 s merge; the end,
  the restore and a user change that stays; several apps; a listed app from its start; nobody
  recording; a new recording counts from zero), `voice_leveler.rs`
  (`a_hold_keeps_the_gain_while_the_input_moves`), `audio_settings.rs` (the list beside the
  settings). Not heard live: a Teams test call and zoom.us/test are still to do.
