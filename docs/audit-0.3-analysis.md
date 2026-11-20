# Audit po 0.3.0: zbytečná práce, tok dat, přepínání modelů

Stav: větev `feat/tray-mic-level`, 2026-11-20. Audit byl jen čtení kódu na Macu. Pracovní dokument: po
zapracování smazat, podstatné je v `DEVLOG.md` (milník 66, „After an audit“).

**Opraveno ve stejný den** (na Macu jen typová kontrola a clippy pro Windows, testy mic řetězce a služby
poběží až v CI / na Windows): nálezy 1, 2, 3 (jen když je vypnutý i leveler), 5 a z nálezu 7 tick
duckingu a 8 (šumové enginy sidetone se staví až při zvolení rámcového režimu). **Ponecháno:** 4 (tap
nemůže čekat na čtenáře: služba otevírá výstup teprve proto, že tap píše), 6, 9, 10, 11 a necachované
čtení hlasitosti z nálezu 7.

Značení jistoty: **ověřeno** = četl jsem ten kód sám, **subagent** = nález levnějšího modelu, který jsem
nepřekontroloval řádek po řádku, **hypotéza** = z kódu to plyne, ale chybí měření.

## Závěr

- Architektura drží i s ChatMixem a sidetone. Žádný AI model neběží kvůli tomu, že aplikace skončila:
  když služba zmizí, do 3 s (`OWNER_TIMEOUT`) mikrofonní řetězec propouští zvuk, ducking se vypne,
  sidetone pustí ring a výstupní APO přestane psát referenci.
- V audio vláknech se nenašla alokace, zámek ani logování po blocích (kromě známé výjimky tract).
- Jeden skutečný rozpor kódu s DEVLOGem (nález 1) a jedna malá chyba zvuku při přepnutí modelu (nález 2).
- Zbytek jsou drobné úspory. Největší z nich je už v DEVLOGu jako „neuděláno“ (detektor při potlačení
  šumu 0).
- Pro rozdílné vytížení po přepnutí modelu tam a zpět **kód nedává jednoznačnou příčinu**. Ustálená
  cena jednoho modelu na historii nezávisí. Jsou tři mechanismy, které rozdíl umí udělat; jak je
  rozlišit z logu, je v poslední kapitole.

## Nálezy podle dopadu

| #  | Kde                                         | Co                                                              | Jistota   |
| -- | ------------------------------------------- | --------------------------------------------------------------- | --------- |
| 1  | `win_audio/stream_router.rs:224-243`        | Router měří po 5 ms i při samotném přehrávači (rozpor s M66)    | ověřeno   |
| 2  | `ai_denoiser/mod.rs:865-877`                | Po přepnutí na DFN3 zazní ~40 ms starého zvuku                  | ověřeno   |
| 3  | `ai_denoiser/mod.rs:490-505`                | „Vypnuto“ stojí víc CPU než RNNoise (detektor ~2,8 % jádra)     | ověřeno   |
| 4  | `apo/sidetone_tap.rs:79-93`                 | Sidetone počítá a píše do ringu, i když ho nikdo nečte          | ověřeno   |
| 5  | `apo/stream.rs:150`                         | Silero běží na chatu, i když není co ztlumit                    | ověřeno   |
| 6  | `win_audio/chatmix/mod.rs:508-518, 732-810` | Při otevřeném okně plný průchod všech sessions každou sekundu   | subagent  |
| 7  | `win_audio/ducking/view.rs:94`, `mod.rs:109`| Tick duckingu formátuje a klonuje i při vypnuté funkci          | subagent  |
| 8  | `apo/sidetone_tap.rs:34`                    | Tři šumové enginy na každou instanci mic APO, i bez sidetone    | ověřeno   |
| 9  | `apo/mic.rs:144`, `shm/header.rs:367`       | Čerstvé mapování: při startu streamu se načte DFN3 místo volby  | ověřeno   |
| 10 | `apo/entry.rs:525-530`                      | Tiché bloky mikrofonu řetězec přeskočí, ale nezapauzují         | hypotéza  |
| 11 | `ui/app.rs:208`                             | Okno překresluje po 30 ms i zakryté jiným oknem                 | subagent  |

