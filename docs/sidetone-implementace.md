# Příposlech: návod k implementaci

Pro agenta (Antigravity) na Windows stroji. Větev `sidetone`. Příkazy jsou pro PowerShell
z kořene repa. Proč je co navrženo, je v `docs/sidetone-zadani.md`; tady je jen co udělat.

Postupuj přesně po krocích. Každý krok má **Udělej**, **Ověř** a **Hotovo, když**. Dokud
„Hotovo, když" neplatí, nepokračuj dalším krokem.

## Co už je hotové (psáno a otestováno na Macu)

| Soubor                                                   | Co obsahuje                                                                                               | Testy  |
| -------------------------------------------------------- | --------------------------------------------------------------------------------------------------------- | ------ |
| `crates/gamersequalizer-common/src/sidetone.rs`          | `SidetoneRing`, `SidetoneReader`, `BandExpander`, `SidetoneChain`, `SidetonePairs`, `device_hash`         | 21     |
| `crates/gamersequalizer-common/src/shm.rs`               | sdílená paměť v20: pole `sidetone_*`, `ShmLayout::sidetone`, `write_sidetone`, `read_sidetone`, zrcadlení | 1 nový |
| `crates/gamersequalizer-mic-vst/src/sidetone_denoise.rs` | `SidetoneDenoiser`: WebRTC NS, RNNoise, RNNoise 0.2 za jedním rozhraním                                   | 5      |

Tyhle tři soubory jsou přeložené a testy na nich prošly. Zbytek tohoto návodu (kód pro APO,
daemon a UI) je **psaný bez překladače**: může mít chyby překladu.

## Pravidla

1. **Chyby překladu smíš opravit, záměr ne.** Každou opravu kódu z tohoto návodu vyjmenuj
   v DEVLOGu (soubor, co bylo špatně).
2. **Neměň chování** v `sidetone.rs`, `sidetone_denoise.rs` ani pole `sidetone_*` v `shm.rs`.
   Když test neprojde nebo ti tam něco chybí, zapiš čísla a **zastav se**.
3. **Neměň konstanty** v těch souborech (`MARGIN_*`, `BAND_EDGES_HZ`, `KNEE_DB`, ...).
4. **Kód v APO běží v audiodg.** Pád shodí zvuk celého systému. Proto v `gamersequalizer-apo`:
   - žádné `unwrap`, `expect`, `panic!`, indexování `v[i]` (crate to má jako `deny`);
   - ve zpracování zvuku žádné zámky, logování ani čtení souborů;
   - alokace jen tam, kde to návod výslovně dovoluje (růst pomocného bufferu);
   - každé volání do příposlechu je v `catch_unwind`, jak je v návodu.
5. **Příposlech nesmí hrát jinde než na výstupu dvojice.** Mikrofon + reproduktory = zpětná
   vazba. Když v kroku 3 výstupní APO nepozná své zařízení (`hash seen 0`), **zastav se**.
   Nepřidávej žádné „když nevím, hraju".
6. **Instalaci APO dělá člověk** (admin, restart zvuku). Když krok říká „požádej člověka",
   napiš mu přesně co má udělat a počkej.
7. Po každém kroku: `cargo test` pro dotčené crate, `cargo clippy --workspace --all-targets`,
   commit `Sidetone: krok N, <co>`, push na `sidetone`. **Nemerguj** nikam.
8. Výsledky měření zapisuj do `DEVLOG.md` jako nový milník (další volné číslo), anglicky jako
   zbytek DEVLOGu.
9. Když se při pozdějším slučování potká `SHM_VERSION` s jinou větví: vezmi vyšší číslo + 1
   a stejné číslo dej do obou jmen (`SHM_NAME`, `APO_SHM_NAME`).

## Krok 0: přeložit a pustit testy

**Udělej:**

```powershell
git fetch; git checkout sidetone; git pull
cargo test -p gamersequalizer-common
cargo test --release -p gamersequalizer-mic-vst --lib sidetone_denoise
cargo clippy --workspace --all-targets
```

**Hotovo, když:**

- `gamersequalizer-common`: `54 passed`.
- `sidetone_denoise`: `5 passed`.
- clippy nehlásí nic v `sidetone.rs`, `sidetone_denoise.rs` ani u polí `sidetone_*`.

Když `delay_matches_the_measured_values` nebo `steady_hiss_is_turned_down` neprojde na
Windows, zapiš výpis a zastav se (jiné chování knihoven než na Macu).

## Krok 1: změřit periody zvukového enginu

Rozhoduje o tom, jaká latence je dosažitelná (zadání, kapitola 4).

**Udělej:** nový soubor `crates/gamersequalizer-apo/examples/period_probe.rs`:

```rust
//! Prints the shared-mode engine periods of the default microphone and the default output:
//! the shortest one decides how low the sidetone delay can get.
//!
//!   cargo run -p gamersequalizer-apo --example period_probe

use windows::Win32::Media::Audio::{eCapture, eConsole, eRender, IAudioClient3, IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};

fn main() -> windows::core::Result<()> {
  // SAFETY: straightforward WASAPI calls on this thread; the format is only read.
  unsafe {
    CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
    for (name, flow) in [("microphone", eCapture), ("output", eRender)] {
      let device = enumerator.GetDefaultAudioEndpoint(flow, eConsole)?;
      let client: IAudioClient3 = device.Activate(CLSCTX_ALL, None)?;
      let format = client.GetMixFormat()?;
      let rate = (*format).nSamplesPerSec;
      let (mut default, mut fundamental, mut min, mut max) = (0u32, 0u32, 0u32, 0u32);
      client.GetSharedModeEnginePeriod(format, &mut default, &mut fundamental, &mut min, &mut max)?;
      let ms = |frames: u32| frames as f32 * 1000.0 / rate as f32;
      println!("{name}: {}", device.GetId()?.to_string().unwrap_or_default());
      println!("  {rate} Hz, default {default} frames ({:.2} ms), min {min} ({:.2} ms), max {max}, step {fundamental}", ms(default), ms(min));
    }
  }
  Ok(())
}
```

```powershell
cargo run -p gamersequalizer-apo --example period_probe
```

**Hotovo, když:** výpis obou zařízení je v DEVLOGu (tabulka: zařízení, frekvence, default,
min). Výsledek nic nezastavuje: i s `min` = 10 ms se pokračuje.

## Krok 2: mic APO zapisuje do kruhového bufferu

Soubory: `crates/gamersequalizer-mic-vst/src/plugin.rs`, `crates/gamersequalizer-apo/src/lib.rs`.

### 2.1 `plugin.rs`: přístup k celému mapování

Do `use gamersequalizer_common::shm::{...}` přidej `ShmLayout`. Hned pod `shm_header` přidej:

```rust
  /// The whole mapping (the sidetone ring lives beside the header).
  pub fn shm_layout(&self) -> Option<&ShmLayout> {
    self.shm_client.as_ref().map(|c| c.layout())
  }
```

### 2.2 `lib.rs` (APO): importy a konstanty

Uprav existující řádky `use` (nepřidávej duplicitní):

```rust
use std::ptr::{addr_of, addr_of_mut, null_mut};
use gamersequalizer_common::limiter::{soft_limit, PeakLimiter};
use gamersequalizer_common::shm::{OwnerWatch, ShmHeader, ShmLayout, APO_SHM_NAME, REF_SAMPLE_RATE};
use gamersequalizer_common::sidetone::{device_hash, NoiseMode, SidetoneChain, SidetoneReader};
use gamersequalizer_mic_vst::sidetone_denoise::{self, SidetoneDenoiser};
// do seznamu z windows::Win32::Media::Audio::Apo přidej: APOInitSystemEffects
use windows::Win32::Media::Audio::IMMDeviceCollection;
// do seznamu z windows::Win32::System::Com přidej: CoTaskMemFree
```

Ke konstantám:

```rust
/// Sidetone telemetry (peak, gate, mode) is published every this many blocks.
const SIDETONE_TELEMETRY_BLOCKS: u32 = 10;
/// Numbers the sidetone taps of this process (ring owner = process id + this).
static SIDETONE_INSTANCE: AtomicU32 = AtomicU32::new(0);
```

### 2.3 `lib.rs`: nový typ `SidetoneTap`

K ostatním typům (za `MicProcessor`):

