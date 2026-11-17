# CPU mikrofonního řetězce: kam jde čas a co s tím

Odpověď na `docs/merge.md` a na otázku, proč je aplikace na notebooku s Ryzenem 7 6800H tak náročná.
Pracovní dokument: po zapracování smazat a podstatná čísla přenést do `DEVLOG.md`.

## Závěr

> Ověřeno 2026-11-16 na vývojovém PC, viz kapitola 7. Platí body 1, 3 (zapracováno), 4 a 6 jen pro DFN3.
> Bod 2 neprošel zprávami kvality, bod 5 neplatí pro AVX-512, ale platí pro studenou cache.
> Bod 7 přibyl 2026-11-17 a je nasazený (kapitola 8); body 1 a 6 upřesňuje.

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
7. **Váhy modelů v f16 zlevní DFN3 o 12–16 %** (vývojové PC, teplá i studená cache; celý odšumovač
   o 8–9 %), kvalita beze změny. Šetří se čtení vah, ne výpočet, proto DPDFNet (čas v `GruSeq`) jen
   0–5 %. Na AVX-512 to chtělo vlastní kernel a opravu výběru kernelu ve forku tractu: režie knihovny
   to není (bod 1 platí), chyběl jen kernel pro f16 váhy.

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

## 7. Ověřeno na vývojovém PC (Ryzen 7 9800X3D, 2026-11-16)

Kroky 1, 2, 3, 5–8 postupu a měření na notebooku se Zenem 3. Krok 4 (log od testera) zbývá. Čísla jsou v % jednoho jádra,
`cpu_breakdown_report` (přibyly v něm `COLD_CACHE=1` a řádky „keys only“), opakovaný běh se liší o ±3–5 %.

### Kroky 1 a 2: AVX-512 téměř nic, studená cache ano

`TRACT_CPU_ISA=-avx512f,-avx512fp16` funguje i na x86: tract pak volí `fma_mmm_f32_64x1` místo
`avx512_mmm_f32_128x1` (ověřeno přes `MmmDispatch::native().pick`).

| Část                       | AVX-512, teplá | AVX2, teplá | AVX-512, studená | AVX2, studená |
| -------------------------- | -------------- | ----------- | ---------------- | ------------- |
| DFN3 samotný               | 1,88           | 2,02        | 3,77             | 4,55          |
| DPDFNet samotný            | 9,57           | 10,11       | 12,74            | 14,00         |
| Celý odšumovač, DFN3       | 3,54           | 3,65        | 6,30             | 6,69          |
| Celý odšumovač, DPDFNet    | 11,06          | 11,36       | 14,96            | 15,58         |

- Bez AVX-512 je DFN3 pomalejší jen o 5–7 %. I bez AVX2 a FMA (jen AVX) vyjde stejně: násobení matice
  jedním sloupcem čeká na váhy z paměti, šířka instrukcí nerozhoduje. Bod 5 závěru tu neplatí.
- Studená cache: DFN3 2,0×, DPDFNet 1,33× (Mac 1,9–2,2× a 1,25×). S AVX2 2,4× a 1,46×, jejich poměr
  1,65 sedí na rozdíl u testera (1,66). Zbylý asi trojnásobek je u obou modelů stejný: takt a jádro
  notebooku (hypotéza z kapitoly 2), potvrdí až krok 4.

### Pauza mezi bloky stojí skoro jako studená cache

`PACED=1`: jádro po každém bloku 8 ms čeká, jako při živém zvuku. Stejný běh, AVX-512:

| Část                           | Teplá, bez pauz | S pauzou 8 ms | Studená cache |
| ------------------------------ | --------------- | ------------- | ------------- |
| DFN3 samotný                   | 1,90            | 3,50          | 3,85          |
| DPDFNet samotný                | 9,58            | 13,26         | 12,87         |
| Celý odšumovač, DFN3, řeč      | 3,53            | 5,76          | 6,28          |
| Celý odšumovač, DFN3, klávesy  | 3,15            | 5,07          | 5,82          |

I na 9800X3D tedy živý provoz stojí asi 1,8× víc než benchmark bez pauz: během 8 ms čekání
jádro zpomalí nebo usne a cache vychladne. Teplá čísla jsou spodní mez, pauza bližší skutečnosti.

### Notebook: Ryzen 5 7530U (Zen 3, 2026-11-16)

