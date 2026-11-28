# Optimalizace mikrofonního řetězce: hypotézy k postupnému ověření na Windows

Pro agenta (Antigravity) na Windows stroji, kde jsou privátní nahrávky autora (`test_data/private/`),
překladač MSVC, `scripts/check.ps1` a x86_64 CPU s AVX2 / VNNI.

Větev `perf/ui-and-audio` vychází z `perf/ui-startup` (`1d5fa8a`, kde už jsou hotové optimalizace
startu okna). Tento dokument **není seznam změn, které se musí za každou cenu prosadit do kódu** –
vznikl na Macu analýzou kódu a měřením nad veřejnými vzorky `test_data/denoise/` (`speech_clean.wav` +
ESC-50), tedy **bez přístupu k reálnému hlasu autora v `test_data/private/`**. Každý bod níže (kromě
bitově identického zrychlení CPU v kroku 1) ber jako **hypotézu**: postupuj striktně po jednom kroku,
po každém kroku změř dopad na hlas, případně parametr dolaď – a jakmile nápad začne sebemíň ořezávat
hlas nebo zhorší kvalitu řeči a nejde doladit, **okamžitě ho zahoď (`git checkout -- ...`)**.

Příkazy níže jsou pro PowerShell z kořene repozitáře. Dodržuj `AGENTS.md` a `.claude/rules/*.md`
(žádné `unwrap` / indexování v produkčním kódu, `// SAFETY:` u každého `unsafe`, zarovnané Markdown
tabulky, na konci celá brána `./scripts/check.ps1`).

---

## 1. Železné pravidlo a pracovní postup (čti před jakoukoliv změnou)

1. **Hlas má absolutní přednost (`0,0` zhoršení):**
   Základní pravidlo projektu zní *„raději pustit pár kláves než useknout jedno písmeno“*. Žádná
   ponechaná změna **nesmí zhoršit žádnou metriku zachování hlasu** oproti výchozímu stavu (baseline)
   na nahrávkách autora v `test_data/private/` – netoleruj ani „malé“ zhoršení o `0,2` nebo `0,5`
   procentního bodu na koncích slov, tichých slabikách ani šepotu.
2. **Přísná izolace kroků (jeden krok = jedna změna -> měření -> doladění nebo zahození -> vlastní commit):**
   - Nikdy nemíchej dvě zvukové změny dohromady, jinak nepoznáš, která ořízla slabiku, a drobné ztráty
     se nasčítají.
   - Začni krokem 1 (100% bitově identické zrychlení CPU bez jakékoliv změny zvuku).
   - U každého dalšího kroku nejprve vyzkoušej mřížku parametrů od jemných po agresivnější.
   - **Když krok projde** s nulovým zhoršením hlasu a měřitelným přínosem (aspoň o `1,5 dB` tišší pauzy,
     o `>= 20 %` méně falešně otevřené brány na klávesách, ušetřené CPU nebo opravené učení leveleru za
     AEC3), ulož ho jako **samostatný commit** – aby šel v případě potřeby po živém poslechu jedním
     `git revert` vrátit bez ztráty ostatních kroků.
   - **Když krok neprojde** (každá varianta buď ořezává hlas, nebo nepřináší znatelný zisk), vrať kód do
     čistého stavu (`git checkout -- ...`), zapiš čísla do `DEVLOG.md` do sekce *„zkusilo se a vyřadilo“*
     a pokračuj dalším krokem. Stejně cenný výsledek jako přijatá optimalizace je čisté zamítnutí
     hypotézy podložené čísly.
3. **Co už bylo vyvráceno dřív nebo na Macu (vůbec nezkoušet):**