```rust
/// Sidetone tap of the mic APO: raw input -> cuts -> noise suppression -> level -> ring.
/// Independent of the mic chain: it runs when the chain failed and only reads the buffer.
struct SidetoneTap {
  chain: SidetoneChain,
  /// Frame-based suppressors (48 kHz only); None at another rate: the expander stands in.
  denoiser: Option<SidetoneDenoiser>,
  rate: u32,
  /// `device_hash` of the endpoint this APO sits on; 0 = unknown.
  endpoint_hash: u32,
  instance: u32,
  /// Owner value while this tap writes the ring.
  owner: Option<u64>,
  watch: OwnerWatch,
  mono: Vec<f32>,
  peak: f32,
  blocks: u32,
  failed: bool,
}
```

K ostatním `impl` (za `impl MicProcessor`):

```rust
impl SidetoneTap {
  fn new(rate: u32) -> Self {
    Self {
      chain: SidetoneChain::new(rate as f32),
      denoiser: (rate == sidetone_denoise::SAMPLE_RATE).then(SidetoneDenoiser::new),
      rate,
      endpoint_hash: 0,
      instance: SIDETONE_INSTANCE.fetch_add(1, Ordering::Relaxed).wrapping_add(1),
      owner: None,
      watch: OwnerWatch::default(),
      // ~85 ms at 48 kHz; grows only if the engine sends longer blocks.
      mono: vec![0.0; 4096],
      peak: 0.0,
      blocks: 0,
      failed: false,
    }
  }

  /// `buf`: the interleaved raw input of this block. Never changed.
  fn process(&mut self, layout: &ShmLayout, buf: &[f32], channels: usize, qpc_hns: Option<u64>) {
    if self.failed || channels == 0 {
      return;
    }
    if catch_unwind(AssertUnwindSafe(|| self.process_inner(layout, buf, channels, qpc_hns))).is_err() {
      self.failed = true;
      self.release(layout);
    }
  }

  fn process_inner(&mut self, layout: &ShmLayout, buf: &[f32], channels: usize, qpc_hns: Option<u64>) {
    let h = &layout.header;
    let r = Ordering::Relaxed;
    let pair_hash = h.sidetone_mic_hash.load(r);
    // Reported only by the instance that is the pair's microphone or cannot tell: the APO of
    // another microphone would overwrite it.
    if self.endpoint_hash == pair_hash || self.endpoint_hash == 0 {
      h.sidetone_mic_hash_seen.store(self.endpoint_hash, r);
    }
    // The pair's microphone. An endpoint we could not identify may write too: a wrong
    // microphone is only a wrong voice, and the ring takes one writer.
    let ours = pair_hash != 0 && (self.endpoint_hash == pair_hash || self.endpoint_hash == 0);
    let mut settings = h.read_sidetone();
    settings.enabled &= ours && self.watch.live(h, Instant::now()) && !h.master_bypass.load(r);
    self.chain.configure(&settings, h.mic_input_gain_db_x10.load(r) as f32 / 10.0, h.mic_muted.load(r));
    if self.chain.is_silent() {
      // Off, muted or not our pair: give the ring up (a clean stop for the reader), and tell
      // the suppressors about the pause, so nothing from before it is played afterwards.
      self.release(layout);
      if let Some(denoiser) = self.denoiser.as_mut() {
        denoiser.process(NoiseMode::Off, 0.0, &mut []);
      }
      return;
    }
    if self.owner.is_none() {
      // SAFETY: no preconditions.
      self.owner = layout.sidetone.claim_writer(unsafe { GetCurrentProcessId() }, self.instance);
      if self.owner.is_some() {
        // Filter memory and levels are from before the pause; the level fades in from zero.
        self.chain.reset();
      }
    }
    let Some(owner) = self.owner else {
      return; // another mic APO instance holds the ring
    };

    let frames = buf.len() / channels;
    if self.mono.len() < frames {
      self.mono.resize(frames, 0.0);
    }
    let Some(mono) = self.mono.get_mut(..frames) else {
      return;
    };
    for (dst, frame) in mono.iter_mut().zip(buf.chunks_exact(channels)) {
      *dst = frame.first().copied().unwrap_or(0.0);
    }
    self.chain.run_cuts(mono);
    let wanted = self.chain.noise_mode();
    let mut active = if wanted == NoiseMode::Off { NoiseMode::Off } else { NoiseMode::Expander };
    if let Some(denoiser) = self.denoiser.as_mut() {
      if wanted.is_framed() {
        active = denoiser.process(wanted, settings.gate_range_db, mono);
      } else {
        // Tells it about the pause, so the next frame-based run starts fresh.
        denoiser.process(wanted, 0.0, &mut []);
      }
    }
    if active == NoiseMode::Expander {
      self.chain.expander_mut().process(mono);
    }
    self.chain.run_level(mono);
    let end_qpc = qpc_hns.map_or(0, |t| t.saturating_add(frames_to_hns(frames as u64, self.rate).max(0) as u64));
    if !layout.sidetone.write(owner, mono, self.rate, end_qpc) {
      // Another instance took the ring over (we were not called for a while): claim again.
      self.owner = None;
      return;
    }

    h.sidetone_writer_beat.fetch_add(1, r);
    h.sidetone_mic_rate.store(self.rate, r);
    h.sidetone_mic_block_frames.store(frames as u32, r);
    self.peak = mono.iter().fold(self.peak, |p, s| p.max(s.abs()));
    self.blocks += 1;
    if self.blocks >= SIDETONE_TELEMETRY_BLOCKS {
      h.sidetone_peak_permille.store((self.peak * 1000.0) as u32, r);
      let gate_db = if active == NoiseMode::Expander { self.chain.expander_mut().gain_db() } else { 0.0 };
      h.sidetone_gate_gain_db_x10.store((gate_db * 10.0) as i32, r);
      h.sidetone_noise_mode_active.store(active.to_u32(), r);
      let delay = self.denoiser.as_ref().map_or(0, |d| d.delay_samples(active)) as u64;
      h.sidetone_noise_delay_us.store((delay * 1_000_000 / u64::from(sidetone_denoise::SAMPLE_RATE)) as u32, r);
      (self.peak, self.blocks) = (0.0, 0);
    }
  }

  fn release(&mut self, layout: &ShmLayout) {
    if let Some(owner) = self.owner.take() {
      layout.sidetone.release_writer(owner);
    }
  }
}
```

### 2.4 `lib.rs`: zapojit do `MicProcessor`

1. Do `struct MicProcessor` přidej pole: `sidetone: SidetoneTap,`
2. V `MicProcessor::create` do literálu `MicProcessor { ... }` přidej: `sidetone: SidetoneTap::new(rate),`
3. Začátek `MicProcessor::process` nahraď. Bylo:

   ```rust
       if self.failed || channels == 0 {
         return;
       }
   ```

   Bude:

   ```rust
       if channels == 0 {
         return;
       }
       // Sidetone first, from the untouched input; it has its own failure flag.
       if let Some(layout) = self.plugin.shm_layout() {
         self.sidetone.process(layout, buf, channels, qpc_hns);
       }
       if self.failed {
         return;
       }
   ```

4. Za `impl MicProcessor` přidej (bez toho by po zavření streamu zůstal buffer obsazený a další
   instance by nesměla zapisovat):

   ```rust
   impl Drop for MicProcessor {
     fn drop(&mut self) {
       if let Some(layout) = self.plugin.shm_layout() {
         self.sidetone.release(layout);
       }
     }
   }
   ```

### 2.5 `lib.rs`: APO pozná své zařízení

1. Do `struct ApoState` přidej:

   ```rust
     /// `device_hash` of the endpoint from Initialize; 0 = the engine did not say.
     endpoint: AtomicU32,
   ```

   a do `ApoState::new`: `endpoint: AtomicU32::new(0),`

2. V `apo_initialize` před `S_OK`:

   ```rust
     if let Some(hash) = endpoint_hash(size, data) {
       st.endpoint.store(hash, Ordering::Relaxed);
     }
   ```