Balíček z `scripts/cpu-bench-package.ps1`, notebook v zásuvce, plán Rovnováha. Tract jede na AVX2
(`avx512f: false`). V závorce poměr k vývojovému PC ve stejném režimu (teplá a studená s AVX2, pauza
s AVX-512, které na pauzu vliv nemá).

| Část                           | Teplá         | Studená       | S pauzou 8 ms |
| ------------------------------ | ------------- | ------------- | ------------- |
| DFN3 samotný                   | 3,63 (1,8×)   | 9,98 (2,2×)   | 12,10 (3,5×)  |
| DPDFNet samotný                | 20,77 (2,1×)  | 28,05 (2,0×)  | 35,63 (2,7×)  |
| RNNoise 0.2 (`in_net`)         | 0,87          | 3,32          | 2,70          |
| Celý odšumovač, DFN3, řeč      | 7,84          | 18,52         | 15,40 (2,7×)  |
| Celý odšumovač, DPDFNet, řeč   | 24,40         | 36,18         | 32,41         |

- **Pauza je na notebooku hlavní příčina.** DFN3 s pauzou stojí 3,3× víc než bez ní (vývojové PC 1,8×),
  DPDFNet 1,7×. DFN3 pracuje 0,36 ms z 10, jádro mezi rámci zpomalí a cache vychladne; delší práce
  DPDFNetu ho drží výš. To vysvětluje, proč u testera vychází DFN3 hůř než DPDFNet, i bez AVX-512.
- Měřič počítá čas na hodinách při taktu, který jádro zrovna má. Při hře je jádro výš, procenta tak
  mohou být nižší než v klidu; se hrou, která vytlačuje cache, zase vyšší (režim `COLD_CACHE+PACED`
  přibyl do balíčku dodatečně).
- Tester (6800H, živě v aplikaci) měl 27 % s DFN3 a 47 % s DPDFNetem, tady s pauzou 15,4 a 32,4 %. Zbytek
  dělá nejspíš hra, která sdílí L3, a režie aplikace; krok 4 to rozliší.

ONNX Runtime na notebooku: kodér 154, dekodér ERB 173, dekodér hlubokého filtrování 147 µs (dohromady
473 µs, tract celý DFN3 i se STFT 363 µs, tract o 23 % rychlejší); DPDFNet 1896 µs proti 2077 µs tractu
(ORT o 9 % rychlejší). Na Zenu 3 tedy totéž co na vývojovém PC.

### Krok 3: ONNX Runtime proti tractu na x86

ONNX Runtime 1.32.0, jedno vlákno, skript z přílohy. Tract: DFN3 s vynucenými prahy, DPDFNet z benche.

| Model                    | tract AVX-512 | tract AVX2 | ONNX Runtime | Rozdíl                     |
| ------------------------ | ------------- | ---------- | ------------ | -------------------------- |
| DFN3 kodér (+ STFT)      | 75 µs         | 90 µs      | 98 µs        | tract lepší                |
| DFN3 dekodér ERB         | 66–96 µs      | 67 µs      | 111 µs       | tract lepší                |
| DFN3 dekodér hl. filtr.  | 70 µs         | 68 µs      | 94 µs        | tract lepší                |
| DPDFNet                  | 970 µs        | 1010 µs    | 926 µs       | ORT o 5 % / 9 % lepší      |

Na x86 vede ONNX Runtime u DPDFNetu jen o 5–9 % (Mac 25 %). Úprava `GruSeq` by dala nanejvýš pár procent
DPDFNetu: téma uzavřené (kapitola 3, E).

### Krok 5–7: A1, A2, B

Pokus s přepínači ve forku (`skip_df`, `zero_mask` v `apply_stages`) a v `AiVoiceIsolator`, čítač „vstupní
detektory (`in_net`, Silero) neslyšely hlas“. Úspora, jen klávesy -30 dBFS, DFN3 vzhůru:

| Varianta | Teplá cache  | Studená, AVX2 | Kvalita                                                              |
| -------- | ------------ | ------------- | -------------------------------------------------------------------- |
| Dnes     | 3,80         | 7,25          | –                                                                    |
| A1       | 3,08         | 5,93          | Neprošlo: pauzy o 5–27 dB hlasitější                                 |
| A2       | 2,47         | 4,64          | Neprošlo: konce slov ztlumené o > 6 dB 21,3 → 36,6 %                 |
| B        | 3,37         | 6,80          | Prošlo, zapracováno                                                  |
| A2 + B   | 2,00         | 4,26          | Neprošlo (A2)                                                        |

