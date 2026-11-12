# Jde zrychlit ekvalizér a ostatní filtry?

Stav 2026-11-11, větev `sidetone`. Měřeno na Macu (Apple M4 Pro, arm64). Na Windows (x86-64) je
potřeba měření zopakovat, viz [Postup](#postup).

## Závěr

- **EQ zrychlit jde, 2× až 3,7×, a stačí na to obyčejný Rust**: jiné pořadí smyček. Asm ani SIMD
  k tomu nejsou potřeba.
- **V absolutních číslech je to málo.** Mikrofonní EQ dnes stojí 0,11–0,17 % jednoho jádra,
  výstupní 0,43 %. Celý mikrofonní řetězec bere podle logu 2–24 % času bloku (podle modelu), takže
  EQ je zhruba jeho setina.
- **Jediné místo, kde se to sčítá, je EQ proudů**: až 64 biquadů stereo = 1,4 % jádra na každý
  přehrávající proud.
- **Ostatní vlastní filtry** (limiter, leveler, expandér, detektor ořezu) jsou tak levné, že na
  nich není co získat. Převzorkování, FFT a AEC3 už SIMD mají ve svých knihovnách.

## Naměřeno

`cargo run --release -p gamersequalizer-common --example eq_bench`, blok 480 vzorků (10 ms),
bílý šum −20 dBFS, nejlepší ze 7 běhů po 2000 blocích. Procenta = podíl z 10 ms, tedy z jednoho jádra.

| Případ                         | Dnes              | Nejlepší beze změny výstupu | Nejlepší s f64 mezi stupni |
| ------------------------------ | ----------------- | --------------------------- | -------------------------- |
| Mikrofonní EQ, mono, 10 pásem  | 10,8 µs (0,11 %)  | 5,0 µs (2,2×)               | 3,5 µs (3,1×)              |
| Mikrofonní EQ, mono, 16 pásem  | 17,3 µs (0,17 %)  | 8,1 µs (2,1×)               | 5,3 µs (3,3×)              |
| Výstupní EQ, stereo, 20 pásem  | 43,0 µs (0,43 %)  | 18,3 µs (2,3×)              | 11,5 µs (3,7×)             |
| EQ proudu, stereo, 64 pásem    | 138,3 µs (1,38 %) | 76,3 µs (1,8×)              | 44,5 µs (3,1×)             |
| Limiter, stereo, tichý signál  | 2,6 µs (0,03 %)   | neměněno                    | neměněno                   |
| Limiter, stereo, při omezování | 3,0 µs (0,03 %)   | neměněno                    | neměněno                   |

Co které varianty znamenají:

| Varianta         | Popis                                                                                   |
| ---------------- | --------------------------------------------------------------------------------------- |
| Dnes             | Celý blok projde stupněm 1, pak stupněm 2 atd.; kanály jeden po druhém.                 |
| Po vzorcích      | Jeden vzorek projde všemi stupni, pak další vzorek.                                     |
| Oba kanály naráz | Levý a pravý kanál v jednom průchodu (stejné koeficienty, vlastní paměť filtru).        |
| f64 mezi stupni  | Bez dnešního zaokrouhlení na f32 mezi stupni; výstup se liší nejvýš o 1e-7 (−140 dBFS). |
| Dva stupně naráz | Druhý stupeň běží o vzorek za prvním. Nepřineslo nic (0,95×).                           |

Dnes se výsledek každého stupně ukládá do bufferu jako f32. Varianta „f64 mezi stupni“ je proto
přesnější než dnešní kód, ale už ne bit po bitu stejná.

Nejlepší varianta beze změny výstupu: mono „po vzorcích“, stereo 20 pásem „po vzorcích, oba kanály
naráz“, stereo 64 pásem „po stupních, oba kanály naráz“. U všech vyšel rozdíl proti dnešku přesně 0.

Zátěž celého mikrofonního řetězce pro srovnání:

- `docs/gamersequalizer (5).log`, 2026-11-07: `DSP load peak` 3–24 %, `avg max` 2–20 % podle
  zvoleného modelu; jednou 661 % s DPDFNet (opožděný blok).
- `DEVLOG.md`: DFN3 5 %, DPDFNet 13 % (medián na blok); RNNoise 0.2 Full asi 2,8 % jádra na Windows.

## Odvozeno z kódu

- **Proč je dnešní smyčka pomalá** ([biquad.rs:198-206](../crates/gamersequalizer-common/src/biquad.rs#L198-L206)):
  uvnitř jednoho stupně musí každý vzorek počkat na paměť filtru z předchozího vzorku
  (`z1` → `y` → `z1`). Procesor tak počítá jediný řetěz sčítání → násobení → odčítání a zbytek
  jeho výpočetních jednotek stojí. Vychází 2,3 ns na vzorek a stupeň. Když jde vzorek všemi
  stupni, čeká se jen na násobení a sčítání a paměti filtrů se dopočítávají souběžně; dva kanály
  naráz dávají dva nezávislé řetězy. Nejlepší varianta má 0,7 ns na vzorek a stupeň.
- **Jedno místo pro všechny**: mikrofonní EQ, horní propust před řetězcem, ořezy a EQ příposlechu,
  EQ proudů i výstupní EQ volají `BiquadCascade::process_strided`. Změna pořadí smyček v něm
  zrychlí všechno najednou.
- **„Oba kanály naráz“ chce změnu rozhraní**: dnes má každý kanál vlastní kaskádu se stejnými
  koeficienty ([lib.rs:835-849](../crates/gamersequalizer-apo/src/lib.rs#L835-L849),
  [lib.rs:979-986](../crates/gamersequalizer-apo/src/lib.rs#L979-L986)). Potřebná je jedna kaskáda
  s pamětí pro každý kanál.
- **Ostatní filtry**:

| Filtr                                           | Stav                                                       | Co s tím          |
| ----------------------------------------------- | ---------------------------------------------------------- | ----------------- |
| `PeakLimiter` + `soft_limit`                    | 2,6 µs na stereo blok (změřeno)                            | Nic               |
| Leveler, expandér, detektor ořezu, předzesílení | Jedna jednoduchá smyčka na vzorek (neměřeno)               | Nic               |
| Převzorkování (`rubato`), FFT (`rustfft`)       | Vlastní SIMD s detekcí procesoru za běhu                   | Hotovo v knihovně |
| AEC3 (`sonora`)                                 | Má vlastní crate `sonora-simd`                             | Hotovo v knihovně |
| RNNoise 2017 (`nnnoiseless`)                    | Žádné ruční SIMD; 39 µs na 10 ms blok (arm64, `DEVLOG.md`) | Viz hypotéza 3    |

## Hypotéza

1. **Na x86-64 vyjde poměr jinak.** Aplikace se překládá pro základní x86-64 (SSE2, bez FMA).
   Převody f32 ↔ f64 jsou tam dražší než na M4, takže varianty „beze změny výstupu“ mohou získat
   méně a varianta s f64 mezi stupni relativně víc. Neměřeno.
2. **SIMD navíc by přidalo nejvýš dalších 2× u sterea a nic u mona.** Šlo by dát levý a pravý kanál
   do jednoho vektoru 2 × f64 (SSE2, bez detekce procesoru). U výstupního EQ by to bylo asi
   0,06 % jádra a potřebovalo by `unsafe`, které má workspace zakázané. Nestojí to za to. Neměřeno.
3. **Jestli má smysl ruční SIMD ještě někde, pak v `nnnoiseless`**: běží několikrát na blok
   (filtr, výplň pauz, `out_net`, `ClickRescue`) a ruční SIMD nemá. To je ale model, ne filtr,
   a stejná práce jako u jádra `rnnoise2`. Neměřeno, kolik z bloku dnes skutečně bere.

## Postup

1. Na Windows spustit `cargo run --release -p gamersequalizer-common --example eq_bench` a
   porovnat s tabulkou výše.
2. Pokud i tam vyjde aspoň 1,5×: v `process_strided` přehodit smyčky na „po vzorcích“ se
   zachovaným zaokrouhlením na f32. Výstup zůstane bit po bitu stejný, takže stávající testy
   platí beze změny. Je to asi 10 řádků v jednom souboru.
3. Volitelně pro EQ proudů a výstupní EQ: jedna kaskáda s pamětí pro každý kanál a průchod oběma
   kanály naráz. Zasáhne `StreamProcessor` a `OutputProcessor` v `gamersequalizer-apo/src/lib.rs`.
4. f64 mezi stupni až nakonec a jen když bude potřeba: mění výstup v řádu 1e-7, takže je předem
   nutné projít testy, které porovnávají s uloženými referencemi.
5. Asm ani SIMD pro EQ nedělat.

Na co při úpravě nezapomenout:

- Zachovat nulování paměti filtru po bloku, když v ní není konečné číslo
  ([biquad.rs:207-213](../crates/gamersequalizer-common/src/biquad.rs#L207-L213)). Kandidáti
  v benchmarku ho nemají, což jim dává výhodu pod 1 %.
- Benchmark běží v profilu `release`, APO se dodává v `apo-dist` (fat LTO), a to v procesu
  `audiodg`. Poměry se proto mohou mírně lišit i na stejném procesoru.
- Stereo varianty v benchmarku mají pevný krok 2, dnešní kód krok podle počtu kanálů za běhu.
  Čistě pořadí smyček porovnává jen dvojice „dnes“ a „po vzorcích“.

Benchmark je v `crates/gamersequalizer-common/examples/eq_bench.rs` (nový soubor). Kandidátní
smyčky jsou zatím jen v něm, kód aplikace se nezměnil.
