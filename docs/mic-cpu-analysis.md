# CPU mikrofonního řetězce: kam jde čas a co s tím

Odpověď na `docs/merge.md` a na otázku, proč je aplikace na notebooku s Ryzenem 7 6800H tak náročná.
Pracovní dokument: po zapracování smazat a podstatná čísla přenést do `DEVLOG.md`.

## Závěr

1. **Režie knihovny v DFN3 není.** 95 % času jednoho rámce jsou vrstvy modelu, z toho většina násobení
   matic a GRU. Fork tractu ani ruční přepis inference proto nic podstatného nepřinese.
2. **Největší úspora je práci nedělat:** v rámcích bez hlasu vynechat dekodér hlubokého filtrování DFN3
   (−32 % času DFN3), případně oba dekodéry (−58 %). Kodér běží dál, takže model zůstává zahřátý.
3. **Záchrana kliknutí má běžet jen při hlasu.** Mimo hlas její dva průchody RNNoise nic neslyšitelného
   nedělají. Ušetří dvakrát víc než sloučení detektorů z `merge.md` a z velké části ho nahrazuje.
4. **Zhoršení po sloučení `out_net` má jinou příčinu, než `merge.md` hádá.** Zhoršil se i řádek s vypnutým
   spánkem, kde žádné předávání neproběhne. Jistý rozdíl v kódu je, že brána po sloučení slyší signál před
   příměsí RNNoise, dřív ho slyšela po ní.
5. **Měření na vývojovém PC je pro notebook příliš optimistické.** Tract tam jede na AVX-512, které 6800H
   nemá, a obří cache 9800X3D skrývá, že DFN3 se studenou cache trvá dvakrát déle.
6. **ONNX Runtime už u DFN3 rychlejší není**, tract 0.23 ho dohnal. U DPDFNetu vede o 25 % a celý
   rozdíl je v jedné operaci tractu (`GruSeq`). To je jediné místo, kde má úprava tractu smysl.

## 1. Naměřeno

Měřeno na Macu (Apple M4 Pro, ARM), tract 0.23.10, náš fork `deep_filter` s prahy aplikace (−20 / 25 / 20),
`speech_clean.wav` a klávesy z `test_data/denoise`, 1059 rámců po 10 ms. 100 µs na rámec je 1 % jádra.
Platí **poměry**, absolutní čísla na x86 budou jiná. Jak měření zopakovat na Windows, je v příloze.

### DFN3: podíl částí na jednom rámci (jen klávesy, běží všechny tři části)

| Část                             | µs na rámec | Podíl | Váhy modelu |
| -------------------------------- | ----------- | ----- | ----------- |
| Kodér                            | 75          | 38 %  | 1,95 MB     |
| Dekodér ERB masky                | 57          | 29 %  | 3,29 MB     |
| Dekodér hlubokého filtrování     | 57          | 29 %  | 3,34 MB     |
| STFT, příznaky, masky, ISTFT     | 8           | 4 %   | –           |

Uvnitř modelů: `OptMatMul` 50–71 %, `Scan` (GRU) 9–24 %, konvoluce 9–14 %. Pomocné operace (změny tvaru,
posuny os) jsou pod 5 %. Graf má po optimalizaci 82 + 68 + 38 uzlů a rámec jich zavolá jen 125.

### DFN3: co ušetří vynechání dekodérů (jen klávesy)

| Co běží                    | Teplá cache | Studená cache |
| -------------------------- | ----------- | ------------- |
| Všechny tři části          | 199 µs      | 375 µs        |
| Kodér + dekodér ERB        | 134 µs      | 267 µs        |
| Jen kodér (nulová maska)   | 83 µs       | 159 µs        |

### Vliv studené cache (před každým rámcem přepsáno 256 MB paměti)

