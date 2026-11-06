# Master Technical Specification: GamersEqualizer (Pure Rust Edition)

**Role:** Principal Rust Systems & Real-Time Audio DSP Architect  
**Target Platform:** Windows 10 / Windows 11 (x64)  
**Tech Stack:** 100% Pure Rust (`Cargo` workspace), Equalizer APO Bridge, Native GPU UI (`eframe`/`egui`)  

---

## 1. Non-Negotiable Core Principles
1. **100% Air-Gapped (Zero Network Code):**
   - **STRICTLY FORBIDDEN:** Any networking crates (`reqwest`, `ureq`, `hyper`, `curl`, `tokio::net`, `std::net`). The binary must be physically incapable of opening a network socket.
   - No telemetry, no auto-updaters, no analytics.
   - A manual "Check for Updates" button in the UI may only invoke Win32 `ShellExecuteW` to open the GitHub Releases page in the user's default browser.
   - Distribution and updates are handled externally by OS package managers (**Microsoft Winget** / **MS Store MSIX**).
2. **Zero Web Rendering (Anti-Bloatware):**
   - **STRICTLY FORBIDDEN:** WebView2, Tauri, Electron, HTML/CSS/JS, CEF, or heavy XAML islands.
   - Use pure hardware-accelerated native rendering (`eframe` + `egui` + `egui_plot` over WGPU/DirectX) styled with Windows 11 native **Mica/Acrylic backdrop** (`window-vibrancy`) to reflect the visual philosophy and dark utility layout of SteelSeries Sonar, but with instant (< 10ms) window creation.
3. **Decoupled Service & Control Tool Memory Model (< 4 MB Background RAM):**
   - The application runs primarily as a lightweight background daemon (`gamersequalizer.exe --daemon`) executing audio monitoring, game detection, and Equalizer APO sync.
   - The standalone UI Control Center can be opened to adjust volumes or EQs and closed at any time; closing drops 100% of GPU/window RAM while the daemon continues uninterrupted.
   - Optional autostart on Windows boot via User Registry (`HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Run`).
4. **No Paid Kernel Drivers Required:**
   - Do not create virtual WDM audio devices.
   - Operate entirely in User-Mode by orchestrating Windows CoreAudio APIs (`WASAPI`) and feeding configurations + a lightweight Rust VST plugin into the signed open-source **Equalizer APO** pipeline.
5. **Nothing Is Impossible, Nothing Is Hidden, Everything Is Explained:**
   - "Not possible" is never a final answer. When a feature collides with a constraint (air gap, no virtual devices, Windows limits), find the way that respects the constraint, name its cost, and offer it.
   - Technical details are not hidden from the user. The UI says what is happening and why: which component does the work (APO, daemon), what is installed where, what needs admin rights, what is not applied yet and what it would take.
   - Every such statement is explained in plain words, in place (label, tooltip, status line), so the user can decide without reading code. Example: the air gap means the app never downloads AutoEQ itself; the UI explains that the user downloads the file in a browser and how to import it.
   - Limits that remain are stated honestly with the next step, never silently ignored (e.g. "Chat EQ is not applied yet: needs per-app processing, planned as ...").

---

## 2. Workspace Architecture (`Cargo.toml` Multi-Crate Structure)

The repository is structured as a Rust Cargo Workspace with three targets:
1. **`gamersequalizer-common` (`lib`):** Shared models, profiles, filter biquads, and lock-free SPSC shared memory layout (`Local\GamersEqualizer_SpeakerRef_Buffer`).
2. **`gamersequalizer` (`bin`):** Dual-mode executable (`gamersequalizer.exe`) running either as the headless background service (`--daemon`) or the native 5-channel mixer GUI.
3. **`gamersequalizer-mic-vst` (`cdylib`):** Lightweight, zero-latency VST2/VST3 plugin (`gamersequalizer_mic_vst.dll`) loaded by Equalizer APO for classical Acoustic Echo Cancellation (AEC) and tiny offline AI voice isolation.

---

## 3. Detailed Module Specifications

