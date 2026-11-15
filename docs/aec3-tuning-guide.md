# AEC3: návod na doladění podle nahrávek

Pro agenta (Antigravity) na Windows stroji, kde jsou `test_data/private/captures/`.
Souvislosti: `docs/mic-aec3-voice-analysis.md` (proč), DEVLOG milníky 43-45 (co se změnilo).
Kapitoly 2-10 jsou hotové (milník 44). Aktuální úkol je **kapitola 11** (síla echa, milník 45).
Příkazy jsou pro PowerShell, spouštěj je z kořene repa.

## 1. Stav: co je hotové a co není ověřené

| Část                                                        | Stav                                             |
| ----------------------------------------------------------- | ------------------------------------------------ |
| `aec3.rs`: zesílení mikrofonu do AEC3 (`set_mic_gain_db`)   | přeloženo a otestováno na Macu, jen na syntetice |
| `aec3/tests.rs`: testy a `capture_tuning_report`            | běží na Macu na dvou umělých „captures"          |
| `chain.rs`: předání zesílení z leveleru do AEC (2 řádky)    | **psáno bez překladače**                         |
| `voice_leveler.rs`: `gain_to_default_db`                    | přeloženo na Macu                                |
| `ai_denoiser/tests.rs`: `AEC_WAV` v `capture_replay_report` | **psáno bez překladače**                         |
| Cokoli na skutečných nahrávkách nebo naživo                 | **neověřeno**                                    |

Co změna dělá: AEC3 má absolutní prahy úrovně a tichý mikrofon ho drží mimo lineární filtr. Pak
tlumí hlas podle hlasitosti reference. Mikrofon se proto před AEC3 zesílí a za ním stejně
zeslabí. Zesílení je to, co naučené úrovni řeči chybí do -24 dBFS (výchozí cíl leveleru),
v krocích po 6 dB (0-24 dB). S vypnutým levelerem nebo před naučením úrovně je 0.

## 2. Přeložit a pustit testy

```powershell
cargo test --release -p gamersequalizer-mic-chain --lib aec3:: -- --nocapture
cargo clippy --workspace --all-targets
```

Čekaný výsledek: 14 testů projde, 3 jsou `ignored`. Hodnoty na Macu:
`quiet mic, no gain` hlas -5,4 dB a pásmo 4-8 kHz -19 dB; `quiet mic, 24 dB` -1,6 a -2,4 dB.

Když se nepřeloží `chain.rs`: záměr je jen tento a nic dalšího se měnit nemá. Před
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
cargo test --release -p gamersequalizer-mic-chain --lib capture_tuning_report -- --ignored --nocapture
```

`CAP` omezí výběr (`$env:CAP = "20261109-130319,20261109-210655"`); bez něj jdou všechny captures.
Varianty odděluje `;`. Prázdná varianta je AEC3 tak, jak je v kódu. `pregain=0` je chování před
touto změnou. Bez `pregain=` se zesílení počítá z `vars.tsv` nahrávky stejně jako v řetězu
(sloupce `leveler_level_db`, `input_gain_db`, `leveler_on`).

Sloupce (okna 100 ms od 3. sekundy, tříděná Silerem na surovém mikrofonu):

| Sloupec | Význam                                                               | Lepší je    |
| ------- | -------------------------------------------------------------------- | ----------- |
| `gain`  | zesílení mikrofonu na konci (dB)                                     | -           |
| `strength` | změřená síla echa: kolik dB pod referencí je echo na mikrofonu (`<`: jen horní mez, echo je v šumu) | - |
| `fallb` | záložní odhad echa v AEC3 na konci (dB pod referencí, -40 = beze změny) | -        |
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

| Klíč           | Co mění                                                         | Výchozí |
| -------------- | --------------------------------------------------------------- | ------- |
| `erle=<a>,<b>` | strop odhadu ERLE pod 4 kHz / nad 4 kHz (`erle.max_l`, `max_h`) | 30,10   |
| `hf=0`         | biny nad 2 kHz už nenásledují zisk binu na 2 kHz                | 0       |
| `len=<bloky>`  | délka lineárního filtru, 1 blok = 4 ms                          | 13      |
| `masks=<k>`    | prahy potlačovače krát k (větší = měkčí)                        | 1       |
| `gain=<x>`     | záložní zisk echa (`default_gain`), jen když filtr neběží       | 0.01    |
| `open`         | potlačovač průhledný (jen měření)                               | -       |
| `pregain=<dB>` | pevné zesílení místo hodnoty z nahrávky                         | -       |

Sloupec Výchozí platí od milníku 44: `tuning()` má `erle` 30,10 a `hf` 0. Samotné AEC3 má 4,1.5 a 1;
s těmi hodnotami se ladění v milníku 44 začínalo.

Pořadí:

```powershell
$env:VARIANTS = ";erle=10,4;erle=30,10;erle=100,30;hf=0;erle=30,10 hf=0;erle=100,30 hf=0"
cargo test --release -p gamersequalizer-mic-chain --lib capture_tuning_report -- --ignored --nocapture