- **A1:** bez dekodéru hlubokého filtrování tlumí ERB maska klávesy v pauzách mnohem hůř. `click_report`:
  pauzy 39,1 → 12,3 dB; `near_pause_report`: 21,3 → 16,2 dB; `pause_burst_report` 20261110-141254:
  41 oken nad -80 dBFS místo 1; brána se na samotném psaní otevřela v 15,6 % času (test dovoluje 8 %).
  Vedlejší zjištění: konce slov se zlepšily (ztracené > 6 dB 21,3 → 9,9 %), hluboké filtrování je dnes ořezává.
- **A2:** pauzy výrazně tišší (`pause_burst` 20261109-210655: 5 → 0–1 oken, `click_report` pauzy +10–20 dB),
  ale konce slov ztlumené o > 6 dB 21,3 → 36,6 %, řeč o > 15 dB 3,4 → 4,5 %. S čekáním 500 / 1000 ms místo
  200 ms pořád 30,9 / 24,8 %. Detektory konce slov neslyší a nulová maska je smaže.
- **B:** kvalita beze změny kromě rámců pod kliknutími -3 dBFS ztracených o > 6 dB (12,6 → 13,3 %).
  Na samotném psaní běží RNNoise záchrany jen ve 14 % bloků (klávesy občas vzbudí vstupní detektory).
  Zapracováno: `RESCUE_IDLE_SAMPLES` = 2 s, po návratu hlasu `rnnoise.restart()` a 300 ms zahřátí, test
  `test_rescue_stops_without_voice_and_comes_back`.

### Krok 8 (D): denormální čísla nic nezpomalí

`digital_silence_report`: 2 s řeči, pak 10 s přesných nul. Průměr na nulách je u každé části stejný nebo
nižší než na řeči, FTZ/DAZ (`FTZ_DAZ=1`) ho nemění, nejpomalejší bloky skáčou náhodně v obou bězích
(plánovač). MXCSR nenastavovat.

### Vedlejší nález: test probuzení

`test_wake_without_voice_keeps_the_gate_closed` zpracovává vstup na místě, takže DPDFNet slyší výstup
DFN3, ne místnost. Na surové místnosti s DPDFNetem první rámec `out_net` po probuzení (stav z doby před
spánkem) dá 0,32 a brána se otevře na 21 bloků. Nechat `out_net` během probouzení poslouchat bez účinku to
v testu spravilo, ale v `pause_burst_report` (20261110-141254, 15,96 s) otevřelo bránu ve 25 z 30 bloků
místo 0: vráceno, otevřené.

## 8. Zadání: pokus s vahami v f16

### Cíl

Zjistit, jestli váhy modelů uložené v f16 (počítání dál v f32) zlevní DFN3 a DPDFNet tam, kde dnes
drahá je paměť: studená cache a pauza mezi bloky (kapitola 7, notebook 7530U: DFN3 3,63 % teplá,
9,98 % studená, 12,10 % s pauzou). Výsledek je rozhodnutí přijmout / zamítnout s čísly, ne hotová funkce.

### Proč to může fungovat

- DFN3 čte každých 10 ms 8,6 MB vah, DPDFNet podobně. Násobení matice vektorem (n = 1) čeká na paměť,
  šířka instrukcí nerozhoduje (AVX-512 dalo jen 5–7 %). Polovina bajtů je polovina přenosu.
- Tract 0.23.10 už má kernel: `fma_mmm_f32_32x1` v `tract-linalg/src/x86_64/mmm.rs` bere balení
  `f16f32` (váhy f16, vstup f32, výpočet a akumulace f32), vyžaduje AVX + FMA + F16C, takže běží i na Zenu 3.
  Výběr kernelu podle typů vstupů dělá `tract-core/src/ops/einsum/kernel_selection.rs` (`query` čte
  `datum_type` obou vstupů).
- f16 má 11bitovou mantisu, pro váhy obvykle bez slyšitelného rozdílu. Ověří to zprávy kvality.

### Kde to je