3. K pomocným funkcím (vedle `processing_mode`):

   ```rust
   /// `device_hash` of the endpoint the engine creates this APO for: the last device in the
   /// init block's collection (documented for APOInitSystemEffects2, and how Equalizer APO
   /// finds its endpoint). None when the block is neither APOInitSystemEffects nor
   /// APOInitSystemEffects2, which starts with the same fields.
   ///
   /// # Safety
   /// `data` must point to `size` readable bytes or be null (audio engine contract).
   unsafe fn endpoint_hash(size: u32, data: *const u8) -> Option<u32> {
     let (v1, v2) = (size_of::<APOInitSystemEffects>(), size_of::<APOInitSystemEffects2>());
     if data.is_null() || (size as usize) < v1 {
       return None;
     }
     let init = data as *const APOInitSystemEffects;
     let declared = addr_of!((*init).APOInit.cbSize).read_unaligned() as usize;
     // APOInitSystemEffects3 has other fields at these offsets: only the two known layouts.
     if declared != v1 && declared != v2 {
       return None;
     }
     // Borrowed: the engine owns the collection; no AddRef, no Release.
     let raw = addr_of!((*init).pDeviceCollection).cast::<*mut c_void>().read_unaligned();
     let collection = IMMDeviceCollection::from_raw_borrowed(&raw)?;
     let last = collection.GetCount().ok()?.checked_sub(1)?;
     let device = collection.Item(last).ok()?;
     let id = device.GetId().ok()?;
     let text = id.to_string().ok();
     CoTaskMemFree(Some(id.as_ptr() as *const c_void));
     text.map(|t| device_hash(&t))
   }
   ```

4. V `cfg_lock_for_process` před `let processor = match st.kind {` přidej
   `let endpoint = st.endpoint.load(Ordering::Relaxed);` a větev `ApoKind::Mic` změň na:

   ```rust
       ApoKind::Mic => MicProcessor::create(rate as u32).map(|mut p| {
         p.sidetone.endpoint_hash = endpoint;
         Processor::Mic(Box::new(p))
       }),
   ```

**Ověř:**

```powershell
cargo test -p gamersequalizer-apo
cargo clippy -p gamersequalizer-apo --all-targets
```

**Hotovo, když:** všechny dosavadní testy APO projdou (počet stejný jako před krokem) a clippy
je čistý. Zvuk se zatím nikde neozve; to je v pořádku.

## Krok 3: Output EQ APO přimíchává příposlech

Soubor: `crates/gamersequalizer-apo/src/lib.rs`.

### 3.1 Pole a konstruktor

Do `struct OutputProcessor`:

```rust
  /// Sidetone: reader of the mic APO's ring, its mono block, and this endpoint's hash
  /// (0 = unknown: never plays).
  sidetone: SidetoneReader,
  sidetone_buf: Vec<f32>,
  sidetone_endpoint: u32,
  /// Sidetone was mixed into the previous block.
  sidetone_playing: bool,
```

Do literálu v `OutputProcessor::create`:

```rust
      sidetone: SidetoneReader::default(),
      sidetone_buf: Vec::with_capacity(4096),
      sidetone_endpoint: 0,
      sidetone_playing: false,
```

V `cfg_lock_for_process` větev `ApoKind::Output` změň tak, aby procesor dostal otisk zařízení:

```rust
    ApoKind::Output => {
      let shm = SharedMemoryClient::create_named(APO_SHM_NAME, Some(APO_SHM_SDDL)).ok();
      OutputProcessor::create(shm, rate, format.dwSamplesPerFrame).map(|mut p| {
        p.sidetone_endpoint = endpoint;
        Processor::Output(p)
      })
    }
```

### 3.2 `process_inner` a `run_eq_in_place`

Celé tělo `OutputProcessor::process_inner` nahraď tímto. Proti dnešku jsou nové jen řádky se
slovem `sidetone` a rozdělení `run_eq_in_place` na `run_eq` + limiter:

```rust
  fn process_inner(&mut self, buf: &mut [f32], channels: usize) {
    let Some(shm) = self.shm.as_ref() else {
      return;
    };
    let layout = shm.layout();
    let h: &ShmHeader = &layout.header;
    h.out_apo_blocks.fetch_add(1, Ordering::Relaxed);
    h.out_apo_sample_rate.store(self.sample_rate as u32, Ordering::Relaxed);
    h.out_apo_channels.store(channels as u32, Ordering::Relaxed);
    // Nobody's settings to apply (app not running for the user at the console): untouched.
    let enabled = self.owner.live(h, Instant::now()) && !h.master_bypass.load(Ordering::Relaxed);
    if enabled {
      let version = h.out_eq_version.load(Ordering::Acquire);
      if self.version != Some(version) {
        self.version = Some(version);
        let (bands, preamp_db) = h.read_out_eq();
        for cascade in self.cascades.iter_mut() {
          cascade.set_bands(&bands, self.sample_rate);
        }
        self.preamp_gain = 10f32.powf(preamp_db / 20.0);
        self.active = self.cascades.first().is_some_and(|c| !c.is_empty()) || preamp_db != 0.0;
      }
    }
    let goal = if enabled && self.active { 1.0 } else { 0.0 };
    // The mono sidetone block for this buffer is in `sidetone_buf` when this is true.
    let sidetone = read_sidetone(
      &mut self.sidetone,
      &mut self.sidetone_buf,
      layout,
      buf.len() / channels,
      self.sample_rate,
      self.sidetone_endpoint,
    );
    let sidetone_started = sidetone && !std::mem::replace(&mut self.sidetone_playing, sidetone);
    if self.wet <= 0.0 && goal <= 0.0 {
      if sidetone {
        if sidetone_started {
          // The limiter did not run while nothing was mixed in: its gain is from back then.
          self.limiter = PeakLimiter::new(self.sample_rate);
        }
        // No EQ, but the sum of the mix and the voice still must not clip.
        add_sidetone(buf, channels, &self.sidetone_buf);
        self.limiter.process(buf, channels);
      }
      return;
    }
    if self.wet <= 0.0 {
      // Fading in after a pass-through stretch: the filter memory is from back then.
      self.cascades.iter_mut().for_each(BiquadCascade::reset_state);
    }
    if self.wet >= 1.0 && goal >= 1.0 {
      // After the EQ (the voice gets no game profile), before the limiter (it guards the sum).
      self.run_eq(buf, channels);
      if sidetone {
        add_sidetone(buf, channels, &self.sidetone_buf);
      }
      self.limiter.process(buf, channels);
      return;
    }
    if self.scratch.len() < buf.len() {
      self.scratch.resize(buf.len(), 0.0);
    }
    let mut scratch = std::mem::take(&mut self.scratch);
    if let Some(wet_buf) = scratch.get_mut(..buf.len()) {
      wet_buf.copy_from_slice(buf);
      self.run_eq_in_place(wet_buf, channels);
      for (dry_frame, wet_frame) in buf.chunks_exact_mut(channels).zip(wet_buf.chunks_exact(channels)) {
        self.wet = if goal > self.wet { (self.wet + self.fade_step).min(1.0) } else { (self.wet - self.fade_step).max(0.0) };
        for (d, w) in dry_frame.iter_mut().zip(wet_frame) {
          *d += self.wet * (w - *d);
        }
      }
    }
    self.scratch = scratch;
    if sidetone {
      // shortcut: during the 20 ms EQ fade the sum only gets the static safety curve.
      add_sidetone(buf, channels, &self.sidetone_buf);
      buf.iter_mut().for_each(|s| *s = soft_limit(*s));
    }
  }

  /// Preamp and the cascades, without the limiter.
  fn run_eq(&mut self, buf: &mut [f32], channels: usize) {
    if self.preamp_gain != 1.0 {
      buf.iter_mut().for_each(|s| *s *= self.preamp_gain);
    }
    for (channel, cascade) in self.cascades.iter_mut().enumerate().take(channels) {
      cascade.process_strided(buf, channels, channel);
    }
  }

  fn run_eq_in_place(&mut self, buf: &mut [f32], channels: usize) {
    self.run_eq(buf, channels);
    // Volume matching / user gain may let peaks pass 0 dBFS: turn them down instead of clipping.
    self.limiter.process(buf, channels);
  }
```

### 3.3 Dvě pomocné funkce

K pomocným funkcím na konci souboru (před testy):