### Module 1: `win_audio` (Isolated Win32 & COM Layer)
All `unsafe` Win32/COM calls MUST be isolated inside `src/win_audio/` using the official `windows` crate (v0.58+). Expose only safe Rust methods to the rest of the app.

1. **Hardware Endpoint Lock (Anti-Monitor Audio Hijack):**
   - Register an `IMMNotificationClient` listening to `OnDefaultDeviceChanged`.
   - If the active Windows audio output changes away from the user's locked primary DAC/Soundcard (e.g., hijacked by NVIDIA/AMD HDMI monitor audio during a GPU driver update), immediately force it back within < 20ms.
   - Define the undocumented `IPolicyConfig` COM interface (`IID: f8679f50-850a-41cf-9c72-430f290290c8`, `CLSID: 870af99c-171d-4f9e-af0d-e63df40c2bc9`) and invoke `SetDefaultEndpoint` across `eConsole`, `eMultimedia`, and `eCommunications`.
2. **Dolby Atmos (Spatial Audio) Guard:**
   - Query `ISpatialAudioClient` on the active endpoint to check if **Dolby Atmos for Headphones** is active.
   - Automatically differentiate between **Headphones Mode** (enforce/verify Dolby Atmos ON, disable Speaker AEC) and **Speakers Mode** (Dolby Atmos OFF, enable Speaker AEC).
   - Display live status in UI and Tray; alert user if Windows resets Spatial Audio.
3. **Device-less ChatMix (Per-Process Volume Mixer):**
   - Enumerate active audio sessions via `IAudioSessionManager2` and `IAudioSessionEnumerator`.
   - Match process IDs (`IAudioSessionControl2::GetProcessId`) to executable names:
     - **Game Bus:** Currently detected game `.exe` (or user-assigned game processes).
     - **Aux/Browser/Chat Bus:** `chrome.exe`, `msedge.exe`, `firefox.exe`, `brave.exe`, `opera.exe`, `discord.exe`, `spotify.exe`.
   - Control per-app volumes directly via `ISimpleAudioVolume::SetMasterVolume`.
   - Provide a single Sonar-style **Game <-> Aux Balance Slider** (plus global hotkeys) that dynamically attenuates browser/chat audio while keeping game audio at 100% (or vice versa) with 0ms DSP latency.
4. **Zero-CPU Game Detector:**
   - Hook foreground window changes using `SetWinEventHook` (`EVENT_SYSTEM_FOREGROUND`, `WINEVENT_OUTOFCONTEXT`) — strictly zero polling loops.
   - Resolve `HWND` $\rightarrow$ `PID` (`GetWindowThreadProcessId`) $\rightarrow$ Executable path (`QueryFullProcessImageNameW`).
   - Automatically activate the matching Game EQ Profile when a mapped game is running.

---

### Module 2: `dsp_apo` (Dual-Layer Parametric EQ & Equalizer APO Controller)
1. **Cascaded Dual-EQ Engine:**
   - **Layer A (Hardware Correction - Always Active):** User's headphone calibration profile (e.g., Rode headphones).
   - **Layer B (Game Profile - Dynamic):** Game-specific parametric EQ (e.g., footsteps boost) switched automatically by Module 1.
   - **Microphone EQ:** Dedicated parametric EQ + compressor chain for the user's microphone.
2. **Supported Filter Types:**
   - `Peaking (PK)`, `LowShelf (LSC)`, `HighShelf (HSC)`, `LowPass (LP)`, `HighPass (HP)`, `Notch (NO)`.
   - Parameters per band: `Frequency (20–20000 Hz)`, `Gain (-24 to +24 dB)`, `Q-Factor (0.1 to 30.0)`, `Enabled (bool)`.
3. **Mathematical Auto-Preamp (Anti-Clipping Protection):**
   - Calculate the exact complex frequency response $H(e^{j\omega})$ of Layer A + Layer B combined across 512 logarithmically spaced frequencies using RBJ Audio-EQ-Cookbook biquad transfer functions.
   - Find the maximum peak boost $G_{\max} = \max_{f} (0.0, \text{Gain}_{\text{dB}}(f))$.
   - Automatically prepend `Preamp: -(G_max + 0.5) dB` to prevent digital clipping before the signal hits Dolby Atmos and the DAC.