| Model   | Načtení a optimalizace                                                     | Pozor na                         |
| ------- | -------------------------------------------------------------------------- | -------------------------------- |
| DFN3    | Fork `crates/vendor/deep_filter/src/tract.rs`: `init_encoder_impl`,        | Tři grafy, pulzování, GRU uvnitř |
|         | `init_erb_decoder_impl`, dekodér hlubokého filtrování, `into_optimized`    | `Scan` (váhy v těle smyčky)      |
| DPDFNet | `crates/gamersequalizer-mic-chain/src/dpdfnet.rs`, `model_for_proto_model` | Stav GRU jako vstup / výstup     |
|         | ... `into_optimized()`                                                     |                                  |

Fork je mimo workspace a jeho lints; testy forku: `cargo test --release --manifest-path
crates/vendor/deep_filter/Cargo.toml`.

### Postup

0. **Proveditelnost (nejdřív, malé).** V testu vzít jeden graf DFN3 (kodér), před `into_optimized`
   převést konstantní váhy vstupující do maticových násobení (`EinSum` / `MatMul`, v GRU i v těle `Scan`)
   na f16 tak, aby aktivace zůstaly f32. Po `into_optimized` ověřit, že tract opravdu vybral
   `fma_mmm_f32_32x1` s balením `f16f32`, a ne že váhy složil zpátky na f32 (konstantní `Cast` se při
   optimalizaci skládá). Kontrola: výpis optimalizovaného modelu (`{:?}` uzlů, název kernelu a balení),
   případně `MmmDispatch::native().pick` s typy vah. Možné cesty, od nejmenší:
   - `FloatPrecisionTranslator` (`tract-core/src/floats.rs`, transformace `f32_to_f16`) s `with_filter`
     jen na konstanty vah. Celý model na f16 (i aktivace) nechceme: prvkové operace v f16 na x86 nemají
     rychlé kernely; změřit ho jen jako srovnání, když vyjde levně.
   - Vlastní malá transformace ve forku podle `tract-core/src/ops/matmul/de_block_quant.rs`
     (`BlockQuantTransform`): přepíše konstantní váhu `EinSumMatMul` na f16 tensor. Ta už větev pro f16
     váhy má.
   Když tract `f16f32` u některých násobení nevybere (např. GRU v `Scan`), zapsat kde a proč a pokračovat
   s tím, co jde. Když nevybere nikde, pokus končí tady se zprávou.
1. **Přepínač, ne náhrada.** Proměnná prostředí čtená jen při načtení modelu (např.
   `GE_F16_WEIGHTS=1`, platí pro oba modely), výchozí stav dnešní f32. Bez F16C (CPU před Haswellem)
   vždy f32: `is_x86_feature_detected!("f16c")`. Nic jiného se nemění: shared memory, nastavení, UI ne.
2. **Numerická kontrola.** Na `speech_clean.wav` a `speech_over_keyboard` porovnat výstup f16 proti f32
   (rozdíl v dB pod signálem, nejhorší blok). Jen jako informace a pojistka proti chybě v převodu
   (rozdíl blízko 0 dB = rozbité).
3. **CPU.** `cpu_breakdown_report` (DFN3 a DPDFNet samotné i celý odšumovač) ve čtyřech režimech: teplá,
   `COLD_CACHE=1`, `PACED=1`, `COLD_CACHE=1` + `PACED=1`. Na vývojovém PC s `TRACT_CPU_ISA=-avx512f`
   (AVX-512 kernely `f16f32` nemají, bez knoflíku by se měřil jiný kernel) a jednou bez něj (co dostanou
   procesory s AVX-512). Doba načtení modelu před a po (běží na pomocném vlákně, ale nesmí výrazně
   narůst). Pak balíček `scripts/cpu-bench-package.ps1` (přidat do `run.ps1` druhý průchod s
   `GE_F16_WEIGHTS=1`) a běh na notebooku 7530U.
4. **Kvalita.** `click_report`, `keyboard_call_report`, `near_pause_report`, `whisper_gate_report`,
   `pause_burst_report`, `capture_replay_report`, f32 proti f16, pro DFN3 i DPDFNet. Pravidlo z `merge.md`:
   jakékoli zhoršení nad rozptyl běhu znamená zamítnout (pro daný model zvlášť).
5. **Brána** `scripts/check.ps1` (agent `dev-rebuild`) a testy forku.

### Hotovo, když

