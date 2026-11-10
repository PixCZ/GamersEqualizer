# AEC3: návod na doladění podle nahrávek

Pro agenta (Antigravity) na Windows stroji, kde jsou `test_data/private/captures/`.
Souvislosti: `docs/mic-aec3-voice-analysis.md` (proč), DEVLOG milník 43 (co se změnilo).
Příkazy jsou pro PowerShell, spouštěj je z kořene repa.

## 1. Stav: co je hotové a co není ověřené

| Část                                                        | Stav                                                    |
| ----------------------------------------------------------- | ------------------------------------------------------- |
| `aec3.rs`: zesílení mikrofonu do AEC3 (`set_mic_gain_db`)   | přeloženo a otestováno na Macu, jen na syntetice        |
| `aec3.rs`: testy a `capture_tuning_report`                  | běží na Macu na dvou umělých „captures"                 |
| `plugin.rs`: předání zesílení z leveleru do AEC (2 řádky)   | **psáno bez překladače**                                |
| `voice_leveler.rs`: `gain_to_default_db`                    | přeloženo na Macu                                       |
| `ai_denoiser.rs`: `AEC_WAV` v `capture_replay_report`       | **psáno bez překladače**                                |
| Cokoli na skutečných nahrávkách nebo naživo                 | **neověřeno**                                           |

Co změna dělá: AEC3 má absolutní prahy úrovně a tichý mikrofon ho drží mimo lineární filtr. Pak
tlumí hlas podle hlasitosti reference. Mikrofon se proto před AEC3 zesílí a za ním stejně
zeslabí. Zesílení je to, co naučené úrovni řeči chybí do -24 dBFS (výchozí cíl leveleru),
v krocích po 6 dB (0-24 dB). S vypnutým levelerem nebo před naučením úrovně je 0.

## 2. Přeložit a pustit testy

```powershell
cargo test --release -p gamersequalizer-mic-vst --lib aec3:: -- --nocapture
cargo clippy --workspace --all-targets
```

Čekaný výsledek: 14 testů projde, 3 jsou `ignored`. Hodnoty na Macu:
`quiet mic, no gain` hlas -5,4 dB a pásmo 4-8 kHz -19 dB; `quiet mic, 24 dB` -1,6 a -2,4 dB.

Když se nepřeloží `plugin.rs`: záměr je jen tento a nic dalšího se měnit nemá. Před
`self.run_aec(...)` se každý blok zavolá

```rust
let aec_mic_gain_db = self.leveler.gain_to_default_db().filter(|_| leveler_on).unwrap_or(0.0);
self.aec.set_mic_gain_db(aec_mic_gain_db);
```

`gain_to_default_db` je nový getter ve `voice_leveler.rs`: `DEFAULT_TARGET_DB` mínus naučená
úroveň mínus vstupní zisk, `None` před naučením.

## 3. Základní měření

```powershell
$env:VARIANTS = "pregain=0;;open;pregain=0 open"
cargo test --release -p gamersequalizer-mic-vst --lib capture_tuning_report -- --ignored --nocapture
```

`CAP` omezí výběr (`$env:CAP = "20261109-130319,20261109-210655"`); bez něj jdou všechny captures.
Varianty odděluje `;`. Prázdná varianta je AEC3 tak, jak je v kódu. `pregain=0` je chování před
touto změnou. Bez `pregain=` se zesílení počítá z `vars.tsv` nahrávky stejně jako v řetězu
(sloupce `leveler_level_db`, `input_gain_db`, `leveler_on`).

Sloupce (okna 100 ms od 3. sekundy, tříděná Silerem na surovém mikrofonu):