4. **Atomic Equalizer APO Config Writer:**
   - Write the generated filter chain atomically to `C:\Program Files\EqualizerAPO\config\opensonar_live.txt`.
   - Ensure Equalizer APO is attached as **SFX/EFX** (Post-mix) so **Dolby Atmos** computes spatial binaural rendering first, and Equalizer APO applies the combined Game + Headphone EQ curve second.

---

### Module 3: `opensonar-mic-vst` (Classical AEC + Tiny AI Voice Isolation)
A standalone Rust audio plugin (`opensonar_mic.dll`) loaded by Equalizer APO to clean microphone audio in real time (< 10ms latency).

1. **Lock-Free Shared Memory Loopback (Speaker Reference Tap):**
   - Establish a lock-free SPSC (Single-Producer Single-Consumer) Ring Buffer in Windows Shared Memory (`CreateFileMappingW` / `MapViewOfFile` at `Local\OpenSonar_SpeakerRef_Buffer`).
   - **Output Tap Instance** (placed on Speakers in Equalizer APO): Passively copies the final stereo speaker mix into the shared ring buffer without modifying playback audio.
   - **Mic Processor Instance** (placed on Microphone in Equalizer APO): Reads the exact speaker reference signal from the shared ring buffer.
2. **Stage 1: Classical DSP Acoustic Echo Cancellation (NO AI):**
   - Implement **pure mathematical adaptive filtering** (Frequency-Domain NLMS / MDF — Multidelay Block Frequency Domain Adaptive Filter, via pure Rust implementation or `speexdsp-aec` / `webrtc-aec3` C-free/static bindings) coupled with a **Delay Estimator** (cross-correlation to automatically handle acoustic flight time from speakers to mic).
   - Subtract the speaker reference signal and room reflections from the microphone input *deterministically* (ensuring NPC voices, music, and explosions from speakers are canceled without AI hallucination).
   - Include a non-linear residual echo suppressor (RES) and double-talk detector (DTD) so the filter does not diverge when the user speaks over loud speaker audio.
3. **Stage 2: Tiny Local AI Voice Isolation (Keyboard/Mouse/Fan Removal):**
   - After classical AEC removes speaker bleed, pass the 48kHz mono frame into an embedded, offline neural speech enhancer (model weights compiled directly into the `.dll` via `include_bytes!`):
     - **Default Engine:** `nnnoiseless` (100% pure Rust RNNoise implementation, ~100 KB embedded weights, < 0.3% CPU usage, 10ms frame size) OR `df` (`libDF` DeepFilterNet3 in pure Rust via `tract-onnx`, ~3 MB embedded weights for studio 48kHz clarity).
   - Controlled via shared memory parameters from the main UI (`AEC Enabled: bool`, `AI Voice Isolation Strength: 0.0 - 1.0`, `Noise Gate Threshold: dB`).

---

### Module 4: `sonar_importer` (Offline SteelSeries Sonar Importer)
Extract both **Factory Game Presets** and **User Custom Presets** directly from SteelSeries GG local SQLite files (zero internet required):
1. **Target Locations:**
   - Default SteelSeries Sonar DB: `C:\ProgramData\SteelSeries\GG\apps\sonar\db\database.db`
   - Support manual file picker for backed-up `database.db` files (allowing the user to completely uninstall SteelSeries GG).
2. **Extraction Logic (`rusqlite` + `serde_json`):**
   - Read both factory game EQ presets and user custom EQ presets across Game, Chat, Media, and Mic channels.
   - Convert Sonar's parametric filter definitions into OpenSonar's clean human-readable JSON format (`%AppData%\OpenSonar\profiles\games\*.json` and `headphones\*.json`).

---

### Module 5: `ui` (Sonar-Inspired Native GPU Interface)
Built with `eframe` (`egui` + `egui_plot`) and `window-vibrancy` (Windows 11 Mica dark theme).

