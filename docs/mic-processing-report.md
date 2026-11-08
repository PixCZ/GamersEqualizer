# Zpracování mikrofonu v GamersEqualizer: popis a naměřené výsledky

Podklad pro návrh vylepšení potlačení hluku. Popisuje celý řetězec zpracování mikrofonu, jak je
nastavená každá proměnná a jak funguje každý algoritmus (s ukázkami kódu). Pak následují výsledky
všech testů včetně dlouhé nahrávky vývojáře podle testovacího scénáře.

Stav kódu: větev `main`, commit `8105ce1` („Gate never cuts the voice: output detector and -12 dB floor“)
plus necommitnuté úpravy v pracovním stromu. Měřeno 2026-11-06 na vývojářově PC (Windows 11, release build).

**Aktualizace 2026-11-08:** popis řetězce (kapitoly 2–7) odpovídá kódu po milnících 29–31 v `DEVLOG.md`:
AEC3 (`sonora`) místo NLMS/MDF, pevný HP 70 Hz před denoiserem, lookahead podle modelu, floor gate vázaný
na leveler, učící leveler, peak limiter. Čísla v kapitolách 9–10 a v příloze A jsou pořád z 2026-11-06
(před těmito změnami). Nová čísla jsou v DEVLOG.

Rozlišení v textu: **Naměřeno** = číslo z testu. **Odvozeno z kódu** = plyne přímo ze zdrojáku.
**Hypotéza** = vysvětlení, které ještě nikdo neověřil poslechem ani testem.

---

## 1. Omezení, která musí každý návrh respektovat

| Omezení                         | Proč / kde                                                                                 |
| ------------------------------- | ------------------------------------------------------------------------------------------ |
| Běží uvnitř `audiodg.exe`       | Capture APO. Pád = výpadek zvuku celého systému. Každé volání DSP je v `catch_unwind`.      |
| Žádné panic za běhu             | Lints: `unwrap_used`, `expect_used`, `panic`, `indexing_slicing` (warn). Chyby jsou hodnoty. |
| Real-time vlákno                | Žádné blokování (`try_lock` only), žádné alokace v ustáleném stavu, bloky ~10 ms.          |
| Řetězec běží na 48 kHz mono     | Jiné frekvence převádí `rate_bridge` (rubato sinc, +~5 ms). Zpracuje se 1. kanál a zkopíruje se do všech. |
| Jen 32-bit float formáty        | APO jiné formáty nepřijme (pass-through).                                                  |
| CPU rozpočet                    | Těžký model, který 3 okna po ~100 ms za sebou nestíhá, se přepne na RNNoise.               |
| Modely jsou vložené v DLL       | `include_bytes!`: APO nikdy nečte z disku. Runtime je `tract` 0.23.10 pro oba těžké modely (DFN3 přes vlastní fork `crates/vendor/deep_filter`). |
| `tract` runtime není `Send`     | Model nejde načíst na jiném vlákně. Přepnutí modelu za běhu = jednorázová mezera ~0,1–0,2 s. |
| Jen oficiální modely/API        | Žádné cloudové služby, vše lokálně.                                                        |
| Nastavení přes sdílenou paměť   | Daemon (UI) zapisuje atomiky do Global mappingu, APO je čte v každém bloku.                |

---

## 2. Cesta signálu

```
Fyzický mikrofon
  └─ Windows audio engine (audiodg.exe), capture endpoint
       └─ GamersEqualizer Capture APO  (crates/gamersequalizer-apo/src/lib.rs, MicProcessor)
            ├─ owner check: když neběží daemon pro uživatele u konzole → surový mic (pass-through)
            ├─ 1. kanál → mono_in
            ├─ MicClock: vyhlazený QPC čas bloku (pro AEC)
            ├─ RateBridge (jen když zařízení ≠ 48 kHz): device rate → 48 kHz → … → device rate
            └─ GamersEqualizerPlugin::mic_process  (crates/gamersequalizer-mic-vst/src/plugin.rs)
                 0. master_bypass? → surový signál, konec
                 1. AEC: DelayEstimator (GCC-PHAT) + WebRTC AEC3 (sonora)  [jen když hraje reference]
                 2. pevný high-pass 70 Hz (2. řád), pak AiVoiceIsolator (ai_denoiser.rs)
                      a. VAD na surovém vstupu (RNNoise prob.)
                      b. denoiser: RNNoise | DeepFilterNet 3 | DPDFNet-2
                      c. VAD na výstupu denoiseru
                      d. lookahead delay (výchozí zapnuto): RNNoise 15, DFN3 15, DPDFNet 10 ms
                      e. noise gate (VAD-keyed, floor -12 dB a o zisk leveleru níž, max -24 dB)
                 3. Parametrický EQ (biquad kaskáda; výchozí HP 100 Hz/48 dB, LP 9 kHz/24 dB)
                 4. Učící voice leveler (VoiceLeveler: cíl -24 dBFS, zisk -18 až +24 dB)
                 5. Hlasitost/boost (0–300 %) + PeakLimiter (okamžitý attack, release 80 ms, strop -0,26 dBFS)
            └─ výsledek do všech kanálů
  └─ aplikace (Discord, hra, …) dostanou už zpracovaný signál
```

Existuje i starší cesta `crates/gamersequalizer-daemon/src/win_audio/mic_stream.rs`: WASAPI stream
v daemonu → stejný `GamersEqualizerPlugin` → virtuální mikrofon. Používá se, když APO není
nainstalované. DSP je stejné.

### 2.1 APO: vstup do řetězce

```rust
// crates/gamersequalizer-apo/src/lib.rs — MicProcessor::process
let qpc_hns = self.clock.next(qpc_hns, frames as u64);
if *follow_owner && !plugin.shm_header().is_some_and(|h| owner.live(h, Instant::now())) {
  return; // nikdo nemá nastavení → surový mic
}
for (dst, frame) in input.iter_mut().zip(buf.chunks_exact(channels)) {
  *dst = frame.first().copied().unwrap_or(0.0);   // jen 1. kanál
}
let ran = catch_unwind(AssertUnwindSafe(|| match bridge {
  Some(bridge) => { /* resample → plugin.process_timed → resample zpět, clamp ±1 */ }
  None => { plugin.process_timed(&[&*input], &mut [&mut *output], qpc_hns); true }
}));
if !matches!(ran, Ok(true)) { *failed = true; return; } // po panice navždy pass-through
for (frame, &sample) in buf.chunks_exact_mut(channels).zip(output.iter()) {
  frame.fill(sample);                                  // mono do všech kanálů
}
```

`OWNER_TIMEOUT = 3 s`: APO zpracovává jen tehdy, když daemon během posledních 3 s změnil `owner_beat`.

---

## 3. Nastavení (sdílená paměť) a výchozí hodnoty

| Pole v `ShmHeader`              | Typ / rozsah                  | Výchozí                       | Kdo čte                       |
| ------------------------------- | ----------------------------- | ----------------------------- | ----------------------------- |
| `master_bypass`                 | bool                          | `false`                       | plugin: vše vypnuto           |
| `aec_enabled`                   | bool                          | `true`                        | plugin                        |
| `ai_voice_strength_permille`    | 0–1000                        | `750` (síla 0,75)             | denoiser                      |
| `noise_gate_threshold_db`       | i32 = režim                   | `1` (`NOISE_GATE_AUTO_LOOKAHEAD`) | gate                      |
| `denoise_model`                 | 0 RNNoise, 1 DFN3, 2 DPDFNet  | `0` RNNoise                   | denoiser                      |
| `mic_volume_permille`           | 1000–3000 = boost             | `1000`                        | boost                         |
| `mic_eq_version` + pásma        | biquady                       | preset „Clean voice“          | EQ                            |
| `aec_cached_delay_samples`      | vzorky 48 kHz                 | 0                             | warm start AEC                |

Režim gate (`crates/gamersequalizer-common/src/shm.rs`):

```rust
pub const NOISE_GATE_AUTO: i32 = 0;            // automatická gate
pub const NOISE_GATE_AUTO_LOOKAHEAD: i32 = 1;  // + lookahead 10–15 ms podle modelu („Chránit začátky slov“), výchozí
pub const NOISE_GATE_OFF: i32 = -80;           // ≤ -79,5 = vypnuto
```

Pole se historicky jmenuje „threshold_db“, ale prahová úroveň už neexistuje. Gate je řízená
detektorem řeči, ne hlasitostí.

---

## 4. Fáze 1: AEC (potlačení ozvěny z reproduktorů)

Kód: `mic-vst/src/delay_estimator.rs`, `mic-vst/src/aec3.rs`, `plugin.rs::run_aec`.