| Sloupec | Význam                                                               | Lepší je    |
| ------- | -------------------------------------------------------------------- | ----------- |
| `gain`  | zesílení mikrofonu na konci (dB)                                     | -           |
| `echo`  | o kolik dB ubylo echo, když hraje samo (medián oken)                 | víc         |
| `left`  | hladina výstupu, kterou překročí jen každé desáté okno s echem (dBFS)| níž         |
| `voice` | průměrná ztráta úrovně v oknech s hlasem, mic -> výstup (dB)         | míň         |
| `>6dB`  | podíl oken s hlasem, která ztratila přes 6 dB (%)                    | míň         |
| `>10dB` | totéž přes 10 dB (%)                                                 | míň         |
| `onset` | ztráta v prvním okně slova po pauze (dB)                             | míň         |
| `4-8k`  | ztráta pásma 4-8 kHz v oknech s hlasem (dB)                          | míň         |
| `ERLE`  | vlastní odhad AEC3, když hraje jen echo (dB)                         | viz níže    |

Okna s hlasem obsahují i echo, takže `voice` nikdy není 0. Porovnávej varianty mezi sebou.
Řádek `open` (průhledný potlačovač) ukazuje, co dělá lineární filtr sám: jeho `voice` je spodní
mez ztráty a jeho `echo` je skutečné lineární ERLE.

Jak to vyšlo na syntetice (tichý mikrofon, echo -68 dBFS, hlas -46 dBFS), pro srovnání:

| Varianta    | gain | echo | left  | voice | >6dB | 4-8k | ERLE |
| ----------- | ---- | ---- | ----- | ----- | ---- | ---- | ---- |
| `pregain=0` | 0    | 13,3 | -79,5 | 6,2   | 37   | 16,8 | 0,2  |
| (as built)  | 24   | 14,4 | -80,3 | 1,2   | 2    | 2,8  | 7,9  |
| `open`      | 24   | 5,5  | -71,4 | 0,4   | 2    | 0,4  | 7,9  |

## 4. Co z prvního měření vyčíst

Zapiš tabulku pro každou nahrávku a odpověz na tyto otázky:

1. **Pomáhá zesílení?** Porovnej `pregain=0` a `(as built)` u nahrávek, kde `gain` > 0.
   Čekání: `voice`, `>6dB` a `4-8k` klesnou, `echo` a `left` se nezhorší o víc než 2 dB.
   `ERLE` u `pregain=0` kolem 0 a u `(as built)` několik dB potvrzuje příčinu z analýzy (3.1).
2. **Kde je `gain` 0?** Leveler byl vypnutý, neměl naučenou úroveň, nebo řeči chybí do -24 dBFS
   méně než 4,5 dB. Tam se nic nemění a je to správně.
3. **Kolik umí lineární filtr?** `echo` v řádku `open`:

| `open`: `echo`                     | Co to znamená                           | Další krok             |
| ---------------------------------- | --------------------------------------- | ---------------------- |
| blízko `echo` řádku `(as built)`   | potlačovač nepřidává nic, jen bere hlas | krok 5: `erle`, `masks`|
| 5-12 dB, `(as built)` výrazně víc  | filtr omezuje dozvuk nebo reference     | krok 5: `len`, pak `erle` |
| kolem 0 dB i se zesílením          | AEC3 filtr nepoužívá                    | zastav se, viz kapitola 9 |

Když zesílení na některé nahrávce **škodí** (víc echa v `left` o 3 dB a víc, nebo horší `voice`),
zapiš kterou a proč, a nepokračuj laděním dalších variant, dokud to není vysvětlené.

## 5. Varianty k vyzkoušení

Klíče (lze kombinovat mezerou, definice v `aec3::tests::apply_variant`):

| Klíč           | Co mění                                                        | Výchozí |
| -------------- | -------------------------------------------------------------- | ------- |
| `erle=<a>,<b>` | strop odhadu ERLE pod 4 kHz / nad 4 kHz (`erle.max_l`, `max_h`) | 4,1.5   |
| `hf=0`         | biny nad 2 kHz už nenásledují zisk binu na 2 kHz               | 1       |
| `len=<bloky>`  | délka lineárního filtru, 1 blok = 4 ms                         | 13      |
| `masks=<k>`    | prahy potlačovače krát k (větší = měkčí)                       | 1       |
| `gain=<x>`     | záložní zisk echa (`default_gain`), jen když filtr neběží      | 0.01    |
| `open`         | potlačovač průhledný (jen měření)                              | -       |
| `pregain=<dB>` | pevné zesílení místo hodnoty z nahrávky                        | -       |