- Je zapsaná tabulka: model × režim × f32 / f16 (vývojové PC AVX2, vývojové PC AVX-512, notebook),
  zprávy kvality f32 / f16, doba načtení, numerický rozdíl. Do kapitoly 7 tohoto dokumentu.
- Je rozhodnuto pro každý model zvlášť:
  - **Přijmout**, když kvalita beze změny a s pauzou nebo studenou cache na notebooku ušetří aspoň
    10 % času modelu. Pak f16 zapnout napevno (bez proměnné), s F16C kontrolou a testem, že model
    s f16 vahami dává výstup blízký f32.
  - **Zamítnout** jinak: kód přepínače odstranit, výsledek a důvod zapsat sem.
- Brána prošla.

### Mimo rozsah

- 4bitové váhy (`block_quant`, Q4_0): až když f16 nestačí, samostatný pokus.
- ONNX Runtime, jiné knihovny, int8 kvantizace přes nástroje ONNX, změna modelu nebo trénink.
- Režim „vytížená ostatní jádra“ (takt proti cache, bod 3 návrhu): samostatná úloha.
- Úprava APO, shared memory, UI.

### Rizika

- Tract váhy při optimalizaci složí zpět na f32 nebo `f16f32` vybere jen pro část násobení: krok 0 to
  odhalí dřív, než vznikne zbytek.
- Na procesorech s AVX-512 může f16 cesta vyjít hůř než dnešní `avx512_mmm_f32_128x1` v teplé cache;
  proto měření s i bez `TRACT_CPU_ISA` a rozhodnutí podle notebooku a studené cache.
- Změny ve forku jsou mimo lints workspace: držet je malé, s komentářem a testem ve forku.

### Kroky 0–2: hotovo na Macu (2026-11-16)

> První verze převodu (všechna násobení, bez `Scan`). Nasazený převod (jen násobení matice vektorem,
> i v tělech `Scan`) je v části „Varianty kernelů a převodu“.

Převod je `df::f16_weights::convert` ve forku: f32 konstanta, kterou čtou jen `EinSum` v f32, se před
`into_optimized` uloží jako f16. Volá se ve třech grafech DFN3 (`RuntimeParams::f16_weights`, výchozí
z `GE_F16_WEIGHTS`) a v `DpdfEngine::try_load` (nově `into_typed` + `into_decluttered` před převodem).
Na x86 bez F16C nebo FMA zůstává f32.

- **Tract váhy zpátky nesložil.** Na ARM (M4 Pro, stejný mechanismus `f16f32` jako `fma_mmm_f32_32x1` na
  x86) běží s balením `PackedF16` 22 z 27 násobení DFN3 a 87 z 92 u DPDFNetu, žádný `Cast`. Zbylá
  násobení jsou konvoluce s váhami pod 10 kB. Na x86 to zbývá ověřit (krok 3, výpis v testu níže).
- **Kolik bajtů:** DFN3 4,55 → 2,28 MB; rekurentní váhy GRU (asi 3,9 MB) jsou v DFN3 uvnitř těl `Scan`,
  kam tato verze nesahala (nasazená ano). DPDFNet 9,43 → 4,72 MB; jeho `GruSeq` (0,8 MB) si rekurentní
  váhy balí sám jako f32 (`pack_r`) a zůstávají f32 i dnes.
- **Numericky:** syntetický signál (tón + šum), výstup f16 proti f32: DFN3 47,7 dB, DPDFNet 55,0 dB pod
  signálem. Převod tedy proběhl a přesnost drží.
- **Čas na M4 (teplá cache, jen orientačně):** DFN3 všechny tři grafy 207–215 → 198–200 µs (−4 až −8 %),
  DPDFNet 1256 → 1261 µs (beze změny). Mac má velkou cache, rozhodne studená cache a pauza na x86.

Test ve forku: `cargo test --release --features tract,default-model --manifest-path
crates/vendor/deep_filter/Cargo.toml f16_weights -- --nocapture` (vypíše kernel a balení každého
násobení, na Windows tím je hotová kontrola kroku 0 pro x86).

Další na Windows: krok 3 (`cpu_breakdown_report` s `GE_F16_WEIGHTS=1`, balíček
`scripts/cpu-bench-package.ps1` už má druhý průchod), krok 4 (zprávy kvality s `GE_F16_WEIGHTS=1`
proti bez), krok 5. V živé aplikaci se proměnná do audiodg nedostane: APO zůstává na f32.

