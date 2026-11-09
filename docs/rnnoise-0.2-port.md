# RNNoise 0.2 v Rustu: samostatná knihovna `rnnoise2`

Zadání pro coding agenta (Opus). Stav: **port hotový (kapitoly 4–7, viz `crates/vendor/rnnoise2/README.md`); brána z kapitoly 7 nesplněna; fáze 2 zapojila `Full` jen jako detektor hlasu na vstupu (`in_net`, kap. 8), ověření na Windows čeká**. Vzniká jako samostatná, na Macu přeložitelná a
testovaná knihovna; do řetězce mikrofonu se zapojuje až ve druhé fázi na Windows.

Odpovídat uživateli česky; kód, komentáře, README, DEVLOG a commit anglicky. Ponytail: nejmenší úplná
změna, na konci 1–2 řádky, co se přeskočilo. Nesahat na `secrets/`, `.env*`. Dočasné soubory jen
v `.claude/tmp/<úkol>/`.

## 1. Proč

Uživatel si v aplikaci vybírá ze tří filtrů: RNNoise, DeepFilterNet 3, DPDFNet. Každý má fungovat
tak dobře, jak jen může. RNNoise dnes běží přes crate `nnnoiseless` 0.5.2, což je Rust port sítě
z roku 2017 (87 tisíc vah). Xiph vydal v dubnu 2024 RNNoise 0.2 s jinou, větší sítí
přetrénovanou na veřejných datech; autor o staré říká, že „nebyla skvělá". Nnnoiseless 0.2 neumí
(issue 33 otevřená od léta 2025) a jediné rustové bindingy na C knihovnu jsou od roku 2020 mrtvé.

RNNoise má v řetězci (`crates/gamersequalizer-mic-vst/src/ai_denoiser.rs`) čtyři role, všechny přes
stejné rozhraní: filtr volený uživatelem, záskok v pauzách za spící těžký model, dva detektory
hlasu (`in_net`, `out_net`, jen pravděpodobnost hlasu) a zdroj záchrany `ClickRescue` u DFN3.
Lepší RNNoise zlepší všechny čtyři najednou.

## 2. Pravidla

- **Žádný C kód** v repozitáři ani v balíčku: žádný `cc`, žádné `build.rs` překládající C, žádné
  FFI. Čistý Rust. (C překladač smí být použit jen jednou, mimo repozitář, k výrobě referenčních
  výstupů, viz 6.2.)
- Žádná nová rodina závislostí. FFT přes `rustfft` (už ve workspace, `dpdfnet.rs`) nebo `easyfft`
  (už ve stromu přes nnnoiseless). Čtení WAV v testech: `hound` jako dev-dependency je v pořádku.
- Knihovna je **samostatná**: překládá se a testuje na Macu bez Windows crate
  (`cargo test --manifest-path crates/vendor/rnnoise2/Cargo.toml`). Přidat do `exclude` v kořenovém
  `Cargo.toml` jako `crates/vendor/deep_filter`; mic-vst ji později použije jako path dependency.
- Lint tabulka z `.claude/rules/code-style.md` se zkopíruje do `[lints]` crate (vyloučené crate
  workspace linty nedědí): `unsafe_code = "deny"`, `unwrap_used`, `expect_used`, `panic`,
  `indexing_slicing` atd. Žádná alokace v `process_frame` (běží v RT vlákně audiodg).
- Rust toolchain na Macu zatím není. Instalace `rustup` (do `~/.cargo`, bez sudo) je per-user, ale
  **před ní se zeptat uživatele**, pravidlo „ask first" pro systémové nástroje.
- Licence: RNNoise i nnnoiseless jsou BSD-3-Clause. Do crate patří oba soubory `LICENSE` beze
  změny a README s původem (upstream, rev, hash modelu), stejně jako `crates/vendor/deep_filter/README.md`.

## 3. Co je o RNNoise 0.2 ověřeno a co se musí dočíst