```rust
/// Next sidetone block (mono, `frames` long) into `block`. True = there is audio to mix in.
fn read_sidetone(
  reader: &mut SidetoneReader,
  block: &mut Vec<f32>,
  layout: &ShmLayout,
  frames: usize,
  rate: f32,
  endpoint: u32,
) -> bool {
  let h = &layout.header;
  let r = Ordering::Relaxed;
  let pair = h.sidetone_output_hash.load(r);
  // Reported only by the instance that is the pair's output or cannot tell: the APO of
  // another output would overwrite it.
  if endpoint == pair || endpoint == 0 {
    h.sidetone_output_hash_seen.store(endpoint, r);
  }
  // Only on the pair's output, and only when this endpoint is known to be it: sidetone in
  // speakers is a feedback loop.
  if !h.sidetone_enabled.load(r) || pair == 0 || pair != endpoint {
    reader.pause();
    return false;
  }
  if block.len() < frames {
    block.resize(frames, 0.0);
  }
  let Some(out) = block.get_mut(..frames) else {
    return false;
  };
  let info = reader.read(&layout.sidetone, out, rate);
  h.sidetone_out_rate.store(rate as u32, r);
  h.sidetone_out_block_frames.store(frames as u32, r);
  h.sidetone_underruns.store(reader.underruns(), r);
  h.sidetone_resyncs.store(reader.resyncs(), r);
  if !info.playing {
    return false;
  }
  h.sidetone_reader_beat.fetch_add(1, r);
  h.sidetone_queued_us.store((u64::from(info.queued) * 1_000_000 / u64::from(info.ring_rate.max(1))) as u32, r);
  // Capture to mix; the units are 100 ns.
  let age_us =
    now_hns().zip(layout.sidetone.capture_qpc_hns(info.queued)).map_or(0, |(now, captured)| now.saturating_sub(captured) / 10);
  h.sidetone_age_us.store(u32::try_from(age_us).unwrap_or(u32::MAX), r);
  true
}

/// Adds the mono sidetone to the first two channels (left, right) of an interleaved buffer.
fn add_sidetone(buf: &mut [f32], channels: usize, mono: &[f32]) {
  for (frame, sample) in buf.chunks_exact_mut(channels.max(1)).zip(mono) {
    frame.iter_mut().take(2).for_each(|channel| *channel += sample);
  }
}
```

### 3.4 Test

Do `mod tests` v `lib.rs`:

```rust
  #[test]
  fn sidetone_plays_on_the_pair_output_only() {
    use gamersequalizer_common::sidetone::SidetoneSettings;
    const MIC: u32 = 11;
    const OUT: u32 = 22;
    let name = format!(r"Local\GamersEqualizer_SidetoneTest_{}", std::process::id());
    let shm = SharedMemoryClient::create_named(&name, None).unwrap();
    let control = SharedMemoryClient::open_existing(&name).unwrap();
    let layout = control.layout();
    let h = &layout.header;
    h.write_sidetone(&SidetoneSettings {
      enabled: true,
      gain_db: 0.0,
      noise_mode: NoiseMode::Off,
      low_cut_hz: 0.0,
      ..Default::default()
    });
    h.sidetone_mic_hash.store(MIC, Ordering::Relaxed);
    h.sidetone_output_hash.store(OUT, Ordering::Relaxed);

    let mut tap = SidetoneTap::new(48_000);
    tap.endpoint_hash = MIC;
    let mut output = OutputProcessor::create(Some(shm), 48_000.0, 2).unwrap();
    output.sidetone_endpoint = OUT;
    // 10 ms blocks of a 1 kHz tone at 0.25 on the mic, silence on the output.
    let run = |tap: &mut SidetoneTap, output: &mut OutputProcessor, blocks: usize| -> f32 {
      let mut peak = 0.0f32;
      for k in 0..blocks {
        h.owner_beat.fetch_add(1, Ordering::Release);
        let mic: Vec<f32> =
          (0..480).flat_map(|i| [((k * 480 + i) as f32 * 2.0 * std::f32::consts::PI * 1000.0 / 48_000.0).sin() * 0.25; 2]).collect();
        let before = mic.clone();
        tap.process(layout, &mic, 2, None);
        assert_eq!(mic, before, "the tap only reads the microphone buffer");
        let mut out = vec![0.0f32; 960];
        output.process(&mut out, 2);
        assert!(out.as_chunks::<2>().0.iter().all(|[l, r]| l == r && l.is_finite()));
        if k >= blocks / 2 {
          peak = out.iter().fold(peak, |m, s| m.max(s.abs()));
        }
      }
      peak
    };

    let heard = run(&mut tap, &mut output, 100);
    assert!((heard - 0.25).abs() < 0.02, "sidetone at 0 dB: {heard}");
    assert_eq!(h.sidetone_underruns.load(Ordering::Relaxed), 0);
    assert!(h.sidetone_writer_beat.load(Ordering::Relaxed) > 0 && h.sidetone_reader_beat.load(Ordering::Relaxed) > 0);

    // Another output (speakers), and an output that does not know what it is: silence.
    for endpoint in [OUT + 1, 0] {
      output.sidetone_endpoint = endpoint;
      assert_eq!(run(&mut tap, &mut output, 20), 0.0, "endpoint {endpoint}");
    }
    output.sidetone_endpoint = OUT;
    assert!(run(&mut tap, &mut output, 100) > 0.2, "back on the pair's output");

    // The microphone of another pair stops feeding the ring.
    tap.endpoint_hash = MIC + 1;
    assert_eq!(run(&mut tap, &mut output, 100), 0.0);

    // Muted microphone: silence, no click (the level ramps down first).
    tap.endpoint_hash = MIC;
    assert!(run(&mut tap, &mut output, 100) > 0.2);
    h.mic_muted.store(true, Ordering::Relaxed);
    assert_eq!(run(&mut tap, &mut output, 100), 0.0);
  }

  #[test]
  fn endpoint_hash_rejects_blocks_it_does_not_know() {
    // SAFETY: null, and a buffer of the stated size.
    unsafe {
      assert_eq!(endpoint_hash(0, std::ptr::null()), None);
      let short = [0u8; 8];
      assert_eq!(endpoint_hash(short.len() as u32, short.as_ptr()), None);
      // Right size, but cbSize says another layout (e.g. APOInitSystemEffects3).
      let mut init: APOInitSystemEffects2 = std::mem::zeroed();
      init.APOInit.cbSize = size_of::<APOInitSystemEffects2>() as u32 + 8;
      assert_eq!(endpoint_hash(size_of::<APOInitSystemEffects2>() as u32, &init as *const _ as *const u8), None);
      // Known layout, no device collection.
      init.APOInit.cbSize = size_of::<APOInitSystemEffects2>() as u32;
      assert_eq!(endpoint_hash(size_of::<APOInitSystemEffects2>() as u32, &init as *const _ as *const u8), None);
    }
  }
```

**Ověř:**

```powershell
cargo test -p gamersequalizer-apo
cargo clippy -p gamersequalizer-apo --all-targets
```

**Hotovo, když:** oba nové testy projdou, starý `output_eq_follows_shared_memory_and_master_switch`
projde beze změny (příposlech vypnutý = výstup bit po bitu jako dřív) a clippy je čistý.

## Krok 4: zkouška na skutečném zvuku bez UI

### 4.1 Nástroj `sidetone_probe`

Nový soubor `crates/gamersequalizer-apo/examples/sidetone_probe.rs`:

