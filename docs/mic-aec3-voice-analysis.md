# AEC3 a ztráta hlasu: analýza a nápady (2026-11-10)

Vstup: DEVLOG (milníky 25, 26, 30, 36, 41, 42), `aec3.rs`, `plugin.rs`, `shm.rs`, `loopback.rs`
a zdroják `sonora-aec3` 0.3.0 (verze z `Cargo.lock`, čteno z lokální cargo registry).
Na tomto stroji není Rust toolchain ani `test_data/private/`, takže **nic z toho není změřeno**.

Značení: **Naměřeno** = číslo z DEVLOGu. **Z kódu** = ověřeno čtením zdrojáku.
**Hypotéza** = vyplývá z kódu, ale potřebuje měření na captures.

Cesty `sonora-aec3/...` a `sonora/...` znamenají `~/.cargo/registry/src/*/sonora-aec3-0.3.0/src/...`
a `.../sonora-0.3.0/src/...`.

## 0. Stav po implementaci (2026-11-10, večer)

Část nápadů je hotová a syntetika některé hypotézy rozhodla. Podrobnosti: DEVLOG milník 43,
postup pro doladění na nahrávkách: `docs/aec3-tuning-guide.md`.

| Bod analýzy                         | Výsledek na syntetice (skutečné AEC3, umělé signály)                  |
| ----------------------------------- | --------------------------------------------------------------------- |
| 3.1 absolutní práh úrovně           | **potvrzeno**: echo -68 / hlas -46 dBFS ztrácí 4,7 dB hlasu, se zesílením 24 dB jen 1,4 dB |
| 3.3 strop ERLE                      | **potvrzeno**, když filtr ubírá hodně (-7,8 -> -4,1 dB); při ERLE 12 dB malý vliv |
| 3.4 ořez výšek podle binu 2 kHz     | **potvrzeno** v malém: pásmo 4-8 kHz o 1-2 dB lepší                    |
| 3.5 okno filtru mimo přímou cestu   | **vyvráceno**: `delay_ms` je 0 od začátku, B2 nemá žádný účinek        |
| B1 zesílení mikrofonu               | **implementováno** (`EchoCanceller3::set_mic_gain_db`), neověřeno na nahrávkách |
| A1, A2 diagnostika                  | **implementováno** jako `aec3::tests::capture_tuning_report` (sloupec ERLE, varianta `open`) |
| B3, B4, B5                          | připraveno jako varianty reportu (`erle=`, `hf=`, `len=`), výchozí hodnoty beze změny |
| C1 sluchátka, C2 stereo, C5 kanály  | neimplementováno                                                      |

## 1. Závěr v kostce

1. AEC3 se na zachycených nahrávkách nechová jako echo canceller, ale jako **ducker řízený
   hlasitostí reference**. Lineární filtr je jediná část, která hlas neumí poškodit (jen odečítá
   to, co koreluje s referencí). Buď se vůbec nepoužívá, nebo mu AEC3 věří nejvýš 6 dB.
2. Dosavadní ladění (`default_gain`, masky, detekce near-endu) hýbe jen tím, jak hluboko ducker
   tlumí. `default_gain` 0.01 je navíc přesně hodnota, kterou má AEC3 natvrdo pro režim „žádné
   echo" (`sonora-aec3/residual_echo_estimator.rs:12`). Tahle cesta je vyčerpaná: 0.003 už echo
   pouští (milník 42).
3. Další zisk je v tom, aby se lineární filtr **používal** (úroveň signálu, zpoždění) a aby se mu
   **věřilo** (strop ERLE). Obojí je jen konfigurace a dá se ověřit replayem stávajících captures.
4. Na sluchátkách nemá AEC běžet vůbec. To je rozhodnutí mimo AEC3 a jde udělat hned.
5. Chybí testovací případ, kvůli kterému AEC existuje: **řeč z reproduktorů** (spoluhráči).
   Všechny captures jsou hudba.

## 2. Co je naměřeno (DEVLOG)