### Kroky 3–5 na Windows (9800X3D, 2026-11-16)

> Překonáno částí „Varianty kernelů a převodu“: tam měřeno s připnutým vláknem a mediánem z více kol.
> Ztráta s AVX-512 v teplé cache (+25 %) se tam neopakuje (−1,5 % se stejným převodem a tractem).

**Kernely na x86:** test forku vypsal 22 z 27 násobení DFN3 s `PackedF16` v `fma_mmm_f32_32x1` / `32x3`,
i na procesoru s AVX-512 (tract pro f16 váhy AVX-512 kernel nemá). Výstup 47,7 dB pod signálem, DPDFNet
55,8 dB (dočasný test, odebrán).

**Kvalita** (`GE_F16_WEIGHTS=1` proti bez, všech šest zpráv): beze změny. Rozdíly ±0,1 dB, jediný horší
řádek je DFN3 bez záchrany kliknutí (diagnostický, aplikace ho nepoužívá): kliknutí −3 dBFS, > 15 dB
16,3 → 16,7 %. U DPDFNetu shodné na desetinu dB.

**CPU**, % jádra, f32 → f16 (změna). Režim s pauzou kolísá mezi běhy o ±25 % (f32 AVX-512 DFN3 2,78 /
3,50 / 3,82 ve třech bězích), teplá a studená o ±3–5 %.

| Část                         | ISA     | Teplá                 | Studená               | S pauzou              | Studená + pauza      |
| ---------------------------- | ------- | --------------------- | --------------------- | --------------------- | -------------------- |
| DFN3 samotný                 | AVX2    | 2,08 → 1,94 (−7 %)    | 4,57 → 3,65 (−20 %)   | 3,75 → 3,57 (−5 %)    | 5,36 → 4,83 (−10 %)  |
| DPDFNet samotný              | AVX2    | 10,58 → 9,97 (−6 %)   | 13,67 → 11,89 (−13 %) | 16,31 → 16,27 (0 %)   | 17,40 → 16,50 (−5 %) |
| Celý odšumovač, DFN3, řeč    | AVX2    | 3,87 → 3,59 (−7 %)    | 6,99 → 6,59 (−6 %)    | 6,32 → 5,93 (−6 %)    | 8,24 → 7,74 (−6 %)   |
| Celý odšumovač, DPDFNet, řeč | AVX2    | 12,09 → 11,20 (−7 %)  | 16,00 → 15,01 (−6 %)  | 16,68 → 16,48 (−1 %)  | 20,43 → 18,69 (−9 %) |
| DFN3 samotný                 | AVX-512 | 1,87 → 2,33 (+25 %)   | 3,86 → 3,91 (+1 %)    | 2,78 → 3,54 (+27 %)   | 4,64 → 4,50 (−3 %)   |
| DPDFNet samotný              | AVX-512 | 10,18 → 11,34 (+11 %) | 12,94 → 13,07 (+1 %)  | 11,70 → 16,85 (+44 %) | 15,19 → 15,57 (+3 %) |
| Celý odšumovač, DFN3, řeč    | AVX-512 | 3,57 → 3,64 (+2 %)    | 6,20 → 6,36 (+3 %)    | 4,69 → 5,82 (+24 %)   | 7,32 → 7,17 (−2 %)   |
| Celý odšumovač, DPDFNet, řeč | AVX-512 | 11,42 → 12,72 (+11 %) | 14,91 → 15,50 (+4 %)  | 12,49 → 15,39 (+23 %) | 17,07 → 16,88 (−1 %) |

- **Bez AVX-512 (notebooky, Zen 3, většina Intelů) je f16 rychlejší ve všech režimech:** model −5 až
  −20 %, celý odšumovač −6 až −9 %. Splňuje hranici z „Hotovo, když“ (studená cache DFN3 −20 %, DPDFNet
  −13 %); potvrdí notebook (balíček má druhý průchod s f16).
- **S AVX-512 f16 prohrává v teplé cache** (DFN3 +25 %, DPDFNet +11 %), ve studené je vyrovnaný. f16
  ztratí AVX-512 kernel, a ten má ve studené cache náskok 10–21 % (kroky 1 a 2). Pauza tu kvůli
  kolísání nerozhoduje.