| Nápad                                            | Proč nezkoušet                                                                                  |
| ------------------------------------------------ | ----------------------------------------------------------------------------------------------- |
| Spektrální post-filter v DFN3 (`set_pf_beta`)    | Změřeno na Macu: 10–50 ms za slovem +0,0 dB (ve 20 ms -2,3 dB), konce slov horší o +1,1..2,1 b. |
| `RESCUE_SOFT_PROB` 0,15 -> 0,30                  | Změřeno v milníku 41: konce slov ztracené o > 6 dB stouply 21,3 -> 22,3 %                       |
| Pásmové dělení `ClickRescue` / transient limiter | Limiter za DFN3 vyřazen v milníku 37; úhoz klávesy do desky má spoustu energie i pod 1 kHz      |
| Alokace ve wrapperech kolem `tract`              | Úspora ~0,005 % jádra vedle interních alokací `tract`, zbytečný odklon forku `deep_filter`      |
| FFT místo matice v `silero_vad`                  | Celé Silero bere ~0,16 % jádra a je záměrně bez závislostí                                      |
| Antialiasing před 6× decimací, 40ms `pre_gain`   | Žádný doložený problém v nahrávkách                                                             |
| Modely `DPDFNet-4/8`, `GTCRN`, `TEN-VAD`         | `DPDFNet-4` nemá 48 kHz, `DPDFNet-8` ~27 % jádra, `GTCRN` jen 16 kHz, `TEN-VAD` vadná licence   |

---

## 2. Krok 0: Naměření baseline a uložení referenčních WAVů pro poslech

Ještě než sáhneš na jediný řádek kódu, spusť na současném kódu všechny reporty nad `test_data/private/`
a ulož si jejich kompletní výstup (např. do dočasného souboru mimo git), abys měl přesná referenční
čísla:

```powershell
cargo test -p gamersequalizer-mic-chain --release private_report -- --ignored --nocapture
cargo test -p gamersequalizer-mic-chain --release near_pause_report -- --ignored --nocapture
cargo test -p gamersequalizer-mic-chain --release whisper_gate_report -- --ignored --nocapture
cargo test -p gamersequalizer-mic-chain --release scenario_report -- --ignored --nocapture
cargo test -p gamersequalizer-mic-chain --release keyboard_call_report -- --ignored --nocapture
cargo test -p gamersequalizer-mic-chain --release click_report -- --ignored --nocapture
cargo test -p gamersequalizer-mic-chain --release pause_burst_report -- --ignored --nocapture
cargo test -p gamersequalizer-mic-chain --release leveler_report -- --ignored --nocapture
cargo test -p gamersequalizer-mic-chain --release cpu_breakdown_report -- --ignored --nocapture
Get-ChildItem test_data/private/captures -Directory -ErrorAction SilentlyContinue | ForEach-Object {
  $env:CAP = $_.Name
  cargo test -p gamersequalizer-mic-chain --release capture_replay_report -- --ignored --nocapture
}
Remove-Item Env:CAP -ErrorAction SilentlyContinue
```

Zazálohuj vygenerované poslechové WAVy ze `scenario_report` a `keyboard_call_report` do
`test_data/private/out/baseline/` (složka `test_data/private/` je v `.gitignore`), aby si autor mohl po
skončení pustit přímé poslechové A/B porovnání svého hlasu před a po:

```powershell
New-Item -ItemType Directory -Force test_data/private/out/baseline | Out-Null
Copy-Item test_data/private/out/*.wav test_data/private/out/baseline/
```

### Matice ochrany hlasu (musí projít u každého zvukového kroku)

| Test (`--ignored --nocapture`) | Soubor v `test_data/private/` | Metriky hlasu, které se nesmí zhoršit oproti Kroku 0                                |
| ------------------------------ | ----------------------------- | ----------------------------------------------------------------------------------- |
| `private_report`               | `speech.wav` + `keys.wav`     | `soft%` a `loud%` lost (všechny 3 mixy × 3 modely, `gate auto` i `auto+lookahead`)  |
| `near_pause_report`            | `speech.wav` + `keys.wav`     | `speech lost > 6 dB %`, `> 15 dB %` a hlavně **`tails lost > 6 dB %`** (konce slov) |
| `whisper_gate_report`          | `whisper_scenario.wav`        | Všech 6 hlasových/šeptaných úseků: dB pod ungated i `% rámů > 6 dB` pod ungated     |
| `scenario_report`              | `scenario.wav`                | `voice frames under closed gate %` a `speech seconds level vs input`                |
| `keyboard_call_report`         | `keyboard_call.wav`           | `speech frames cut > 10 dB under RNNoise %`                                         |
| `click_report`                 | `speech.wav` + kliky myši     | `soft%` / `loud%` lost, pod kliky `mean dB`, `> 6 dB %`, `> 15 dB %`                |
| `capture_replay_report`        | `captures/*/04_after_aec.wav` | Ve **všech** uložených captures: `voice windows: mean loss dB` a `> 10 dB %`        |

