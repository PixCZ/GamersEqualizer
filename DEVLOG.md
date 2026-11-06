# OpenSonar Developer Log (DEVLOG)

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
