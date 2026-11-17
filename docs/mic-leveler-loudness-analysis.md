# Leveler: hlas s DFN3 vychází o 6-8 dB nad cílem. Analýza a postup (2026-11-17)

Vstup: měření `chain_report` na čtyřech commitech (Windows, 2026-11-17), DEVLOG (milníky 22, 29, 35,
36, 38, 40), `voice_leveler.rs`, `chain.rs`, `ai_denoiser/` a historie gitu.
Psáno na Macu, kde není `test_data/private/`: **mechanismus je odvozený z kódu a ze starších čísel
v DEVLOGu, na nahrávce ho potvrdí až jeden běh reportu** (kapitola 5).

**Stav (2026-11-17, později): potvrzeno na Windows a opraveno** (DEVLOG milník 64).
`leveler_report` ukázal u DFN3 a DPDFNet 3-3,7 s učení před hlasem a zisk +13,6 / +18,5 dB v 84 s;
s detektorem z roku 2017 je to 710 ms a +6,9 / +7,4 dB. Nasazená je varianta **B** (kapitola 6), ne A:
leveler bere řeč až po 6 rámcích v řadě nad 0,95 z `in_net`, bez druhé sítě. `chain_report` u DFN3
při 0 dB: hlas -22,0 / -24,4 dBFS, leveler +7,3 dB.

Značení: **Naměřeno** = číslo z měření nebo z DEVLOGu. **Z kódu** = ověřeno čtením zdrojáku.
**Hypotéza** = vyplývá z kódu, potřebuje měření.

## 1. Závěr v kostce