**Reference:** render APO (nebo loopback) zapisuje, co hraje do reproduktorů, do ring bufferu ve
sdílené paměti s QPC časy. Mic blok se k referenci zarovná podle QPC času (`ref_index_at(t)`).

**Odhad zpoždění (GCC-PHAT):**

| Konstanta              | Hodnota        | Význam                                                          |
| ---------------------- | -------------- | --------------------------------------------------------------- |
| `DS_FACTOR`            | 6              | korelace na 8 kHz                                               |
| `WINDOW_DS`            | 8192 (~1,02 s) | délka okna jednoho odhadu                                       |
| `MAX_DELAY_DS`         | 3200 (400 ms)  | nejdelší hledané zpoždění                                       |
| `FFT_LEN`              | 16384          | korelace bez aliasu                                             |
| `MIN_PEAK_RATIO`       | 10             | peak/průměr PHAT korelace pod tím = okno se zahodí              |
| `AGREE_SAMPLES`        | 96 (2 ms)      | dva odhady „souhlasí“                                           |
| `CONFIRM_FIRST`        | 2              | souhlasných oken pro první zámek                                |
| `CONFIRM_SWITCH`       | 3              | pro změnu zamčeného zpoždění                                    |
| `REFINE_WINDOWS`       | 30             | průměrování (~30 s EMA)                                         |
| `MIN_REF_RMS`          | 0,002 (-54 dBFS) | reference musí nést signál                                    |
| `MIN_MIC_RMS`          | 0,0003 (-70 dBFS) |                                                              |

**Filtr: WebRTC AEC3 (`sonora`, pure Rust, BSD-3)** místo dřívějšího NLMS a pak MDF (odstraněn 2026-11-08):

```rust
// aec3.rs
pub const FRAME: usize = 480;                  // AEC3 pracuje po 10 ms
pub const PRE_DELAY_SAMPLES: usize = 144;      // reference jde 3 ms před změřeným zpožděním
pub const AEC3_LATENCY_SAMPLES: usize = 430;   // ~9 ms vlastní latence (band split + rámce)
// Config: 48 kHz interně, vlastní high-pass AEC3 vypnutý (běží jen s referencí,
// basy mikrofonu by se zapínaly a vypínaly s druhou stranou).
```

Lineární filtr (coarse + refined) do 8 kHz plus nelineární supresor zbytku ve všech pásmech do 24 kHz
s comfort noise. Selhání rámce nebo NaN = rámec projde nezpracovaný.

Naměřeno (A/B na debug capturech 2026-11-07): o 10–25 dB víc odstraněné ozvěny než MDF, když hraje jen
druhá strana, ~1,5 dB z hlasu při dvojí řeči. Syntetické testy v `aec3.rs`: dozvuk 200 ms 28 dB,
dvojí řeč 54 dB ozvěny pryč / hlas -1,6 dB, tichý hlas pod hudbou 37,5 dB / -1,3 dB, změna cesty 35 dB.

Maska `echo_only` (gate zavřená při čisté ozvěně) se nezavedla: po AEC3 slyší vstupní detektor řeč
v blocích „jen ozvěna“ jen v 1–11 % a zbytek za denoiserem má -55 až -66 dBFS (`echo_gate_report`).

---

## 5. Fáze 2: AiVoiceIsolator (denoiser + gate)

Kód: `crates/gamersequalizer-mic-vst/src/ai_denoiser.rs`, `dpdfnet.rs`.

### 5.1 Pořadí v jednom bloku

```rust
pub fn process_samples(&mut self, samples: &mut [f32]) {
  // výběr aktivního modelu (těžký jen když je načtený a není přetížený), jinak RNNoise
  if self.strength <= 0.001 { self.bypassed = true; return; }   // síla 0 = bypass
  let voice = self.vad.process(samples);                        // VAD na SUROVÉM vstupu
  match (self.active, ...) {
    (DeepFilterNet, Some(df), _) => df.process(samples, self.strength),
    (DpdfNet, _, Some(dp))       => dp.process(samples, atten_lim(self.strength)),
    _                            => self.rnnoise.process(samples, self.strength),
  }
  let voice_out = self.gate.enabled && self.vad_out.process(samples);  // VAD na VÝSTUPU
  self.delay.set_len(if self.gate.enabled && self.lookahead { self.lookahead_len() } else { 0 }); // podle modelu
  self.delay.process(samples);                                  // lookahead: zpozdí audio, ne detektory
  let hold = match (voice, voice_out) {
    (true, _)      => GATE_HOLD_SAMPLES + latency_samples(self.active) + self.delay.len(),
    (false, true)  => GATE_HOLD_SAMPLES + self.delay.len(),
    (false, false) => 0,
  };
  self.gate.process(samples, hold);
}
```

### 5.2 Modely

| Model           | Knihovna / runtime                    | Rámec               | Latence modelu | CPU (naměřeno, 1 jádro) |
| --------------- | ------------------------------------- | ------------------- | -------------- | ----------------------- |
| RNNoise         | `nnnoiseless 0.5` (Rust port)         | 480 vz. (10 ms)     | 20 ms          | 0,6 %                   |
| DeepFilterNet 3 | fork `deep_filter` (`d375b2d`), tract 0.23.10 | hop 480, FFT 960 + 2 lookahead | 40 ms | 2,0–2,2 % (na 0.21: 2,9–3,1 %) |
| DPDFNet-2 48 kHz| ONNX v DLL, tract 0.23.10, vlastní STFT | hop 480, Vorbis okno 960, 4 lookahead hopy | 60 ms | 10,7 % |

CPU je čas zpracování / délka audia při běhu celé nahrávky. Zahrnuje i vstupní VAD (jeden RNNoise průchod navíc).

#### Síla (strength) → co dělá v každém modelu

```rust
const DF_MIN_ATTEN_DB: f32 = 6.0;
const DF_MAX_ATTEN_DB: f32 = 60.0;
fn atten_lim(strength: f32) -> Option<f32> {          // DFN3 i DPDFNet
  if strength >= 0.999 { None }                       // 100 % = neomezeno
  else { Some(10f32.powf(-(DF_MIN_ATTEN_DB + strength * (DF_MAX_ATTEN_DB - DF_MIN_ATTEN_DB)) / 20.0)) }
}
// RNNoise: lineární wet/dry mix
self.output_fifo.push(alpha * (wet / I16_SCALE) + (1.0 - alpha) * (dry / I16_SCALE)); // alpha = strength
```

**Odvozeno z kódu:**

* Výchozí síla 0,75 dá u DFN3/DPDFNet strop útlumu 6 + 0,75·54 = **46,5 dB**.
* U RNNoise je stejná síla mix s **25 % suchého signálu**. RNNoise tak z principu nemůže ztlumit
  šum víc než 20·log10(0,25) ≈ **-12 dB**. Přesně tohle se naměřilo: RNNoise potlačí 10–12 dB
  všude, bez ohledu na druh šumu.
* **Zůstává tak záměrně (2026-11-08).** Mix v dB (suchý podíl 6 dB až 20 / 30 dB) ztlumil pauzy o
  19 / 25 dB místo 12–14, ale gate pak uřízla víc tichých slabik: 1,6–2,4 → 2,6–4,3 / 3,8–5,9 %.
  Přísnější výstupní detektor pro RNNoise (práh 0,3 → 0,5) snížil otevírání na klávesy 31 → 18 %, ale
  stál +0,3–0,4 bodu slabik. Pravidlo: radši pár kláves navíc než jedno uříznuté písmeno.
* Suchý signál je u RNNoise zpožděný o 1 rámec (`prev_dry`), aby se mix nefázoval.

#### DeepFilterNet 3: prahy

```rust
const DF_MIN_DB_THRESH: f32 = -20.0;     // lokální SNR pod tím = rámec je jen šum (upstream -10)
const DF_MAX_DB_ERB_THRESH: f32 = 30.0;  // nad tím se přeskočí DF stage (upstream default)
const DF_MAX_DB_DF_THRESH: f32 = 20.0;   // upstream default
```

Upstream -10 dB vyhazoval 16–26 % tichých slabik pod hlasitou klávesnicí, -20 dB jen 3–8 %.

#### DPDFNet-2

Port upstream `StreamEnhancer`: STFT 960/480 s Vorbis oknem, ONNX dostane 1 spektrální rámec
`[1,1,481,2]` a plochý vektor stavu (normalizace + RNN stavy). Výstup se skládá overlap-add.
Strength = mix suchého signálu zpožděného o 4 hopy. NaN ve výstupu resetuje stav modelu.

### 5.3 Detektor řeči (VAD) a gate

Oba detektory jsou RNNoise `process_frame` → pravděpodobnost řeči 0–1 na 10ms rámec.