- **Návrh (nepoužit, viz „Varianty“):** f16 jen na procesorech bez AVX-512 (`wanted()` = F16C a FMA a ne `avx512f`), f32 s AVX-512.
  Kernel `f16f32` pro AVX-512 (větev v `avx512_mmm_f32_128x1`, `vcvtph2ps zmm` je AVX-512F) jako PR do
  tractu; po jeho vydání jedna cesta pro všechny.

### Notebook 7530U s f16 (2026-11-16)

f32 → f16, % jádra: DFN3 teplá 3,65 → 3,12 (−15 %), studená 9,50 → 8,52 (−10 %), s pauzou 14,38 → 14,57
(+1 %), studená + pauza 15,80 → 14,54 (−8 %). DPDFNet 20,20 → 16,93 (−16 %), 30,49 → 25,19 (−17 %),
34,76 → 35,68 (+3 %), 34,65 → 31,12 (−10 %). Celý odšumovač −8 až −16 % mimo pauzu. Bez AVX-512 tedy
f16 přijmout. S pauzou nepomáhá: tam brzdí takt po nečinnosti, ne paměť. ORT v tomto běhu DPDFNet
1530 µs proti tractu f32 2020 / f16 1693 µs (minule 1896 proti 2077): notebook kolísá mezi běhy.

### AVX-512 kernel pro f16 váhy (tract větev `f16-avx512`, 691bdd5fa)

`avx512_mmm_f32_128x1` dostal balení `f16f32`; testy tract-linalg prošly (4065), kontrola kódu bez
nálezu. Všech 13 násobení s n = 1 v DFN3 běží v něm. Malé matice (m 16–60) se ale doplňují na 128 řádků,
konvoluce zůstávají ve FMA kernelu. Měření na 9800X3D (f32 → f16, jeden běh, f32 a f16 střídavě):

| Část                         | Teplá                | Studená               | S pauzou (2 běhy)             | Studená + pauza      |
| ---------------------------- | -------------------- | --------------------- | ----------------------------- | -------------------- |
| DFN3 samotný                 | 2,48 → 2,03 (−18 %)  | 4,17 → 4,19 (0 %)     | 3,59 → 3,05 / 2,90 → 2,96     | 4,55 → 4,87 (+7 %)   |
| DPDFNet samotný              | 10,65 → 11,16 (+5 %) | 13,45 → 15,32 (+14 %) | 16,40 → 12,16 / 11,65 → 12,44 | 15,30 → 16,02 (+5 %) |
| Celý odšumovač, DFN3, řeč    | 3,74 → 3,82 (+2 %)   | 6,90 → 6,63 (−4 %)    | 6,41 → 4,78 / 4,75 → 5,84     | 7,10 → 7,45 (+5 %)   |
| Celý odšumovač, DPDFNet, řeč | 11,76 → 12,39 (+5 %) | 15,76 → 15,70 (0 %)   | 16,00 → 13,16 / 13,03 → 15,45 | 16,98 → 17,94 (+6 %) |

Nerozhodné: s AVX-512 kernelem je f16 zhruba na úrovni f32, v rozptylu běhů (f32 DFN3 teplá 2,48 proti
1,87 v minulém běhu). Další krok: opakované běhy (3×) a odstranit doplňování malých matic na 128 řádků
(výběr kernelu podle m, nebo f16 cesta v `avx512_mmm_f32_16x1`), pak znovu změřit. (Hotovo
v další části; f16 je nakonec zapnuté na všech CPU.)

### Varianty kernelů a převodu (9800X3D, 2026-11-17)

Měřeno přesněji: vlákno připnuté na jedno jádro, varianty střídavě v náhodném pořadí, medián z 3–5 kol.

**Mikrobenchmark kernelů** (`linalg/benches/f16_mat_vec.rs` v tractu, ns na jedno násobení matice
vektorem; `l3` = 32 MB vah v rotaci, `dram` = 512 MB):

| Matice, stav     | f32 128x1 | f16 FMA 32x1 | f16 128x1 (nový) | f32 16x1 |
| ---------------- | --------- | ------------ | ---------------- | -------- |
| 768×256, teplá   | 2767      | 5228         | 2485             | 3892     |
| 768×256, L3      | 6178      | 5360         | 3340             | 6252     |
| 768×256, RAM     | 12146     | 6780         | 7051             | 16042    |
| 256×256, L3      | 2180      | 1882         | 1218             | 2212     |
| 32×64, L3        | 548       | 166          | 485              | 204      |
| 16×32, L3        | 549       | 193          | 520              | 141      |