| Model         | Teplá   | Studená | Zpomalení |
| ------------- | ------- | ------- | --------- |
| DFN3          | 174 µs  | 345 µs  | 1,9–2,2×  |
| DPDFNet       | 1180 µs | 1460 µs | 1,25×     |
| RNNoise 2017  | 29 µs   | 39 µs   | 1,3×      |

### DPDFNet

1180 µs na rámec, tedy 6× víc než DFN3. `GruSeq` 42 % (8 volání po 62 µs), `OptMatMul` 36 % (92 volání),
pomocné operace kolem 6 %. Graf je jeden celek (493 uzlů ONNX), části vynechat nejde.

### ONNX Runtime proti tractu (stejný Mac, jedno vlákno, jeden rámec na volání)

ONNX Runtime 1.32.0 z Pythonu, režie volání 2,5 µs. Grafy DFN3 dostaly vstup o délce jednoho rámce bez
přenášení stavu, což dá stejnou nebo menší práci než proudové zpracování v tractu.

| Model                    | tract 0.23.10 | ONNX Runtime | Rozdíl             |
| ------------------------ | ------------- | ------------ | ------------------ |
| DFN3 kodér               | 75 µs         | 79 µs        | tract o 5 % lepší  |
| DFN3 dekodér ERB         | 57 µs         | 63 µs        | tract o 10 % lepší |
| DFN3 dekodér hl. filtr.  | 57 µs         | 60 µs        | tract o 4 % lepší  |
| DPDFNet                  | 1180 µs       | 890 µs       | ORT o 25 % lepší   |

Rozdíl u DPDFNetu je v jedné operaci. Model má čtyři obousměrné GRU přes kmitočtovou osu (skrytá
velikost 64). Tract je počítá jako 8 volání `GruSeq`, dohromady 497 µs. Profil ONNX Runtime jim dává
24 % času, po přepočtu na běh bez profilování asi 210 µs. Ostatní vrstvy (násobení matic, konvoluce)
vycházejí v obou knihovnách podobně.

## 2. Proč je notebook o tolik pomalejší než vývojové PC

Čísla z `merge.md`: celý odšumovač s DFN3 stojí 3,68 % jádra na vývojovém PC a asi 27 % u testera, s
DPDFNetem 10,71 % a asi 47 %.

| Model   | Vývojové PC | Tester | Poměr |
| ------- | ----------- | ------ | ----- |
| DFN3    | 3,68 %      | 27 %   | 7,3×  |
| DPDFNet | 10,71 %     | 47 %   | 4,4×  |

Takt procesoru by oba modely zpomalil stejně. Nezpomalil, takže hraje roli něco, co závisí na modelu.

- **Odvozeno z kódu: AVX-512.** `tract-linalg` 0.23.10 má jádra pro násobení matic v `x86_64/fma` i
  `x86_64/avx512` a vybírá je za běhu (`mmm.rs:638`, `is_x86_feature_detected!("avx512f")`). 9800X3D
  (Zen 5) AVX-512 má, 6800H (Zen 3+) ne. Tvrzení z `merge.md`, že tract volí SIMD za běhu, tedy platí, jenže
  vývojové PC tím měří rychlejší jádra, než jaká poběží na notebooku. RNNoise, Silero a AEC3 AVX-512
  nepoužívají, proto se jich to netýká.
- **Naměřeno: studená cache.** DFN3 se studenou cache trvá 1,9–2,2× déle, DPDFNet 1,25×. Poměr těch dvou
  čísel (asi 1,6) odpovídá tomu, o kolik hůř vychází u testera DFN3 než DPDFNet (7,3 / 4,4 = 1,66). DFN3
  projde za rámec 8,6 MB vah. 6800H má 512 kB L2 na jádro a 16 MB L3 sdílených s hrou, 9800X3D má 96 MB L3.
  Bench navíc počítá rámce těsně za sebou, naživo je mezi nimi 10 ms, kdy na jádře běží něco jiného.