Pořadí:

```powershell
$env:VARIANTS = ";erle=10,4;erle=30,10;erle=100,30;hf=0;erle=30,10 hf=0;erle=100,30 hf=0"
cargo test --release -p gamersequalizer-mic-vst --lib capture_tuning_report -- --ignored --nocapture

$env:VARIANTS = ";len=20;len=32;len=32 erle=30,10 hf=0;len=32 open"
cargo test --release -p gamersequalizer-mic-vst --lib capture_tuning_report -- --ignored --nocapture

$env:VARIANTS = ";masks=2;masks=4;erle=30,10 hf=0 masks=2"
cargo test --release -p gamersequalizer-mic-vst --lib capture_tuning_report -- --ignored --nocapture
```

Co čekat podle syntetiky (skutečné repráky budou horší, echo je tam i nelineární):

- `erle`: pomáhá tam, kde lineární filtr ubírá hodně. Krátká místnost, filtr sám -40 dB:
  hlas -7,8 dB -> -4,6 (`30,10`) -> -4,1 (`100,30`), echo o 1-3 dB horší. Dlouhá místnost
  (filtr -11,6 dB): jen -3,2 -> -2,6 dB.
- `hf=0`: pásmo 4-8 kHz o 1-2 dB lepší, echo beze změny.
- `len=32`: echo -21 -> -32 dB v místnosti s dozvukem 200 ms. Delší filtr se učí pomaleji:
  hlídej `onset` a první sekundy po změně hlasitosti repráků.
- Víc zesílení, než žádá leveler (`pregain=24` všude), **škodí**: řeč nad asi -15 dBFS uvnitř
  AEC3 se tlumí víc (hlas -33 dBFS: s 12 dB -5,6 dB, s 24 dB -8,5 dB). Nezkoušej zvedat `MAX_MIC_GAIN_DB`.

## 6. Kritéria

Vyhrává varianta, která na reproduktorových nahrávkách (`122712`, `125717`, `130319`, `205013`,
`210655`) sníží `>6dB`, `>10dB`, `onset` a `4-8k`, a přitom:

- `left` nestoupne o víc než 3 dB proti `(as built)` a zůstane pod -60 dBFS,
- `echo` neklesne o víc než 3 dB,
- na sluchátkových nahrávkách (`115931`, `122520`) se `voice` nezhorší,
- projde `cargo test --release -p gamersequalizer-mic-vst --lib aec3::`.

Pravidlo projektu: pár propuštěných zvuků je lepší než jedno useknuté písmeno. Platí pro hudbu
a hru. **Neplatí pro řeč z reproduktorů** (spoluhráči by slyšeli sami sebe). Takovou nahrávku
zatím nemáme, viz krok 8.

## 7. Celý řetěz (AEC + denoiser)

Zbytek echa po AEC3 má odstranit denoiser a gate. Ověření vítězné varianty:

```powershell
$env:CAP = "20261109-130319"; $env:WAV = "1"; $env:VARIANTS = ";erle=30,10 hf=0"
cargo test --release -p gamersequalizer-mic-vst --lib capture_tuning_report -- --ignored --nocapture
# vznikne test_data/private/aec_ab/20261109-130319_v0.wav (as built) a _v1.wav (varianta)
Remove-Item Env:WAV; Remove-Item Env:VARIANTS
$env:AEC_WAV = "20261109-130319_v1.wav"
cargo test --release -p gamersequalizer-mic-vst capture_replay_report -- --ignored --nocapture
Remove-Item Env:AEC_WAV
```