```rust
// vstupní detektor (surový mic, slyší onset o latenci modelu dřív než výstup)
const VOICE_PROB: f32 = 0.5;        // otevře se nad 0,5 ...
const VOICE_MIN_FRAMES: u32 = 2;    // ... 2 rámce po sobě (klik klávesy je kratší než slabika)
const VOICE_KEEP_PROB: f32 = 0.05;  // hystereze: když se mluví, drží nad 0,05 ...
const VOICE_ACTIVE_FRAMES: u32 = 20;// ... pokud byla řeč v posledních 200 ms
// výstupní detektor (po denoiseru, klávesy už tam většinou nejsou)
const OUT_VOICE_PROB: f32 = 0.3;
const OUT_VOICE_MIN_FRAMES: u32 = 1;
const OUT_VOICE_KEEP_PROB: f32 = 0.02;
const OUT_VOICE_ACTIVE_FRAMES: u32 = 50;   // 500 ms

fn process(&mut self, samples: &[f32]) -> bool {           // VoiceDetector
  // ... po rámcích 480:
  let prob = self.state.process_frame(&mut self.scratch, &self.frame);
  self.run = if prob > self.open_prob { self.run.saturating_add(1) } else { 0 };
  let frame_voice = self.run >= self.min_frames || (self.active_left > 0 && prob > self.keep_prob);
  self.active_left = if frame_voice { self.active_frames } else { self.active_left.saturating_sub(1) };
  voice |= frame_voice;
}
```

Gate:

```rust
const GATE_FLOOR_DB: f32 = -12.0;                 // zavřená = -12 dB, NIKDY mute
const GATE_FLOOR_EXTRA_MAX_DB: f32 = 12.0;        // + o zisk leveleru níž (set_gate_floor_extra_db)
const RNNOISE_LOOKAHEAD_SAMPLES: usize = 48 * 15; // lookahead podle modelu
const DF_LOOKAHEAD_SAMPLES: usize = 48 * 15;
const DPDF_LOOKAHEAD_SAMPLES: usize = 48 * 10;
const GATE_HOLD_SAMPLES: usize = 48 * 200;        // 200 ms + latence modelu + lookahead
const GATE_OPEN_COEF: f32 = 1.0 / 48.0;           // ~1 ms náběh
const GATE_CLOSE_COEF: f32 = 1.0 / (48.0 * 50.0); // ~50 ms doběh

fn process(&mut self, samples: &mut [f32], hold: usize) {
  self.hold_left = self.hold_left.max(hold);      // hold se nikdy nezkracuje
  for s in samples.iter_mut() {
    self.hold_left = self.hold_left.saturating_sub(1);
    let (target, coef) = if self.hold_left > 0 { (1.0, GATE_OPEN_COEF) } else { (self.floor, GATE_CLOSE_COEF) };
    self.gain += (target - self.gain) * coef;     // jednopólové vyhlazení zisku
    *s *= self.gain;
  }
}
```

Návrhová filozofie (z komentářů v kódu): **gate nesmí nikdy uříznout hlas**. Raději propustí pár kláves.
Proto floor -12 dB místo mute, dva detektory, hystereze a lookahead.

---

## 6. Fáze 3 a 4: EQ, boost, limiter

* **EQ** (`gamersequalizer-common/src/biquad.rs`): RBJ biquad kaskáda, strmé řezy rozložené
  do více stupňů (12/24/48 dB/okt.). Výchozí preset „Clean voice (cut rumble & hiss)“:

  ```rust
  preset(DEFAULT_PRESET, [steep_hp(100.0), steep_lp(9000.0)].concat()),
  // steep_hp: HighPass Q 0.707, 48 dB/okt.;  steep_lp: LowPass Q 0.707, 24 dB/okt.
  ```

  EQ běží **až po denoiseru**. Další presety: Male voice, Radio voice, Discord clarity, Tame harsh S
  sounds, Walkie-talkie, Flat.
* **Pevný high-pass 70 Hz** (2. řád, Butterworth) běží *před* detektory a denoiserem (`PRE_HIGH_PASS_HZ`
  v `plugin.rs`), aby dunění stolu a klimatizace neviděly modely ani VAD. EQ s HP 100 Hz zůstává.
* **Voice leveler** (`voice_leveler.rs`): učí se úroveň řeči jen z rámců, kde detektor slyší řeč, a
  táhne ji k cíli -24 dBFS, zisk -18 (`MAX_CUT_DB`) až +24 dB.
* **Hlasitost/boost**: s levelerem je `mic_volume_permille` 0–3000 to, jak hlasitě vás slyší ostatní
  (100 % = cíl); bez něj jen boost > 100 %.
* **PeakLimiter** (`gamersequalizer_common::limiter`, vždy poslední): okamžitý attack, release 80 ms,
  strop 0,97, pak `soft_limit`. Dřív byl na konci jen `soft_limit` (níže), který od vstupu 1,003 ořezával.
* **Soft limiter** (uvnitř PeakLimiteru jako pojistka):

  ```rust
  pub const LIMITER_KNEE: f32 = 0.95;            // -0,45 dBFS, pod tím bit-exact
  pub fn soft_limit(x: f32) -> f32 {
    let abs = x.abs();
    if abs <= LIMITER_KNEE { x }
    else { x.signum() * (LIMITER_KNEE + (abs - LIMITER_KNEE) / (1.0 + abs - LIMITER_KNEE)).min(1.0) }
  }
  ```

AGC je jen učící leveler (pomalý, učí se z řeči), žádný rychlý kompresor.

---

## 7. Latence (výchozí nastavení)

| Část                                | RNNoise  | DFN3     | DPDFNet  |
| ----------------------------------- | -------- | -------- | -------- |
| Model (priming + okno + lookahead)  | 20 ms    | 40 ms    | 60 ms    |
| Gate lookahead (výchozí zapnutý)    | 15 ms    | 15 ms    | 10 ms    |
| AEC3 (jen když hraje reference)     | ~9 ms    | ~9 ms    | ~9 ms    |
| RateBridge (jen mic ≠ 48 kHz)       | ~5 ms    | ~5 ms    | ~5 ms    |
| **Celkem navíc proti surovému mic** | 35–49 ms | 55–69 ms | 70–84 ms |

Skutečnou latenci za běhu měří `latency_meter.rs`.

Latence modelů jsou ověřené křížovou korelací (`test_measured_latency_matches_constants`):
RNNoise 960 vz., DFN3 1920 vz., DPDFNet 2877 vz. (konstanta 2880).

---

## 8. Jak se měří (definice metrik)

### 8.1 Syntetické testy s referencí (`run_quality` → `Quality`)

Čistá řeč (`test_data/denoise/speech_clean.wav`, nebo vývojářova `private/speech.wav`
s high-passem 100 Hz) + klávesnice/myš (ESC-50 klipy, nebo `private/keys.wav`). Řeč se nastaví na danou
úroveň (dBFS RMS) a šum podle SNR. Pak se po 10ms rámcích porovná výstup (posunutý o latenci)
s čistou řečí:

```rust
let is_lost = db(rms(o)) - c_db < -15.0;        // rámec ztracen = výstup > 15 dB pod čistou řečí
if c_db >= clean_db { loud += 1; lost += is_lost }                        // hlasitá řeč → dropouts
else if c_db >= clean_db - 15.0 { soft += 1; soft_lost += is_lost }       // tichá řeč → soft_dropouts
else if c_db < clean_db - 40.0 { /* pauza: energie vstupu vs výstupu */ } // → noise_suppression_db
```

* `dropouts`: podíl ztracených rámců hlasité řeči.
* `soft_dropouts`: totéž pro tiché části (začátky a konce slabik, do 15 dB pod průměrem).
* `noise_suppression_db`: o kolik je výstup tišší než vstup v pauzách řeči (jen klávesy).
* `open_share`: podíl rámců, kdy gate propouští (gated výstup je do 6 dB od výstupu bez gate).

### 8.2 Dlouhá nahrávka bez reference (`scenario_report`, nový test)

Nahrávka `test_data/private/scenario.wav` (git-ignored, obsahuje i licencovanou hudbu): 183,9 s,
48 kHz, stereo 16 bit, z Windows Diktafonu (m4a AAC → WAV přes Media Foundation). Bere se 1. kanál
stejně jako v APO. Čistá reference neexistuje, takže se měří:

* úroveň vstupu a výstupu po sekundách, bez gate a s gate (auto + lookahead);
* `vad>0.5%`: podíl rámců, kde RNNoise na surovém vstupu dává > 0,5. Slouží jako hrubý ukazatel řeči.
  **Pozor:** reaguje i na hudbu a nereaguje na šepot (nemá znělost).
* „voice frames under closed gate“: rámce s VAD > 0,5, kde je gated výstup > 6 dB pod ungated.
* Výstupy se uloží do `test_data/private/out/{Model}_{gate|nogate}.wav` na poslech.

---