---

## 3. Krok 1 (bezztrátový, nulové riziko pro zvuk): Zrychlení `rnnoise2` a velikost DLL

Začni tímto krokem, protože **nesmí změnit zvuk ani pravděpodobnosti VAD ani o jediný bit** – pouze
snižuje spotřebu CPU v `audiodg.exe` (po milníku 63/72 bere DFN3 na Ryzenu 7 9800X3D `1,54 %` jádra z
celkových `3,24 %`, zbytek tvoří hlavně vstupní detektor `in_net` v `rnnoise2`).

### 1a. Přeskočit `dense_out` ve `voice_probability`

V `crates/vendor/rnnoise2/src/denoise.rs:165-175` volá `voice_probability` funkci `analyze` ->
`RnnModel::compute` (`src/nnet.rs:78-97`), která v každém 10ms rámci počítá i výstupní vrstvu zisků
`self.dense_out.compute(gains, &cat)` (`1536 -> 32` float mat-vec + 32× `sigmoid_approx`) a pole `g` hned
zahodí. Do `RnnState` tyto zisky nijak nevstupují.

- V `nnet.rs` odděl společný průchod `conv1..gru3 -> cat` + `vad_dense` (např. `compute_vad(&self,
  state: &mut RnnState, features: &[f32; CONV1_IN]) -> f32`) od plného `compute` a volej `compute_vad` z
  `voice_probability`.
- Ověř bitovou shodu testem `voice_probability_matches_process_frame` v
  `crates/vendor/rnnoise2/tests/reference.rs`.

### 1b. VNNI int8 jádro (`vpdpbusd`) v `crates/vendor/rnnoise2/src/kernel.rs`

Dnešní AVX2 cesta rozšiřuje `i8 -> i16` a sčítá přes `_mm256_madd_epi16` + horizontální `hadd` a
`permute`. Paměťové rozložení bloku v `cgemv8x4` (`block: [u8; 32]`, kde `w[k * 4 + c]` leží po
čtveřicích vstupů `c = 0..4` pro každý z 8 výstupů `k = 0..8`) je přitom **přímo nativní tvar pro
jedinou instrukci `vpdpbusd`** – bez rozšiřování na `i16` a bez horizontální redukce.

#### Matematika pro 100% bitovou shodu se skalární referencí

`vpdpbusd` násobí bez znaménka `u8` × se znaménkem `i8` a sčítá čtveřice do `i32` (nesaturující). Naše
kvantizované vstupy `x` jsou `i8` (`-128..=127`). Posunem o `+128` (`x_u8 = (x as u8) ^ 0x80`) platí pro
každý výstup `y_i`:

$$\sum_{j} (x_j + 128) \cdot w_{i,j} - 128 \sum_{j} w_{i,j} = \sum_{j} x_j \cdot w_{i,j}$$

Protože v `Int8Matrix::new` platí `counts[g] <= MAX_GROUPS (96)` (nejvýš `384` součinů na výstup), je
`sum(x_u8 * w)` v rozsahu `±(384 * 255 * 128) = ±12,5 M`, tedy hluboko v rozsahu `i32` bez přetečení a se
100% bitovou shodou se skalární referencí.

- V `Int8Matrix::new` si jednou předpočítej `bias_corr: Box<[i32]>` délky `nb_out`: pro každý výstup
  `-128 * součet všech jeho int8 vah přes přítomné bloky`.