- **Hypotéza: takt.** Zvukové vlákno pracuje 1–3 ms z každých 10 ms. Správa napájení notebooku takové
  jádro nemusí vytočit na plný takt a měřič počítá čas na hodinách, ne odvedenou práci.

Shoda poměrů u cache je nepřímý důkaz na jiném procesoru. Ověření je v postupu, kroky 1 a 2.

## 3. Nápady podle přínosu

Úspory jsou v bodech jednoho jádra na vývojovém PC a platí pro stav „model bdí, hlas není" (psaní,
myš, zvuky hry). To je u hráče většina času, protože aktivita model budí i bez hlasu.

| Změna                                      | Úspora      | Z 3,68 % | Riziko                                   |
| ------------------------------------------ | ----------- | -------- | ---------------------------------------- |
| A1. DFN3 bez dekodéru hlubokého filtrování | asi 0,6     | 16 %     | Nízké, ERB maska hlas dál propouští      |
| A2. DFN3 jen s kodérem (nulová maska)      | asi 1,1     | 30 %     | Uříznuté náběhy slabik                   |
| B. Záchrana kliknutí jen při hlasu         | 0,54        | 15 %     | První slovo po pauze bez záchrany        |
| Sloučení `out_net` (z `merge.md`)          | 0,27        | 7 %      | Naměřené zhoršení pauz                   |

A1 a B dohromady dají asi 1,1 bodu z 3,68. Přepočet na notebook stejným podílem je odhad: zhruba 8 z 27 %.

### A. DFN3: dekodéry jen tam, kde je hlas

**Odvozeno z kódu.** Upstream už části podle místního SNR vynechává (`apply_stages` ve forku,
`tract.rs:648`). Nad 20 dB neběží hluboké filtrování, nad 25 dB ani ERB maska. V měření proto dekodér
hlubokého filtrování běžel jen v 69 % rámců řeči s klávesami a ve 37 % rámců čisté řeči. Model tedy
snáší, že se dekodér uprostřed řeči vynechává a jeho GRU stav zastarává.

Spodní práh (−10 dB, pod ním nulová maska) je vypnutý, protože samotné SNR pod hlasitými klávesami
mazalo 16–26 % tichých slabik (`deepfilter.rs`). Chybí mu to, co řetězec má: dva detektory hlasu na
surovém vstupu (`in_net`, Silero), které běží před `df.process` ve stejném bloku, a jsou tedy zarovnané
s rámcem, který DFN3 právě počítá.

- **A1 (doporučuji).** Když detektory na vstupu neslyšely hlas déle než dobu držení brány, DFN3 vynechá
  jen hluboké filtrování. ERB maska běží dál a slabý hlas, který detektory přeslechnou, propustí jako
  dnes. Mění se jen to, čím se v pauzách tlumí pásmo do 4,8 kHz: maskou místo filtru. Jestli to pauzy
  nezhorší, řeknou `click_report`, `near_pause_report` a `pause_burst_report`.
- **A2 (jen když A1 projde a chce se víc).** Navíc nulová maska, tedy vynechat i dekodér ERB. V pauzách
  tlumí klávesy lépe než model, ale detektor potřebuje dva rámce (`VOICE_MIN_FRAMES`), takže první
  20 ms slabiky by šlo do nuly. A slabika, kterou dnes brána pustí o 12 dB tišeji, by zmizela celá.
  Rozhodne `whisper_gate_report` a `keyboard_call_report`.

Změna je malá: příznak „hlas není" předaný z `AiVoiceIsolator::process` do `DfTract` a jedna podmínka v
`apply_stages`. Vynechané dekodéry zároveň nečtou 3,3 MB (A1) nebo 6,6 MB (A2) vah z 8,6 MB, takže na
notebooku se studenou cache pomůžou stejným dílem (tabulka v kapitole 1).

### B. Záchrana kliknutí jen při hlasu