### 1. StreamRouter měří po 5 ms, i když hraje jen přehrávač

`DEVLOG.md:3537-3539` (milník 66) říká, že router spí 500 ms, dokud nehraje Chat nebo Game session, a že
samotný Media stream se počítá jako klid. Kód spí jen při prázdném seznamu sessions:

```rust
let playing = self.sessions.values().any(|s| matches!(s.bus, StreamBus::Chat | StreamBus::Game));
CHAT_OR_GAME_PLAYING.store(playing, Ordering::Relaxed);
if self.sessions.is_empty() { /* sleep 500 ms */ }
// jinak: GetPeakValue na každé session každých 5 ms
```

`playing` se použije jen pro smyčku služby a referenční vlákno, ne pro router samotný. Scénář: Automatický
ChatMix zapnutý, žádná hra ani chat, hraje prohlížeč nebo Spotify: asi 200 probuzení a 200 COM volání do
audiodg za sekundu, po celou dobu přehrávání.

Návrh: spát také při `!playing && !header.stream_eq_active()`. Měření Media session je potřeba jen pro
spárování, když je zapnutý EQ chatu nebo médií; samotný ducking páruje jen Chat a Game.

### 2. Přepnutí zpět na DeepFilterNet 3 pustí starý zvuk

`DeepFilterEngine::reset` maže jen vstupní a výstupní FIFO; stav uvnitř forku (lookahead spektra, GRU)
zůstává z doby, kdy model běžel naposledy. `flush` to řeší umlčením první latence
(`mod.rs:716`, `mute_left`), probuzení ze spánku to schová za přebírání. `reset_engines` při přepnutí
modelu ale nastaví `mute_left = 0` a žádné přebírání není, takže první latence DFN3 (40 ms) po přepnutí
RNNoise -> DFN3 nebo DPDFNet -> DFN3 obsahuje zvuk z minula. DPDFNet se resetuje načisto, toho se to netýká.

Návrh: po `self.active = wanted` v `process_samples` nastavit `mute_left` stejně jako ve `flush`. U prvního
načtení to nic nemění (první latence je stejně ticho z plnění).

### 3. „Vypnuto“ je dražší než RNNoise

Při síle 0 běží vstupní detektor `in_net` (RNNoise 0.2, podle komentáře ~2,8 % jádra), protože z něj učí
leveler a automatický vstupní zisk. Se zvoleným RNNoise běží jen RNNoise + `out_net` + Silero
(řádově 0,5-1 %). DEVLOG M66 to má jako „neuděláno“. Stejný detektor běží vedle obou těžkých modelů, i když
spí: DFN3 sám je ~2-3 %, takže spánek DFN3 ušetří méně než polovinu jeho celkové ceny. To je vědomé
rozhodnutí kvůli kvalitě (milníky 35, 38), ne chyba; uvádím to, protože to vysvětluje čísla v `[Cpu]`.

### 4. Sidetone bez čtenáře

Tap běží, kdykoli je sidetone zapnutý, je to náš mikrofon a služba žije. Na `sidetone_reader_beat` se
nedívá. Když výstupní APO na spárovaném výstupu není (jiné zařízení, odpojeno, nenainstalováno), mic APO
dál počítá EQ, expander nebo šumový model a píše 480 vzorků na blok do ringu, dokud nějaká aplikace nahrává.
Služba stav zná (verdikt v `win_audio/sidetone.rs:412`), ale `enabled` kvůli němu nevypne.

Návrh: služba zapíše do sdílené paměti „čtenář připojen“ (nebo při verdiktu bez čtenáře nezrcadlí
`enabled`), tap se podle toho umlčí. Hlasitost 0 a ztlumení už řešené jsou.

### 5. Silero na chatu bez hry

`is_speech = role == Voice && settings.enabled && self.detect_speech(..)`: stačí zapnutý Automatický ChatMix
a běžící Discord, model jede každý blok, i když žádný Game slot není čerstvý. Pozor při opravě: úroveň
řeči z chatu čte i cesta `MixDucker` ve službě (hry, které stream efekty nevidí), takže podmínka musí být
„žádný čerstvý Game slot a mix režim nechce referenci“. Cena Silera je malá, priorita nízká.