- Ve VNNI `gemv` inicializuj 8-prvkový akumulátor `acc` z `bias_corr[8*g .. 8*g + 8]` místo nuly, vstupy
  připrav jako `[u32; 256]` se zflipovaným znaménkovým bitem každého bajtu (`u32::from_le_bytes(...) ^
  0x8080_8080`) a ve vnitřní smyčce dělej `_mm256_set1_epi32(xu[col])` + `_mm256_loadu_si256` +
  `dpbusd` (vyzkoušej 1 vs. 2 akumulátory ob-blok pro skrytí latence VNNI pipeline).

#### Rozdíl AMD Zen 4 vs. Intel Alder Lake+ v `core::arch::x86_64`

V Rustu `1.100.0` existují dvě různé stabilní VNNI funkce s různými `target_feature`:

| Funkce v `core::arch::x86_64` | Požadované `target_feature` | Které CPU ji mají                                        |
| ----------------------------- | --------------------------- | -------------------------------------------------------- |
| `_mm256_dpbusd_epi32`         | `"avx512vnni,avx512vl"`     | AMD Zen 4 (Ryzen 7000), Zen 5 (9800X3D), Intel Ice Lake+ |
| `_mm256_dpbusd_avx_epi32`     | `"avxvnni"`                 | Intel Alder Lake / Raptor Lake / Arrow Lake, AMD Zen 5   |

AMD Zen 4 nemá VEX `avxvnni` (má jen EVEX `avx512vnni` + `avx512vl`), zatímco spotřebitelské Intely od
12. generace mají `avxvnni` bez AVX-512. V `Kernel::detect()` proto detekuj v pořadí:
1. `avx512vnni` + `avx512vl` -> `Path::Avx512Vnni` (`_mm256_dpbusd_epi32`),
2. `avxvnni` -> `Path::AvxVnni` (`_mm256_dpbusd_avx_epi32`),
3. `avx2` -> `Path::Avx2`,
4. `Path::Scalar`.

V `kernel::tests::simd_matches_scalar` otestuj každou podporovanou cestu bit po bitu proti
`Kernel::SCALAR` (včetně extrémů `i8::MIN` a `127`) a aktualizuj `crates/vendor/rnnoise2/README.md`.

### 1c. Kontrola `rnnoise_little.bin` ve velikosti DLL

`crates/vendor/rnnoise2/src/lib.rs:27` vkládá přes `include_bytes!` jak `rnnoise_full.bin` (3,5 MB), tak
`rnnoise_little.bin` (1,55 MB), ačkoliv produkce bere jen `Model::Full`. Zkontroluj velikost
`target/apo/gamersequalizer_apo.dll` a `target/apo-dist/gamersequalizer_apo.dll`: pokud linker
`LITTLE_BLOB` sám neodstraní, schovej `Model::Little` za Cargo feature zapnutou pro testy `rnnoise2`.

### Ověření a commit Kroku 1

```powershell
cargo test --release --manifest-path crates/vendor/rnnoise2/Cargo.toml
cargo test --release --manifest-path crates/vendor/rnnoise2/Cargo.toml --lib stage_timing -- --ignored --nocapture
cargo test --release --manifest-path crates/vendor/rnnoise2/Cargo.toml --test report rnnoise2_bench -- --ignored --nocapture --test-threads=1
cargo test -p gamersequalizer-mic-chain --release cpu_breakdown_report -- --ignored --nocapture
cargo test -p gamersequalizer-mic-chain --release private_report -- --ignored --nocapture
```

Čísla v `private_report` musí vyjít **identicky s Krokem 0**. Pokud VNNI + `compute_vad` zrychlí `in_net`,
commitni Krok 1 samostatně ještě před zkoušením zvukových hypotéz.

---

## 4. Krok 2 (hypotéza): Výstupní detektor brány (`vad_out`) při psaní po domluvení

### Co chceme zjistit