| Capture  | Situace                           | Echo na mikrofonu | Co se stalo s hlasem                                      |
| -------- | --------------------------------- | ----------------- | --------------------------------------------------------- |
| `115931` | sluchátka, hudba -28, hlas -40    | asi -70 dBFS      | výstup AEC 3,5 s na -85 dBFS; po ladění ztráta 5,6 dB     |
| `130319` | repro, hudba pořád                | -55 dBFS          | hlasité bloky -2,7 dB; střední -12 dB; tiché -14 dB       |
| `125717` | repro, hudba + řeč                | asi -54 dBFS      | hlasitá půlka bloků -13 dB (bez ladění -19 dB)            |
| `210655` | repro, hudba -18, echo 50 dB dole | -67..-68 dBFS     | začátek slova -46 -> -81 dBFS; po 0.01 -55 dBFS           |
| `205013` | repro, hudba                      | -                 | `in_net` za AEC3 slyší hlas v 8 % rámců (raw mic: 74 %)   |

Další naměřená fakta:

- V `210655` AEC3 nepoužívalo lineární filtr. S nástupem slova hýbou jen `default_gain` a
  `model_reverb_in_nonlinear_mode`, 20 dalších nastavení ne (milník 42).
- `130319`: sekané jsou dozvuky slabik na -46..-54 dBFS, tedy 1-9 dB nad echem (milník 36).
- S hudbou bere AEC3 hlasu asi 10 dB v pásmu 4-8 kHz; nastavení „high bands" to nemění
  (milník 42, otevřené).
- Syntetický test stav z `210655` nereprodukoval (milník 42).
- Vypínání AEC podle `EchoProbe` zrušeno: detektor kmital s pauzami v řeči a každé přepnutí
  posunulo mikrofon o 430 vzorků (milník 36).

## 3. Co je v kódu AEC3

### 3.1 Kdy AEC3 lineární filtr použije

**Z kódu** (`sonora-aec3/aec_state.rs:269-281`): odhad z lineárního filtru je „použitelný", jen když
platí všechno:

- uběhlo 0,4 s aktivní reference od startu a 0,2 s od resetu,
- vlastní estimátor zpoždění AEC3 našel echo, **nebo** byl aspoň jednou vidět konvergovaný filtr,
- není aktivní transparent mode.

**Z kódu** (`sonora-aec3/subtractor_output_analyzer.rs:41-50`): „konvergovaný filtr" má **absolutní
práh úrovně**. Blok mikrofonu (64 vzorků, pásmo 0-8 kHz, měřítko int16) musí mít RMS přes 50, tedy
přes **-56,3 dBFS**, a zbytek po filtru musí být pod polovinou energie. Volnější práh pro hrubý
filtr (krmí transparent mode) je RMS 20, tedy -64,3 dBFS.

Důsledek proti tabulce v kapitole 2:

- echo -67..-70 dBFS (`210655`, sluchátka): konvergenci nelze uznat nikdy. Blok přes -56 dBFS
  je jen ten s hlasem, a tam zbytek po filtru pod polovinu neklesne.
- echo -55 dBFS (`130319`): přesně na hraně.
- Transparent mode (HMM) se rozhoduje podle stejných pozorování. Tiché echo z repráků proto
  vypadá jako „žádné echo", a v tom režimu se lineární filtr nepoužívá.

AEC3 je laděné na úrovně hovoru po AGC (řeč kolem -20..-30 dBFS). Mikrofon tady běží o 20-30 dB
níž (Windows hlasitost 30 %, hlas -45..-60 dBFS). Je to stejný druh chyby jako úrovňový útes
DeepFilterNet 3 v milníku 36.

### 3.2 Odhad zbytkového echa, podle kterého se tlumí

**Z kódu** (`sonora-aec3/residual_echo_estimator.rs:182-253`):

| Režim                            | Odhad zbytkového echa                    | Výstup jde z     |
| -------------------------------- | ---------------------------------------- | ---------------- |
| lineární filtr použitelný        | odhad echa filtru / ERLE + dozvuk        | výstupu filtru   |
| nepoužitelný, ne transparent     | (`default_gain` x reference)^2 + dozvuk  | mikrofonu        |
| transparent mode                 | (0.01 x reference)^2, bez dozvuku        | mikrofonu        |

Ve dvou spodních řádcích AEC3 o skutečné cestě echa nic neví. Odhad je reference mínus 40 dB.
V `210655` (echo 50 dB pod referencí) je to 10 dB nad skutečným echem. Hlas pod touto hladinou
se tlumí vždy.

### 3.3 Strop ERLE: lineárnímu filtru se věří nejvýš 6 dB

**Z kódu** (`sonora-aec3/subband_erle_estimator.rs:15-19`, výchozí hodnoty `config.rs:590-598`):
odhad ERLE je shora omezen na `erle.max_l` = 4 (6 dB) pro 0-4 kHz a `erle.max_h` = 1.5
(**1,8 dB**) pro 4-8 kHz. I kdyby filtr reálně ubral 25 dB, AEC3 počítá se zbytkem
„echo - 6 dB", nad 4 kHz „echo - 1,8 dB".

