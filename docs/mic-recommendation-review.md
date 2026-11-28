# Druhý názor na doporučení k výkonu a kvalitě mikrofonního řetězce

Stav: 2026-11-27, větev `perf/ui-startup`. Pracovní dokument, po zapracování smazat.

Každé tvrzení z doporučení je ověřené proti kódu a `DEVLOG.md`, fakta o modelech proti README,
licencím a článkům. Nic z toho nebylo spuštěno: na Macu nejsou `test_data/private/` ani Windows build.

Značky: **Naměřeno** (číslo z DEVLOGu nebo ze zdroje), **Odvozeno z kódu** (přečteno, nespuštěno),
**Hypotéza** (neověřeno).

## Shrnutí

Doporučení cituje kód přesně, všechna citovaná místa existují. Slabé je ve třech věcech:

1. Diagnóza klávesnice se v hlavním bodě rozchází s měřením v DEVLOGu (milestone 37).
2. Část „výkon“ je pravdivá, ale úspory jsou v setinách až desetinách procenta jádra.
3. Tabulka modelů má dvě vymyšlené položky a u hlavního kandidáta zamlčuje cenu.

Z celého seznamu stojí za práci čtyři věci, všechny jako měření přes existující reporty:
`vad_out` po domluvení, post-filter DFN3, 100 % wet RNNoise pro záchranu a VNNI jádro pro `in_net`.

## 1. Klávesnice s DeepFilterNet 3

| Tvrzení                                         | Verdikt                                       |
| ----------------------------------------------- | --------------------------------------------- |
| `ClickRescue` pouští klávesy vedle slov         | Mechanismus sedí, hlavní příčina to není      |
| Záchrana dostává 25 % surového signálu          | Pravda, levné vyzkoušet                       |
| Porovnání obálek jen pod 3,5 kHz                | Hypotéza bez dat, víc kódu                    |
| `RESCUE_SOFT_PROB` 0,15 -> 0,25–0,30            | Už změřeno a rozhodnuto, žádná nová data      |
| `vad_out` drží bránu otevřenou při psaní        | Pravda, nezměřeno, nejlepší kandidát          |
| Post-filter DFN3 je vypnutý                     | Pravda, popis účinku je nepřesný              |

### ClickRescue

- **Naměřeno** (milestone 37, `near_pause_report`): únik kláves 20–100 ms za slovem není od záchrany.
  S vypnutou záchranou vyšlo totéž v rozmezí 0,7 dB. Závěr tam zní „DFN3's leak next to words is the
  model's own“. Doporučení jmenuje záchranu jako první ze tří příčin a tohle měření neuvádí.
- **Naměřeno** (milestone 41): při současném mluvení a bušení do myši je záchrana zapnutá skoro pořád
  a kliky ztrácejí jen 3–6 dB. To je vědomé rozhodnutí „hlas před klávesami“, ne chyba.