$env:VARIANTS = ";len=20;len=32;len=32 erle=30,10 hf=0;len=32 open"
cargo test --release -p gamersequalizer-mic-chain --lib capture_tuning_report -- --ignored --nocapture

$env:VARIANTS = ";masks=2;masks=4;erle=30,10 hf=0 masks=2"
cargo test --release -p gamersequalizer-mic-chain --lib capture_tuning_report -- --ignored --nocapture
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
- projde `cargo test --release -p gamersequalizer-mic-chain --lib aec3::`.

Pravidlo projektu: pár propuštěných zvuků je lepší než jedno useknuté písmeno. Platí pro hudbu
a hru. **Neplatí pro řeč z reproduktorů** (spoluhráči by slyšeli sami sebe). Takovou nahrávku
zatím nemáme, viz krok 8.

## 7. Celý řetěz (AEC + denoiser)

Zbytek echa po AEC3 má odstranit denoiser a gate. Ověření vítězné varianty:

```powershell
$env:CAP = "20261109-130319"; $env:WAV = "1"; $env:VARIANTS = ";erle=30,10 hf=0"
cargo test --release -p gamersequalizer-mic-chain --lib capture_tuning_report -- --ignored --nocapture
# vznikne test_data/private/aec_ab/20261109-130319_v0.wav (as built) a _v1.wav (varianta)
Remove-Item Env:WAV; Remove-Item Env:VARIANTS
$env:AEC_WAV = "20261109-130319_v1.wav"
cargo test --release -p gamersequalizer-mic-chain capture_replay_report -- --ignored --nocapture
Remove-Item Env:AEC_WAV
```

Druhý příkaz pustí i `aec3::tests::capture_replay_report`; důležitý je řádek `RESULT` z
`ai_denoiser`: „loud windows without voice ... removed" nesmí klesnout a „voice windows ... mean
loss" má klesnout. Stejné srovnání udělej pro `_v0.wav`. WAV soubory si také poslechni.

## 8. Zapsat výsledek