Souvisí: `Vad48k` se staví pro každý 48 kHz stream včetně her a médií (`apo/stream.rs:43`). Je to
zdokumentovaný kompromis (váhy sdílené, ~13 kB na stream).

### 6.-7. Služba (nálezy subagenta)

- Při otevřeném okně nebo hrajícím chatu/hře se oznámení od Windows nepoužijí a každou sekundu běží plný
  průchod: výčet zařízení, session manager na každém výstupu, `OpenProcess` na každou session. M66 schéma
  s oznámeními omezil na klid; otevřené okno přes celý večer tím pokryté není.
- `Ducking::tick` před `if !enabled { return }` skládá text ladění (`Vec<String>` + join), klonuje seznam
  streamů a jednou za 2 s volá `fs::metadata` na `duck_tuning.json`. Stačí posunout kontrolu `enabled` výš.
- `endpoint_volume.rs:107-157`: bez vybraného id zařízení se enumerátor a `Activate` volají při každém
  čtení hlasitosti (250 ms / 1 s), cache se používá jen s id.

### 8.-11. Drobnosti

- `SidetoneDenoiser::new` staví WebRTC NS, RNNoise i RNNoise 0.2 pro každý `MicProcessor`, i když sidetone
  nikdo nepoužívá nebo běží výchozí expander. Paměť a čas startu streamu, ne CPU za běhu.
- `preload_selected_model` čte model ze sdílené paměti. Když mapování právě vzniklo (první stream po startu
  audiodg), je tam výchozí DFN3, načte se blokujícně (~80 ms) a zůstane v paměti, i když má uživatel RNNoise.
- Blok mikrofonu s příznakem `BUFFER_SILENT` řetězec vůbec nezavolá, ale `pause()` nenastane. Po návratu
  zvuku tak FIFO stupňů drží zvuk z doby před tichem. Neověřeno, jestli capture ten příznak v praxi posílá.
- Okno: 30 ms snímek, dokud není minimalizované; zakryté hrou v okně běží dál naplno včetně 10 Hz čtení
  z audio služby.

## Zkontrolováno a v pořádku

- Reference pro AEC: výstupní APO píše jen pro čtenáře (mic řetězec s AEC, nebo mix režim); vypnuto stojí
  jedno načtení atomiku na blok. Druhý zdroj reference (loopback ve službě) neexistuje.
- Výstupní APO s plochým EQ, vypnutým EQ nebo bez vlastníka zvuk nemění, limiter běží jen se sidetone.
- Stream APO: mrtvý slot přestane platit do 50 ms (`FRESH_HNS`), hra nezůstane ztlumená kvůli spadlému chatu.
- Mix režim ve službě nedělá vlastní záznam ani Silero, čte ring výstupní APO; Stream a Mix se vylučují.
- Obnova hlasitosti: vypnutí funkce, ukončení služby i pád (`ducking_lowered.json`) jsou pokryté.
- Zrcadlení sdílené paměti: ~70-90 atomiků na tick, telemetrie jen při změně `aec_telemetry_seq`.
- Logy duckingu jsou událostní. Jediný periodický řádek je `[Chain]` po 10 s při zapnutém debug logu
  (zdokumentováno).
- Sidetone smyčka služby: bez horké smyčky, při chybě 1 s pauza, otevření se zkouší po 10 s.

## Přepínání modelů a rozdílné CPU

**Odvozeno z kódu.** Pro daný zvolený model je ustálený stav stejný po startu streamu i po přepnutí tam
a zpět: oba těžké modely zůstanou v paměti, ale počítá jen aktivní; `reset_engines` vrací spánek, záchranu
kliků i FIFO do výchozího stavu. Trvalý rozdíl tedy musí být v tom, **co doopravdy běží**, ne v modelu.