- **Odvozeno z kódu**: `rnnoise.process(buf, strength, ...)` ve [mod.rs:645](../crates/gamersequalizer-mic-chain/src/ai_denoiser/mod.rs#L645)
  opravdu míchá `1 - strength` surového signálu i do větve záchrany, při výchozích 750 ‰ tedy 25 %.
  Když záchrana běží, RNNoise neslouží ničemu jinému (`rescue_on` vyžaduje `takeover.is_none()`),
  takže 100 % wet je změna jednoho argumentu.
- **Hypotéza**: přínos bude malý ve framech, kde zní hlas i klik zároveň (RNNoise tam klik skoro
  nebere, `click_report`: 0 % ztracených framů), a větší ve 100 ms držení po slabice.
- Pásmové porovnání pod 3,5 kHz stojí na tvrzení, že klik mechanického spínače má většinu energie nad
  3 kHz. Dopad klávesy na desku má energie hodně i pod 1 kHz. Bez spektra vlastních nahrávek to
  nedělat.
- `RESCUE_SOFT_PROB` 0,30: **naměřeno** v milestone 41. Druhý únik -44,8 -> -76,0 dBFS, ale konce slov
  21,3 -> 22,3 % a hlasitá okna bez hlasu 45,6 -> 43,2 dB. Doporučení cituje jen první číslo.

### `vad_out` po domluvení

- **Odvozeno z kódu** ([voice.rs:34-37](../crates/gamersequalizer-mic-chain/src/ai_denoiser/voice.rs#L34-L37),
  `frame_at`): po jednom framu nad 0,3 stačí každých 500 ms jeden frame nad 0,02 a `active_left` se
  natáhne znovu. Řetězí se to bez omezení. Komentář u konstant to přiznává („typing alone keeps it
  open most of the time“).
- **Naměřeno**: číslo „klávesy samotné otevřou bránu v 1,1 %“ (milestone 40) tenhle případ nepokrývá.
  Bez předchozí řeči se `active_left` nikdy nenabije.
- Konstanty vznikly před `in_net` (milestone 35) a Silero (milestone 40). Po milestone 40 „the gate
  itself now cuts next to nothing“, takže `vad_out` může být z velké části nadbytečný. **Hypotéza.**
- Zisk je omezený: zavřená brána tlumí o `GATE_FLOOR_DB` = 12 dB plus zdvih leveleru, ne o 40 dB.
- Měřicí přípravek existuje: `gate_sweep` má záznam `talk_then_type` a sloupec „open% while typing
  after speech“, jen dnes mění vstupní detektor (`isolator.vad`), ne výstupní.

### Post-filter

- **Odvozeno z kódu**: `post_filter: false` v [tract.rs:178](../crates/vendor/deep_filter/src/tract.rs#L178),
  `with_post_filter` existuje, volá se před limitem útlumu a jen když běží ERB stupeň. V DEVLOGu
  o něm není ani zmínka, vyzkoušený nebyl.
- Dopočet ze vzorce pro beta 0,02 (zisk masky -> další útlum):

  | Zisk masky | Útlum masky | Post-filter přidá |
  | ---------- | ----------- | ----------------- |
  | 0,5        | 6 dB        | 0,2 dB            |
  | 0,2        | 14 dB       | 1,5 dB            |
  | 0,13       | 18 dB       | 3,3 dB            |
  | 0,1        | 20 dB       | 5,0 dB            |
  | 0,03       | 30 dB       | 20 dB             |

- Nemíří tedy na „střední útlum“, jak doporučení píše, ale na biny už silně potlačené. V pásmu, kde
  DFN3 za slovem nechává 11–18 dB, přidá zhruba 1–3 dB. **Hypotéza** o riziku: stejně zeslabí slabé
  souhlásky a konce slov s maskou 0,1–0,2, a to je metrika, kde je DFN3 už dnes nejhorší
  (konce slov ztracené o > 6 dB: 23,2 %).

## 2. Detekce a filtrace

| Tvrzení                                       | Verdikt                                                  |
| --------------------------------------------- | -------------------------------------------------------- |
| `in_net` za AEC3 při hudbě skoro neslyší      | Pravda, v DEVLOGu vedeno jako otevřené (74 -> 8 %)       |
| Spánek modelu ignoruje Silero                 | Pravda, ale `activity` (12 dB nad šumem) ho drží vzhůru  |
| Posun `pre_gain` o 40 ms                      | Pravda, přechodná odchylka, neslyšitelná                 |
| Propad po „Learn again“                       | Hypotéza, neověřitelná čtením                            |
| Aliasing při decimaci 6×                      | Čísla sedí, problém nedoložen                            |
| Silero mimo 48 kHz                            | Pravda, v kódu označeno jako `shortcut`                  |

- **Silero jako záložní klíč leveleru při hudbě**: jediný bod této části s doloženým příznakem
  (`heard_voice` jen ve 26–44 % hlasových oken). Pozor na dvě věci z DEVLOGu: Silero dalo 0,20–0,30
  na zbytky kliků za AEC3 a milestone 64 vznikl právě proto, že leveler se učil z šepotu. Práh 0,85
  na dvou chuncích je rozumný začátek, ověřit přes `leveler_key_report` na šepotu i na nahrávce
  s hudbou.
- **`pre_gain`**: chyba je `g(t - 40 ms) / g(t)`. Zisk leveleru se hýbe v ~100 ms, takže při skoku
  o 6 dB je výstup asi 100 ms o 2,4 dB vedle. Opravovat až když to ukáže nahrávka.
- **Decimace**: boxcar přes 6 vzorků tlumí 4 kHz o 3,8 dB, 6 kHz o 10 dB, 7 kHz o 17 dB. Obě strany
  korelace procházejí stejnou decimací. Bez případu, kdy se zpoždění nezamklo, nedělat.
- **Silero mimo 48 kHz**: 44,1 -> 16 kHz není celočíselný poměr, „jednoduchý decimátor“ znamená
  převzorkování. Pro detektor stačí lineární interpolace, ale chce to test proti 48 kHz větvi.

## 3. Výkon

**Naměřeno** (milestone 72): celý denoiser s DFN3 na řeči 3,24 % jádra, DFN3 sám 1,54 %. Zbylých
1,7 % jsou detektory a RNNoise pro záchranu. V tom má doporučení pravdu.

| Návrh                                    | Pravda? | Odhad úspory                          | Verdikt                   |
| ---------------------------------------- | ------- | ------------------------------------- | ------------------------- |
| Alokace ve wrapperech kolem tract        | Ano     | ~0,005 % jádra                        | Vynechat                  |
| Sloučit `rescue.net` a `out_net`         | Ano     | ~0,26 % jádra, jen při řeči           | Malé, až po kroku 1 níže  |
| Přeskočit `dense_out` v `in_net`         | Ano     | ~1,7 % ceny `in_net`, ~0,02 % jádra   | Pár řádků, klidně         |
| VNNI jádro pro `rnnoise2`                | Ano     | až ~2× na int8 násobení               | Jediná větší úspora       |
| `rnnoise_little.bin` jen pro testy       | Ano     | 1,6 MB DLL, žádné CPU                 | Nejdřív změřit DLL        |
| FFT místo matice v Silero                | Ano     | ~39 % z ~0,7 M násobení na 32 ms      | Vynechat                  |

- **Alokace**: všech pět míst existuje ([lib.rs:342](../crates/vendor/deep_filter/src/lib.rs#L342),
  [lib.rs:379](../crates/vendor/deep_filter/src/lib.rs#L379), dva `clone` a `to_owned` v `tract.rs`,
  `Tensor::zero` v [dpdfnet.rs:196](../crates/gamersequalizer-mic-chain/src/dpdfnet.rs#L196)). Je to
  asi 500 alokací za sekundu vedle mnohem většího počtu uvnitř tract, který pravidla výslovně
  povolují. Jitter to nezmění a fork se vzdálí upstreamu.
- **Sloučení detektorů**: detektor musí zůstat před mixem záchrany. Za mixem by slyšel kliky, které
  záchrana pustila, a sám by ji držel zapnutou (komentář v `rescue.rs` popisuje stejnou past
  u vstupního detektoru).
- **VNNI**: dnešní AVX2 cesta je přesná přes rozšíření na i16 a `madd`. `vpdpbusd` (nesaturující)
  s posunem vstupu o 128 a předpočítanou korekcí na řádek zůstane bitově shodná. Pomůže jen Zen 4+
  a Alder Lake+, tedy strojům, které to potřebují nejméně. Notebook 7530U (Zen 3) nezíská nic.
  Ověřit, že toolchain má stabilní `avxvnni` intrinsics.
- **`rnnoise_little.bin`**: v produkci se volá jen `Model::Full`, ale `apo-dist` má fat LTO a blob
  mohl vypadnout sám. Stačí porovnat velikost DLL.

## 4. Nové modely

Ověřeno proti README, licencím a arXivu (2026-11-27).

| Tvrzení                                        | Verdikt                                                          |
| ---------------------------------------------- | ---------------------------------------------------------------- |
| FastEnhancer: ICASSP 2026, MIT, 48 kHz, ONNX   | Potvrzeno                                                        |
| FastEnhancer: „STFT 704 / hop 320, 14,7 ms“    | Špatně: FFT 1024, hop 320; 704 je zpoždění okno minus hop        |
| FastEnhancer: třetinová latence proti DFN3     | Přehnané: našim počítáním ~21 ms proti 40 ms                     |
| `faster-enhancer.c`, 432 KB stavu              | Existuje, od třetí strany, jen model M, licence neověřena        |
| GTCRN / H-GTCRN „48kHz adaptace“               | Nedoloženo: žádné 48 kHz váhy; H-GTCRN je dvoukanálový systém    |
| DPDFNet-4 48 kHz                               | Neexistuje: 48 kHz jsou jen `dpdfnet2` a `dpdfnet8`              |
| DPDFNet-4/8: „18–35 % jádra“                   | Nedoloženo: autoři CPU čísla nezveřejnili                        |
| TEN-VAD: licence s doložkou proti Agoře        | Potvrzeno                                                        |
| DFN3: 40 ms                                    | Potvrzeno z konfigurace modelu                                   |

Co doporučení o FastEnhanceru neříká, je cena. **Naměřeno autory** (48 kHz, jedno vlákno, Apple M5):

| Varianta | Parametry | MAC za sekundu | Podíl jádra M5 |
| -------- | --------- | -------------- | -------------- |
| T        | 28 K      | 0,18 G         | 1,2 %          |
| B        | 101 K     | 0,75 G         | 2,6 %          |
| S        | 207 K     | 1,8 G          | 4,2 %          |
| M        | 512 K     | 8,0 G          | 13,5 %         |
| L        | 1132 K    | 32,4 G         | 42 %           |

- Doporučená varianta M stojí řádově tolik co DPDFNet (9,2 % jádra 9800X3D), ne co DFN3 (1,54 %).
  Cenou odpovídá DFN3 varianta B. Int8 C runtime pro M udává 6,9 % jádra Apple M2.
- Hop 6,67 ms znamená 150 průchodů sítí za sekundu místo 100.
- **Hypotéza**, že kratší krok „méně rozmaže klik“, nemá v článku oporu; modely se hodnotí na
  VoiceBank-DEMAND a DNS, ne na klávesnicích.
- Neověřeno: které 48 kHz váhy jsou skutečně ve vydáních a jestli ONNX (GRU + attention) projde tract.
- DPDFNet-8 má 3× víc MAC než DPDFNet-2 (7,17 proti 2,42 G). Odhad z našeho měření: ~27 % jádra
  9800X3D. Závěr doporučení („ne jako výchozí“) platí.

## Postup

Pořadí podle poměru přínosu a práce. Kroky 1–3 jsou jen měření na Windows přes existující reporty.

1. **`vad_out` po domluvení.** V `gate_sweep` měnit `isolator.vad_out.keep_prob` a `active_frames`
   místo vstupního detektoru. Zkusit (50; 0,02) dnes, (20; 0,02), (20; 0,05), (15; 0,1) a `vad_out`
   úplně vypnutý. Sledovat „open% while typing after speech“ a ztracené tiché slabiky; k tomu
   `private_report` a `whisper_gate_report`. Kritérium: žádná slabika navíc, brána po řeči zavřená
   znatelně dřív. Když vyjde vypnutí, odpadá i jeden průchod RNNoise.
2. **Post-filter.** Dočasně `.with_post_filter(beta)` v [deepfilter.rs:30](../crates/gamersequalizer-mic-chain/src/ai_denoiser/deepfilter.rs#L30),
   beta 0,01 / 0,02 / 0,04. `near_pause_report`: sloupce 20 ms a 30–50 ms za slovem proti „Speech
   lost > 6 dB“ a „Tails lost > 6 dB“. Čekat 1–3 dB; když konce slov stoupnou o víc než bod, zahodit.
3. **Záchrana se 100 % wet.** Ve [mod.rs:645](../crates/gamersequalizer-mic-chain/src/ai_denoiser/mod.rs#L645)
   předat `1.0`, když je `rescue_on`. `click_report`, `keyboard_call_report`, `capture_replay_report`
   na `204900`. Kritérium: řečové framy ztracené o > 15 dB beze změny, kliky při řeči tišší.
4. **Leveler při hudbě z reproduktorů.** Záložní klíč ze Silero jen při aktivním AEC. Ověřit
   `leveler_key_report` (šepot nesmí přes 2 %) a nahrávkou `205013`.
5. **Výkon.** `dense_out` přeskočit, VNNI cestu přidat s testem bitové shody proti skalární. Sloučení
   detektorů řešit až podle výsledku kroku 1.
6. **FastEnhancer.** Offline, varianta B (cenou srovnatelná s DFN3), případně S. Nejdřív zjistit,
   jestli 48 kHz ONNX existuje ke stažení a jestli ho načte tract. Pak tytéž reporty jako pro DFN3.
   M jen jako srovnání s DPDFNet.

Vynechat: alokace ve wrapperech, FFT v Silero, antialiasing před decimací, `pre_gain` historii,
pásmovou záchranu, GTCRN, TEN-VAD, DPDFNet-8.