1. Vítězné hodnoty do `tuning()` v `crates/gamersequalizer-mic-chain/src/aec3.rs`, například:

   ```rust
   cfg.erle.max_l = 30.0;
   cfg.erle.max_h = 10.0;
   cfg.suppressor.high_frequency_suppression.bands_in_limiting_gain = 0;
   cfg.filter.refined.length_blocks = 32; // pak i cfg.filter.coarse.length_blocks
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

## 11. Síla echa (milník 45)

Co přibylo: AEC3 dosud předpokládalo, že echo je o 40 dB slabší než reference. Teď se síla echa
měří (`EchoMeter` v `aec3.rs`), pamatuje pro dvojici mikrofon + výstup a záložní odhad AEC3 ji
následuje, když je echo silnější. Všechno se vypisuje do debug logu. Sdílená paměť má layout
v18, takže **APO je potřeba přeinstalovat**.

| Část                                                     | Stav                                      |
| -------------------------------------------------------- | ----------------------------------------- |
| `aec3.rs`, `aec3/tests.rs` (měřič, záložní odhad, testy) | přeloženo a otestováno na Macu            |
| `shm.rs` (5 nových polí, v18)                            | přeloženo a otestováno na Macu            |
| `chain_log.rs` (řádky logu, test)                        | čistá část přeložena a otestována na Macu |
| `chain.rs`, `apo_link.rs`, `aec_delay_cache.rs`          | **psáno bez překladače**                  |

### 11.1 Přeložit

```powershell
cargo test --release -p gamersequalizer-mic-chain --lib aec3:: -- --nocapture
cargo test -p gamersequalizer-common
cargo test -p gamersequalizer-daemon chain_log
cargo clippy --workspace --all-targets
```

Čekaný výsledek: 16 testů `aec3::` projde (3 `ignored`). Když se nepřeloží soubory psané naslepo,
záměr je:

- `chain.rs`: číst `aec_cached_echo_db_x10` z hlavičky; když se změní a není 0, zavolat
  `self.aec.seed_echo_strength_db(x10 / 10)`; při přepnutí výstupu (`reference_seq`) zavolat
  `self.aec.forget_echo_strength()` (před `self.aec.reset()`) a vynulovat `cached_echo_seen`;
  v `publish_telemetry` zapsat `aec_echo_db_x10` (0 = nic, změřená hodnota záporná a nejvýš -1,
  horní mez kladná), `aec_fallback_echo_db_x10`, `aec_mic_gain_db_x10`.
- `apo_link.rs`: při změně klíče (mikrofon|výstup) načíst hodnotu z `echo_strength_cache.json`,
  zapisovat ji do `aec_cached_echo_db_x10` vedle `aec_cached_delay_samples` (před zvýšením
  `aec_reference_seq`). Když je AEC `Locked` a od posledního zápisu uběhlo 10 s: zápornou
  (změřenou) hodnotu uložit, pokud se pohnula o 2 dB; při kladné (mez) zapamatovanou smazat.

### 11.2 Co měřič čte na nahrávkách

**Celý postup testu na nahrávkách, včetně nasimulovaného silného echa, je v
`docs/aec3-echo-strength-test.md`. Postupuj podle něj;** zbytek této podkapitoly je jen přehled.

```powershell
$env:VARIANTS = ""
cargo test --release -p gamersequalizer-mic-chain --lib capture_tuning_report -- --ignored --nocapture
```

Do DEVLOGu tabulku: nahrávka, `strength`, `fallb`, a pro kontrolu úroveň echa a reference
z dřívějších milníků. Čekání:

| Nahrávka                  | Čekaná `strength`                    | Čekaný `fallb` |
| ------------------------- | ------------------------------------ | -------------- |
| `130319` (reproduktory)   | kolem -37                            | -40            |
| `210655` (reproduktory)   | kolem -50                            | -40            |
| `115931`, `122520` (sluchátka) | mez (`<...`)                    | -40            |
| nahrávky bez reference    | `-`                                  | -40            |

Když je `fallb` u některé nahrávky jiný než -40, změní se její čísla proti milníku 44: zapiš
oba řádky a rozhodni podle kritérií z kapitoly 6. Když měřič čte něco výrazně jiného, než říká
úroveň echa v pauzách (rozdíl přes 5 dB), zastav se a zapiš to. Nelaď konstanty měřiče.

Sloupec ukazuje hodnotu na konci nahrávky. Replay startuje bez zapamatované hodnoty, takže
u silného echa obsahuje jednu přestavbu AEC3 uprostřed (naživo jen v první relaci na daném výstupu).

### 11.3 Naživo a z logu

Na stránce Log zapnout „Ukládat debug na disk". Stačí obyčejný build, nahrávka není potřeba.
V logu hledat:

```text
[Chain] start: ... echo -43.5 dB under the reference (AEC fallback echo -40.0 dB, AEC mic gain +18.0 dB) ...
[Chain] changed: echo not measured -> -21.5 dB under the reference, AEC fallback echo -40.0 -> -22.0
[Chain] changed: echo not measured -> not confirmed (at most -35.0 dB under the reference)
[Chain] 10 s: ... ERLE 7.9 dB, echo -43.5 dB under the reference (AEC fallback echo -40.0 dB, AEC mic gain +18.0 dB), ...
[ApoLink] saving echo strength -43.5 dB under the reference
[ApoLink] remembered echo strength for this mic/output: -43.5 dB under the reference
```

Co z logu od testera vyčíst:

| V logu                                             | Znamená                                              |
| -------------------------------------------------- | ---------------------------------------------------- |
| `echo ... dB under the reference`                  | síla echa na jeho sestavě; čím blíž k 0, tím hůř     |
| `not confirmed (at most X dB ...)`                 | žádné echo, které by sledovalo referenci; X je jen horní mez a může to být hlas (sluchátka, řeč do hudby) |
| `echo not measured` i po minutě hudby              | reference je tišší než -30 dBFS, nebo AEC neběží     |
| `AEC fallback echo` jiné než -40.0                 | echo je silnější než asi 32 dB pod referencí         |
| `echo` skáče o víc než 6 dB tam a zpět             | měřič je nestabilní: poslat log, nelaď naslepo       |
| `AEC mic gain` se mění víckrát za minutu           | zesílení kmitá: zvětšit `MIC_GAIN_HYSTERESIS_DB`     |
| `ERLE` kolem 0 při hrající hudbě                   | AEC3 nepoužívá lineární filtr                        |

Naživo vyzkoušet: reproduktory nahlas (síla echa má stoupnout a po pár sekundách se objeví
`saving echo strength`), ztlumit reproduktory (má klesnout; když neklesne, loopback se bere až
za hlasitostí Windows a je to dobrá zpráva), přepnout výstup na sluchátka (po
přepnutí `not measured`, pak mez nebo nízká hodnota), restart aplikace (`remembered echo
strength` a hodnota v řádku `start:`).

### 11.4 Co není hotové

AEC se na sluchátkách samo nevypíná. Se zesílením mikrofonu ztrácí sluchátkové nahrávky v AEC3
0,6-0,7 dB hlasu (milník 44), takže zisk by byl malý, a detektor „echo je v šumu" ještě neviděl
skutečný stroj. Nejdřív posbírat logy z kroku 11.3; vypínat až podle nich.