## 9. Výsledky

### 9.1 Jednotkové testy

`cargo test -p gamersequalizer-mic-vst -p gamersequalizer-common --release`: **57 prošlo, 0 selhalo**
(2 reporty jsou `#[ignore]`). Clippy v kódu testů nic nového nehlásí, staré varování jsou jen ve `vst_bridge.rs` a `shm.rs`.

Vybrané výpisy (syntetická řeč z fixtures + ESC-50 klávesnice):

| Situace                                    | Model   | gate soft% / loud% [pauza dB] | bez gate soft% / loud% [pauza dB] |
| ------------------------------------------ | ------- | ----------------------------- | --------------------------------- |
| hlas -20 dBFS, SNR 0                       | DFN3    | 2,2 / 0,0 [44,5]              | 2,2 / 0,0 [39,1]                  |
| hlas -20 dBFS, SNR 0                       | DPDFNet | 1,3 / 0,0 [29,7]              | 1,3 / 0,0 [29,7]                  |
| hlas -20 dBFS, SNR 0                       | RNNoise | 0,0 / 0,0 [11,3]              | 0,0 / 0,0 [10,2]                  |
| tichý hlas -45 dBFS, SNR 0                 | DFN3    | 5,3 / 0,0 [38,4]              | 5,3 / 0,0 [27,7]                  |
| tichý hlas -45 dBFS, SNR 0                 | DPDFNet | 1,9 / 0,0 [28,6]              | 1,9 / 0,0 [28,6]                  |
| tichý hlas -45 dBFS, SNR 0                 | RNNoise | 0,9 / 0,0 [13,4]              | 0,0 / 0,0 [10,1]                  |
| hlasité klávesy (-30 dBFS, SNR -10)        | DFN3    | 5,3 / 0,3 [46,2]              | 5,0 / 0,3 [43,9]                  |
| hlasité klávesy (-30 dBFS, SNR -10)        | DPDFNet | 5,3 / 0,0 [47,1]              | 5,3 / 0,0 [46,2]                  |
| hlasité klávesy (-30 dBFS, SNR -10)        | RNNoise | 0,3 / 0,7 [11,6]              | 0,0 / 0,0 [10,2]                  |

Gate otevřená při psaní bez řeči (ESC-50): -20 dBFS: DFN3 5,2 %, DPDFNet 1,8 %, **RNNoise 31,1 %**;
-40 dBFS: 0 % / 0 % / 11,3 %.

### 9.2 `private_report`: vývojářův hlas + vývojářova klávesnice (mixované synteticky)

Formát: `soft% / loud% ztraceno [potlačení v pauzách dB]`. Sloupce: gate auto | auto + lookahead (výchozí) | bez gate.

| Hlas      | SNR    | Model   | gate auto          | auto + lookahead   | bez gate           |
| --------- | ------ | ------- | ------------------ | ------------------ | ------------------ |
| -20 dBFS  | 0 dB   | DFN3    | 3,5 / 1,2 [30,0]   | 2,4 / 1,2 [30,0]   | 2,1 / 1,2 [25,1]   |
| -20 dBFS  | 0 dB   | DPDFNet | 4,3 / 0,4 [50,4]   | 3,7 / 0,4 [50,4]   | 3,5 / 0,4 [47,0]   |
| -20 dBFS  | 0 dB   | RNNoise | 3,5 / 0,0 [15,5]   | 1,6 / 0,0 [14,1]   | 0,0 / 0,0 [11,7]   |
| -45 dBFS  | 0 dB   | DFN3    | 5,9 / 2,0 [22,7]   | 4,2 / 1,8 [22,7]   | 3,0 / 1,8 [19,3]   |
| -45 dBFS  | 0 dB   | DPDFNet | 5,7 / 0,4 [50,6]   | 5,6 / 0,4 [50,6]   | 4,7 / 0,4 [47,0]   |
| -45 dBFS  | 0 dB   | RNNoise | 3,3 / 1,0 [15,2]   | 1,9 / 0,2 [13,8]   | 0,0 / 0,0 [11,4]   |
| -30 dBFS  | -10 dB | DFN3    | 9,4 / 2,2 [22,2]   | 8,0 / 1,8 [22,2]   | 5,7 / 1,4 [19,9]   |
| -30 dBFS  | -10 dB | DPDFNet | 13,0 / 1,8 [50,7]  | 12,0 / 1,8 [50,7]  | 11,7 / 1,8 [47,2]  |
| -30 dBFS  | -10 dB | RNNoise | 1,9 / 0,2 [12,5]   | 1,4 / 0,0 [12,1]   | 0,0 / 0,0 [11,8]   |

Gate otevřená, když se jen píše (vlastní klávesnice): -20 dBFS: DFN3 1,1 % (lookahead 1,2 %), DPDFNet 0,0 %,
RNNoise 0,6 %; -40 dBFS: všechny 0 %.

**Co z toho plyne (naměřeno):**

* Na vývojářově hlasu pod hlasitou klávesnicí (SNR -10) ztrácí **DPDFNet nejvíc tichých slabik
  (11,7–13 %)**, víc než DFN3 (5,7–9,4 %). Opak toho, co platí na syntetické řeči z fixtures (5,3 vs 5,0 %)
  a co tvrdí komentář v kódu („best on soft speech“). Tohle je ztráta samotného modelu, ne gate.
* Gate přidá tichým slabikám 0–3,5 procentního bodu ztrát. Lookahead z toho ubere zhruba třetinu až polovinu.
  U RNNoise gate přidává ztráty skoro jen tam, kde je bez gate nula.
* Potlačení kláves v pauzách: DPDFNet ~47–51 dB, DFN3 19–30 dB, RNNoise 11–15 dB (strop ~12 dB, viz 5.2).

### 9.3 `scenario_report`: dlouhá nahrávka podle scénáře

**Souhrn (naměřeno):**

| Model   | CPU    | sekundy bez řeči: útlum s gate (bez gate) | sekundy s řečí: úroveň proti vstupu s gate (bez gate) | rámce řeči pod zavřenou gate |
| ------- | ------ | ----------------------------------------- | ----------------------------------------------------- | ---------------------------- |
| DFN3    | 2,9 %  | 11,2 dB (4,6 dB)                          | -0,7 dB (-0,7 dB)                                     | 0,12 %                       |
| DPDFNet | 10,7 % | 21,8 dB (21,7 dB)                         | -0,5 dB (-0,5 dB)                                     | 0,12 %                       |
| RNNoise | 0,6 %  | 13,3 dB (10,0 dB)                         | -0,3 dB (-0,3 dB)                                     | 0,00 %                       |

78 sekund s řečí (VAD > 0,5 na ≥ 30 % rámců), 77 bez řeči (0 %), 28 smíšených. Útlum je vážený energií,
takže ho ovládnou hlasité nehlasové události (rány, kašel, smích, šepot).

**Ticho v místnosti (s. 1–10, vstup ~ -65 dBFS):**

| Model   | bez gate   | s gate     |
| ------- | ---------- | ---------- |
| DFN3    | -64…-72    | -76…-82    |
| DPDFNet | -108…-112  | -115…-124  |
| RNNoise | -74…-78    | -86…-90    |

DFN3 sám ubere tichému šumu místnosti jen **1–6 dB**, zbytek dělá gate (-12 dB). DPDFNet ho smaže na digitální ticho.

**Přiřazení sekund ke scénáři je jen odhad podle úrovní** (nikdo to neposlouchal se stopkami):

| Sekundy  | Pravděpodobně (hypotéza)              | Co se děje (naměřeno)                                                                  |
| -------- | ------------------------------------- | -------------------------------------------------------------------------------------- |
| 0–11     | 1. ticho                              | viz tabulka výše                                                                       |
| 12–17    | 2. normální řeč (~-30 dBFS)           | všechny modely ±0,2 dB, gate 100 % otevřená                                            |
| 18–24    | 3. tichá řeč (~-40 dBFS)              | beze ztrát, gate otevřená v rámcích s řečí                                             |
| 25–29    | 4. šepot (VAD = 0 v s. 26–27)         | s. 27 (vstup -43,5): **DPDFNet -86 dB (ztráta 43 dB)**, RNNoise -55, DFN3 -44 (bez gate) / -56 (gate) |
| 30–75    | 5.–8. křik, krátká slova, sykavky     | řeč beze ztrát; v pauzách gate zavírá na -12 dB; s. 71–73: DFN3 -1 až -4 dB oproti ostatním |
| 77–82    | 9. tichá řeč + klávesnice?            | DFN3 -1,6…-2,9 dB, RNNoise -0,8…-1,9 dB (odstraněné klávesy, nebo i hlas?)            |
| 85–110   | 10.–11. jen klávesnice, myš           | DFN3 -10…-19 dB (+12 gate), DPDFNet ~-45 dB, RNNoise ~-8…-12 dB                        |
| 111–115  | 12. hudba z repráků (bez řeči)        | **VAD 41–100 %** (hudbu bere jako řeč) → gate 100 % otevřená; DPDFNet -46 dB, DFN3 -7 dB, RNNoise -2…-7 dB |
| 116–120  | 12. řeč přes hudbu                    | řeč zůstává; DFN3 -2 dB                                                                |
| 140–149  | 13. nádech, kašel, smích              | s. 142 (vstup -17,9): DPDFNet -48,9 (**-31 dB**), DFN3 -23,9 (-6), RNNoise -20,1; s. 147 (-25): DPDFNet -71,5 (**-46 dB**), DFN3 -37,9 (-13), RNNoise -32,7 |
| 150–157  | 14.–15. vzdálenost, dlouhé samohlásky?| s. 155 (vstup -41,2, VAD 48 %): **DFN3 -56,1 (-15 dB), DPDFNet -75,6 (-34 dB)**, RNNoise -41,7 |
| 159–176  | 16. rychlá řeč                        | beze ztrát (±0,5 dB)                                                                   |
| 178–183  | 17. ticho                             | jako na začátku                                                                        |