`vad_out` v `crates/gamersequalizer-mic-chain/src/ai_denoiser/voice.rs` (`OUT_VOICE_PROB = 0.3`,
`OUT_VOICE_ACTIVE_FRAMES = 50` = 500 ms, `OUT_VOICE_KEEP_PROB = 0.02`) běží starou sítí RNNoise 2017 na
výstupu denoiseru (`out_net`). Při psaní po větě stačí každých 500 ms jediný 10ms rámec nad `0.02` a
`active_left` se znovu nabije na 500 ms + 200 ms `GATE_HOLD_SAMPLES`, takže brána po domluvení zavírá
pomalu. Konstanty vznikly před milníkem 35 (`in_net`, RNNoise 0.2) a milníkem 40 (`soft_net`, Silero VAD
na `0.15`).

### Co vyšlo na Macu na veřejných vzorcích (`GATE_AUTO_LOOKAHEAD`)

| Varianta `vad_out` | DFN3: ztráta řeči (3 mixy)            | DFN3: psaní po řeči / jen klávesy | RNNoise: ztráta řeči (3 mixy)         | RNNoise: psaní po řeči / jen klávesy |
| ------------------ | ------------------------------------- | --------------------------------- | ------------------------------------- | ------------------------------------ |
| `(50, 0.02)` dnes  | `1,9/0,3` \| `0,3/0,0` \| `3,1/0,0` % | `10,0 %` / `8,1 %`                | `0,0/0,0` \| `0,0/0,0` \| `0,6/0,0` % | `41,7 %` / `32,2 %`                  |
| `(20, 0.05)`       | `1,9/0,3` \| `0,3/0,0` \| `3,1/0,0` % | `8,3 %` / `6,5 %`                 | `0,0/0,0` \| `0,0/0,0` \| `0,6/0,0` % | `20,8 %` / `16,4 %`                  |
| `(15, 0.10)`       | `1,9/0,3` \| `0,3/0,0` \| `3,1/0,0` % | `8,3 %` / `6,5 %`                 | `0,0/0,0` \| `0,0/0,0` \| `0,6/0,0` % | `19,1 %` / `13,1 %`                  |
| `vad_out` OFF      | `2,2/0,3` \| `0,3/0,0` \| `3,1/0,0` % | `3,6 %` / `2,7 %`                 | `0,0/0,0` \| `0,0/0,0` \| `0,6/0,0` % | `5,6 %` / `2,3 %`                    |

> **Varování k ověření na reálném hlase:** Všimni si, že už na syntetickém mixu `-10 dB` na Macu zvedlo
> úplné vypnutí `vad_out` u DFN3 `soft_dropouts` z `1,9 %` na `2,2 %` (`+0,3` b.). Na hlasu autora v
> `test_data/private/` může `vad_out` OFF useknout doznění slova nebo tichý nádech po pauze!

### Postup experimentu na Windows

1. Rozšiř `gate_sweep` v `crates/gamersequalizer-mic-chain/src/ai_denoiser/tests.rs` (nebo si přidej
   dočasný sweep nad `private/speech.wav` + `private/keys.wav` s `GATE_AUTO_LOOKAHEAD`), který proměří
   jemnou řadu variant `vad_out`:
   - `(50, 0.02)` (dnešní baseline),
   - `(35, 0.03)`, `(25, 0.03)`, `(25, 0.04)`, `(20, 0.04)`, `(20, 0.05)`, `(15, 0.08)`, `(15, 0.10)`,
   - `vad_out` OFF.
2. Každou nadějnou variantu prověř **celou maticí ochrany hlasu z Kroku 0** (`private_report`,
   `near_pause_report` – sleduj hlavně `tails lost > 6 dB`, `whisper_gate_report`, `scenario_report` –
   sleduj `voice frames under closed gate %`, `keyboard_call_report`, `pause_burst_report` a všechny
   captures v `capture_replay_report`).