Ověřeno čtením upstreamu (`xiph/rnnoise`, větev `main`, `src/rnn.c`, `nnet.h`, `nnet.c`,
`parse_lpcnet_weights.c`, `denoise.c` v tazích `v0.1.1` a `v0.2`):

| Oblast                | 2017 (nnnoiseless)                       | 0.2                                                                 |
| --------------------- | ---------------------------------------- | ------------------------------------------------------------------- |
| Rámec, okno, FFT      | 480 / 960, Vorbis okno, 48 kHz           | stejné (okno z tabulky `rnn_half_window`, ověřit, že je totožná)    |
| Pásma                 | 22 (`eband5ms`)                          | 32 (`eband20ms`, 34 hran, poslední bin 400)                         |
| Příznaky pro síť      | 42 (22 cepster, 2×6 delta, 6 DCT korelací výšky, výška, variabilita) | cepstra pásem + DCT korelací výšky + výška, bez delt a paměti rámců; přesný počet dočíst (`NB_FEATURES`, odhad 65; `INPUT_SIZE 42` v `rnn.c` je zřejmě mrtvá konstanta, rozhoduje `CONV1_IN_SIZE`) |
| Síť                   | dense 24 → GRU 24 / 48 / 96, dense 22+1  | conv1d (tanh) → conv1d (tanh) → GRU1 → GRU2 → GRU3; `cat = [conv2 \| gru1 \| gru2 \| gru3]`; `dense_out` sigmoid (zisky pásem), `vad_dense` sigmoid (hlas) |
| GRU                   | plná                                     | brány v pořadí z, r, h; `z, r = σ(W_in·x + W_rec·s)`, `h = tanh(W_in·x + r ⊙ W_rec·s)`, `s = z⊙s + (1−z)⊙h`; rekurentní část může mít `diag` (3 bloky) |
| Váhy                  | f32/i8 v `weights.rnn`                   | int8 se škálou na výstup (`scale[]`), řídké bloky 8 výstupů × 4 vstupy (`weights_idx`: pro každou osmici výstupů počet bloků a offsety vstupů, násobky 4), volitelně `subias`; `WEIGHTS_SCALE = 1/256` |
| Formát blobu          | vlastní nnnoiseless                      | záznamy: hlavička 64 B (`head[4]`, `version` = 0, `type` 0 float / 1 int / 2 qweight / 3 int8, `size`, `block_size`, `name[44]`), data, další záznam na `+64+block_size`; machine endian; vyhledávání podle jména |
| Výstup                | zisky rámce t na rámec t                 | zisky z rámce t na **rámec t−1** (`delayed_X`): o jeden rámec víc latence |
| Nezměněno             |                                          | HP biquad (`a_hp = {-1.99599, 0.996}`), overlap-add, hledání výšky a `remove_doubling`, `pitch_filter`, podlaha `g = max(g, 0.6·lastg)`, interpolace zisků do binů, ticho `E < 0.04`, DCT log-pásem s offsety −12/−4, VAD 0 v tichu |