Dosazeno do současných masek (od 1 kHz `enr_transparent` 0.3, `enr_suppress` 0.5):

| Pásmo   | Hlas projde bez útlumu od | Plně potlačen pod |
| ------- | ------------------------- | ----------------- |
| 1-4 kHz | echo - 1 dB               | echo - 3 dB       |
| 4-8 kHz | echo + 3,5 dB             | echo + 1 dB       |

„Echo" je tu úroveň echa **na mikrofonu před filtrem**, v daném frekvenčním binu. To sedí na
`130319` (sekané je vše do ~9 dB nad echem) i na závěr milníku 36 „hlas je prostě moc blízko echa".
Podle kódu to ale není fyzikální mez. Je to strop nastavený pro notebooky se zkreslujícími
repráčky. Rozsah povolený validací je 1..100000 (`config.rs:120-122`).

Milník 42 zkoušel „ERLE" jen na nástupu slova v `210655`, kde lineární filtr nebyl v použití,
takže tam strop nemohl nic změnit. Na `130319` / `125717` podle DEVLOGu zkoušen nebyl.

### 3.4 Pásmo 4-8 kHz

**Z kódu** (`sonora-aec3/suppression_gain.rs:38-55`, `config.rs:903-909`): zisk všech binů nad
2 kHz se ořízne na zisk binu 16 (2 kHz): `high_frequency_suppression.limiting_gain_band` = 16,
`bands_in_limiting_gain` = 1. Když hudba tlumí bin na 2 kHz, sykavky nad ním jdou dolů s ním.
`bands_in_limiting_gain` = 0 to vypne. Je to jiné nastavení než `high_bands_suppression`
(8-24 kHz), které milník 42 zkoušel.

**Hypotéza**: otevřená ztráta 10 dB ve 4-8 kHz je součet tohoto ořezu a stropu `max_h` z 3.3.

### 3.5 Zpoždění uvnitř AEC3

**Z kódu**: řetěz zarovná referenci sám (QPC + `DelayEstimator`) tak, že echo přichází
`PRE_DELAY_SAMPLES` = 3 ms po referenci, a volá `set_stream_delay_ms(0)` (`aec3.rs:121`).
AEC3 má ale `use_external_delay_estimator` = false, takže zpoždění hledá samo. Dokud ho nenajde,
čte referenci starší o `default_delay` = 5 bloků = 20 ms (`sonora-aec3/render_delay_buffer.rs:143-157`).