3. **Vyhodnocení (vyber první splněnou možnost, jinak zahoď):**
   - **Možnost 2A (`vad_out` OFF):** Ponech **výhradně tehdy**, pokud na *žádném* testu z matice ochrany
     hlasu nezhorší ztrátu řeči, konců slov (`tails lost > 6 dB`) ani šepotu oproti Kroku 0 (`0,0`
     zhoršení). Pokud projde, nech v `ShmHeader` a `DenoiseDebug` `out_prob = -1.0` a
     `vad_out_active_left = 0` (`mic_out_prob_max_permille = 0`), ať se nemění `SHM_VERSION` ani sloupce
     `vars.tsv`.
   - **Možnost 2B (jemnější `(active_frames, keep_prob)` s nulovou ztrátou hlasu):** Pokud `vad_out` OFF
     kdekoliv ubere hlas, vyber z mřížky jen takovou dvojici, která má na všech privátních testech
     **nulové zhoršení hlasu** a přitom znatelně zkrátí držení brány na klávesách po řeči.
   - **Možnost 2C (samostatný podkrok – sloučení `rescue.net` a `out_net`):** Pokud `vad_out` zůstává
     zapnutý, vyzkoušej zvlášť (a znovu přeměř!), zda lze u DFN3 krmit `ClickRescue` i `vad_out` z
     jediného průchodu `VoiceNet` spuštěného **před** `ClickRescue` (za mixem by slyšel kliky z RNNoise).
     Pokud by posun `vad_out` před `ClickRescue` zhoršil jedinou metriku hlasu, sloučení zahoď.
   - **Zamítnutí:** Pokud každé zpřísnění `vad_out` na hlasu autora ubere konce slov nebo šepot, **vrať
     změny (`git checkout -- ...`)**, zapiš zjištění do `DEVLOG.md` a nech `(50, 0.02)` beze změny.

---

## 5. Krok 3 (riziková hypotéza – testuj skepticky): Zisk RNNoise uvnitř `ClickRescue`

### Co chceme zjistit

V `crates/gamersequalizer-mic-chain/src/ai_denoiser/mod.rs:645` běží záchranná větev `ClickRescue` s
uživatelským `strength` (`0.75`), takže při záplatování DFN3 přimíchává 25 % surového nefiltrovaného
kliku (`-12 dB`). Vyšší wet poměr uvnitř `rescue_on` ztiší kliky v mezerách těsně u slov, **jenže samotný
RNNoise 2017 při vyšším `strength` víc tlumí i tiché souhlásky**.

### Co vyšlo na Macu na veřejných vzorcích (`GATE_AUTO_LOOKAHEAD`)

| Scénář                 | Záchrana      | Soft / loud lost % | Všechny pauzy / daleké pauzy | Kliky přes řeč: ztráta > 15 dB |
| ---------------------- | ------------- | ------------------ | ---------------------------- | ------------------------------ |
| Klávesy `-10 dB` SNR   | `0.75` (dnes) | `1,9 / 0,3 %`      | `20,1 dB` / `46,5 dB`        | –                              |
| Klávesy `-10 dB` SNR   | `1.00` wet    | `2,8 / 0,3 %`      | `26,0 dB` / `46,5 dB`        | –                              |
| Bušení myší `-10 dBFS` | `0.75` (dnes) | `0,9 / 0,0 %`      | `20,2 dB` / `38,9 dB`        | `1,9 %`                        |
| Bušení myší `-10 dBFS` | `1.00` wet    | `0,9 / 0,0 %`      | `22,0 dB` / `38,9 dB`        | `1,9 %`                        |
| Bušení myší `-3 dBFS`  | `0.75` (dnes) | `0,9 / 0,0 %`      | `19,5 dB` / `38,4 dB`        | `1,4 %`                        |
| Bušení myší `-3 dBFS`  | `1.00` wet    | `1,9 / 0,0 %`      | `20,7 dB` / `38,4 dB`        | `1,4 %`                        |

> **Vysoké riziko ořezávání hlasu:** Plných `1.00` wet už na Macu zvedlo `soft_dropouts` o `+0,9` až
> `+1,0` b. (a může shodit i `assert!(with.quality.soft_dropouts <= without.quality.soft_dropouts)` v
> `test_deepfilter_keeps_speech_under_hammered_mouse_clicks`). Plných `1.00` proto velmi pravděpodobně
> **neprojde** železným pravidlem!