Druhý příkaz pustí i `aec3::tests::capture_replay_report`; důležitý je řádek `RESULT` z
`ai_denoiser`: „loud windows without voice ... removed" nesmí klesnout a „voice windows ... mean
loss" má klesnout. Stejné srovnání udělej pro `_v0.wav`. WAV soubory si také poslechni.

## 8. Zapsat výsledek

1. Vítězné hodnoty do `tuning()` v `crates/gamersequalizer-mic-vst/src/aec3.rs`, například:

   ```rust
   cfg.erle.max_l = 30.0;
   cfg.erle.max_h = 10.0;
   cfg.suppressor.high_frequency_suppression.bands_in_limiting_gain = 0;
   cfg.filter.refined.length_blocks = 32; // pak i cfg.filter.coarse.length_blocks a FILTER_TAPS
   ```

2. Do komentáře nad `tuning()` čísla z nahrávek (která nahrávka, před / po), ve stylu, jakým tam
   jsou dosavadní.
3. Pustit všechny testy `aec3::`. Když se změní čísla v `eprintln!` u starších testů, opravit je
   v DEVLOGu, ne prahy v testech, pokud test dál prochází.
4. Nový milník do `DEVLOG.md`: tabulky z kroků 3 a 5, co bylo zvoleno a proč, co zamítnuto.
5. Naživo to musí ověřit člověk (APO se instaluje s právy správce):
   `scripts/dev-rebuild.ps1 -Apo -Test -Start`, pak instalace APO z banneru aplikace.

Co naživo poslouchat a nahrát (`-Apo -DebugCapture`, nikdy takové APO nebalit):

- **Řeč z reproduktorů** (podcast nebo hovor), střídavě ticho / mluvím já / obojí. Na výstupu
  nesmí být srozumitelná cizí řeč. Je to chybějící případ.
- **Skok zesílení**: v logu řádky `[AEC3] mic gain X -> Y dB`. Každý skok znamená 9 ms ticha
  a AEC3 se učí znovu. Má být jeden v první půlvteřině streamu a pak pár po „Učit znovu",
  ne během hovoru. Když cvakají ve slově, zvětši `MIC_GAIN_LATCH_FRAMES` nebo
  `MIC_GAIN_HYSTERESIS_DB`. Skok také alokuje v audio vlákně: hlídej pozdní bloky v `[Chain]` logu.
- **Výkřik** při tichém mikrofonu: nesmí být oříznutý ani zkreslený.
- Sluchátka s hudbou a reproduktory s hudbou jako v milníku 36.

## 9. Návrat zpět a kdy se zastavit

- Vypnutí celé změny: `MAX_MIC_GAIN_DB = 0.0` v `aec3.rs`. Chování je pak bit po bitu jako dřív
  (testy `test_mic_gain_*` a `test_quiet_mic_*` pak upravit nebo smazat).
- Když `open` ukáže lineární ERLE kolem 0 dB i se zesílením, AEC3 filtr nepoužívá z jiného
  důvodu. Neladit dál potlačovač. Zapsat `ERLE`, `gain` a nahrávku do DEVLOGu; další kandidáti
  jsou v analýze: mono reference (3.6, C2) a transparent mode.
- Když filtr umí jen 5-10 dB a `len` nepomáhá, je na řadě stereo reference (analýza C2).
  To je větší změna (sdílená paměť, debug capture) a rozhoduje o ní člověk.

## 10. Co nedělat

- `default_gain` pod 0.01, širší masky near-endu, snazší detekce near-endu: zkoušeno, milníky 36 a 42.
- Vypínat AEC tak, že se změní latence (cvakne).
- Měnit `delay.default_delay` nebo `use_external_delay_estimator`: na syntetice bez účinku,
  `statistics().delay_ms` je 0 od začátku.
- Ladit podle syntetických testů. Jsou to regresní testy; hodnoty se volí podle nahrávek.
- Balit APO postavené s `-DebugCapture`.