Ověřit se to dá poslechem `test_data/private/out/*.wav` v uvedených sekundách.

---

## 10. Pozorování: kde je prostor ke zlepšení

Seřazeno podle síly důkazu. Tohle je podklad, ne návrh řešení.

1. **RNNoise má strop útlumu ~12 dB** (odvozeno z kódu, potvrzeno měřením všude, 10–12 dB). Wet/dry mix
   při síle 0,75 vrací 25 % suchého signálu. RNNoise je výchozí model, takže běžný uživatel dostane
   jen ~12 dB potlačení klávesnice. Síla u RNNoise neznamená „strop útlumu“ jako u DFN3/DPDFNet, ale lineární mix.
2. **RNNoise VAD řídí gate pro všechny modely a hudbu považuje za řeč** (s. 111–115: 41–100 % rámců).
   Gate je pak při hudbě v místnosti stále otevřená. U DPDFNet to nevadí (hudbu smaže model sám),
   u RNNoise/DFN3 hudba projde. Šepot naopak VAD neslyší (s. 26–27: 0 %). Gate na šepot zavře na -12 dB.
3. **DPDFNet maže nehlasové vokální zvuky a nejspíš i šepot**: smích a kašel -31 až -46 dB, šepot (?) -43 dB,
   dlouhá samohláska (?) -34 dB. Na vlastním hlasu pod hlasitou klávesnicí ztrácí nejvíc tichých slabik
   (11,7–13 %). Naopak má nejlepší potlačení ticha a kláves (45–50 dB). Hypotéza: model je natrénovaný
   na čistou modální řeč, cokoli jiného bere jako šum.
4. **DFN3 skoro nepotlačuje tichý stacionární šum místnosti** (1–6 dB při -65 dBFS). Hypotéza: lokální SNR
   odhad nad `DF_MAX_DB_ERB_THRESH = 30` → model rámec přeskočí jako „čistý“. Ověřit logováním `lsnr`.
   V pauzách to pak dorovnává jen gate (-12 dB).
5. **Gate floor -12 dB je jediný útlum mimo řeč u DFN3/RNNoise.** Je to vědomý kompromis („nikdy neuříznout hlas“).
   Rámce řeči pod zavřenou gate: 0–0,12 %, takže cíl „neuříznout hlas“ gate na této nahrávce plní.
6. **Sykavky:** výchozí EQ má dolní propust 9 kHz/24 dB a DFN3 v s. 71–73 ubírá 1–4 dB víc než ostatní.
   Bez reference nelze říct, jestli jde o sykavky, nebo odstraněné klávesy. Rozhodne poslech.
7. **Žádné AGC.** Úroveň hlasu kolísala mezi nahrávkami o 15 dB (Záznam 3 přebuzený, Záznam 5 -30…-35 dBFS RMS),
   řetězec to nijak nevyrovnává.
8. **Gate po hlasitých neřečových událostech:** RNNoise s. 139–145 drží gate otevřenou 31–100 % jen na základě
   výstupního detektoru (vstupní VAD ~2 %). U RNNoise výstupní detektor slyší, co model propustil.

---

## 11. Otevřené otázky (vývojář, než se navrhne řešení)

* Byla APO s GamersEqualizerem aktivní, když vznikala nahrávka v Diktafonu? Pokud ano, „surový“ vstup už
  prošel řetězcem a výsledky jsou zkreslené. Šum místnosti -65 dBFS a 12 dB útlum RNNoise naznačují spíš
  surový vstup, ale ověřené to není.
* Přesné časy úseků scénáře (stačí poznámka „šepot 25–29 s, hudba 111–120 s“…) zpřesní tabulku v 9.3.
* Nahrávka prošla kompresí AAC, která mění vysoké frekvence. Do budoucna je lepší nahrávat přímo WAV.
* Echo (AEC) se netestovalo, chybí reference reproduktorů.

---

## 12. Jak to zopakovat

```bash
cargo test -p gamersequalizer-mic-vst -p gamersequalizer-common --release -- --nocapture
cargo test -p gamersequalizer-mic-vst --release private_report -- --ignored --nocapture
cargo test -p gamersequalizer-mic-vst --release scenario_report -- --ignored --nocapture
cargo test -p gamersequalizer-mic-vst --release gate_sweep -- --ignored --nocapture
```

`private_report` potřebuje `test_data/private/speech.wav` a `keys.wav` (mono). `scenario_report` potřebuje
`test_data/private/scenario.wav` (jakýkoli počet kanálů, bere 1.). Složka `test_data/private/` je v `.gitignore`.

---

## Příloha A: surový výstup `scenario_report` po sekundách

Sloupce: sekunda, vstup dBFS RMS, % rámců s VAD > 0,5 | pro každý model: bez gate dB, s gate dB, gate otevřená %.
Pořadí modelů: DFN3 | DPDFNet | RNNoise. Sekunda 0 je zahřívání (gate startuje se ziskem 0).