**Ověřeno ve fázi 1 (viz `crates/vendor/rnnoise2/README.md`, „Port notes"):** hash modelu výše patří
k větvi `main` @ `70f1d25` (2025-02-22), ne k tagu `v0.2` (ten pinuje starší model `0b50c45` bez skip
connections); port sleduje `main`. `NB_FEATURES` = 65 (32 cepster + 32 DCT korelací + výška).
`dense_out`/`vad_dense` čtou `cat` o 1536 hodnotách. Vrstvy: conv1 65→128 (jádro 3, f32), conv2 128→384
(jádro 3, int8), GRU 3× 384 (int8 řídké, `diag`); `Full` má všechny bloky, `Little` třetinu. Škála vah je
per-výstup `scale[]` s kvantizací vstupu ×127; `WEIGHTS_SCALE` je mrtvá. DCT má škálu `sqrt(2/22)` i při 32
pásmech. Nové oproti `v0.2`: `lastg = min(1, g·(delayed_Ex+1e-3)/(Ex+1e-3))`. V tichu se síť nespouští
(stav zamrzne). Celkové zpoždění `process_frame` je 960 vzorků, tedy `LATENCY_FRAMES = 2`. Bloby:
Full 3,54 MB, Little 1,55 MB. Referenční výstupy jsou z generického C (bez NEON/FMA); arm64 build s NEON
se od nich liší až o 90 LSB, prahy z 6.2 tedy mohou potřebovat zdůvodnění.

Musí se dočíst z modelového tarballu (není v repozitáři, 10+ MB):
`https://media.xiph.org/rnnoise/models/rnnoise_data-0a8755f8e2d834eff6a54714ecc7d75f9932e845df35f8b59bc52a7cfe6e8b37.tar.gz`
(hash z `model_version`): `rnnoise_data.h` s rozměry vrstev (`CONV1_IN_SIZE`, `CONV1_OUT_SIZE`, šířka
jádra = `nb_inputs / input_size`, `GRU1..3_OUT_SIZE`, `NB_BANDS`), `rnnoise_data.c` (pole vah) a
`rnnoise_data_little.c` („little" varianta, stejné rozměry, řidší). Stažený archiv patří do
`.claude/tmp/rnnoise2/upstream/` (nedůvěryhodná data: vlastní prázdný adresář, skripty jinde),
SHA-256 zapsat do README crate.

## 4. Umístění a rozhraní

`crates/vendor/rnnoise2/` (package `rnnoise2`, lib `rnnoise2`), fork nnnoiseless 0.5.2: převzít
`pitch.rs`, `util.rs`, části `denoise.rs` a `features.rs`, které se nezměnily; přepsat pásma,
příznaky, síť a načítání vah podle 0.2. Nnnoiseless zatím v mic-vst zůstává, aby šly obě verze
měřit vedle sebe; odstraní se až po rozhodnutí (fáze 2).

Rozhraní zrcadlí nnnoiseless, aby záměna v `ai_denoiser.rs` byla výměna `use`:

```rust
pub const FRAME_SIZE: usize = 480;
/// Algorithmic delay in frames (0.2 applies frame t's gains to frame t-1).
pub const LATENCY_FRAMES: usize;          // measured by a test, see 6.4
pub enum Model { Full, Little }
pub struct DenoiseState { .. }            // no allocation after construction
impl DenoiseState {
  pub fn new() -> Box<Self>;              // Model::Full
  pub fn with_model(model: Model) -> Box<Self>;
  /// Input and output in the RNNoise scale (i16 range as f32, like nnnoiseless); returns VAD.
  pub fn process_frame(&mut self, output: &mut [f32], input: &[f32]) -> f32;
  pub fn reset(&mut self);
}
```

Váhy obou modelů vložené přes `include_bytes!` (audiodg nesmí číst z disku), blob zparsovaný
jednou při konstrukci do tabulek s pevnou velikostí; chyba v blobu = `Result::Err` při konstrukci,
nikdy panic. Pokud int8 pole v `rnnoise_data.c` nemají `scale[]` pro každý výstup, dopočítat ve
skriptu stejně jako upstream `dump_rnnoise_weights.py` a ověřit referenčním testem.

## 5. Postup

1. **Toolchain na Macu**: po souhlasu uživatele `rustup` (stable). Ověřit
   `cargo test --manifest-path crates/vendor/deep_filter/Cargo.toml --no-run` neběží (ten crate
   závisí na tractu, nemusí jít; stačí, že `cargo` funguje).
2. **Upstream**: stáhnout tarball modelu a zdroje `xiph/rnnoise` tag `v0.2` do `.claude/tmp/`.
   Z `rnnoise_data.h` vypsat všechny rozměry do README crate (tabulka). Diffnout `denoise.c`
   `v0.1.1` → `v0.2` a doplnit tabulku v kapitole 3, pokud se něco liší od toho, co je tam napsáno.
3. **Převod vah**: `crates/vendor/rnnoise2/tools/convert_weights.py` (Python 3, jen stdlib):
   parsuje C pole z `rnnoise_data.c` / `rnnoise_data_little.c` a zapíše
   `models/rnnoise_full.bin`, `models/rnnoise_little.bin` v upstream formátu záznamů (kapitola 3),
   little endian, plus `.sha256`. Skript je deterministický; test v Rustu ověří hash vložených
   blobů. Int8 varianty (menší; DLL dnes nese DFN3 i DPDFNet, velikost blobů zapsat do README).
4. **Port** po souborech, v tomto pořadí, každý krok s testem (kapitola 6):
   1. `weights.rs`: parser blobu (záznamy, vyhledání podle jména, kontroly velikostí jako
      `find_array_check`), bez `unsafe`, bez alokací po konstrukci.
   2. `nnet.rs`: dense (plná i řídká int8 se škálou), conv1d kauzální se stavem, GRU podle tabulky.
      Jednotkové testy proti ručně spočítaným malým maticím.
   3. `bands.rs` / `features.rs`: `eband20ms`, energie a korelace pásem, interpolace zisků, nové
      příznaky. Test: energie pásem bílého šumu se sečtou na energii spektra (do 1 %).
   4. `denoise.rs`: pipeline 0.2 včetně `delayed_X`, podlahy 0,6 a VAD v tichu.
   5. `lib.rs`: rozhraní z kapitoly 4.
5. **Referenční test** (6.2) a **měření** (6.3, 6.4). Teprve pak README a commit.

## 6. Testy

Všechny běží na Macu v crate, bez privátních nahrávek. Fixtury ze sdíleného `test_data/denoise/`
(`speech_clean.wav`, ESC-50 klipy: `*-31.wav` kliky myši, `*-32.wav` psaní na klávesnici).

### 6.1 Jednotkové

Parser blobu (správný blob, useknutý blob, špatné jméno, špatná velikost → `Err`), dense/řídká
dense/conv1d/GRU na malých maticích, pásma (součet energií), hash vložených blobů.

### 6.2 Referenční výstup upstreamu (rozhodující)

Jednou, mimo repozitář, vyrobit výstupy originálu 0.2 pro tři krátké vstupy (3–5 s, 48 kHz,
16 bit): čistá řeč, řeč + klávesnice (−10 dB SNR), samotné kliky myši. Dvě cesty, první, která
funguje:

- venv v `.claude/tmp/rnnoise2/venv/` s balíkem `pyrnnoise` (binding na upstream 0.2); nebo
- upstream `examples/rnnoise_demo` přeložený clangem v `.claude/tmp/rnnoise2/upstream/`
  (jediné použití C, mimo repozitář, nic z toho se necommituje).

Vstupy i výstupy uložit do `test_data/denoise/rnnoise2_reference/` (celkem pod 2 MB, jinak
zkrátit). Test `reference_matches_upstream`: port na stejných vstupech, po zarovnání latence
maximální absolutní odchylka ≤ 1e-3 plné škály a RMS odchylka ≤ 1e-4 pro oba modely. Pokud
odchylka vyjde větší a soustavná, hledat chybu v portu, ne zvolňovat práh; výjimka jen po
zdůvodnění v README (např. jiné pořadí sčítání ve FFT).

### 6.3 Kvalita vůči staré síti

Test `--ignored` `rnnoise2_report`, tisk tabulky starý nnnoiseless × `Full` × `Little` na
sdílených fixturách (nnnoiseless jako dev-dependency jen pro tento report):

| Metrika                                                     | Jak                                                             |
| ----------------------------------------------------------- | --------------------------------------------------------------- |
| Potlačení v pauzách [dB]                                    | energie výstupu vs. vstupu v rámcích bez řeči (klávesy, kliky)  |
| Ztráta řeči [dB] a podíl rámců řeči ztišených > 6 dB        | řeč + klávesnice vs. čistá řeč, rámce s řečí                    |
| Tiché slabiky: podíl rámců řeči pod −35 dBFS ztišených > 10 dB | totéž, jen tiché rámce                                        |
| VAD: pravděpodobnost ≥ 0,5 na samotných klávesách [%]       | detektor `in_net` dnes na starém RNNoise nikdy nevystřelí       |
| VAD: pravděpodobnost ≥ 0,5 na řeči pod klávesami [%]        | čím víc, tím lepší „ucho" pro bránu                             |

Metriky počítat stejně jako `measure` / `run_clicks` v `ai_denoiser.rs` (přečíst a převzít
definice, ne vymýšlet nové), aby šla čísla porovnat s DEVLOGem (milestone 27, 30, 34, 35).

### 6.4 Latence a výkon

- `latency_matches_constant`: impulz skrz `process_frame`, zpoždění v rámcích == `LATENCY_FRAMES`.
- `--ignored` `rnnoise2_bench`: ns na rámec pro starý nnnoiseless, `Full`, `Little` (release,
  průměr z 10 s zvuku). Zapsat do README; na Macu je to jen poměr, absolutní čísla doměří Windows.

## 7. Rozhodnutí

Knihovna je hotová, když: referenční test projde pro oba modely, žádné alokace v `process_frame`
(test s vlastním alokátorem počítajícím alokace, nebo aspoň review), linty čisté, README s původem,
hashi, rozměry sítě, velikostí blobů a tabulkami z 6.3 a 6.4.

Pokračuje se do fáze 2, pokud `Full` nebo `Little` proti starému nnnoiseless: potlačení v pauzách
lepší a ztráta řeči ne horší, VAD na samotných klávesách ≤ starý. Pokud je 0.2 na klávesách horší
nebo několikanásobně dražší bez zisku, knihovna zůstává v repozitáři s čísly v README a DEVLOGu
a do řetězce se nezapojí.

## 8. Fáze 2: `in_net` na RNNoise 0.2 (provedeno, ověření na Windows čeká)

Brána z kapitoly 7 neprošla pro filtr: `Full` sice v pauzách tlumí klávesy zhruba dvakrát víc v dB,
ale ořezává tiché začátky a konce slabik (30–37 % vs 9 %) a pod klávesami ztrácí o něco víc řeči;
upstream C dělá totéž, je to vlastnost modelu, ne portu. Jako detektor hlasu je ale výrazně lepší
(README, „Voice detector operating points"): při stejném podílu falešných poplachů na samotných
klávesách slyší 94–97 % řečových rámců místo 76–82 % a 31–38 % tichých místo 10–12 %. Pravidlo
„nikdy neuříznout řeč, raději pustit klávesu" proto rozhodlo o jediné roli.

Co se změnilo (`crates/gamersequalizer-mic-vst/src/ai_denoiser.rs`, DEVLOG milestone 35):

- `in_net` (detektor na surovém vstupu: klíčuje `vad` a bránu, když běží těžký model, a leveler
  s automatickým ziskem při vypnutém odšumění) běží na `rnnoise2::Model::Full` přes
  `voice_probability` (analýza + síť, bez syntézy). `VoiceNet` drží jednu ze dvou sítí
  (`VoiceModel::Old` / `New`). Selže-li načtení vah, `in_net` to jednou zaloguje a použije starou
  síť se starými prahy.
- Prahy: na pravděpodobnosti `in_net` otevírá `vad` při `IN_NET_VOICE_PROB = 0.7` a drží při
  `IN_NET_KEEP_PROB = 0.2` (samotné klávesy nad nimi 0,5 % / 5,2 % rámců; stará síť při 0,5 / 0,05:
  0,4 % / 7,8 %). Na vlastní pravděpodobnosti filtru RNNoise (běží sám nebo zaskakuje za spící
  model) zůstávají `VOICE_PROB` 0,5 / `VOICE_KEEP_PROB` 0,05. `min_frames`/`active_frames` beze změny.
- Časování: pravděpodobnost 0.2 popisuje předchozí rámec (síť má jeden rámec lookahead), takže
  `in_net` hlásí o 10 ms později. Výstup těžkého modelu pořád předbíhá o jeho latenci − 10 ms
  (DFN3 30 ms, DPDFNet 50 ms) plus lookahead brány; latence výstupu se nemění. Hold brány se
  počítá od hlášení, končí tedy o 10 ms později (ve prospěch řeči); leveler posune řečové rámce
  o jeden rámec (v rámci 200 ms hystereze detektoru); probuzení spícího modelu `in_net` nepoužívá.
  Nic se neupravovalo.
- Cena: jeden průchod `Full` je asi 10× starý (0,26 % jádra → odhad 2,8 % na Windows), jen když je
  těžký model vzhůru nebo je odšumění obejité.

Co zůstává na nnnoiseless a proč: filtr a záskok v pauzách (ořezávání tichých slabik, viz výše),
`out_net` a `ClickRescue::net` (prahy `OUT_VOICE_*` a `RESCUE_*` jsou naměřené na staré síti, jejich
zarovnání s výstupem počítá s jejím časováním, a tři průchody `Full` by stály ~8 % jádra).
`RNNOISE_LATENCY_SAMPLES` a konstanty lookaheadu beze změny.

Ověření na Windows (na Macu se crate mikrofonu nepřeloží):

1. `scripts/dev-rebuild.ps1 -Apo -Test` (build, testy včetně `test_in_net_rarely_fires_on_keys_alone`
   a `test_in_net_hears_speech_under_keys`, clippy).
2. `cargo test -p gamersequalizer-mic-vst --release keyboard_call_report -- --ignored --nocapture`,
   stejně `click_report`, `private_report`, `scenario_report` a `lookahead_sweep` (lookahead byl
   laděný se starým časováním `in_net`); porovnat s DEVLOG milestone 34.
3. CPU v aplikaci s DFN3/DPDFNet vzhůru (očekáváno +~2,5 % jádra).
4. Poslech: řeč pod psaním s DFN3 a DPDFNet, tiché začátky slov, psaní bez řeči (brána).

## 9. Rizika

- Rozměry sítě a přesný počet příznaků nejsou nikde publikované; kapitola 3 je má jako „dočíst".
  Kdyby se ukázalo, že 0.2 změnila i hledání výšky nebo okno, port je o soubor větší, ne jiný.
- `pyrnnoise` nemusí mít wheel pro Mac; pak cesta přes `rnnoise_demo` (clang je na Macu v
  Xcode CLT).
- Velikost DLL: dva bloby navíc. Pokud by `Full` byl přes ~5 MB, vložit jen ten model, který
  vyhraje ve fázi 2.
- Latence RNNoise roste o jeden rámec (10 ms); záskok v pauzách to kryje `sleep_delay`, filtr
  volený uživatelem to pocítí jako +10 ms.

## 10. Zdroje

- xiph/rnnoise: `README`, `src/rnn.c`, `src/nnet.h`, `src/nnet.c`, `src/nnet_arch.h`,
  `src/parse_lpcnet_weights.c`, `download_model.sh`, `model_version`, `COPYING`;
  `src/denoise.c` v tazích `v0.1.1` a `v0.2`; release 0.2: github.com/xiph/rnnoise/issues/221.
- jneem/nnnoiseless 0.5.2 (BSD-3-Clause): `src/{denoise,features,pitch,rnn,util}.rs`,
  `weights.rnn`; issue 33 (0.2 nepodporováno, autor: „jiná síť, ne jen formát").
- Naše měření starého RNNoise: DEVLOG milestone 27, 30, 34, 35; `ai_denoiser.rs` konstanty
  `VOICE_PROB`, `OUT_VOICE_PROB`, `RNNOISE_LATENCY_SAMPLES`.