**Odvozeno z kódu.** `ClickRescue` míchá RNNoise jen tehdy, když Silero slyší hlas (`voice_now`). Přesto
RNNoise na kopii vstupu i detektor záchrany běží celou dobu, co DFN3 bdí (kroky 4 a 6 v `merge.md`).

Stejný čítač jako v A („hlas nezazněl N sekund") může oba průchody vypnout. Když se hlas vrátí, RNNoise
se spustí znovu (`rnnoise.restart()`) a záchrana počká na jeho zahřátí (`STAND_IN_WARMUP_SAMPLES`,
300 ms). Cena: prvních 300 ms prvního slova po dlouhé pauze je bez záchrany.

### C. Sloučení `out_net` a detektoru záchrany

**Naměřeno v `merge.md`, příloha D.** Řádek `20261109-210655 DeepFilterNet sleep false ... (wakes 0)` se
zhoršil stejně jako řádek se spánkem: z 5 na 6 oken, nové okno ve 42,0 s. S vypnutým spánkem neproběhne
žádné předání, takže hypotézy 1 (historie GRU z předávání) a 2 (zbytek ve FIFO přes spánek) tento řádek
vysvětlit nemůžou.

**Odvozeno z kódu.** Před změnou běžel `out_net` až za `rescue.process`, slyšel tedy výstup včetně
přimíchaného RNNoise. Po změně běží před ním a slyší čistý výstup DFN3. Klíč brány `voice_out` se tak
počítá z jiného signálu. Jeden průchod nemůže slyšet oba, takže to není chyba ve sloučení, ale jeho
podstata. Vedlejší kandidát: `out_net` běží už při načítání modelu na výstupu RNNoise, starý detektor
záchrany ne.

**Doporučení.** Nechat vrácené. Bod B vypne detektor záchrany mimo hlas úplně, sloučení by pak šetřilo
jen při řeči. Kdo chce příčinu potvrdit: v obou sestaveních vypsat po blocích `voice_out` a
`rescue.hold_left > 0` kolem 41,5–42,1 s té nahrávky. Když se liší jen `voice_out`, je to ono.

### D. Denormální čísla (pojistka, ne vysvětlení)

**Odvozeno z kódu.** Nikde v `crates/` se nenastavuje MXCSR (FTZ / DAZ). Na x86 stojí operace s
denormálním číslem řádově sto cyklů.

**Hypotéza.** Hrozí při přesném digitálním tichu (vypínač mikrofonu na sluchátkách posílá nuly): stavy
rekurzivních filtrů a obálek pak dobíhají přes denormální rozsah. DFN3 má vlastní přeskok tichých
rámců, RNNoise, AEC3 a obálky ne. U testera s vestavěným mikrofonem to příčina nejspíš není.

Ověření: do `cpu_breakdown_report` přidat případ „2 s řeči, pak 10 s přesných nul" a porovnat nejdelší
blok. Když vyskočí, nastavit FTZ a DAZ na vstupu `MicChain::process` a na výstupu vrátit původní
hodnotu. Když ne, nic nedělat.

### E. DPDFNet

Části modelu vynechat nejde, je to jeden graf. Jediné místo, kde je tract měřitelně pomalejší než
ONNX Runtime, je `GruSeq` (`tract-core/src/ops/gru_seq.rs`): 497 µs proti asi 210 µs, tedy až 24 % času
DPDFNetu na Macu.

**Odvozeno z kódu.** `GruSeq` je už specializovaná operace: matice R je předbalená a vstupní součin se
počítá jednou pro celou sekvenci. V každém kroku ale přebalí stav a pustí obecné jádro pro násobení
matic (`mmm.run` se dvěma `FusedSpec`) na součin 192 × 64 s jedním sloupcem. U takhle malé matice
převažuje příprava volání nad samotným výpočtem. Cíl případné úpravy je tedy úzký: v kroku GRU pro
malou skrytou velikost počítat součin matice s vektorem přímo, bez `mmm.run`.

**Hypotéza.** Na x86 může poměr vyjít jinak, jádra obou knihoven jsou tam jiná. Než se do toho pustit,
změřit na Windows (postup, krok 3). Úprava patří spíš do upstreamu tractu než do našeho forku: je to
jedna funkce a nese ji test `matches_the_step_by_step_recurrence`.

## 4. Odpovědi na otázky z `merge.md`, kapitola 5

1. **Jsou hypotézy věrohodné?** Pro řádek s vypnutým spánkem ne, viz C. Chybějící příčina je signál před
   příměsí a po ní.
2. **Jde sloučení zachránit?** Varianty a, b, c míří na stav sítě, který příčinou není. Varianta d
   (přijmout zhoršení) je jediná poctivá, ale bod B je lepší obchod.
3. **Jiná sloučení?** Zbrojit záchranu z RNNoise na surovém vstupu už bylo změřeno jako špatné a platí
   to. Místo slučování detektorů vypínat práci bez hlasu (A, B). Přeskakování rámců DFN3 v krátkých
   pauzách je přesně A.
4. **Lepší metrika pauz?** Počet oken nad −80 dBFS je citlivý na jedno okno (5 proti 6). Stabilnější je
   energie nad prahem sečtená přes všechny pauzy nahrávky, po zesílení levelerem, v dB. Jedno číslo na
   nahrávku se pak dá porovnat i s rozptylem mezi nahrávkami.

## 5. Co nedělat

- **Fork tractu nebo ruční přepis kvůli DFN3.** Pomocné operace jsou pod 5 %, zbytek jsou jádra pro
  násobení matic, která tract na AVX2 má, a ONNX Runtime na nich dnes rychlejší není (kapitola 1).
  Dřívější náskok ONNX Runtime smazal přechod na tract 0.23 (DFN3 tehdy 2,97–3,07 → 1,95–2,21 % jádra).
  Řádový skok by dala jen 8bitová kvantizace vah, jak ji má RNNoise 0.2 (a náš `rnnoise2`). To znamená
  ruční port, ověření kvality a týdny práce kvůli položce, která na vývojovém PC stojí 1,94 %.
- **ONNX Runtime v APO.** Platí, co jsme zjistili na začátku: velká knihovna v C++ s vlastními vlákny
  a alokacemi uvnitř audiodg. Slouží jen jako měřítko, kde má tract rezervu.
- **RNNoise 0.2 „Little" jako `in_net`.** Už změřeno v `rnnoise2/README.md`: na samotných klávesách hlásí
  hlas v 18 % rámců, Full v 1,3 %.
- **`target-cpu`, PGO.** Jádra se vybírají za běhu a kód mimo ně je pár procent.
- **Vypnout Control Flow Guard.** Rámec DFN3 vyhodnotí 125 uzlů, tedy stovky nepřímých volání po
  jednotkách nanosekund. Proti 200 µs rámce je to neměřitelné.

## 6. Postup

1. **Vývojové PC bez AVX-512.** `$env:TRACT_CPU_ISA = "-avx512f,-avx512vnni,-avx512fp16"` a pustit
   `cpu_breakdown_report`. Knoflík je v `tract-linalg/src/isa.rs`, na Windows jsem ho nespouštěl. Výsledek
   je číslo, které se dá čekat na procesoru s AVX2.
2. **Studená cache na vývojovém PC.** Do `cpu_breakdown_report` přidat variantu, která před každým
   blokem přepíše 256 MB paměti a měří jen blok. Když DFN3 vyjde kolem dvojnásobku, hypotéza z kapitoly 2
   platí i na x86.
3. **ONNX Runtime proti tractu na x86.** Skript z přílohy pustit na vývojovém PC (venv s `onnxruntime`
   a `numpy`), jednou normálně a tract jednou s vypnutým AVX-512 z kroku 1. Když ONNX Runtime u DPDFNetu
   vede i tam, stojí za to upravit `GruSeq` (kapitola 3, E). Když ne, téma je uzavřené.
4. **Log od testera.** V jednom sezení přepnout RNNoise, DFN3, DPDFNet, vždy minutu mluvit a minutu
   psát, a poslat log (`load avg` už v něm je). Do `[ApoCheck]` přidat jeden řádek: název procesoru,
   AVX-512 ano / ne, síť nebo baterie. Dnes to log neříká.
5. **A1** ve forku a v `ai_denoiser/mod.rs`, pak brána `scripts/check.ps1` a zprávy `click_report`,
   `keyboard_call_report`, `near_pause_report`, `whisper_gate_report`, `pause_burst_report`,
   `capture_replay_report`. Platí pravidlo z `merge.md`: zhoršení znamená vrátit.
6. **B** stejným čítačem, stejné zprávy, hlavně `click_report` a `keyboard_call_report`.
7. **A2** jen po úspěchu A1.
8. **D**: jeden případ v benchi, rozhodnout podle něj.

Odkazy na kód (čísla řádků, názvy zpráv a konstant) jsou ověřené proti větvi `fix/pause-hiss` ve stavu
`4005f0f`. Měření běželo nad forkem `deep_filter` a modely, které jsou v obou větvích stejné.

## Příloha: jak změřit části DFN3 a operace tractu

Tract umí spustit plán s vlastní funkcí pro každý uzel. Stačí dočasně nahradit tři volání
`self.enc.run(..)`, `self.erb_dec.run(..)` a `self.df_dec.run(..)` ve forku (`tract.rs`,
`process_raw`) tímto pomocníkem a sečíst časy podle `node.op().name()`:

```rust
fn timed_run(m: &mut TractModel, inputs: TVec<TValue>, ops: &mut Vec<(String, u64)>) -> TractResult<TVec<TValue>> {
    m.run_plan_with_eval(inputs, |ctx, state, node, input| {
        let t = Instant::now();
        let r = tract_core::plan::eval(ctx, state, node, input);
        ops.push((node.op().name().to_string(), t.elapsed().as_nanos() as u64));
        r
    })
}
```

Vynechání dekodérů se pro měření vynutí prahy: `with_thresholds(-20.0, 100.0, -100.0)` nechá běžet kodér
a dekodér ERB, `with_thresholds(100.0, 100.0, 100.0)` jen kodér. Pro DPDFNet stejný háček nad plánem z
`DpdfEngine::try_load`.

### ONNX Runtime na stejných modelech

DFN3: rozbalit `DeepFilterNet3_onnx.tar.gz` a skriptu dát `enc.onnx`, `erb_dec.onnx`, `df_dec.onnx`.
DPDFNet: `crates/gamersequalizer-mic-chain/models/dpdfnet2_48khz_hr.onnx`; u něj se má stav z výstupu
vracet na vstup, pro čas jednoho rámce to ale stačí i takto.

```python
import sys, time
import numpy as np
import onnxruntime as ort

o = ort.SessionOptions()
o.intra_op_num_threads = o.inter_op_num_threads = 1
rng = np.random.default_rng(1)
for path in sys.argv[1:]:
    s = ort.InferenceSession(path, o, providers=["CPUExecutionProvider"])
    feed = {i.name: (rng.standard_normal([d if isinstance(d, int) else 1 for d in i.shape]) * 0.1).astype(np.float32)
            for i in s.get_inputs()}
    for _ in range(200):
        s.run(None, feed)
    t = time.perf_counter()
    for _ in range(2000):
        s.run(None, feed)
    print(f"{path}: {(time.perf_counter() - t) / 2000 * 1e6:.1f} us/frame")
```

Podíl operací dá vestavěný profil: `o.enable_profiling = True`, po běhu `s.end_profiling()` vrátí cestu
k JSON, kde se sečtou události `*_kernel_time` podle `args.op_name`.