```rust
//! Sidetone without the UI: switches it on for the default microphone + default output, keeps
//! both devices running and prints what the two APOs report, once per second.
//!
//!   cargo run -p gamersequalizer-apo --example sidetone_probe -- 20              (watch only)
//!   cargo run -p gamersequalizer-apo --example sidetone_probe -- 20 on -12 1     (on, -12 dB, noise mode 1)
//!
//! Noise mode: 0 off, 1 band expander, 2 WebRTC NS, 3 RNNoise, 4 RNNoise 0.2.
//! Needs the app's background service running: it mirrors the settings into the APO mapping.
//! "on" is for the time before the service has its own sidetone thread (that thread
//! overwrites these settings).

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use gamersequalizer_common::shm::SHM_NAME;
use gamersequalizer_common::sidetone::{device_hash, NoiseMode, SidetoneSettings};
use gamersequalizer_mic_vst::SharedMemoryClient;
use windows::Win32::Media::Audio::{
  eCapture, eConsole, eRender, EDataFlow, IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator,
  MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};

fn main() -> windows::core::Result<()> {
  let arg = |n: usize| std::env::args().nth(n);
  let secs: u64 = arg(1).and_then(|s| s.parse().ok()).unwrap_or(20);
  let on = arg(2).as_deref() == Some("on");
  let gain_db: f32 = arg(3).and_then(|s| s.parse().ok()).unwrap_or(-12.0);
  let noise_mode = NoiseMode::from_u32(arg(4).and_then(|s| s.parse().ok()).unwrap_or(1));
  let Ok(shm) = SharedMemoryClient::open_existing(SHM_NAME) else {
    println!("the app's background service is not running ({SHM_NAME} missing)");
    return Ok(());
  };
  let h = &shm.layout().header;
  let r = Ordering::Relaxed;
  // SAFETY: straightforward WASAPI calls on this thread; buffers are only handed back.
  unsafe {
    CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
    let open = |flow: EDataFlow| -> windows::core::Result<(String, IAudioClient)> {
      let device = enumerator.GetDefaultAudioEndpoint(flow, eConsole)?;
      let id = device.GetId()?.to_string().unwrap_or_default();
      let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
      client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 1_000_000, 0, client.GetMixFormat()?, None)?;
      Ok((id, client))
    };
    let (mic_id, mic) = open(eCapture)?;
    let (out_id, out) = open(eRender)?;
    println!("microphone {mic_id}  hash {}", device_hash(&mic_id));
    println!("output     {out_id}  hash {}", device_hash(&out_id));
    let capture: IAudioCaptureClient = mic.GetService()?;
    let render: IAudioRenderClient = out.GetService()?;
    let out_frames = out.GetBufferSize()?;
    if on {
      h.sidetone_mic_hash.store(device_hash(&mic_id), r);
      h.sidetone_output_hash.store(device_hash(&out_id), r);
      h.write_sidetone(&SidetoneSettings { enabled: true, gain_db, noise_mode, ..Default::default() });
    }
    mic.Start()?;
    out.Start()?;

    let end = Instant::now() + Duration::from_secs(secs);
    let mut next_print = Instant::now() + Duration::from_secs(1);
    let (mut writer, mut reader) = (h.sidetone_writer_beat.load(r), h.sidetone_reader_beat.load(r));
    // In a closure, so an error in the loop still reaches the "switch it off" below.
    let outcome = (|| -> windows::core::Result<()> {
     while Instant::now() < end {
      std::thread::sleep(Duration::from_millis(5));
      // Keep the microphone stream drained and the output stream fed with silence.
      while capture.GetNextPacketSize()? > 0 {
        let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
        capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
        capture.ReleaseBuffer(frames)?;
      }
      let free = out_frames.saturating_sub(out.GetCurrentPadding()?);
      if free > 0 {
        render.GetBuffer(free)?;
        render.ReleaseBuffer(free, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
      }
      if Instant::now() < next_print {
        continue;
      }
      next_print += Duration::from_secs(1);
      let (w, rd) = (h.sidetone_writer_beat.load(r), h.sidetone_reader_beat.load(r));
      println!(
        "mic: {} blocks/s x {} frames @ {} Hz, hash seen {}, peak {:.3}, gate {:.1} dB, mode {} (+{} us), chain load peak {} permille",
        w.wrapping_sub(writer),
        h.sidetone_mic_block_frames.load(r),
        h.sidetone_mic_rate.load(r),
        h.sidetone_mic_hash_seen.load(r),
        h.sidetone_peak_permille.load(r) as f32 / 1000.0,
        h.sidetone_gate_gain_db_x10.load(r) as f32 / 10.0,
        h.sidetone_noise_mode_active.load(r),
        h.sidetone_noise_delay_us.load(r),
        h.mic_dsp_load_peak_permille.load(r),
      );
      println!(
        "out: {} blocks/s x {} frames @ {} Hz, hash seen {}, queued {} us, age {} us, underruns {}, resyncs {}",
        rd.wrapping_sub(reader),
        h.sidetone_out_block_frames.load(r),
        h.sidetone_out_rate.load(r),
        h.sidetone_output_hash_seen.load(r),
        h.sidetone_queued_us.load(r),
        h.sidetone_age_us.load(r),
        h.sidetone_underruns.load(r),
        h.sidetone_resyncs.load(r),
      );
      (writer, reader) = (w, rd);
     }
     Ok(())
    })();
    if on {
      h.write_sidetone(&SidetoneSettings::default());
    }
    let _ = mic.Stop();
    let _ = out.Stop();
    outcome
  }
}
```

### 4.2 Sestavit a nainstalovat APO

```powershell
scripts/dev-rebuild.ps1 -Apo -Test -Start
```

**Požádej člověka:** „Nainstaluj prosím nové APO na mikrofon i na sluchátka (banner v aplikaci
nebo `scripts/apo-install.ps1`), nasaď si sluchátka a dej mi vědět." Počkej na odpověď.
Sdílená paměť má novou verzi, staré APO s novou aplikací nemluví.

### 4.3 Zkouška

```powershell
cargo run -p gamersequalizer-apo --example sidetone_probe -- 30 on -12 1
```

Sondu nech vždy doběhnout. Po Ctrl+C zůstane příposlech zapnutý (naposledy zapsané nastavení):
pusť ji pak znovu s `on` a nech ji skončit.

**Požádej člověka:** „Mluv 30 s do mikrofonu. Slyšíš se ve sluchátkách? Je v pauzách ticho?"

**Hotovo, když** (zapiš výpis do DEVLOGu):

| Co ve výpisu         | Čekaná hodnota                                | Když ne                                                              |
| -------------------- | --------------------------------------------- | -------------------------------------------------------------------- |
| `mic: ... hash seen` | stejné číslo jako `microphone ... hash`       | Smí být i 0 (mic APO pak píše také). Jiné nenulové číslo: zastav se. |
| `out: ... hash seen` | stejné číslo jako `output ... hash`           | **Zastav se** (pravidlo 5). Zapiš, co APO dostalo v `Initialize`.    |
| `mic: N blocks/s`    | ~100 při 10 ms periodě                        | 0: mic APO neběží nebo nevidí vlastníka; zkontroluj instalaci.       |
| `out: N blocks/s`    | ~100                                          | 0 při správném otisku: zapiš `queued`, `underruns` a zastav se.      |
| `underruns`          | neroste (po prvních 2 s nejvýš o 1 za minutu) | Zapiš, o kolik roste, a pokračuj; řeší se v kroku 7.                 |
| `age`                | 10 000–25 000 µs při 10 ms periodě            | Zapiš hodnotu.                                                       |
| člověk               | slyší se, v pauzách šum zmizí                 | Zapiš přesně, co slyší.                                              |

Když `out: hash seen` není otisk výstupu (APO hlásí jen shodu nebo 0): přidej do `endpoint_hash`
dočasně uložení `declared` a `size` do dvou volných polí telemetrie (např. `sidetone_queued_us`, `sidetone_age_us`), vypiš je sondou,
zapiš do DEVLOGu a zastav se. Dočasný kód necommituj.

### 4.4 Obsahuje loopback příposlech? (riziko pro AEC)

Echo canceller bere referenci z WASAPI loopbacku. Kdyby v ní byl vlastní hlas, AEC3 by ho bral
jako „hru". Změř to:

```powershell
# 1) příposlech vypnutý, nic nehraje
cargo run -p gamersequalizer-apo --example level_probe -- 5 render
# 2) příposlech zapnutý nahlas, bez potlačení šumu, v jiném okně PowerShellu
cargo run -p gamersequalizer-apo --example sidetone_probe -- 20 on 12 0
#    a současně v prvním okně
cargo run -p gamersequalizer-apo --example level_probe -- 5 render
```

**Požádej člověka**, ať během bodu 2 mluví.

**Hotovo, když** je v DEVLOGu RMS z obou běhů a závěr:

- RMS v bodě 2 výrazně vyšší než v bodě 1 → **loopback příposlech obsahuje**. Zapiš to tučně do
  DEVLOGu, **zastav se** a nech rozhodnout člověka (zadání, kapitola 9, riziko 1).
- RMS stejné → loopback je před EFX, AEC je v bezpečí. Pokračuj.

## Krok 5: daemon

Nový soubor `crates/gamersequalizer-daemon/src/win_audio/sidetone.rs`, do `win_audio/mod.rs`
přidej `pub mod sidetone;`. Vzory: úložiště podle `app_channels.rs`, vlákno podle
`stream_router.rs` (spuštění) a `loopback.rs` (otevírání WASAPI streamů, `device_id`).

### 5.1 Úložiště dvojic