| Mechanismus                                 | Před přepnutím               | Po přepnutí                    | Trvá          |
| ------------------------------------------- | ---------------------------- | ------------------------------ | ------------- |
| A. Náhrada RNNoise zůstala „přilepená“      | běží RNNoise místo volby     | běží zvolený těžký model       | trvale        |
| B. Spánek těžkého modelu                    | spí (ticho > 2 s)            | vzhůru                         | 2-3 s         |
| C. DFN3 přeskakuje tiché rámce              | vstup pod -70 dBFS: bez sítě | vstup nad -70 dBFS: celá síť   | dle úrovně    |

- **A (nejlépe sedí na popis).** `overloaded` a chyba načtení platí „dokud se model nezvolí znovu“
  (`mod.rs:786-808`, `select` -> `forget_failure`). Pokud model při startu streamu spadl do náhrady
  (tři okna po 100 ms s průměrem nad 90 % nebo 2+ pozdními bloky, `chain/telemetry.rs:574-582`), běžel
  levný RNNoise. Přepnutí tam a zpět příznak smaže a těžký model se rozběhne: vyšší CPU při „stejném“
  nastavení. V logu by to byl řádek `[Denoise] LIMITED: ... switched to RNNoise` nebo
  `[Denoise] running: RnNoise` při zvoleném těžkém modelu.
- **B.** Každé přepnutí maže spánek (`sleep.clear()`), model je pak nejméně 2 s vzhůru a 300 ms běží oba
  (přebírání). Klik myší na přepínač se navíc počítá jako aktivita: úroveň je hlazená přes 100 ms, takže
  hlasitý klik drží „aktivitu“ stovky ms, ačkoli komentář u `ACTIVITY_MIN_SAMPLES` tvrdí opak. Pohled na
  CPU hned po přepnutí proto ukáže víc než v klidu před ním.
- **C.** Fork DFN3 (`crates/vendor/deep_filter/src/tract.rs:516-524`) po 6 rámcích pod -70 dBFS síť vůbec
  nespustí. Vstup DFN3 je ale zesílený o zisk leveleru (0-24 dB, `set_pre_gain_db`) a přepnutí modelu
  spouští `leveler.relearn()`. Stejný šum místnosti tak může být jednou pod prahem a jednou nad ním.
  Projeví se jen, když je model vzhůru.

Mimo kód: `load` i `[Cpu]` jsou čas, ne práce. Když procesor v klidu sníží frekvenci, stejný blok trvá
déle a procenta stoupnou. Srovnávat jen při podobné zátěži počítače.

**Postup (Windows, build `-CpuStats`, „Save debug to disk“ zapnuté):**

1. Spustit nahrávání, minutu mlčet. Z řádku `[Chain]` opsat: `model`, `model_active`, `limit`, příznak
   `model asleep`, `load` průměr a špička, příznak dalších řetězců (`MIC_CHAIN_OTHERS`).
2. Přepnout model tam a zpět, zase minutu mlčet, opsat totéž a řádek `[Cpu]` té minuty.
3. Vyhodnotit:
   - `model_active` nebo `limit` se liší -> A. Pak hledat, proč model při startu spadl do náhrady.
   - `model asleep` před ano, po ne (i po 10 s ticha) -> spánek se nevrací; poslat log, `quiet_ms` ukáže,
     co ho budí.
   - vše stejné, jen `load` jiný -> C nebo frekvence CPU; zopakovat s levelerem vypnutým.
   - příznak dalších řetězců svítí -> řetězec běží dvakrát (MFX, dva režimy), s modelem to nesouvisí.

## Co nebylo zkontrolováno

- Nic neběželo: žádné měření CPU, žádný poslech. Nálezy 6, 7 a 11 a část „v pořádku“ stojí na zprávách
  subagentů (Sonnet), které jsem ověřil jen u nálezů 1, 4, 5 a 8.
- Nečteno: `aec3.rs` a `delay_estimator.rs` (AEC), `voice_leveler.rs`, párování v
  `stream_router.rs:345-440`, `health/`, instalátor, import ze Sonaru, vnitřek forků ve `crates/vendor/`
  kromě `deep_filter/src/tract.rs`.