```
  s   in dB  vad>0.5%  | DFN3: nogate gate open | DPDFNet: nogate gate open | RNNoise: nogate gate open
   0  -66.4     1.0    |  -72.2  -76.9  49.3 | -112.4 -124.4   0.0 |  -78.3  -83.0  47.1
   1  -65.0     0.0    |  -69.1  -81.1   0.0 | -110.3 -115.1  14.0 |  -76.9  -88.9   0.0
   2  -64.6     0.0    |  -70.4  -82.4   0.0 | -110.1 -116.0  14.0 |  -76.6  -88.6   0.0
   3  -65.5     0.0    |  -69.6  -81.6   0.0 | -110.9 -122.9   0.0 |  -77.4  -89.4   0.0
   4  -63.1     0.0    |  -67.6  -79.6   0.0 | -109.2 -121.2   0.0 |  -75.1  -87.1   0.0
   5  -65.7     0.0    |  -66.6  -78.6   0.0 | -111.4 -123.4   0.0 |  -77.7  -89.7   0.0
   6  -61.9     0.0    |  -65.2  -77.2   0.0 | -108.3 -120.3   0.0 |  -73.9  -85.9   0.0
   7  -65.4     0.0    |  -66.0  -78.0   0.0 | -111.6 -123.6   0.0 |  -77.3  -89.3   0.0
   8  -66.3     0.0    |  -67.5  -79.5   0.0 | -112.4 -124.4   0.0 |  -78.1  -90.1   0.0
   9  -62.6     0.0    |  -63.8  -75.8   0.0 | -108.9 -120.9   0.0 |  -74.6  -86.6   0.0
  10  -64.3     0.0    |  -65.2  -77.2   0.0 | -110.6 -122.6   0.0 |  -76.3  -88.3   0.0
  11  -56.4     0.0    |  -59.2  -71.2   0.0 |  -85.9  -97.9   0.0 |  -63.0  -75.0   0.0
  12  -29.0    55.0    |  -29.0  -29.0  67.0 |  -29.2  -29.2  69.0 |  -29.1  -29.1  65.0
  13  -30.7    79.0    |  -30.7  -30.7 100.0 |  -30.8  -30.8 100.0 |  -30.8  -30.8 100.0
  14  -30.6    74.0    |  -30.6  -30.6 100.0 |  -30.7  -30.7 100.0 |  -30.7  -30.7 100.0
  15  -32.5    81.0    |  -32.6  -32.6 100.0 |  -32.8  -32.8 100.0 |  -32.6  -32.6 100.0
  16  -35.0    42.0    |  -35.1  -35.1  82.0 |  -34.6  -34.6  86.0 |  -35.2  -35.2  85.0
  17  -30.6    67.0    |  -30.6  -30.6 100.0 |  -31.0  -31.0 100.0 |  -30.6  -30.6 100.0
  18  -46.8     7.0    |  -47.8  -47.9  58.0 |  -47.7  -47.7  59.0 |  -47.4  -47.4  59.0
  19  -39.3    28.0    |  -39.3  -39.3  40.0 |  -39.7  -39.7  42.0 |  -39.5  -39.5  38.0
  20  -39.6    69.0    |  -39.7  -39.7 100.0 |  -40.1  -40.1 100.0 |  -39.9  -39.9 100.0
  21  -39.8    91.0    |  -40.0  -40.0 100.0 |  -40.2  -40.2 100.0 |  -40.1  -40.1 100.0
  22  -43.2    27.0    |  -43.3  -43.3  66.0 |  -43.8  -43.8  71.0 |  -43.5  -43.5  64.0
  23  -36.8    57.0    |  -36.8  -36.8 100.0 |  -37.2  -37.2 100.0 |  -37.0  -37.0 100.0
  24  -39.7    49.0    |  -39.8  -39.8 100.0 |  -40.1  -40.1 100.0 |  -39.9  -39.9 100.0
  25  -59.4     0.0    |  -63.9  -73.7   9.0 |  -90.1  -90.4  11.0 |  -68.9  -78.1   9.0
  26  -48.0     0.0    |  -48.4  -60.4   0.0 |  -80.0  -92.0   0.0 |  -53.3  -57.7  11.0
  27  -43.5     0.0    |  -44.0  -56.0   0.0 |  -86.3  -98.3   0.0 |  -54.8  -56.5  52.0
  28  -41.1    21.0    |  -41.2  -41.2  37.0 |  -43.9  -43.9  39.0 |  -41.4  -41.4  35.0
  29  -34.3    84.0    |  -34.4  -34.4 100.0 |  -34.6  -34.6 100.0 |  -34.5  -34.5 100.0
  30  -58.8     0.0    |  -62.7  -68.3  22.0 |  -84.6  -85.6  23.0 |  -69.4  -74.5  23.0
  31  -28.2    43.0    |  -29.6  -29.6  91.0 |  -28.8  -28.8  91.0 |  -28.6  -28.6  87.0
  32  -28.6    53.0    |  -28.6  -28.6  89.0 |  -28.7  -28.7  90.0 |  -28.6  -28.6  87.0
  33  -29.6    63.0    |  -29.6  -29.6 100.0 |  -29.8  -29.8 100.0 |  -29.7  -29.7 100.0
  34  -51.1     0.0    |  -58.5  -64.8  16.0 |  -86.3  -87.1  16.0 |  -63.0  -68.9  18.0
  35  -40.6     3.0    |  -40.6  -40.7   9.0 |  -39.1  -39.1  11.0 |  -40.7  -40.7   8.0
  36  -29.6    27.0    |  -29.6  -29.6  52.0 |  -29.9  -29.9  54.0 |  -29.6  -29.6  52.0
  37  -31.8    38.0    |  -31.8  -31.8  70.0 |  -32.0  -32.0  71.0 |  -31.8  -31.8  71.0
  38  -26.7    32.0    |  -26.8  -26.8  75.0 |  -26.8  -26.8  76.0 |  -26.8  -26.8  75.0
  39  -30.2    31.0    |  -30.2  -30.2  63.0 |  -30.3  -30.3  76.0 |  -30.2  -30.2  60.0
  40  -31.1    45.0    |  -31.1  -31.1  60.0 |  -31.3  -31.3  62.0 |  -31.1  -31.1  58.0
  41  -30.3    14.0    |  -30.4  -30.4  64.0 |  -30.5  -30.5  79.0 |  -30.5  -30.5  62.0
  42  -51.7     0.0    |  -55.0  -67.0   0.0 |  -96.7 -108.4   0.0 |  -63.5  -75.5   0.0
  43  -31.3    25.0    |  -31.3  -31.3  64.0 |  -31.4  -31.4  65.0 |  -31.3  -31.3  72.0
  44  -31.4    27.0    |  -31.4  -31.4  74.0 |  -31.5  -31.5  82.0 |  -31.4  -31.4  78.0
  45  -36.0    53.0    |  -36.0  -36.0  68.0 |  -36.4  -36.4  73.0 |  -36.1  -36.1  72.0
  46  -34.9    39.0    |  -34.9  -34.9  72.0 |  -35.1  -35.1  72.0 |  -35.0  -35.0  70.0
  47  -33.7    41.0    |  -33.8  -33.8  79.0 |  -34.0  -34.0  81.0 |  -33.8  -33.8  77.0
  48  -55.9     0.0    |  -56.7  -57.2  27.0 |  -57.4  -57.4  31.0 |  -56.6  -56.6  30.0
  49  -31.8    16.0    |  -31.8  -31.8  38.0 |  -32.0  -32.0  39.0 |  -31.8  -31.9  38.0
  50  -33.3    52.0    |  -33.3  -33.3 100.0 |  -33.5  -33.5 100.0 |  -33.4  -33.4 100.0
  51  -34.1    82.0    |  -34.1  -34.1 100.0 |  -34.3  -34.3 100.0 |  -34.1  -34.1 100.0
  52  -44.2    26.0    |  -44.7  -44.8  76.0 |  -43.6  -43.7  79.0 |  -44.7  -44.8  76.0
  53  -27.9    72.0    |  -27.9  -27.9 100.0 |  -28.1  -28.1 100.0 |  -27.9  -27.9 100.0
  54  -33.8    37.0    |  -33.8  -33.8  83.0 |  -33.9  -33.9  84.0 |  -33.8  -33.8  80.0
  55  -33.2    70.0    |  -33.2  -33.2 100.0 |  -33.5  -33.5 100.0 |  -33.3  -33.3 100.0
  56  -32.2    35.0    |  -32.3  -32.3  90.0 |  -32.5  -32.5  92.0 |  -32.3  -32.3  89.0
  57  -33.6    45.0    |  -33.6  -33.6 100.0 |  -33.8  -33.8 100.0 |  -33.6  -33.6 100.0
  58  -57.1     0.0    |  -62.3  -66.1  23.0 |  -74.8  -75.5  22.0 |  -67.3  -71.4  23.0
  59  -32.8    39.0    |  -32.8  -32.8  81.0 |  -33.0  -33.0  86.0 |  -32.8  -32.8  77.0
  60  -35.0    57.0    |  -35.0  -35.0  97.0 |  -35.2  -35.2  99.0 |  -35.1  -35.1  97.0
  61  -62.9     0.0    |  -64.3  -75.9   0.0 |  -94.5 -106.2   0.0 |  -71.8  -83.5   0.0
  62  -41.4    14.0    |  -41.5  -41.5  65.0 |  -41.7  -41.7  67.0 |  -41.6  -41.7  63.0
  63  -32.1    39.0    |  -32.1  -32.1 100.0 |  -32.3  -32.3 100.0 |  -32.1  -32.1 100.0
  64  -57.3     0.0    |  -63.1  -73.6   4.0 |  -80.9  -88.1   4.0 |  -67.6  -77.1   3.0
  65  -62.9     0.0    |  -65.6  -77.6   0.0 |  -99.9 -111.9   0.0 |  -72.9  -84.9   0.0
  66  -60.1     0.0    |  -65.2  -77.2   0.0 | -102.1 -114.1   0.0 |  -70.3  -82.3   0.0
  67  -33.8    49.0    |  -33.8  -33.8  58.0 |  -34.0  -34.0  60.0 |  -33.9  -33.9  56.0
  68  -30.6    94.0    |  -30.6  -30.6 100.0 |  -30.9  -30.9 100.0 |  -30.8  -30.8 100.0
  69  -30.8    69.0    |  -30.8  -30.8 100.0 |  -31.0  -31.0 100.0 |  -30.9  -30.9 100.0
  70  -30.5    76.0    |  -30.6  -30.6 100.0 |  -30.5  -30.5 100.0 |  -30.7  -30.7 100.0
  71  -31.1    51.0    |  -32.1  -32.1  78.0 |  -31.6  -31.6  84.0 |  -31.2  -31.2  84.0
  72  -32.6    56.0    |  -35.8  -35.8  91.0 |  -32.8  -32.8  98.0 |  -32.8  -32.8  97.0
  73  -37.4    39.0    |  -41.6  -41.6 100.0 |  -38.1  -38.1 100.0 |  -37.7  -37.7 100.0
  74  -59.8     0.0    |  -62.4  -67.1  21.0 |  -79.2  -80.8  25.0 |  -70.3  -74.2  25.0
  75  -60.6     0.0    |  -62.5  -74.5   0.0 | -106.7 -118.7   0.0 |  -72.0  -84.0   0.0
  76  -62.5     0.0    |  -63.9  -75.9   0.0 | -108.8 -120.8   0.0 |  -74.4  -86.4   0.0
  77  -42.2    28.0    |  -45.1  -45.1  43.0 |  -43.9  -43.9  45.0 |  -43.4  -43.4  41.0
  78  -42.9    27.0    |  -44.7  -44.8  94.0 |  -45.5  -45.5  83.0 |  -44.1  -44.1  99.0
  79  -46.1    27.0    |  -48.6  -48.6  95.0 |  -47.8  -47.8  94.0 |  -46.9  -46.9  95.0
  80  -39.5    57.0    |  -40.4  -40.4  65.0 |  -40.7  -40.7  67.0 |  -41.4  -41.5  63.0
  81  -36.8    93.0    |  -38.4  -38.4 100.0 |  -37.2  -37.2 100.0 |  -37.3  -37.3 100.0
  82  -43.8    31.0    |  -45.3  -45.3  61.0 |  -45.1  -45.1  62.0 |  -44.7  -44.7  61.0
  83  -54.2     0.0    |  -57.1  -69.1   0.0 |  -98.6 -110.6   0.0 |  -64.8  -76.8   0.0
  84  -63.0     0.0    |  -66.7  -78.7   0.0 | -109.2 -121.2   0.0 |  -74.1  -86.1   0.0
  85  -60.4     0.0    |  -66.4  -78.4   0.0 | -106.4 -118.4   0.0 |  -64.1  -76.1   0.0
  86  -52.6     0.0    |  -68.9  -80.9   0.0 |  -99.1 -111.1   0.0 |  -56.3  -68.3   0.0
  87  -56.5     0.0    |  -75.0  -87.0   0.0 | -103.0 -115.0   0.0 |  -65.2  -77.2   0.0
  88  -55.4     0.0    |  -74.0  -86.0   0.0 | -101.9 -113.9   0.0 |  -63.0  -75.0   0.0
  89  -58.0     0.0    |  -75.3  -87.3   0.0 | -104.5 -116.5   0.0 |  -68.7  -80.7   0.0
  90  -57.0     0.0    |  -71.8  -83.8   0.0 | -103.5 -115.5   0.0 |  -68.5  -80.5   0.0
  91  -60.8     0.0    |  -71.6  -83.6   0.0 | -107.2 -119.2   0.0 |  -72.4  -84.4   0.0
  92  -56.8     0.0    |  -73.4  -85.4   0.0 | -103.1 -115.1   0.0 |  -67.1  -79.1   0.0
  93  -60.3     0.0    |  -76.3  -88.3   0.0 | -107.0 -119.0   0.0 |  -72.1  -84.1   0.0
  94  -57.7     0.0    |  -76.7  -88.7   0.0 | -104.2 -116.2   0.0 |  -69.3  -81.3   0.0
  95  -57.6     0.0    |  -74.3  -86.3   0.0 | -104.1 -116.1   0.0 |  -69.4  -81.4   0.0
  96  -58.0     0.0    |  -67.3  -79.3   0.0 | -104.4 -116.4   0.0 |  -69.9  -81.9   0.0
  97  -63.5     0.0    |  -71.7  -83.7   0.0 | -109.8 -121.8   0.0 |  -72.4  -84.4   0.0
  98  -61.9     0.0    |  -70.9  -82.9   0.0 | -108.3 -120.3   0.0 |  -73.8  -85.8   0.0
  99  -59.4     0.0    |  -69.6  -81.6   0.0 | -105.7 -117.7   0.0 |  -71.4  -83.4   0.0
 100  -59.5     0.0    |  -71.1  -83.1   0.0 | -106.1 -118.1   0.0 |  -71.4  -83.4   0.0
 101  -58.2     0.0    |  -66.0  -78.0   0.0 | -104.6 -116.6   0.0 |  -70.2  -82.2   0.0
 102  -59.8     0.0    |  -65.7  -77.7   0.0 | -106.2 -118.2   0.0 |  -71.8  -83.8   0.0
 103  -57.4     0.0    |  -61.2  -73.2   0.0 | -103.8 -115.8   0.0 |  -69.4  -81.4   0.0
 104  -62.0     0.0    |  -66.6  -78.6   0.0 | -108.6 -120.6   0.0 |  -74.0  -86.0   0.0
 105  -64.0     0.0    |  -68.5  -80.5   0.0 | -110.5 -122.5   0.0 |  -76.0  -88.0   0.0
 106  -53.8     0.0    |  -61.4  -73.4   0.0 | -100.2 -112.2   0.0 |  -65.8  -77.8   0.0
 107  -60.1     0.0    |  -62.0  -74.0   0.0 | -106.8 -118.8   0.0 |  -72.2  -84.1   0.0
 108  -63.2     0.0    |  -65.7  -77.7   0.0 | -109.7 -121.7   0.0 |  -75.2  -87.2   0.0
 109  -63.0     0.0    |  -65.2  -77.2   0.0 | -109.4 -121.4   0.0 |  -75.0  -87.0   0.0
 110  -62.0     0.0    |  -64.1  -76.1   0.0 | -108.3 -120.3   0.0 |  -74.0  -86.0   0.0
 111  -60.0    41.0    |  -65.6  -66.4  77.0 | -106.4 -106.7  78.9 |  -65.4  -65.5  75.0
 112  -57.6   100.0    |  -66.9  -66.9 100.0 | -104.1 -104.1 100.0 |  -60.4  -60.4 100.0
 113  -58.5    73.0    |  -66.0  -66.0 100.0 | -105.0 -105.0 100.0 |  -65.1  -65.1 100.0
 114  -58.9    36.0    |  -65.7  -65.7 100.0 | -104.8 -104.8 100.0 |  -65.8  -65.8 100.0
 115  -61.6     0.0    |  -69.4  -69.4 100.0 | -108.0 -108.0 100.0 |  -71.4  -71.4 100.0
 116  -42.2    34.0    |  -43.1  -43.1 100.0 |  -43.6  -43.6 100.0 |  -42.7  -42.7 100.0
 117  -25.1    87.0    |  -25.3  -25.3 100.0 |  -25.2  -25.2 100.0 |  -25.1  -25.1 100.0
 118  -30.2    67.0    |  -32.0  -32.0  99.0 |  -30.3  -30.3  97.0 |  -30.2  -30.2 100.0
 119  -30.5    70.0    |  -32.4  -32.4 100.0 |  -30.7  -30.7 100.0 |  -30.6  -30.6 100.0
 120  -32.2    61.0    |  -34.3  -34.3 100.0 |  -32.4  -32.4 100.0 |  -32.3  -32.3 100.0
 121  -59.4     0.0    |  -63.8  -73.2  10.0 |  -95.9 -101.5  11.0 |  -69.0  -77.0  12.0
 122  -59.9     0.0    |  -67.5  -79.5   0.0 | -106.4 -118.4   0.0 |  -70.8  -82.8   0.0
 123  -58.2     0.0    |  -62.8  -74.8   0.0 |  -65.0  -65.0   3.0 |  -63.2  -71.7   2.0
 124  -35.1    43.0    |  -36.1  -36.1  96.0 |  -35.4  -35.4 100.0 |  -35.2  -35.2 100.0
 125  -36.7    60.0    |  -40.2  -40.2 100.0 |  -36.9  -36.9 100.0 |  -36.8  -36.8 100.0
 126  -35.4    46.0    |  -37.4  -37.4 100.0 |  -35.6  -35.6 100.0 |  -35.5  -35.5 100.0
 127  -58.5     0.0    |  -61.2  -71.7   4.0 | -101.0 -109.3   6.0 |  -69.1  -79.2   5.0
 128  -36.9    31.0    |  -38.5  -38.5  67.0 |  -36.2  -36.2  69.0 |  -37.1  -37.1  65.0
 129  -29.2    81.0    |  -31.6  -31.6 100.0 |  -29.5  -29.5 100.0 |  -29.3  -29.3 100.0
 130  -29.8    52.0    |  -30.3  -30.3  97.0 |  -29.8  -29.8  83.0 |  -29.8  -29.8  83.0
 131  -50.0     0.0    |  -67.2  -78.7   0.0 |  -96.5 -108.5   0.0 |  -61.6  -73.6   0.0
 132  -34.4    81.0    |  -35.7  -35.7  93.0 |  -34.6  -34.6  95.0 |  -34.5  -34.5  95.0
 133  -30.0    70.0    |  -37.2  -37.2 100.0 |  -30.2  -30.2 100.0 |  -30.2  -30.2 100.0
 134  -35.1    25.0    |  -39.1  -39.1  69.0 |  -35.2  -35.2  66.0 |  -35.2  -35.2  67.0
 135  -58.2     0.0    |  -68.3  -80.3   0.0 | -104.3 -116.3   0.0 |  -69.5  -81.5   0.0
 136  -60.3     0.0    |  -67.7  -79.7   0.0 | -106.8 -118.8   0.0 |  -71.1  -83.1   0.0
 137  -59.8     0.0    |  -67.7  -79.7   0.0 | -106.3 -118.3   0.0 |  -70.7  -82.7   0.0
 138  -59.3     0.0    |  -68.6  -80.6   0.0 | -105.9 -117.9   0.0 |  -70.7  -82.7   0.0
 139  -58.7     2.0    |  -63.3  -75.3   0.0 | -105.3 -117.3   0.0 |  -70.1  -73.2  31.0
 140  -54.1    37.0    |  -64.3  -65.7  50.0 | -100.6 -101.7  52.0 |  -61.3  -61.3 100.0
 141  -51.7     2.0    |  -56.2  -65.4  23.0 |  -96.9 -103.1  25.0 |  -61.7  -61.8  71.0
 142  -17.9    18.0    |  -23.9  -23.9  92.0 |  -48.9  -48.9  77.0 |  -20.1  -20.1  79.0
 143  -39.3     0.0    |  -47.4  -47.9  28.0 |  -86.1  -98.0   0.0 |  -50.9  -51.2  28.0
 144  -43.7    10.0    |  -54.9  -65.2  24.0 |  -62.9  -62.9  46.0 |  -45.6  -45.9  45.0
 145  -44.8    10.0    |  -55.8  -56.7  69.0 |  -91.2  -91.5  72.0 |  -49.0  -49.0  89.0
 146  -51.4     0.0    |  -57.1  -69.1   0.0 |  -96.9 -108.9   0.0 |  -59.7  -63.6  28.0
 147  -25.0    50.0    |  -37.9  -38.0  90.0 |  -71.5  -72.0  58.0 |  -32.7  -33.0  54.0
 148  -36.2    44.0    |  -50.0  -51.0  70.0 |  -83.0  -83.1  71.0 |  -41.1  -41.1  70.0
 149  -45.6     0.0    |  -50.2  -62.2   0.0 |  -92.2 -104.2   0.0 |  -57.7  -69.7   0.0
 150  -29.4    80.0    |  -29.6  -29.6  98.0 |  -29.6  -29.6 100.0 |  -29.6  -29.6  96.0
 151  -51.5     1.0    |  -54.2  -63.0  27.0 |  -77.1  -78.1  27.0 |  -62.6  -70.3  27.0
 152  -54.4     0.0    |  -60.8  -72.8   0.0 | -100.8 -112.8   0.0 |  -66.3  -78.3   0.0
 153  -55.3     0.0    |  -59.0  -71.0   0.0 | -101.9 -113.9   0.0 |  -67.4  -79.4   0.0
 154  -54.2     0.0    |  -59.1  -71.1   0.0 | -100.7 -112.7   0.0 |  -66.2  -78.2   0.0
 155  -41.2    48.0    |  -56.1  -57.2  60.0 |  -75.6  -75.6  62.0 |  -41.7  -41.7  58.0
 156  -35.2    47.0    |  -36.2  -36.2 100.0 |  -35.5  -35.5  75.0 |  -35.3  -35.3 100.0
 157  -28.5    81.0    |  -28.7  -28.7 100.0 |  -28.5  -28.6 100.0 |  -28.5  -28.5 100.0
 158  -55.0     0.0    |  -62.7  -72.2   6.0 |  -94.3 -101.9   7.0 |  -66.4  -74.8   9.0
 159  -31.5    55.0    |  -31.5  -31.5  77.0 |  -31.7  -31.7  79.0 |  -31.6  -31.6  75.0
 160  -30.9    70.0    |  -31.1  -31.1 100.0 |  -31.1  -31.1 100.0 |  -31.0  -31.0 100.0
 161  -31.2    78.0    |  -31.7  -31.7 100.0 |  -31.4  -31.4 100.0 |  -31.2  -31.2 100.0
 162  -29.2    83.0    |  -29.2  -29.2 100.0 |  -29.3  -29.3 100.0 |  -29.2  -29.2 100.0
 163  -33.9    20.0    |  -34.1  -34.1  73.0 |  -34.3  -34.3  76.0 |  -34.1  -34.1  73.0
 164  -32.8    68.0    |  -32.9  -32.9 100.0 |  -33.2  -33.2 100.0 |  -33.0  -33.0 100.0
 165  -41.7    55.0    |  -42.3  -42.3 100.0 |  -43.7  -43.7 100.0 |  -43.4  -43.4 100.0
 166  -51.9     1.0    |  -58.4  -63.8  29.0 |  -70.3  -71.7  32.0 |  -61.2  -63.8  32.0
 167  -28.1    27.0    |  -28.1  -28.1  33.0 |  -28.1  -28.1  35.0 |  -28.2  -28.2  33.0
 168  -26.9    49.0    |  -26.9  -26.9  96.0 |  -26.8  -26.8 100.0 |  -27.0  -27.0  94.0
 169  -27.5    36.0    |  -27.5  -27.5  62.0 |  -27.9  -27.9  63.0 |  -27.5  -27.5  64.0
 170  -63.3     0.0    |  -65.4  -77.4   0.0 |  -97.2 -109.2   0.0 |  -72.5  -84.5   0.0
 171  -38.0    14.0    |  -38.0  -38.0  20.0 |  -38.3  -38.3  22.0 |  -38.0  -38.1  18.0
 172  -28.6    91.0    |  -28.6  -28.6 100.0 |  -28.6  -28.6 100.0 |  -28.6  -28.6 100.0
 173  -28.3    89.0    |  -28.3  -28.3 100.0 |  -28.6  -28.6 100.0 |  -28.3  -28.3 100.0
 174  -37.0    48.0    |  -37.3  -37.3 100.0 |  -37.4  -37.4 100.0 |  -37.1  -37.1 100.0
 175  -31.7    74.0    |  -31.7  -31.7 100.0 |  -31.8  -31.8 100.0 |  -31.8  -31.8 100.0
 176  -31.4    73.0    |  -31.4  -31.4 100.0 |  -31.8  -31.8 100.0 |  -31.5  -31.5 100.0
 177  -43.0     1.0    |  -43.3  -48.7  37.0 |  -59.9  -60.2  27.0 |  -54.2  -59.4  28.0
 178  -55.6     0.0    |  -61.8  -73.8   0.0 |  -99.5 -111.5   0.0 |  -67.2  -79.2   0.0
 179  -63.6     0.0    |  -65.6  -77.6   0.0 | -109.6 -121.6   0.0 |  -74.4  -86.4   0.0
 180  -65.0     0.0    |  -68.1  -80.1   0.0 | -111.1 -123.1   0.0 |  -76.2  -88.2   0.0
 181  -62.9     0.0    |  -67.7  -79.7   0.0 | -109.0 -121.0   0.0 |  -74.7  -86.7   0.0
 182  -64.0     0.0    |  -67.1  -79.1   0.0 | -104.1 -116.1   0.0 |  -73.5  -85.5   0.0
```