Stejná stavba jako `app_channels.rs` (statický `RwLock<Store>`, soubor, čas změny):

```rust
pub const SIDETONE_FILE: &str = "sidetone.json";
const RELOAD_CHECK: Duration = Duration::from_secs(1);

struct Store {
  path: Option<PathBuf>,
  modified: Option<SystemTime>,
  last_check: Option<Instant>,
  pairs: SidetonePairs,
  /// Bumped on every load and save: readers compare it to know the pairs changed.
  version: u32,
}

/// Point the store at `<base_dir>/sidetone.json` and load it. Call once per process.
pub fn init(base_dir: &Path);
/// The saved pairs and their version (re-read from disk when the file changed).
pub fn pairs() -> (SidetonePairs, u32);
/// Replace the pairs and save at once (the window calls this).
pub fn save(pairs: SidetonePairs);
```

- Načtení: `SidetonePairs::from_json`; chyba → `log::warn!`, `keep_corrupt(&path)` (jako
  `AudioSettings::load_or_default`), prázdná matice. Chybějící soubor → prázdná matice.
- Uložení: `to_json` + `write_atomic`; chyba → `log::error!`, nikdy panic.
- `init` zavolej v `main.rs` na obou místech, kde se volá `win_audio::app_channels::init`
  (daemon i okno).

Test (v `sidetone.rs`, dočasná složka): uložit dvě dvojice, `init` znovu, `pairs()` vrátí
totéž; poškozený soubor → prázdná matice a běží se dál.

### 5.2 Vlákno

```rust
const POLL: Duration = Duration::from_millis(250);
const BEAT_FRESH: Duration = Duration::from_secs(1);
const SLEEP_ON_ERROR: Duration = Duration::from_secs(1);

/// Runs for the daemon's life: publishes the current pair's settings and keeps the devices
/// running while sidetone needs them.
pub fn spawn() -> Option<std::thread::JoinHandle<()>>;
```

Spusť v `main.rs` hned za `stream_router::spawn()`, se stejným hlášením chyby do logu.
Vlákno: `CoInitializeEx(COINIT_MULTITHREADED)`, `SharedMemoryClient::open_or_create()`, pak
smyčka s dohledem (`architecture.md`): chyba iterace → `warn!`, zavřít držené streamy,
`SLEEP_ON_ERROR`, pokračovat.

Jedna iterace (každých `POLL`), přesně v tomto pořadí:

1. **Dvojice.** `mic_id` = `header.get_selected_mic_id()`, jinak
   `loopback::resolve_default_capture_id()`. `out_id` = `loopback::resolve_render_id(None)`.
2. **Nastavení.** `(pairs, version) = pairs()`. Když se změnilo `mic_id`, `out_id` nebo
   `version` proti minulé iteraci:
   - `settings` = `pairs.find(mic, out).map(|p| p.settings())`, jinak `SidetoneSettings::default()`
     (vypnuto). Chybí-li některé id, také vypnuto.
   - nejdřív otisky: `header.sidetone_mic_hash` a `sidetone_output_hash` = `device_hash(id)`,
     při vypnutém příposlechu 0;
   - potom `header.write_sidetone(&settings)`;
   - `info!("[Sidetone] pair {mic} -> {out}: on/off")`.

   Jindy nastavení **nepřepisuj**: okno je při tažení posuvníku zapisuje přímo (krok 6).
3. **Tepy.** Pomocný typ „hodnota se změnila před méně než `BEAT_FRESH`" (uložená poslední
   hodnota a `Instant` poslední změny):
   - `testing` = tep `header.sidetone_test_beat`;
   - `feeding` = tep `header.sidetone_writer_beat`.
4. **Co držet otevřené.** `enabled` = `header.read_sidetone().enabled`.
   - přehrávací stream na `out_id`: `enabled && (testing || feeding)`;
   - nahrávací stream na `mic_id`: `enabled && testing`.
   (Krok 7 k tomu přidá nízkou latenci.)
5. **Srovnat skutečnost.** Otevři, co má být otevřené a není (nebo je na jiném zařízení),
   zavři, co být otevřené nemá. Do otevřeného přehrávacího streamu doplň ticho
   (`GetCurrentPadding`, `GetBuffer`, `ReleaseBuffer(.., AUDCLNT_BUFFERFLAGS_SILENT)`),
   nahrávací jen vyprázdni (`GetNextPacketSize` / `GetBuffer` / `ReleaseBuffer`). Chyba kteréhokoli
   volání → vrátit `Err` (dohled streamy zavře a zkusí to znovu).

Otevření streamu (obyčejné, 10 ms): zařízení přes `enumerator.GetDevice(PCWSTR(id))` jako
v `loopback.rs`, `Activate::<IAudioClient>`, `GetMixFormat`, `Initialize(AUDCLNT_SHAREMODE_SHARED,
0, 1_000_000 /* 100 ms */, 0, format, None)`, `GetService`, `Start`. Formát uvolni
`CoTaskMemFree` (v `Drop` typu, který stream drží, stejně jako `StreamResources`).

**Ověř:**

```powershell
cargo test -p gamersequalizer-daemon sidetone
scripts/dev-rebuild.ps1 -Test -Start
```

Do `%APPDATA%\GamersEqualizer\profiles\sidetone.json` ručně zapiš jednu dvojici se zapnutým
příposlechem (id vezmi z výpisu `sidetone_probe`):

```json
{ "pairs": [ { "mic_id": "<id mikrofonu>", "output_id": "<id výstupu>", "enabled": true, "gain_db": -12.0 } ] }
```

a sleduj `cargo run -p gamersequalizer-apo --example sidetone_probe -- 60` (bez `on`).

**Hotovo, když** (zapiš do DEVLOGu, co se stalo v každém bodě):

1. Nic nenahrává z mikrofonu: `mic: 0 blocks/s`, indikátor mikrofonu ve Windows nesvítí.
   *(Sonda sama mikrofon otevírá: pro tento bod ji ukonči a zeptej se člověka na indikátor.)*
2. Člověk otevře aplikaci, která nahrává (Discord, Hlasový záznamník): do 1 s hraje příposlech,
   i když nic jiného nehraje.
3. Člověk přepne výchozí výstup na jiné zařízení: do 1 s `out: 0 blocks/s`, v logu
   `[Sidetone] pair ...: off`. Po přepnutí zpět hraje znovu.
4. Člověk ztlumí mikrofon ve Windows: příposlech ztichne bez lupnutí.
5. Hlavní vypínač v záhlaví aplikace vypne i příposlech.

## Krok 6: sekce „Příposlech" v okně

Soubory: `ui/state.rs` (`ActiveTab::Sidetone`, stav sekce), `ui/header.rs` (`NAV`), `ui/app.rs`
(větev v `match`), nový `ui/tab_sidetone.rs`, `ui/mod.rs`, `ui/audio_settings.rs`,
`i18n_cs.rs`. Vzor stránky: `ui/tab_mic.rs` (`theme::page_header`, `theme::card`,
`theme::card_title`, `theme::toggle_labeled`, `theme::meter`, `theme::status_line`,
`theme::info_bar`).

### 6.1 Navigace

- `ActiveTab`: nová varianta `Sidetone`.
- `NAV` v `header.rs`: délku pole změň z 10 na 12 a mezi sekce „Output" a „Input" vlož:

  ```rust
    Nav::Section(tk("Sidetone")),
    Nav::Item(ActiveTab::Sidetone, icons::HEADPHONES, tk("Sidetone"), tk("Your own voice in the headphones")),
  ```

- `app.rs`: do `match self.state.active_tab` přidej
  `ActiveTab::Sidetone => super::tab_sidetone::render_tab_sidetone(ui, &mut self.state, &mut self.win_audio),`
  (uvnitř společné `ScrollArea`, jako `Headphones`).
- V `app.rs` před `match`: když `active_tab != ActiveTab::Sidetone`, nastav
  `self.state.sidetone_ui.testing = false` (zkouška končí odchodem ze sekce).

### 6.2 Stav sekce

V `AppState` nové pole `pub sidetone_ui: SidetoneUi` s:

- `testing: bool`: tlačítko „Vyzkoušet" je zapnuté;
- `pair: SidetonePair`: rozpracované nastavení aktuální dvojice;
- `pair_key: (String, String)`: (mikrofon, výstup), ke kterým `pair` patří;
- `dirty: bool`: změněno a neuloženo.