- **Není to chyba chystaného vydání.** Podezřelý commit je `d4b83bd` (2026-11-09, milník 35,
  „RNNoise 0.2 as the raw-input voice detector") a je ve všech vydaných verzích (0.1.0 až 0.2.0).
- **Co se stalo (Z kódu):** leveler se učí hladinu řeči jen z rámců, které detektor hlasu označí jako
  řeč. Návrh stojí na tom, že detektor **neslyší šepot** (první věta `voice_leveler.rs`). Do milníku 35
  to platilo pro všechny modely. Od milníku 35 dostávají těžké modely (DFN3, DPDFNet) značku
  z detektoru RNNoise 0.2, který byl naladěn pro bránu („radši pustit klávesu než useknout řeč")
  a **šepot slyší** (Naměřeno, milník 38: 18-33 % rámců nad prahem 0,7 a 30-62 % nad 0,2). S RNNoise
  jako modelem zůstal starý detektor z roku 2017, proto je RNNoise v pořádku.
- **Důsledek (Hypotéza):** na `whisper_scenario.wav` je před prvním normálním hlasem (45 s) 14 s
  šepotu. S DFN3 se z něj leveler naučí nízkou hladinu, ta po 3 s platí za „ustálenou" a normální
  hlas pak počítá jako výkyv: každý rámec smí hladinu posunout nejvýš jako o 6 dB hlasitější.
  Hladina tak zůstane o 6 dB níž a hlas vyjde o 6-8 dB nad cílem.
- **Bisekce po 25 commitech není potřeba.** Mezi milníkem 29 (kdy DFN3 sedělo) a 0.2.0 se cesta
  „detektor -> leveler" změnila jen ve třech commitech (kapitola 3). Kód ukazuje na `d4b83bd`.
- **Další krok:** jeden běh `leveler_report` (rozšířený v tomto kole o rozhodující sloupce), pak
  oprava A z kapitoly 6: leveleru vrátit starý detektor, bráně nechat nový.

## 2. Co je naměřeno

`chain_report`, vstup 0 dB, cíl -24 dBFS, hlas 45-49 s / 53-57 s (měření 2026-11-17):

| Commit                   | Hlas DFN3     | Leveler DFN3 | Hlas RNNoise  | Leveler RNNoise |
| ------------------------ | ------------- | ------------ | ------------- | --------------- |
| `01dea4f` (0.2.0)        | -16,0 / -17,5 | +13,3 dB     | -21,5 / -24,2 | +7,4 dB         |
| `d2b9d4c` (HEAD)         | -16,0 / -17,4 | +13,4 dB     | -21,5 / -24,2 | +7,4 dB         |

- Leveler s DFN3 věří, že řeč má -37,4 dBFS (-24 - 13,4); s RNNoise -31,4 dBFS. Rozdíl 6 dB.
- Stejný rozdíl je u přebuzeného vstupu (+20 dB clipped): DFN3 ubírá -5,6 dB, RNNoise -12 dB
  (6,4 dB). **Posun tedy nezávisí na úrovni vstupu.**
- Milník 29 (2026-11-08), tentýž report: DFN3 +20 dB clipped -20,6 / -23,7 dBFS při -12 dB, tedy
  stejně jako RNNoise. **Tehdy DFN3 sedělo.**
- Milník 22: detektor RNNoise z roku 2017 šepot neslyší vůbec („probability ~0").
- Milník 38, RNNoise 0.2 na `whisper_scenario.wav`, podíl rámců nad prahem 0,7 / 0,2:

| Úsek                        | Nad 0,7 | Nad 0,2 |
| --------------------------- | ------- | ------- |
| šepot 20-24 s               | 18,0 %  | 30,2 %  |
| tišší šepot 26,5-31,5 s     | 20,6 %  | 33,0 %  |
| šepot + psaní 36-41 s       | 33,2 %  | 61,8 %  |
| tichý hlas 45-49 s          | 90,5 %  | 95,2 %  |

  Detektor se otevře dvěma rámci nad 0,7 po sobě a pak 200 ms drží na všem nad 0,2. Ze 14 s šepotu
  tak leveler dostane odhadem 3-6 s „řeči" (Hypotéza: přesné číslo dá report, sloupec
  `learned before 44 s`).

## 3. Co je vidět v kódu

**Cesta značky.** `chain.rs:394` dává leveleru `ai_denoiser.heard_voice()`. Pro těžký model ji plní
`in_net` (`ai_denoiser/mod.rs:569-575`), pro RNNoise vlastní pravděpodobnost filtru
(`mod.rs:600-604`, síť z roku 2017, prahy 0,5 / 0,05). `d4b83bd` vyměnil v `in_net` síť z roku 2017
za RNNoise 0.2 s prahy 0,7 / 0,2 (`voice.rs`, `IN_NET_VOICE_PROB`, `IN_NET_KEEP_PROB`). Milník 35
u leveleru zkontroloval jen posun o 10 ms, ne to, co detektor nově slyší. Milník 40 výslovně píše,
že leveler zůstává na `in_net`, „aby se šepot nenaučil jako hladina řeči". To už od milníku 35
neplatilo.

**Jak se leveler učí** (`voice_leveler.rs:261-296`):

- Prvních 300 rámců řeči (3 s): prostý průměr výkonu, každý rámec se počítá, jak je.
- Od 300 rámců: rámec o víc než 12 dB pod hladinou se zahodí (`FOREIGN_DB`), ostatní se oříznou na
  +-6 dB od hladiny (`OUTLIER_DB`). Váha rámce je 1/n, n roste do 2000.
- Když se tedy leveler nejdřív 3 s učí z šepotu a pak přijde hlas o 15 dB silnější, jeden hlasitý
  rámec zvedne hladinu nejvýš o 3/n. Při n = 600 to je asi +2 dB za první sekundu hlasu, při plné
  paměti (n = 2000) jen **+0,65 dB za sekundu hlasitých rámců**. Dohnat 8 dB trvá desítky sekund řeči.
- Pojistky pro start (`RECHECK_*`) běží jen po `seed` a `relearn`, ne průběžně.

**Co jsem vyloučil (Z kódu):**

- **Předzesílení DFN3 (`2065228`)**: při +20 dB clipped leveler ubírá, předzesílení je 0 dB
  (`set_pre_gain_db` ořezává na 0..24), a posun 6 dB tam je taky. Vstup se násobí a výstup dělí
  stejným číslem (`deepfilter.rs:51,69`), v ustáleném stavu se úroveň nemění.
- **`relearn` při usnutí modelu**: `active_model()` vrací vybraný model i ve spánku, v reportu se
  `LevelerInputs` nemění.
- **Posun značky o 10 ms** (RNNoise 0.2 hlásí o rámec později): na výkonový průměr nemá vliv 6 dB.
- **Click rescue (`fbefebc`)**: jen přimíchává RNNoise do výstupu DFN3 před levelerem, leveler to vidí.
- **Změny leveleru z 15. 11. (`3547694`)**: paměť 30 -> 20 s a kontrola po `seed`; v reportu se `seed`
  ani `relearn` nevolá a RNNoise vychází stejně jako v milníku 29.

Commity mezi milníkem 29 a 0.2.0, které mění hlas DFN3 nebo značku pro leveler: `fbefebc`
(click rescue), `d4b83bd` (detektor), `2065228` (předzesílení). Ostatní z těch 25 jsou AEC, sidetone,
přesuny kódu a brána (Silero jde jen do brány).

## 4. Co čísla z `chain_report` sama nerozhodnou

Výstup 45-49 s (-16,0) a 53-57 s (-17,4) odpovídá zisku kolem +13 dB už na začátku hlasu a skoro
stejnému na konci souboru. To sedí na dvě varianty:

- **H1 (pravděpodobnější):** hladina je nízko už ve 44 s, naučená z šepotu, a během hlasu stoupá jen
  pomalu.
- **H2:** leveler se před hlasem nic nenaučil a níž ho drží až rámce uvnitř normální řeči (nový
  detektor slyší víc tichých začátků a konců slabik). Proti H2 mluví počty: prvních 300 rámců je
  prostý průměr, takže by hladina začala správně a za zbylých pár sekund hlasu by nestihla klesnout
  o 6 dB.

H1 a H2 se liší tím, co se děje v běžném hovoru bez šepotu, proto stojí za to je rozlišit měřením.

## 5. Postup na Windows

**Krok 1: jeden report (agent `cmd-runner`).**

```
cargo test -p gamersequalizer-mic-chain --release leveler_report -- --ignored --nocapture
```

`leveler_report` (`ai_denoiser/tests.rs`) má nově: modely RNNoise, DFN3, DPDFNet; sloupec
`learned before 44 s` (kolik řeči se leveler naučil před prvním normálním hlasem); zisk ve 44 s;
a řádky `2017 key`, kde leveler dostává značku ze starého detektoru na syrovém vstupu, jak to bylo
před milníkem 35. Report nemá předzesílení DFN3 ani mic EQ, jde v něm o průběh, ne o absolutní čísla.

| Co vyjde na řádku „input +0 dB"                                                   | Co to znamená                              |
| --------------------------------------------------------------------------------- | ------------------------------------------ |
| RNNoise: `learned before 44 s` kolem 0 ms, zisk ve 44 s 0                         | výchozí stav, podle očekávání              |
| DFN3 a DPDFNet: `learned before 44 s` v sekundách, zisk ve 44 s vysoký (k +24 dB) | **H1 potvrzena**: učí se ze šepotu         |
| DFN3 `2017 key`: kolem 0 ms, zisky a hlas jako RNNoise                            | **oprava A funguje**                       |
| DFN3: 0 ms před 44 s, a přesto zisk v 84 s o 6 dB výš než RNNoise                 | H1 vyvrácena, platí H2 (viz krok 4)        |
| DPDFNet v pořádku, jen DFN3 mimo                                                  | není to detektor, ale DFN3 (viz krok 4)    |

**Krok 2: log z hovoru 16. 11. kolem 20:40 (agent `log-analyst`),** pokud byly zapnuté ladicí řádky.
Řádky `[Leveler]` ukazují každý pohyb zisku a naučené hladiny. Hledat: hladina klesne o víc než
6 dB, když uživatel mlčí nebo mluví potichu, a zisk pak zůstane vysoko, když mluví normálně.

**Krok 3: oprava A** (kapitola 6), přijmout podle:

- `chain_report`: DFN3 při 0 dB hlas kolem -21 / -24 dBFS a leveler kolem +7,4 dB; +20 dB clipped
  kolem -12 dB, jako RNNoise a jako v milníku 29.
- `whisper_gate_report` a `private_report` beze změny (brána dál slyší šepot).
- Regresní test bez soukromých nahrávek: šepot nejde nasimulovat z `test_data/`, takže aspoň test,
  že značka pro leveler u těžkého modelu nejde z `in_net` (stejný vstup, `heard_voice` pro leveler
  se rovná starému detektoru).

**Krok 4, jen když krok 1 H1 vyvrátí:** bisekce má tři kroky, ne pět. Stav před a po `d4b83bd`,
`2065228`, `fbefebc` (crate se tehdy jmenoval `gamersequalizer-mic-vst`, `chain_report` byl
`plugin::tests::chain_report`).

## 6. Nápady na opravu

**A. Leveleru vrátit detektor z roku 2017, bráně nechat RNNoise 0.2 (doporučuji).**
Brána a leveler chtějí opak: brána má slyšet všechno, co je řeč, leveler jen plný hlas. Jeden detektor
oběma neposlouží.

- `AiVoiceIsolator` dostane druhou `VoiceNet::new()` s vlastní `VoiceHysteresis::new()` na syrovém
  vstupu, když běží těžký model nebo je odšumění vypnuté (`strength` 0, tam jde leveler taky přes
  `in_net`). Její výsledek se vrací z `heard_voice()`.
- Uvnitř zůstává `voice` z `in_net` pro bránu, spánek modelu a click rescue (šepot má model budit).
- Cena: jeden průchod sítě z roku 2017, asi 0,26 % jádra (milník 35).
- Vrací přesně stav změřený v milníku 29. Automatické zesílení vstupu (`speech_ms`) a zesílení pro
  AEC3 (`gain_to_default_db`) jdou přes leveler, takže se opraví s ním.
- S DFN3 už síť z roku 2017 na syrovém vstupu běží kvůli click rescue (`rnnoise.process` nad kopií),
  ale jen když byl v posledních 2 s hlas a po pauze začíná studená. Proto vlastní síť, ne tahle.

**B. Přísnější pravidlo nad pravděpodobností `in_net`, bez CPU navíc.** Například řeč pro leveler až
po N rámcích nad 0,9 po sobě a bez držení na 0,2. Nejdřív je třeba změřit rozložení pravděpodobnosti
na šepotu a na hlasu; šepot má 18-33 % rámců nad 0,7, takže oddělení není jisté. Až po A, pokud by
0,26 % jádra vadilo.

**C. Pojistka v leveleru: hladinu naučenou příliš nízko opravit rychleji.** Například průběžná
kontrola „poslední sekunda řeči je o víc než 8 dB nad hladinou". Nedoporučuji samotné: sekunda křiku
by hladinu zvedla, normální hlas by pak byl o 12 dB níž a `FOREIGN_DB` by ho přestal učit. Leveler
podle úrovně nepozná šepot následovaný hlasem od hlasu následovaného křikem; to musí říct detektor.

## 7. Kdy to uživatel uslyší (Hypotéza, platí při H1)

- Těžký model (DFN3, DPDFNet), a leveler slyší nejdřív něco tiššího než plný hlas: šepot, mumlání,
  tichý začátek hovoru.
- Nejhorší je stream bez zapamatované hladiny (první spuštění, nový mikrofon, „Naučit znovu"):
  první 3 s se bere všechno. Se zapamatovanou hladinou platí `FOREIGN_DB` hned, takže zvuky o víc než
  12 dB tišší se neučí, ale tišší řeč do 12 dB pod hladinou ji stahuje dál.
- Nízká hladina se uloží (`voice_level_cache.json`) a další stream s ní začne.
- Vysoký zisk leveleru se přenáší dál: předzesílení DFN3, hlubší dno brány, zesílení mikrofonu pro
  AEC3, hlasitost sidetonu. Hlas jde do limiteru.

## 8. Vedlejší nález

`win_audio/leveler_log.rs:16`: `MAX_CUT_X10 = -60` a text „[at the -6 dB cut cap]". Leveler od milníku
29 ubírá až -18 dB (`MAX_CUT_DB`), takže log hlásí strop u každého zisku pod -6 dB. Neopraveno.