### Postup experimentu na Windows

1. Místo skoku rovnou na `1.0` otestuj v `mod.rs:645` (`if rescue_on { strength.max(w) } else { strength }`)
   jemnou řadu `w` z `[0.75, 0.80, 0.85, 0.90, 0.95, 1.00]`.
2. Pro každé `w` spusť:
   - `cargo test -p gamersequalizer-mic-chain --release` (všechny regresní testy včetně
     `test_deepfilter_keeps_speech_under_hammered_mouse_clicks`),
   - `near_pause_report` (sleduj `speech lost > 6 dB`, `> 15 dB` a `tails lost > 6 dB` + útlum v oknech
     `1-1`, `2-2`, `3-5` rámů od řeči),
   - `click_report` (sleduj `soft%`, `mean dB`, `> 6 dB %` i `> 15 dB %`),
   - `keyboard_call_report` (`speech frames cut > 10 dB under RNNoise %`),
   - `private_report` a `capture_replay_report` (zejména `20261109-204900`).
3. **Vyhodnocení:**
   - Ponech jen takové `w`, u kterého se na hlasu autora **nezhorší ani `soft_dropouts`, ani `tails lost
     > 6 dB`, ani `lost > 6 dB` pod kliky, ani `speech frames cut > 10 dB` v `keyboard_call_report`**
     (`0,0` zhoršení oproti předchozímu commitu) a přitom zlepší potlačení kláves v pauzách aspoň o
     `1,5 dB`.
   - Jestliže **každá** hodnota nad `0.75` zhorší zachování tichých slabik nebo konců slov, **celý Krok 3
     zahoď (`git checkout -- ...`)** a zdokumentuj v `DEVLOG.md`, proč `0.75` uvnitř `ClickRescue`
     chrání hlas.

---

## 6. Krok 4 (hypotéza): Záložní klíč pro `VoiceLeveler` výhradně za aktivním AEC3

### Problém z milníku 41

Když hraje hudba nebo hra z reproduktorů, nelineární potlačovač AEC3 pozmění spektrum hlasu natolik, že
`in_net` (`RNNoise 0.2 > 0.7`) na výstupu AEC3 spadne ze `74 %` na **`8 %`** hlasových rámů (capture
`20261109-205013`, 9–29 s). Přísný klíč leveleru `level_key` (`6` rámů `in_net > 0.95` v řadě, milník 64)
pak při hraní na reproduktorech nesepíná skoro vůbec – leveler i automatická vstupní citlivost se
přestanou učit, a tichý mikrofon může zůstat nezesílený (což naopak vede k tomu, že denoiser a brána hlas
snáz utlumí). `Silero VAD` (`soft_net`) na tomtéž signálu za AEC3 drží `1.00`.

### Tvrdá bezpečnostní omezení

1. **Mimo aktivní AEC3 se nesmí změnit ani bit:** Záložní klíč smí být povolen **výhradně tehdy, když je
   `self.aec_state == AecState::Locked`** (předané příznakem např. `set_aec_active(bool)` z `chain.rs` do
   `AiVoiceIsolator`). Při hraní na sluchátkách nebo v tichu tak zůstane `leveler_report` na
   `whisper_scenario.wav` 100% identický se současným stavem.
2. **Žádné učení z šepotu, kliků ani zbytků ozvěny:**
   - V `whisper_scenario.wav` musí sekce `whisper` i s hypoteticky zapnutým záložním klíčem držet `< 2,0 %`
     označených rámů (dnes `1,2 %`, milník 64).
   - Na `keys.wav` a v bezhlasých / klikacích úsecích reproduktorových captures (`20261109-210655` v 5–9 s,
     kde Silero dalo `0,20–0,27` na zbytky kliků za AEC3, a tiché úseky `20261109-205013`,
     `20261109-125717`, `20261109-130319`) musí být falešné sepnutí klíče **`0,0 %`**.

### Postup experimentu na Windows