## Příloha B: soubory

| Soubor                                                | Co obsahuje                                              |
| ----------------------------------------------------- | -------------------------------------------------------- |
| `crates/gamersequalizer-apo/src/lib.rs`               | COM APO, `MicProcessor`, `MicClock`, owner watch         |
| `crates/gamersequalizer-apo/src/rate_bridge.rs`       | resampling ne-48kHz mikrofonů                            |
| `crates/gamersequalizer-mic-vst/src/plugin.rs`        | řetězec: bypass → AEC → denoise → EQ → boost/limit, telemetrie, overload fallback |
| `crates/gamersequalizer-mic-vst/src/ai_denoiser.rs`   | modely, VAD, gate, lookahead, všechny testy a reporty    |
| `crates/gamersequalizer-mic-vst/src/dpdfnet.rs`       | DPDFNet-2: STFT, ONNX krok, overlap-add                  |
| `crates/gamersequalizer-mic-vst/src/aec3.rs`          | WebRTC AEC3 (sonora) echo canceller                      |
| `crates/gamersequalizer-mic-vst/src/delay_estimator.rs` | GCC-PHAT odhad zpoždění ozvěny                         |
| `crates/gamersequalizer-common/src/shm.rs`            | sdílená paměť, výchozí hodnoty, enumy                    |
| `crates/gamersequalizer-common/src/biquad.rs`, `limiter.rs` | EQ a limiter                                       |
| `crates/gamersequalizer-daemon/src/ui/mic_presets.rs` | presety EQ mikrofonu                                     |
| `crates/gamersequalizer-daemon/src/win_audio/mic_stream.rs` | starší cesta přes WASAPI + virtuální mic           |