### 6.3 Co stránka dělá každý snímek

1. Aktuální dvojice: mikrofon = `state.selected_mic_endpoint_id`, jinak výchozí mikrofon;
   výstup = výchozí výstup (`win_audio.endpoint_lock.get_default_output_id()`). Názvy
   z `state.capture_endpoints` a `state.endpoints`.
2. Když se klíč liší od `pair_key`: `pair` = uložená dvojice z `sidetone::pairs()`, jinak
   `SidetonePair::new(mic_id, out_id, mic_name, out_name)`; `dirty = false`.
3. Vykresli prvky (tabulka níže). Při každé změně hodnoty:
   - `pair.set_settings(&settings)`, `dirty = true`;
   - hned zapiš do sdílené paměti: `header.write_sidetone(&settings)` (odezva při tažení);
   - při zapnutí vypínače dvojice navíc zapiš otisky `sidetone_mic_hash` / `sidetone_output_hash`.
4. Když `dirty` a žádný posuvník se právě netáhne (`!ui.ctx().is_using_pointer()`):
   `pairs.upsert(pair.clone())`, `sidetone::save(pairs)`, `dirty = false`.
5. Když `testing`: `header.sidetone_test_beat.fetch_add(1, Ordering::Relaxed)` a
   `ui.ctx().request_repaint_after(Duration::from_millis(200))`. Bez toho egui v klidu
   nepřekresluje, tep se zastaví a daemon zkoušku ukončí.

### 6.4 Prvky

| Prvek                    | Typ                        | Hodnota / rozsah                                     | Kam se zapisuje                      |
| ------------------------ | -------------------------- | ---------------------------------------------------- | ------------------------------------ |
| Dvojice                  | text                       | „{mikrofon} → {výstup}"                              | -                                    |
| Zapnuto pro tuto dvojici | `theme::toggle_labeled`    | `enabled`                                            | dvojice                              |
| Upozornění               | `theme::info_bar` (Info)   | stálý text, viz texty                                | -                                    |
| Stav                     | `theme::status_line`       | viz 6.5                                              | -                                    |
| Vyzkoušet                | tlačítko zap/vyp           | `sidetone_ui.testing`; jen když je dvojice zapnutá   | jen tep                              |
| Hlasitost                | posuvník, krok 0,5 dB      | `GAIN_DB_MIN`..`GAIN_DB_MAX`                         | dvojice                              |
| Úroveň                   | `theme::meter`             | `sidetone_peak_permille`                             | -                                    |
| Potlačení šumu           | výběr z 5                  | `NoiseMode`, u každé volby zpoždění                  | dvojice                              |
| Citlivost                | posuvník, 1 dB             | `GATE_MARGIN_DB_MIN`..`MAX`; jen pro expander        | dvojice                              |
| Hloubka                  | posuvník, 1 dB             | 0..`GATE_RANGE_DB_MAX`; ne pro „vypnuto" a WebRTC NS | dvojice                              |
| Útlum expanderu          | `theme::meter`             | `sidetone_gate_gain_db_x10`; jen pro expander        | -                                    |
| Ořez hloubek             | vypínač + posuvník, 10 Hz  | 0 nebo `LOW_CUT_HZ_MIN`..`MAX`                       | dvojice                              |
| Ořez výšek               | vypínač + posuvník, 500 Hz | 0 nebo `HIGH_CUT_HZ_MIN`..`MAX`                      | dvojice                              |
| Nízká latence            | `theme::toggle_labeled`    | `header.sidetone_low_latency`                        | `AudioSettings.sidetone_low_latency` |
| Latence                  | text, barva                | viz 6.5                                              | -                                    |
| Uložené dvojice          | seznam + „Smazat"          | `mic_name → output_name`, zapnuto/vypnuto            | `pairs.remove`, `sidetone::save`     |

Konstanty jsou v `gamersequalizer_common::sidetone`. Zpoždění u voleb potlačení šumu: expander
a vypnuto „bez zpoždění"; ostatní `gamersequalizer_mic_vst::sidetone_denoise` (333, 480, 960
vzorků při 48 kHz → „+7 ms", „+10 ms", „+20 ms"). U běžící volby ukaž skutečnou hodnotu
`sidetone_noise_delay_us`. Když `sidetone_noise_mode_active` ≠ zvolený režim, ukaž pod výběrem
varování „Běží expander: tento režim potřebuje mikrofon na 48 kHz." (resp. náhradní RNNoise).

`AudioSettings`: nové pole `sidetone_low_latency: bool` (výchozí `false`), doplň do
`from_header` a do místa, kde se uložené hodnoty zapisují do sdílené paměti.

### 6.5 Stav a latence

Stav (první platný řádek shora):

| Podmínka                                                                     | Text                                                 | Závažnost |
| ---------------------------------------------------------------------------- | ---------------------------------------------------- | --------- |
| APO chybí na mikrofonu nebo výstupu                                          | dnešní `ApoBanner`                                   | -         |
| dvojice vypnutá                                                              | „Off for this pair."                                 | Info      |
| `mic_muted`                                                                  | „The microphone is muted: sidetone is silent."       | Warn      |
| `sidetone_output_hash_seen` ≠ 0 a ≠ `sidetone_output_hash`, tep čtečky stojí | „Another output is playing: sidetone waits for {}."  | Info      |
| tep zapisovače stojí                                                         | „Waiting: no app is recording from this microphone." | Info      |
| tep čtečky běží                                                              | „Playing."                                           | Ok        |
| jinak                                                                        | „Starting…"                                          | Info      |

„Tep běží" = hodnota se změnila za posledních 1 s (ulož si poslední hodnotu a čas změny, jako
`out_apo_seen` v `AppState`).

Latence (jen když tep čtečky běží):

```
latence_us = sidetone_age_us + sidetone_noise_delay_us
           + sidetone_out_block_frames * 1_000_000 / sidetone_out_rate
```

Text: „Delay about {} ms (without the converters)". Barva: zelená ≤ 10 ms, žlutá ≤ 20 ms,
červená víc. Pod tím malým písmem periody: „Microphone {} ms, output {} ms" z
`sidetone_mic_block_frames / sidetone_mic_rate` a `sidetone_out_block_frames / sidetone_out_rate`,
a „Dropouts: {}" = `sidetone_underruns`.

### 6.6 Texty

Všechny texty přes `tr` / `trf` / `tk`, anglický klíč + český překlad do `i18n_cs.rs`
(nová skupina `// --- sidetone ---`). Použij přesně tyto:

| Anglicky (klíč)                                                                       | Česky                                                                              |
| ------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------- |
| Sidetone                                                                              | Příposlech                                                                         |
| Your own voice in the headphones                                                      | Vlastní hlas ve sluchátkách                                                        |
| Sealed headphones block your own voice. Sidetone plays the microphone back into them. | Těsnící sluchátka utlumí vlastní hlas. Příposlech vrací mikrofon do sluchátek.     |
| {} → {}                                                                               | {} → {}                                                                            |
| On for this pair                                                                      | Zapnuto pro tuto dvojici                                                           |
| Sidetone plays only while an app is recording from the microphone.                    | Příposlech hraje, jen když z mikrofonu nahrává nějaká aplikace.                    |
| Try it                                                                                | Vyzkoušet                                                                          |
| Stop trying                                                                           | Ukončit zkoušku                                                                    |
| Plays without a recording app. Windows shows the microphone as in use meanwhile.      | Hraje i bez nahrávající aplikace. Windows mezitím ukazuje mikrofon jako používaný. |
| Volume                                                                                | Hlasitost                                                                          |
| Level                                                                                 | Úroveň                                                                             |
| Noise suppression                                                                     | Potlačení šumu                                                                     |
| Off                                                                                   | Vypnuto                                                                            |
| Band expander (no delay)                                                              | Pásmový expander (bez zpoždění)                                                    |
| WebRTC noise suppressor (+{} ms)                                                      | Potlačení šumu WebRTC (+{} ms)                                                     |
| RNNoise (+{} ms)                                                                      | RNNoise (+{} ms)                                                                   |
| RNNoise 0.2 (+{} ms)                                                                  | RNNoise 0.2 (+{} ms)                                                               |
| The expander removes steady noise in pauses. Loud short sounds (keys) pass.           | Expander odstraní stálý šum v pauzách. Hlasité krátké zvuky (klávesy) projdou.     |
| Running the band expander: this mode needs the microphone at 48 kHz.                  | Běží pásmový expander: tento režim potřebuje mikrofon na 48 kHz.                   |
| Running RNNoise: RNNoise 0.2 could not be loaded.                                     | Běží RNNoise: RNNoise 0.2 se nepodařilo načíst.                                    |
| Sensitivity                                                                           | Citlivost                                                                          |
| Depth                                                                                 | Hloubka                                                                            |
| Reduction now                                                                         | Útlum teď                                                                          |
| Tone                                                                                  | Zabarvení                                                                          |
| Low cut                                                                               | Ořez hloubek                                                                       |
| High cut                                                                              | Ořez výšek                                                                         |
| Low latency                                                                           | Nízká latence                                                                      |
| Asks Windows for its shortest audio period on both devices. Uses more CPU.            | Požádá Windows o nejkratší periodu zvuku na obou zařízeních. Zatíží víc procesor.  |
| Off for this pair.                                                                    | Pro tuto dvojici vypnuto.                                                          |
| The microphone is muted: sidetone is silent.                                          | Mikrofon je ztlumený: příposlech mlčí.                                             |
| Another output is playing: sidetone waits for {}.                                     | Hraje jiný výstup: příposlech čeká na {}.                                          |
| Waiting: no app is recording from this microphone.                                    | Čeká: z tohoto mikrofonu žádná aplikace nenahrává.                                 |
| Playing.                                                                              | Hraje.                                                                             |
| Starting…                                                                             | Spouští se…                                                                        |
| Delay about {} ms (without the converters)                                            | Zpoždění asi {} ms (bez převodníků)                                                |
| Microphone {} ms, output {} ms                                                        | Mikrofon {} ms, výstup {} ms                                                       |
| Dropouts: {}                                                                          | Výpadky: {}                                                                        |
| Saved pairs                                                                           | Uložené dvojice                                                                    |
| Delete                                                                                | Smazat                                                                             |
| Exclusive-mode playback bypasses Windows effects: sidetone is silent then.            | Přehrávání ve výhradním režimu obchází efekty Windows: příposlech pak mlčí.        |

Klíč, který už v `i18n_cs.rs` je (např. „Off", „Volume", „Delete"), nepřidávej podruhé.

**Ověř:**

```powershell
cargo test -p gamersequalizer-daemon
cargo clippy --workspace --all-targets
scripts/dev-rebuild.ps1 -Test -Start
```

**Hotovo, když** (**požádej člověka**, ať projde body a napíše výsledek ke každému):

1. Sekce „Příposlech" je v navigaci mezi Výstupem a Vstupem; `cargo test` nehlásí chybějící překlad.
2. Zapnutí dvojice + „Vyzkoušet": slyší se bez Discordu, stav „Hraje.", indikátor mikrofonu svítí.
3. Odchod na jinou kartu: do 1 s ticho, indikátor zhasne. Návrat: tlačítko je vypnuté.
4. Tažení hlasitosti: mění se plynule, bez lupání. Po puštění je hodnota v `sidetone.json`.
5. Zavření okna a nové otevření: hodnoty zůstaly.
6. Druhý headset (jiný mikrofon nebo jiný výstup): stránka ukáže novou dvojici, vypnutou.
   Po zapnutí jsou v seznamu „Uložené dvojice" dvě. Přepnutí zpět použije první nastavení.
7. „Smazat" u dvojice ji odstraní ze seznamu i ze souboru.
8. Všech pět režimů potlačení šumu jde zvolit; u každého stav ukáže zpoždění; přepnutí
   udělá nejvýš krátkou mezeru.

## Krok 7: nízká latence

Jen v `win_audio/sidetone.rs`. Když `header.sidetone_low_latency` je `true`:

1. Otevírej držené streamy přes `IAudioClient3`:
   - `let client3: IAudioClient3 = client.cast()?;`
   - `GetSharedModeEnginePeriod(format, &mut default, &mut fundamental, &mut min, &mut max)`;
   - `client3.InitializeSharedAudioStream(0, min, format, None)`;
   - chyba `AUDCLNT_E_ENGINE_PERIODICITY_LOCKED`: `GetCurrentSharedModeEnginePeriod` a
     inicializuj s tou periodou; zapiš `info!` s oběma hodnotami;
   - jakákoli jiná chyba: `warn!` a otevři obyčejně jako v kroku 5. Nikdy `Err` kvůli tomu,
     že nízká latence nejde.
2. Podmínky z kroku 5 bod 4 se mění na:
   - přehrávací stream: `enabled && (testing || feeding)` (beze změny, jen krátká perioda);
   - nahrávací stream: `enabled && (testing || (low_latency && crate::mic_usage::microphone_in_use()))`.
     `microphone_in_use` nepočítá naši aplikaci, takže se vlastním streamem nedrží sama.
3. Změna přepínače = zavřít a znovu otevřít držené streamy.

**Pojistka zátěže.** Krátká perioda na mikrofonu dává řetězci (AEC, model) na jedno volání
méně času. Po zapnutí sleduj 5 minut řeči `mic: ... chain load peak` v `sidetone_probe`
a `mic_call_gaps`:

- špička pod 600 ‰ a `mic_call_gaps` neroste: v pořádku;
- jinak v `sidetone.rs` nízkou latenci na **mikrofonu** nepoužívej (nahrávací stream otevírej
  obyčejně), nech ji jen na výstupu, a zapiš to do DEVLOGu i s čísly.

Pozor: rámcové režimy (WebRTC NS, RNNoise) z krátké periody na mikrofonu nic nemají (zadání,
kapitola 5.1).

**Hotovo, když** je v DEVLOGu tabulka: perioda mikrofonu a výstupu (ze sondy), `age`, zátěž
řetězce a `underruns` za 5 minut, pro „nízká latence vypnuto" a „zapnuto".

## Krok 8: měření a zápis

Každé měření zapiš do DEVLOGu. Nic neupravuj podle výsledků; rozhodne člověk.

### 8.1 Skutečná latence ústa → ucho

1. `scripts/dev-rebuild.ps1 -Apo -DebugCapture`, **požádej člověka** o instalaci.
2. **Požádej člověka:** „Zapni příposlech (zkouška), hlasitost +6 dB, potlačení šumu vypnuto.
   Přitiskni jedno sluchátko k mikrofonu a pětkrát ťukni nehtem do mikrofonu, po sekundě."
3. `cargo run --release -p gamersequalizer-apo --example debug_capture -- 10`
4. V surové stopě (vstup řetězce) je každé ťuknutí dvakrát: přímo a z příposlechu. Změř
   rozestup (vrchol autokorelace mezi 2 a 80 ms). To je latence včetně převodníků.
5. Opakuj pro: 10 ms periody; nízká latence; každý rámcový režim potlačení šumu.
6. APO s `-DebugCapture` potom nahraď obyčejným (`scripts/dev-rebuild.ps1 -Apo`, instalace).

Tabulka do DEVLOGu: režim, perioda mikrofonu, perioda výstupu, změřeno ms, číslo z UI ms.

### 8.2 AEC se zapnutým příposlechem

Hudba hraje do sluchátek dvojice 60 s a člověk občas mluví. Jednou s vypnutým a jednou se
zapnutým příposlechem. Z logu `[Chain]` porovnej stav AEC,
zpoždění a sílu echa. Čekaný výsledek: bez rozdílu.

### 8.3 Jedno pásmo, nebo čtyři?

Na nahrávce šumu mikrofonu s řečí z `test_data/private` (ignorovaný test v `sidetone.rs`, vzor:
testy v `aec3.rs`, které čtou nahrávky): `BandExpander` s `set_single_band(true)` a `false`.
Zapiš útlum šumu v pauzách a ve slovech (RMS výstup/vstup po úsecích). Kód neměň.

### 8.4 Kolik který režim ubere z hlasu

Stejná nahrávka přes `SidetoneDenoiser` ve třech režimech a přes `BandExpander`: RMS ve
slovech a v pauzách proti vstupu. Tabulka do DEVLOGu.

### 8.5 Zápis

Milník v DEVLOGu: co je hotové, tabulky z kroků 1, 4, 7 a 8, seznam oprav kódu z návodu
(pravidlo 1), co zůstalo otevřené. Commit, push. Nemerguj.