- Nový f16 kernel je u velkých matic o 40–46 % rychlejší než f32 (L3, RAM).
- U malých matic (m ≤ 64) je kernel se 128 řádky špatná volba i pro f32: doplňuje řádky až osmkrát.
  Tract ho přesto vybíral pro m = 32 a 60 (na AMD pravidlo „16x1 jen pod m = 31“; pro f16 váhy vždy
  nejširší dlaždice). Oprava výběru (větev `f16-sel`): dlaždice s mr ≥ 2·m ustoupí užší.
- Zahozeno, bez měřitelného přínosu: smyčka rozvinutá na 2 a 4 kroky, dvě sady akumulátorů (vše v
  ±1–3 %), prefetch (o 10 % horší), f16 cesta v `16x1` (výběr ji nepoužije, FMA 32x1 je rychlejší),
  f16 v konvolučních kernelech `16x12`/`16x8` (konvoluce s f16 vždy pomalejší než s f32).

**Profil DFN3 po operacích** (test `f16_weights_op_profile_report` v `deep_filter`, teplá cache, µs na
rámec): násobení matice vektorem f32 59,5 → f16 s opravou výběru 38,2; konvoluce f32 35,3, s f16 37–39;
`Scan` (GRU) 41 µs, jeho rekurentní váhy byly f32. Proto se převod změnil: f16 jen pro váhy násobení
matice vektorem (konvoluce zůstávají f32) a nově i uvnitř těl `Scan` (4,2 MB místo 2,3 MB v DFN3).

Celý DFN3, změna proti f32 ve stejném běhu:

| tract                            | Převod jako dosud | Jen n = 1 | Jen n = 1 + Scan |
| -------------------------------- | ----------------- | --------- | ---------------- |
| 0.23.10, teplá                   | −1,5 %            | −8,6 %    | −11,1 %          |
| 0.23.10, studená                 | −6,4 %            | −6,4 %    | −11,4 %          |
| 0.23.10 bez AVX-512, teplá       | −5,9 %            | −7,2 %    | −10,6 %          |
| 0.23.10 bez AVX-512, studená     | −10,8 %           | −11,3 %   | −18,5 %          |
| kernel + oprava výběru, teplá    | −10,6 %           | −11,6 %   | −16,1 %          |
| kernel + oprava výběru, studená  | −6,3 %            | −7,7 %    | −13,3 %          |
| jen kernel bez opravy, teplá     | −0,6 %            | −4,6 %    | −10,2 %          |

Celý řetězec (`cpu_breakdown_report`, tract s kernelem a opravou výběru, % jádra, medián ze 3 kol):

| Část                         | Teplá f32 → f16      | Studená f32 → f16    |
| ---------------------------- | -------------------- | -------------------- |
| DFN3 samotný                 | 1,89 → 1,59 (−16 %)  | 3,62 → 3,17 (−12 %)  |
| Celý odšumovač, DFN3, řeč    | 3,57 → 3,30 (−8 %)   | 6,06 → 5,54 (−9 %)   |
| DPDFNet samotný              | 10,04 → 10,10 (0 %)  | 12,96 → 12,55 (−3 %) |
| Celý odšumovač, DPDFNet, řeč | 11,28 → 10,97 (−3 %) | 14,82 → 14,14 (−5 %) |

Kvalita s novým převodem: reporty jako v krocích 3–5, rozdíly ±0,1 dB (konce slov ztracené o > 6 dB
21,3 → 21,7 %).

Závěr: f16 se vyplatí na všech měřených CPU i s nezměněným tractem 0.23.10; kernel s opravou výběru
přidá na AVX-512 asi 5 bodů (teplá) a 2 body (studená).
DPDFNet z f16 skoro nic nemá: čas tráví v `GruSeq` s malými vahami (0,8 MB), brzdí ho výpočet.

Nasazeno (2026-11-17): f16 zapnuté na všech CPU s F16C + FMA (`GE_F16_WEIGHTS=0` vrací f32 pro
srovnání), tract z forku github.com/PixCZ/tract, větev `gamersequalizer-0.23.10` (kořenový
`Cargo.toml`). Pull requesty do sonos/tract jsou připravené ve forku, otevřou se později.

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