1. **Header Bar (System Status & Locks):**
   - **Output Device Selector + Lock Toggle:** Shows locked DAC/Soundcard and counts blocked hijack attempts.
   - **Mode Switch:** `Headphones (Dolby Atmos + Dual EQ)` vs `Speakers (Speaker EQ + Classical Mic AEC)`.
   - **Dolby Atmos Indicator:** Real-time status badge (`ACTIVE` / `OFF`).
   - **Active Game Indicator:** Shows currently detected running game and active EQ preset.
2. **Tab 1 — Master & Game Equalizer:**
   - Interactive `egui_plot` frequency response graph (20 Hz – 20 kHz) rendering **3 simultaneous curves**:
     - *Curve 1 (Dimmed Dotted):* Base Headphone Calibration (e.g., Rode).
     - *Curve 2 (Colored Dashed):* Active Game Preset (e.g., CS2 / Tarkov).
     - *Curve 3 (Bright Solid):* Final Combined Response + Auto-Preamp headroom ceiling line.
   - Interactive draggable nodes (Freq, Gain, Q-factor) + "Import from SteelSeries Sonar DB" button.
3. **Tab 2 — Mixer (Virtual-less ChatMix):**
   - Vertical channel strips for **Game**, **Browsers/Media**, and **Chat (Discord)** using live Windows Session volumes.
   - Central **Game <-> ChatMix Balance Slider** with customizable global hotkeys.
4. **Tab 3 — Microphone (Classical AEC + AI ClearVoice):**
   - Toggle: **Classical Speaker Bleed Cancellation (AEC)** with real-time ERLE (Echo Return Loss Enhancement) meter showing how many dB of speaker bleed are being mathematically subtracted.
   - Slider: **AI Voice Isolation (Keyboard/Mouse Filter)** (`0%` Off to `100%` Strict Voice Only).
   - Parametric Mic EQ (Broadcast voice shaping).

---

## 4. Workspace `Cargo.toml` Blueprint

```toml
[workspace]
members = ["crates/opensonar-daemon", "crates/opensonar-mic-vst"]
resolver = "2"

[workspace.dependencies]
windows = { version = "0.58", features = [
    "Win32_Foundation",
    "Win32_System_Com",
    "Win32_System_Com_StructuredStorage",
    "Win32_System_Threading",
    "Win32_System_ProcessStatus",
    "Win32_System_Memory",
    "Win32_Media_Audio",
    "Win32_Media_Audio_Endpoints",
    "Win32_Media_Audio_Spatial",
    "Win32_UI_Accessibility",
    "Win32_UI_WindowsAndMessaging",
    "Win32_UI_Shell",
    "Win32_UI_Input_KeyboardAndMouse"
] }
windows-core = "0.58"
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
nnnoiseless = "0.5" # Pure Rust RNNoise (100KB embedded model)

[profile.release]
opt-level = 3
lto = "fat"
codegen-units = 1
panic = "abort"
strip = true
```

---

## 5. Implementation Roadmap for AI Coding Agent

Execute development strictly in these 4 sequential milestones (verify `cargo check` and `cargo test` after each):

- **Milestone 1 (`win_audio` + Core Daemon):** Implement `IPolicyConfig` endpoint lock, `ISpatialAudioClient` Dolby Atmos check, `ISimpleAudioVolume` per-app mixer, and `SetWinEventHook` foreground game detector.
- **Milestone 2 (`dsp_apo` + `sonar_importer`):** Implement biquad frequency response math, Auto-Preamp calculator, atomic Equalizer APO writer, and SQLite extractor for SteelSeries Sonar `database.db`.
- **Milestone 3 (`opensonar-mic-vst`):** Implement lock-free Shared Memory ring buffer, Classical Frequency-Domain NLMS/MDF Acoustic Echo Canceller (with delay estimation and double-talk detection), and `nnnoiseless` AI voice isolation inside a VST2/VST3 plugin.
- **Milestone 4 (`ui` + Packaging):** Build the lazy-loaded `eframe`/`egui` + `window-vibrancy` GUI, System Tray menu, and Winget manifest generator.