1. Rozšiř `leveler_key_report` v `tests.rs` o vyhodnocení pravidla „posledních `k` chunků `Vad48k` nad
   prahem `t`“ pro `t` z `[0.80, 0.85, 0.90, 0.95]` a `k` z `[2, 3, 4]` na `whisper_scenario.wav`,
   `speech.wav`, `scenario.wav`, `keyboard_call.wav`, `keys.wav` i na `04_after_aec.wav` všech
   reproduktorových captures (odděleně úseky s hlasem a bez hlasu / s ozvěnou).
2. Pokud existuje dvojice `(t, k)` splňující všechna omezení výše:
   - zapoj ji do `heard_level_voice` pouze při `aec_active == true` (pohlídej, aby `soft_net` v
     `process_samples` běželo i při `voice_wanted && aec_active` s vypnutou bránou),
   - ověř `leveler_report` a `chain::tests::chain_report` – zisk leveleru na řeči za AEC3 nesmí nikdy
     klesnout pod rozumnou úroveň ani pumpovat na zbytcích hudby z reproduktorů.
3. Pokud žádná dvojice `(t, k)` nedokáže na reálných captures čistě oddělit hlas za AEC3 od zbytků
   ozvěny/kliků a šepotu (`0,0 %` na bezhlasých úsecích a `< 2,0 %` na šepotu), **krok zahoď** a zapiš
   tabulku do `DEVLOG.md`.

---

## 7. Krok 5 (volitelný průzkum bokem, mimo produkční kód): `FastEnhancer-B` (48 kHz)

Pouze pokud zbyde čas po dokončení kroků 1–4:

- Z GitHub Releases `aask1357/fastenhancer` (MIT) stáhni do dočasné složky mimo git (např.
  `test_data/private/models/`) 48kHz váhy varianty **B** (`fastenhancer_b_spec.onnx` / `fastenhancer_b.onnx`,
  `n_fft = 1024`, `hop_size = 512`).
- Jen v offline `#[ignore]`d testu nebo scratch přípravku vyzkoušej, zda graf projde přes `tract 0.23.11`
  a jak si stojí v latenci, CPU a na `private_report` + `near_pause_report` (`tails lost > 6 dB`) proti
  DFN3 a DPDFNet-2.
- **Do produkčního kódu ani UI nic neintegruj** – do `DEVLOG.md` zapiš jen naměřená čísla.

---

## 8. Závěrečná kontrola, poslechové soubory a brána kvality

1. Znovu spusť `scenario_report` a `keyboard_call_report`, aby v `test_data/private/out/` ležely finální
   WAVy hned vedle `test_data/private/out/baseline/` pro autorův vlastní poslech:
   ```powershell
   cargo test -p gamersequalizer-mic-chain --release scenario_report -- --ignored --nocapture
   cargo test -p gamersequalizer-mic-chain --release keyboard_call_report -- --ignored --nocapture
   ```
2. Spusť celou bránu projektu:
   ```powershell
   ./scripts/check.ps1
   ```
   Vše musí projít s nulou varování (formátování, clippy včetně `debug-capture` a `cpu-stats`, workspace
   testy, `apo-install.tests.ps1` v PowerShellu 7 i Windows PowerShellu 5.1, testy všech vendor crates).
3. Zapiš kompletní výsledky do `DEVLOG.md`: u každého kroku tabulku `před -> po`, které kroky prošly a
   zůstaly v samostatných commitech, a které hypotézy byly zahozeny (včetně přesného důvodu a čísel).
4. V závěrečné zprávě pro uživatele vypiš:
   - seznam commitů, které na větvi přibyly (a co bylo naopak zahozeno),
   - které dvojice WAV souborů v `test_data/private/out/baseline/` vs. `test_data/private/out/` si má
     poslechnout (pokud vůbec prošla nějaká zvuková změna mimo Krok 1),
   - a připomeň příkaz `./scripts/dev-rebuild.ps1 -Apo -Start` (případně instalaci APO s admin právy),
     pokud chce změny rovnou otestovat živě na svém mikrofonu.
   *(Nesleduj ani nemaž netrackované soubory jako `docs/mic-recommendation-review.md`.)*