**Hypotéza**: filtr dlouhý 52 ms pak pokrývá zpoždění asi 20-72 ms a přímá cesta echa (3 ms) je
do té doby mimo jeho okno. Slabé echo vlastní
estimátor najít nemusí. Pak filtr nezkonverguje nikdy, bez ohledu na úroveň. Sedí to na poznámku
u testů („s 20 vzorky se AEC3 zaseklo na ~10 dB") i na to, že syntetický test se silným echem
stav nereprodukoval. Ověření: `statistics().delay_ms` (`sonora-aec3/block_processor.rs:109-115`).

### 3.6 Reference, kterou filtr dostává

**Z kódu**:

- Reference je mono downmix `(L + R) / 2` (`crates/gamersequalizer-common/src/shm.rs:992-995`).
  Echo je ale `hL*L + hR*R`. Stranovou složku stereo hudby a hry z mono reference předpovědět
  nejde, takže lineární ERLE má strop daný šířkou sterea. AEC3 stereo referenci umí
  (`StreamConfig::new(48_000, 2)` pro render, `multi_channel.detect_stereo_content`).
- Loopback bere jen první dva kanály (`crates/gamersequalizer-daemon/src/win_audio/loopback.rs:203`).
  U 5.1 / 7.1 chybí v referenci center (dialogy) a surroundy.
- Filtr má 13 bloků = 52 ms (`aec3.rs:28`). Dozvuk místnosti za touto hranicí řeší jen potlačovač.
  Validace dovolí delší filtr.

## 4. Nápady

Seřazeno podle poměru přínos / cena. A a B jsou jen úpravy `tuning()` a testů, bez přeinstalace APO.

### A. Nejdřív zviditelnit, v jakém stavu AEC3 je

Dnes se stav AEC3 odvozuje nepřímo z toho, které nastavení hýbe výstupem.

- **A1.** `capture_replay_report` vypíše po sekundách `apm.statistics()`: `delay_ms`,
  `echo_return_loss`, `echo_return_loss_enhancement`, `divergent_filter_fraction`. Jedna testovací
  metoda na `EchoCanceller3`, která vrátí `statistics()`.
- **A2. Replay s průhledným potlačovačem**: v `normal_tuning` i `nearend_tuning`, v `mask_lf` i
  `mask_hf`, `enr_transparent` = 99 a `enr_suppress` = 100 (maximum je 100, `config.rs:321-326`).
  Zisk je pak prakticky vždy 1 a výstup je čistý výstup lineárního filtru (nebo mikrofon, když
  se filtr nepoužívá). Rozdíl mic -> výstup v úsecích jen s echem je **skutečné
  lineární ERLE**. 0 dB znamená „filtr se nepoužívá".

Výsledek A2 rozhodne, kudy dál:

| Lineární ERLE v úseku jen s echem | Co to znamená                         | Pokračovat  |
| --------------------------------- | ------------------------------------- | ----------- |
| kolem 0 dB                        | filtr se nepoužívá                    | B1, B2      |
| 5-10 dB                           | používá se, ale reference ho omezuje  | C2, B5      |
| 15 dB a víc                       | filtr funguje, škodí jen potlačovač   | B3, B4, C3  |

### B. Jen konfigurace

- **B1. Zesílit mikrofon před AEC3 a za ním ho zase zeslabit** (jako pre-gain DFN3, milník 36).
  Echo -68 dBFS se s +24 dB dostane na -44 dBFS, nad práh -56 dBFS. Aby se odhad v nelineárním
  režimu nezměnil, `default_gain` se násobí stejným ziskem. Zdroj zisku: boost leveleru, který už
  dostává DFN3 (`plugin.rs:436`), ale **zaaretovaný** (změna zisku je pro AEC3 změna cesty echa;
  měnit jen při resetu nebo po krocích 6 dB s hysterezí). Pozor na detekci saturace: vzorek přes
  ±32700 v měřítku int16 (`sonora/echo_canceller3.rs:180`) přepne odhad na „vše je echo".
- **B2. Zpoždění**: `delay.use_external_delay_estimator` = true (AEC3 věří zarovnání řetězu a samo
  nehledá), nebo `delay.default_delay` = 1. S externím estimátorem se filtr stane použitelným až
  po viděné konvergenci, takže dává smysl jen spolu s B1. Případně zvednout `PRE_DELAY_SAMPLES`
  na 240-480, protože `DelayEstimator` má toleranci 2 ms.
- **B3. Strop ERLE**: `erle.max_l` 4 -> 10 / 30 / 100, `erle.max_h` 1.5 -> 4 / 10 / 30. Má smysl
  až když A2 ukáže, že filtr reálně ubírá. Očekávání: hlas projde až o skutečné ERLE níž pod echem.
- **B4.** `suppressor.high_frequency_suppression.bands_in_limiting_gain` = 0. Měřit ztrátu hlasu
  ve 4-8 kHz.
- **B5.** `filter.refined.length_blocks` a `filter.coarse.length_blocks` 13 -> 24 / 32, pokud A2
  ukáže ERLE omezené dozvukem.

### C. Mimo AEC3

- **C1. Sluchátka: AEC automaticky vypnout.** Nejlevnější je tvarový faktor výstupu z Windows
  (`PKEY_AudioEndpoint_FormFactor`: Headphones / Headset). Přepínač AEC by měl tři stavy:
  Auto / Zapnuto / Vypnuto. Stačí změna v daemonu (`aec_enabled` ve sdílené paměti), APO se
  nemění. Mez: analogový jack se často hlásí jako Speakers. Pro ten případ až jako druhý krok
  měřený detektor. Oba důvody, proč byl `EchoProbe` zrušen, jsou dnes řešitelné:
  - kmitání s pauzami v řeči: úroveň echa měřit jen v oknech, kde Silero neslyší hlas (Silero
    přišlo až v milníku 40),
  - cvaknutí při přepnutí: obtok přes zpožďovací linku 430 vzorků a prolnutí, latence se nemění.
- **C2. Stereo reference.** Ring už drží `StereoFrame`; chybí stereo čtení a dva kanály do
  `process_render_f32`. `DelayEstimator` může zůstat mono. Před tím změřit přínos: debug capture
  rozšířit o L a R reference a na úseku jen s echem porovnat least-squares fit z mono a ze sterea
  (stejný postup jako fit v milníku 42).
- **C3. Měkčí potlačovač, zbytek nechat na denoiseru a gate.** Zbytek hudby po lineárním filtru
  je pro DFN3 šum. Zkouška bez nového kódu: masky x3 / x10 a AEC výstup pustit denoiserem
  (`ai_denoiser::tests::capture_replay_report`). **Pozor**: řeč spoluhráčů z repráků pro DFN3 šum
  není a prošla by. Proto nejdřív capture s řečí z repráků (krok 1 postupu).
- **C4. Vlastní fork `sonora-aec3` v `crates/vendor/`** jen když konfigurace nestačí: spodní mez
  zisku potlačovače, stav (lineární / nelineární / transparent, ERLE) do telemetrie a debug
  capture. `BlockProcessor` je veřejný a lineární výstup umí vrátit
  (`sonora-aec3/echo_remover.rs:370-375`), ale jen pásmo 0-8 kHz.
- **C5.** Loopback: u vícekanálového mixu smíchat do reference všechny kanály, ne jen první dva.

## 5. Postup

1. **Nové captures** (30-60 s, build s `-DebugCapture`):
   - řeč z reproduktorů (podcast nebo hovor), střídavě ticho / já mluvím / oba najednou,
   - totéž s Windows hlasitostí mikrofonu kolem 80 % (kontrola hypotézy o úrovni z 3.1).
2. **A1 + A2** na `130319`, `125717`, `122712`, `210655`, `115931`, `122520` a na nových.
   Zapsat po 5 s: `delay_ms`, ERL, lineární ERLE.
3. **B1** offline: mikrofon x 4 a x 16 (12 a 24 dB), výstup / stejným číslem, `default_gain` x
   stejným číslem. Znovu krok 2. Změnilo se `delay_ms` a lineární ERLE?
4. **B2** offline, samostatně i spolu s B1.
5. Jakmile je lineární ERLE aspoň 10-15 dB: **B3 a B4** s normálními maskami. Měřit tabulkou
   z milníku 36 pro `130319` (bloky hlasité / střední / tiché / pauzy):
   - cíl: „AEC cut > 6 dB" u středních bloků z 67 % pod 20 %, u tichých z 92 % pod 50 %,
   - hlídat: pauzy po denoiseru (teď -112 dBFS) a úseky jen s echem na všech 15 captures,
   - hlídat: nová capture s řečí z repráků nesmí pouštět srozumitelnou řeč.
6. Vítěznou kombinaci do `tuning()`, čísla do komentáře a DEVLOGu, unit testy (`split`,
   reverberant echo, weak echo) musí projít.
7. **C1** nezávisle na krocích 2-6 (jiná část kódu, žádné měření předem).
8. **C2** až podle výsledku A2 (řádek „5-10 dB").

Vedlejší metrika u každého kroku: podíl rámců, kde `in_net` za AEC3 slyší hlas při hudbě
(milník 41: 8 %). Když AEC3 přestane hlas tvarovat, mělo by to stoupnout samo.

## 6. Co nezkoušet znovu (DEVLOG)

- `default_gain` pod 0.01: 0.003 pouští echo v prvních 5 s.
- Širší masky, masky near-endu jako normální, snazší detekce dominantního near-endu, subband
  detekce: na `125717` nic nebo ~0,5 dB (milník 36).
- Vypínání AEC, při kterém se mění latence.
- Jednotapová korelace jako detektor echa na reprácích.

## 7. Nejistoty

- Nic nebylo spuštěno. Kapitola 3 je čtení portu `sonora` 0.3.0, ne měření. Port se může
  v detailech lišit od C++ WebRTC.
- 3.5 (okno filtru mimo přímou cestu) je nejméně jistá úvaha: závisí na pořadí volání render /
  capture uvnitř `sonora`. A1 ji potvrdí nebo vyvrátí jedním výpisem.
- Prahy v 3.3 jsou spočtené z masek a stropu ERLE pro jeden bin, bez modelu dozvuku a bez
  vyhlazování zisku. Berou se jako řád, ne jako přesná čísla.
- B3 jde proti záměru autorů AEC3 (strop chrání před únikem echa u nelineárních repráků).
  Monitorové repráky přes HDMI z milníku 26 jsou přesně ten případ, takže únik echa hlídat
  hlavně tam.
