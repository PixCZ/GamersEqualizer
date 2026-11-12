# Příposlech (sidetone): analýza a zadání

Proč je příposlech navržený tak, jak je. Rozhodnutí člověka jsou v kapitole 1.1 (2026-11-10).
**Postup krok za krokem je v `docs/sidetone-implementace.md`.**

Stav (větev `sidetone`): jádro, sdílená paměť v20 a rámcové potlačovače šumu jsou napsané
a otestované. APO, daemon a UI zbývají. Latence jsou odhady, dokud neproběhne měření.

Značení: **Z kódu** (ověřeno čtením repa), **Z dokumentace** (odkaz v kapitole 11),
**Hypotéza** (ověřit měřením).

## 1. Cíl

Těsnící sluchátka (hlavně in-ear) utlumí vlastní hlas přicházející vzduchem. Zůstane jen dunivý
hlas vedený kostí. Příposlech vrací mikrofon do sluchátek:

- hlasitost jde nastavit jemně (po 0,5 dB), ne ve 3 stupních jako u zvukovky SteelSeries,
- v pauzách není slyšet stálý šum (šum mikrofonu, větrák),
- zpoždění je tak malé, že při mluvení neruší.

Mez: výchozí cesta příposlechu nemá žádný model ani blokové zpracování, každý stupeň má nulové
algoritmické zpoždění. Jediná výjimka jsou volitelné rámcové režimy (kapitola 5.1), které uživatel zapne
s vědomím, kolik ms přidá. Do odchozího zvuku mikrofonu (to, co slyší ostatní) příposlech
nezasahuje.

### 1.1 Rozhodnuto

| Otázka                               | Rozhodnutí                                                                                          |
| ------------------------------------ | --------------------------------------------------------------------------------------------------- |
| Kdy příposlech hraje                 | Jen když z mikrofonu nahrává nějaká aplikace. UI na to upozorňuje. Režim „stále zapnuto" se nedělá. |
| Nastavování bez nahrávající aplikace | Tlačítko „Vyzkoušet" (zapnout/vypnout). Samo skončí při odchodu ze sekce a při zavření okna.        |
| Střídání headsetů                    | Matice: každá dvojice mikrofon + sluchátka má vlastní uložené nastavení. Neznámá dvojice = vypnuto. |
| Ztlumený mikrofon                    | Příposlech ztichne s ním.                                                                           |
| Zvukovka neumí periodu pod 10 ms     | Postavit celé i tak. Sekce ukáže změřenou latenci červeně.                                          |
| Potlačení šumu                       | Vypnuto / pásmový expander (výchozí, 0 ms) / WebRTC NS (+7 ms) / RNNoise (+10) / RNNoise 0.2 (+20). |

## 2. Co už projekt má

**Z kódu.** Příposlech se dá postavit skoro celý z existujících dílů:

| Díl                                                 | Kde                                                         | K čemu poslouží                                  |
| --------------------------------------------------- | ----------------------------------------------------------- | ------------------------------------------------ |
| Mic APO v audiodg, dostává surový vstup po blocích  | `crates/gamersequalizer-apo/src/lib.rs`, `MicProcessor`     | odbočka příposlechu před řetězcem                |
| Output EQ APO na výstupu, po mixu i po Atmosu       | tamtéž, `OutputProcessor::run_eq_in_place`                  | přimíchání do sluchátek                          |
| Limiter špiček na konci Output EQ                   | `crates/gamersequalizer-common/src/limiter.rs`              | součet hry a hlasu nepřebudí výstup              |
| Sdílená paměť obou APO a daemona (`Global\..._v19`) | `crates/gamersequalizer-common/src/shm.rs`                  | kruhový buffer, nastavení, telemetrie            |
| Zrcadlení nastavení daemon → APO                    | `ShmHeader::copy_controls_from`, `win_audio/apo_link.rs`    | nová pole se přidají sem                         |
| Biquady                                             | `crates/gamersequalizer-common/src/biquad.rs`               | horní a dolní propust příposlechu                |
| Zesílení hlasitosti mikrofonu ve Windows, v dB      | `mic_input_gain_db_x10`                                     | příposlech neskočí, když automatika pohne úrovní |
| Jitter volání mic APO, zátěž řetězce                | `mic_call_jitter_peak_us`, `mic_dsp_load_peak_permille`     | rezerva bufferu, rozpočet pro kratší periodu     |
| WASAPI vlákno s dohledem, sleduje výchozí výstup    | `win_audio/loopback.rs`, `StreamResources`                  | vzor pro vlákno, které drží streamy otevřené     |
| „Nahrává právě někdo z mikrofonu?"                  | `crates/gamersequalizer-daemon/src/mic_usage.rs`            | režim „jen když mikrofon někdo používá"          |
| Navigace, karty, trvalé nastavení                   | `ui/header.rs` (`NAV`), `ActiveTab`, `ui/audio_settings.rs` | nová sekce a její volby                          |

Čtyři vlastnosti dnešního stavu, které návrh určují:

1. **Výstup mic řetězce je pro příposlech pozdě.** AEC3 přidá ~9 ms, lookahead brány 10–15 ms a
   k tomu blok modelu (DEVLOG, milník 30). Odbočka proto musí být ze surového vstupu.
2. **Mic APO běží, jen když má mikrofon otevřený nějaká aplikace** (milník 17). Bez Discordu
   příposlech mlčí. Tak to zůstane (1.1); pro nastavování je tlačítko „Vyzkoušet".
3. **Výstupní engine stojí, když nic nehraje** (loopback pak nedává pakety, milník 17).
   Příposlech by vypadl v každém tichu.
4. **APO neví, na kterém zařízení sedí** (`apo_link.rs`: „the APO's endpoint isn't reported yet").
   Příposlech se nesmí dostat do reproduktorů: mikrofon + reproduktory = zpětná vazba.

## 3. Kudy zvuk povede

| Varianta                                        | Latence                 | Verdikt                                                                   |
| ----------------------------------------------- | ----------------------- | ------------------------------------------------------------------------- |
| A. Hardwarový sidetone zvukovky (SteelSeries)   | ~0 ms                   | Ne: 3 stupně, propustí všechen šum. Proto tohle zadání vzniklo.           |
| B. Windows „Poslouchat toto zařízení"           | desítky až stovky ms    | Ne: pozdě, a bere zvuk až za řetězcem.                                    |
| C. Daemon: WASAPI nahrávání → WASAPI přehrávání | o 1–2 periody víc než D | Ne: dvakrát navíc přes engine a plánovač, stream je vidět ve směšovači.   |
| **D. Mic APO → kruhový buffer → Output EQ APO** | nejnižší dosažitelná    | **Ano.** Oba konce už běží v audiodg a sdílejí paměť.                     |
| E. Vlastní ovladač, ASIO, exkluzivní režim      | jednotky ms             | Ne: mimo projekt (žádná virtuální zvukovka, ostatní aplikace by oněměly). |

Varianta D:

```
mikrofon → engine → [Mic APO] ──────────────→ řetězec (AEC, model, EQ) → aplikace
                        │ surový vstup
                        ▼
              příposlech: propusti → expander → hlasitost
                        ▼
              kruhový buffer ve sdílené paměti
                        ▼
hra, chat → mix → [Output EQ APO: EQ → + příposlech → limiter] → sluchátka
```

## 4. Rozpočet latence

**Hypotéza, změřit v krocích 1, 4 a 8 návodu.** Značení: `Pm` = perioda enginu na mikrofonu, `Pv` = perioda
enginu na výstupu, `hw` = zpoždění ovladačů a převodníků na obou koncích dohromady.

- Vzorek čeká v enginu, než se naplní blok: až `Pm`.
- Čtečka musí hrát plynule, takže se drží za zapisovačem o `Pm` + rezervu na jitter volání.
- Output APO míchá do bloku, který engine teprve chystá. Ten se přehraje za `Pv` + ovladač.

```
ústa → ucho ≈ Pm + rezerva (1–3 ms) + Pv + hw (odhad 3–8 ms)
```

| Perioda mikrofonu | Perioda výstupu | Odhad ústa → ucho | Jak toho dosáhnout                           |
| ----------------- | --------------- | ----------------- | -------------------------------------------- |
| 10 ms             | 10 ms           | 25–35 ms          | výchozí stav Windows                         |
| 10 ms             | 2,7 ms          | 18–25 ms          | krok 7 návodu: krátká perioda jen na výstupu |
| 2,7 ms            | 2,7 ms          | 9–15 ms           | krok 7 návodu: krátká perioda i na mikrofonu |

Rozhoduje tedy perioda enginu, ne náš kód. Krátkou periodu si může vyžádat daemon přes
`IAudioClient3` (podrobnosti a citace v kapitole 11). Jestli ji zvukovka SteelSeries umí, ukáže
krok 1 návodu.

Kolik je dost (**Z dokumentace**, kapitola 11): u naslouchadel s uzavřenou vložkou přestává
zpoždění vadit kolem 9 ms, nad 15–20 ms už ruší vlastní řeč, norma pro telefonní náhlavní
soupravy chce ≤ 5 ms. Pracovní cíl: **do 10 ms dobré, do 20 ms použitelné, nad 30 ms nevydávat
jako hotové.**

Z toho plyne: výchozí perioda 10 ms na obou koncích (25–35 ms) nejspíš stačit nebude. Krátká
perioda není vylepšení na konec, ale podmínka dobrého výsledku. Jestli ji zařízení umí, zjistí
krok 1 návodu. Funkce se postaví i bez ní (1.1) a latenci ukáže uživateli.

Riziko kratší periody na mikrofonu: řetězec dnes dostane 480 vzorků na volání a jeden krok modelu
se vejde do 10 ms. Při 128 vzorcích padne celý krok modelu (DeepFilterNet 3, DPDFNet) na jedno
volání s rozpočtem 2,7 ms. Když ho překročí, vypadne zvuk mikrofonu všem aplikacím. Proto je
krátká perioda na mikrofonu podmíněná měřením zátěže (krok 7 návodu).

## 5. Zpracování příposlechu

Vše vzorek po vzorku, bez lookaheadu, na vzorkovací frekvenci mikrofonu (před `RateBridge`):

| Stupeň                 | Výchozí                     | Proč                                                                               |
| ---------------------- | --------------------------- | ---------------------------------------------------------------------------------- |
| EQ příposlechu         | horní propust 200 Hz        | Stejný editor pásem jako EQ hlasu. Hloubky už dodává kost (okluze), proto ořez.    |
| Expander (měkká brána) | 4 pásma, hloubka 18 dB      | V pauzách stáhne šum. Viz níže.                                                    |
| Hlasitost              | 100 % (0–300 %)             | 100 % = hlas tak hlasitý, jak ho posílá vyrovnání hlasitosti. Náběh ~10 ms.        |
| Vztažná úroveň         | `plugin.leveler_gain_db()`  | Zisk vyrovnání hlasu: příposlech nezávisí na zesílení mikrofonu ve Windows.        |

Expander pracuje **po pásmech** (`BandExpander` v `sidetone.rs`), jak to dělají naslouchadla:

- Čtyři pásma: pod 500 Hz, 500 Hz – 2 kHz, 2–5 kHz, nad 5 kHz. Pásmo je rozdíl dvou dolních
  propustí, takže při ziscích 1 dá součet pásem zpět přesně vstup (test: chyba pod −90 dB).
- Úroveň každého pásma měří zvlášť strmá pásmová propust: hlasitá samohláska tak neotevře
  pásmo sykotu.
- Šum mikrofonu má stálé spektrum. Každé pásmo si proto dno změří samo (minimum úrovně za
  poslední 3 s) a stáhne se, kdykoli v něm není nic hlasitějšího.
- Daň: pásma se překrývají pozvolna, zavřené pásmo ubere tónu v sousedním až ~1 dB.
- `set_single_band(true)` dá jeden zisk pro celý signál (obyčejný expander), pro porovnání.

Pravidla jednoho pásma:

- Úroveň: špičková, doběh 10 ms. Vyhodnocuje se jednou za 16 vzorků.
- Pásmo se otevírá od „dno + citlivost − 4 dB" a je celé otevřené na „dno + citlivost".
  Citlivost je 3–20 dB, výchozí 10. Ruční práh v dBFS není: po pásmech nedává smysl.
- Zavřené pásmo je tišší o „hloubku" (0–40 dB, výchozí 18). Žádné tvrdé vypnutí: zbytek šumu
  zní přirozeněji než ticho, které se zapíná a vypíná.
- Otevření 1 ms, podržení 150 ms, zavření 150 ms.

Otestováno (`cargo test -p gamersequalizer-common sidetone`): bílý šum −60 dBFS je po ustálení
o 16–19 dB tišší; hlas −30 dBFS projde od 2. ms nejvýš o 1,5 dB tišší; sykot nad 6 kHz pod
tónem 200 Hz ubude o víc než 12 dB, kdežto s jedním pásmem ho tón drží otevřený.

Co expander umí a co ne (říct to i v UI):

- Stálý šum v pauzách zmizí, tedy i po celou dobu hry, kdy nemluvíš. To je hlavní stížnost na
  hardwarový sidetone.
- Během řeči se stáhnou pásma, kde hlas zrovna není (typicky sykot ve výškách pod samohláskou).
  Kde hlas je, šum zůstane a hlas ho maskuje.
- Krátké hlasité zvuky nad prahem (klávesy, klik myši) projdou. Bez zpoždění je v první
  milisekundě od hlásky rozlišit nejde. Zmírní je obě propusti a mikrofon blízko úst.

### 5.1 Volitelně: potlačovače po rámcích

Režim potlačení šumu je přepínač s pěti polohami: **vypnuto / pásmový expander / WebRTC NS /
RNNoise / RNNoise 0.2**. Poslední tři pracují po rámcích, takže stojí zpoždění. Uživatel si
vybere a sekce mu cenu ukáže. Všechny tři knihovny už v projektu jsou.

**Naměřeno** (2026-11-10, Mac; bloky 480 vzorků při 48 kHz). Zpoždění = vrchol vzájemné
korelace vstupu a výstupu na umělém „hlasu" (harmonické 120 Hz se slabikovou obálkou); hlídá
ho test `delay_matches_the_measured_values`. Stálý šum = bílý šum, útlum po 6 s, hloubka 40 dB.

| Režim            | Knihovna                              | Zpoždění | Útlum stálého šumu                                                     |
| ---------------- | ------------------------------------- | -------- | ---------------------------------------------------------------------- |
| Pásmový expander | vlastní (kapitola 5)                  | 0 ms     | podle hloubky (výchozí 18 dB)                                          |
| WebRTC NS        | `sonora`, `Config::noise_suppression` | 6,9 ms   | 16–17 dB (pevná úroveň High), stejně od −60 do −40 dBFS                |
| RNNoise          | `nnnoiseless` 0.5                     | 10,0 ms  | **3–9 dB** při −50 a −40 dBFS; 37 dB při −60 dBFS nebo po prvním hlasu |
| RNNoise 0.2      | `crates/vendor/rnnoise2`, model Full  | 20,0 ms  | 34–37 dB, stejně od −60 do −40 dBFS                                    |

RNNoise z roku 2017 tedy stálý šum, se kterým začne od nuly, skoro nechá. Zabere, až uslyší
hlas. WebRTC NS a RNNoise 0.2 to nepotřebují. Pro „mikrofon šumí při hře, kdy nemluvím" jsou
proto spolehlivější expander, WebRTC NS a RNNoise 0.2.

Co měření neříká: kolik který režim ubere z hlasu (signály jsou umělé). Když bloky od enginu
nemají 480 vzorků (krátká perioda, 44,1 kHz), přidá se fronta až na celý rámec, tedy až +10 ms.

Jak se liší:

- **WebRTC NS** je klasický Wienerův filtr s odhadem šumu, bez sítě. Bere jen stálý šum, klávesy
  nechá. Nejmenší zpoždění z rámcových režimů, nejmírnější útlum.
- **RNNoise** (síť z roku 2017) bere i klávesy a šum pod hlasem. Lehká: dva průchody dnes stojí
  ~0,5 % jádra (`ClickRescue`).
- **RNNoise 0.2** (**z DEVLOGu**, milník 35): klávesy v pauzách tlumí asi dvakrát víc v dB
  (36–41 proti 18), ale ukusuje tiché začátky a konce slabik (30–37 % jich ztratí přes 10 dB,
  proti 9 %). Průchod stojí ~2,8 % jádra a zpoždění je dvojnásobné (`LATENCY_FRAMES = 2`).
  Na výběr je, doporučený není.

Odhad ústa → ucho podle režimu (základ z kapitoly 4 + zpoždění režimu + případná fronta):

| Perioda mikrofonu / výstupu | Expander | WebRTC NS | RNNoise  | RNNoise 0.2 |
| --------------------------- | -------- | --------- | -------- | ----------- |
| 10 / 10 ms                  | 25–35 ms | 32–42 ms  | 35–45 ms | 45–55 ms    |
| 10 / 2,7 ms                 | 18–25 ms | 25–32 ms  | 28–35 ms | 38–45 ms    |
| 2,7 / 2,7 ms                | 9–15 ms  | 26–32 ms  | 29–35 ms | 39–45 ms    |

Krátká perioda na mikrofonu tedy rámcovým režimům nepomůže: co ušetří, sní fronta na celý rámec.

Zpoždění nedělá síť, ale rámec: jemné rozlišení ve frekvenci potřebuje dlouhé okno. Bez zpoždění
to jde jen tak, že se zisky spočítají stranou a na živý signál se nasadí filtrem v čase. To dělá
pásmový expander (hrubě, 4 pásma) a postranní řetězec z kapitoly 7.1 (jemně).

Provedení (`SidetoneDenoiser` v `crates/gamersequalizer-mic-vst/src/sidetone_denoise.rs`):

- Vlastní instance všech tří jen pro příposlech, postavené jednou mimo zvukové vlákno.
  RNNoise 0.2 bez načtených vah nahradí RNNoise z 2017 a UI to řekne.
- Posuvník „hloubka" platí pro expander a oba RNNoise: u RNNoise je to podíl suchého signálu,
  zpožděného stejně jako výstup sítě. WebRTC NS běží na pevné úrovni `High`.
- Jen při 48 kHz na mikrofonu. Jiná frekvence = expander a hláška v UI.
- Přepnutí režimu začíná z ticha: mezera dlouhá jako zpoždění nového režimu. Prolínání se
  nedělá.
- Expander je v rámcových režimech vypnutý.

## 6. Jak je to rozdělené

| Vrstva         | Kde                                                      | Stav   | Co dělá                                                                     |
| -------------- | -------------------------------------------------------- | ------ | --------------------------------------------------------------------------- |
| Jádro          | `crates/gamersequalizer-common/src/sidetone.rs`          | hotovo | kruhový buffer, čtečka, expander, řetězec, matice dvojic, otisk zařízení    |
| Sdílená paměť  | `crates/gamersequalizer-common/src/shm.rs` (v20)         | hotovo | nastavení dvojice, otisky zařízení, telemetrie obou APO, buffer             |
| Rámcové režimy | `crates/gamersequalizer-mic-vst/src/sidetone_denoise.rs` | hotovo | WebRTC NS, RNNoise, RNNoise 0.2                                             |
| Mic APO        | `crates/gamersequalizer-apo/src/lib.rs`                  | zbývá  | odbočka ze surového vstupu → řetězec → buffer                               |
| Output EQ APO  | tamtéž                                                   | zbývá  | čtečka → přimíchání za EQ, před limiter                                     |
| Daemon         | `win_audio/sidetone.rs`                                  | zbývá  | matice v souboru, nastavení aktuální dvojice, držení streamů, nízká latence |
| Okno           | `ui/tab_sidetone.rs`                                     | zbývá  | sekce „Příposlech"                                                          |

Co z návrhu plyne pro jednotlivé vrstvy:

- **Odbočka je nezávislá na řetězci mikrofonu.** Běží, i když řetězec selhal nebo je model
  přetížený, a vstupní buffer jen čte. Odchozí zvuk mikrofonu se zapnutím příposlechu nezmění.
- **Hlavní vypínač** v záhlaví a **ztlumení mikrofonu** stáhnou příposlech plynule na nulu.
  Zapisovač pak buffer uvolní, což čtečka bere jako úmyslné zastavení, ne jako výpadek.
- **Čtečka** převádí vzorkovací frekvenci, vyrovnává rozdíl hodin mikrofonu a sluchátek (±200 ppm
  bez výpadku) a drží ve frontě jen tolik, kolik je bezpečné: jeden blok zapisovače + rezervu
  2 ms. Po každém skutečném výpadku rezervu o 1 ms zvětší (nejvýš na 10 ms).
- **Přimíchání je za EQ a před limiterem.** Hlas tak nedostane herní profil a limiter hlídá
  součet. Mono do levého i pravého kanálu.
- **Zařízení.** Daemon zapisuje otisk mikrofonu a výstupu aktuální dvojice. Output APO hraje,
  jen když otisk jeho vlastního zařízení sedí; když své zařízení nezná, nehraje. Mic APO, které
  své zařízení nezná, zapisovat smí (špatný mikrofon je jen špatný hlas, ne zpětná vazba).
- **Daemon** drží tichý přehrávací stream, dokud někdo nahrává (výstupní engine jinak v tichu
  stojí), a při zkoušce navíc nahrávací stream (aby mic APO běželo). Nastavení přepisuje jen
  při změně dvojice nebo souboru; při tažení posuvníku je zapisuje okno přímo.
- **Matice dvojic** je v `profiles/sidetone.json`. Okno ji ukládá, daemon ji znovu načte při
  změně souboru.

## 7. Postup

Kroky, příkazy, kód k vložení a co má být výsledkem: `docs/sidetone-implementace.md`.

| Krok | Co                                                     | Stav   |
| ---- | ------------------------------------------------------ | ------ |
| -    | jádro, sdílená paměť v20, rámcové režimy, testy        | hotovo |
| 0    | přeložit a pustit testy na Windows                     | zbývá  |
| 1    | změřit periody enginu obou zařízení                    | zbývá  |
| 2    | mic APO zapisuje do bufferu, APO pozná své zařízení    | zbývá  |
| 3    | Output EQ APO přimíchává                               | zbývá  |
| 4    | zkouška na zvuku bez UI; obsahuje loopback příposlech? | zbývá  |
| 4.5  | bere se loopback před hlasitostí Windows, nebo za ní?  | zbývá  |
| 5    | daemon: matice, vlákno, držení streamů                 | zbývá  |
| 6    | sekce „Příposlech" v okně                              | zbývá  |
| 7    | nízká latence                                          | zbývá  |
| 8    | měření: latence, AEC, 1 vs 4 pásma, ztráta hlasu       | zbývá  |
| 9    | návrh: co z příposlechu získá echo canceller (7.2)     | čeká   |

### 7.1 Později, jen když expander nestačí a rámcové režimy jsou moc pozdě

- **RNNoise jako postranní řetězec.** Síť běží vedle, zvuk příposlechu jde dál bez zpoždění.
  Z ní se vezmou jen zisky po pásmech (z minulého rámce, 10–20 ms staré) a nasadí se na živý
  signál bankou IIR filtrů. Stálý šum se mění pomalu, takže staré zisky sedí; první hláska po
  pauze je 10–20 ms tišší v zašuměných pásmech. `nnnoiseless` zisky nevydává, vlastní port
  `crates/vendor/rnnoise2` jde upravit. **Hypotéza**, nejdřív ověřit offline na nahrávkách.
- Totéž s klasickým odhadem šumu místo sítě (`sonora-ns` už v projektu je; ověřit, jestli
  vydá zisky Wienerova filtru, jinak vlastní odhad minima po pásmech FFT).
- Citlivost podle stavu řeči: detektory řetězce (RNNoise, Silero) přijdou pozdě na první
  hlásku, ale stačí na „posledních 2 s nikdo nemluvil → citlivost o 6 dB výš".
- Hlasitost podle naučené úrovně hlasu (`mic_leveler_level_db_x10`): stejně hlasitý příposlech
  s každým mikrofonem.

### 7.2 Co z příposlechu získá echo canceller (návrh, nerozhodnuto)

Příposlech staví tři věci, které echo canceller (větev `aec3-mic-gain`, milníky 43–50) nemá.
Nic z toho není součástí kroků 0–8 a o všem rozhoduje člověk. Podklady pro implementaci jsou
v návodu, krok 9.

| Návrh                                               | Co příposlech dává                                                                                            | Co by to vyřešilo                                                                      |
| --------------------------------------------------- | ------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------- |
| A. AEC vypnuté pro dvojici se zapnutým příposlechem | Zapnutím dvojice uživatel říká „tento výstup jsou sluchátka" (pravidlo 5 návodu).                             | Na sluchátkách echo není; AEC3 tam stojí ~9 ms latence a 0,6–0,7 dB hlasu (milník 44). |
| B. Kalibrace echa tlačítkem                         | Vlákno daemona umí na povel držet mikrofon i výstup otevřené bez nahrávající aplikace (`sidetone_test_beat`). | Zpoždění a síla echa změřené zkušebním signálem ještě před prvním hovorem.             |
| C. Klíč cache podle dvojice                         | Matice pracuje s mikrofonem, který se opravdu používá (`get_selected_mic_id`).                                | Cache zpoždění a síly echa dnes klíčuje výchozí mikrofon Windows (`apo_link.rs`).      |

Proč to stojí za úvahu:

- **A.** Rozpoznat „žádné echo" z měření se nepodařilo spolehlivě. Vypínání podle sondy echa
  kmitalo (milník 36). Měřič síly echa bral hlas za echo (milník 46) a po opravě už jen říká
  „nepotvrzeno" (milník 48), což neznamená „echo není". Zapnutý příposlech je naopak výslovné
  tvrzení uživatele.
- **B.** Pasivní měřič potvrdil echo jen na 2 z 11 nahrávek s reproduktory (milníky 48 a 50):
  hudba a dozvuk dávají slabou shodu úrovní. Šum s řízeně kolísající hlasitostí, do ticha, ji
  dá. Zpoždění se navíc dnes změří až během hovoru, a jen když zrovna něco hraje.

Otázky pro člověka:

- A: má jít AEC pro takovou dvojici zapnout ručně (Auto / Zapnuto / Vypnuto)? Otevřená
  sluchátka nahlas echo dělat mohou.
- B: smí aplikace při kalibraci asi 8 s hrát šum z reproduktorů? Kdy ji nabízet: při prvním
  spuštění, u každého nového výstupu, jen na vyžádání?
- B závisí na kroku 4.5. Když se loopback bere před hlasitostí Windows, platí změřená síla echa
  jen pro hlasitost, při které se měřilo.

## 8. Kdy je hotovo

- Latence ústa → ucho změřená a zapsaná v DEVLOGu; cíl ≤ 10 ms, přijatelné ≤ 20 ms. Horší
  výsledek funkci nezastaví, sekce ho ukáže červeně.
- Bez nahrávající aplikace příposlech mlčí a mikrofon není otevřený. Zkouška ho zapne a po
  odchodu ze sekce do 1 s skončí (indikátor mikrofonu zhasne).
- Dvě dvojice s různým nastavením: přepnutí zařízení použije správný záznam do 1 s.
- Ztlumení mikrofonu příposlech ztiší bez lupnutí.
- Hlasitost 0–300 % (100 % = úroveň vyrovnání hlasitosti hlasu), bez lupání při tažení posuvníku.
- Šum v pauzách o ≥ 15 dB tišší než při vypnutém potlačení v režimu expander a RNNoise 0.2,
  WebRTC NS ≥ 12 dB. RNNoise (2017) až poté, co uslyší hlas (kapitola 5.1).
- Rámcové režimy: přidaná latence změřená a ukázaná u volby; při jiné frekvenci než 48 kHz
  sekce řekne, že běží expander.
- 10 minut řeči a hry: 0 podtečení, 0 výpadků mikrofonu (`mic_call_gaps` neroste rychleji než dnes).
- Odchozí zvuk mikrofonu je stejný se zapnutým i vypnutým příposlechem (odbočka buffer jen čte;
  hlídá to test `sidetone_plays_on_the_pair_output_only`).
- Telemetrie AEC se zapnutím příposlechu nezmění: stav, zpoždění, síla echa (`echo ...` v řádcích
  `[Chain]` zůstane `not confirmed` nebo na stejné hodnotě) a `fit` vedle ní nestoupne (krok 8.2).
- Výstup do reproduktorů: příposlech nehraje.
- `cargo test` a `cargo clippy --workspace --all-targets` čisté.

## 9. Rizika a otevřené body

| Č. | Riziko nebo otázka                                                                                                                                                | Co s tím                                                                                                                                |
| -- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| 1  | **Vlastní hlas v referenci AEC.** Když loopback obsahuje výstup EFX, AEC3 dostane hlas jako „hru".                                                                | Krok 4.4 návodu. Když ano: referenci plnit z Output EQ APO (před přimícháním) místo z WASAPI loopbacku.                                 |
| 2  | Zvukovka neumí periodu pod 10 ms. Microsoft ji dokládá jen pro HDAudio; pro USB ovladače údaj není.                                                               | Krok 1 návodu. Zůstane ~25–35 ms, podle literatury rušivé. Staví se i tak (1.1), latence je vidět v UI.                                 |
| 3  | Krátká perioda na mikrofonu přetíží řetězec.                                                                                                                      | Krok 7 návodu: na mikrofonu jen s rezervou, jinak jen na výstupu.                                                                       |
| 4  | Zpětná vazba při netěsných sluchátkách a vysoké hlasitosti.                                                                                                       | Strop 300 %, limiter.  Detektor pískání nedělat, dokud se to nestane.                                                                   |
| 5  | Exkluzivní režim přehrávání obchází engine i APO.                                                                                                                 | Příposlech v tu chvíli nehraje. Napsat do UI.                                                                                           |
| 6  | Tichý stream daemona změní chování loopbacku (stálé tiché pakety místo žádných).                                                                                  | V kroku 8.2 návodu ověřit stav AEC „bez reference" vs „ticho" a zpoždění.                                                               |
| 7  | Id zařízení se po přeinstalaci ovladače změní a záznam v matici osiří.                                                                                            | Uživatel dvojici zapne znovu; starý záznam smaže v seznamu. Párování podle názvu nedělat.                                               |
| 8  | APO pozná své zařízení podle poslední položky kolekce v `Initialize`. Dokumentováno je to pro `APOInitSystemEffects2`; naše APO dostává nejspíš starší strukturu. | Krok 4.3 návodu porovná otisk, který APO vidí, se skutečným. Když výstupní APO zařízení nepozná, příposlech nehraje a práce se zastaví. |

K riziku 1 od milníku 47: echo canceller má měřič síly echa, který echo pozná podle toho, že
úroveň mikrofonu sleduje úroveň reference. Kdyby reference obsahovala vlastní hlas, sledoval by
ji mikrofon dokonale. Měřič by pak potvrdil silné echo, které neexistuje, zapamatoval si ho pro
dvojici a AEC3 by při každém startu tlumilo hlas. Proto kroky 4.4 a 8.2 hlídají i `echo` a `fit`
v logu `[Chain]`.

## 10. Co se nedělá

- Režim „stále zapnuto" bez nahrávající aplikace (mikrofon by byl otevřený pořád).
- DeepFilterNet 3 a DPDFNet v příposlechu (desítky ms navíc). Ze sítí jen RNNoise a RNNoise 0.2.
- Ovládání hardwarového sidetone SteelSeries přes HID.
- Bluetooth v režimu hands-free (latence spojení je řádově vyšší než cokoli tady).
- Příposlech do více výstupů najednou, stereo, vlastní plný EQ, rychlý posuvník ve směšovači.

## 11. Z dokumentace

Ověřeno 2026-11-10. Co se najít nepodařilo, je řečeno výslovně.

**Krátká perioda enginu** ([Low Latency Audio](https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/low-latency-audio)):

- Platí pro celé zařízení, ne pro jeden stream: „if one application requests the usage of small
  buffers, then the audio engine will start transferring audio using that particular buffer size.
  In that case, all applications that use the same endpoint and mode will automatically switch to
  that small buffer size." Naše APO tedy dostanou krátké bloky, jakmile si je daemon vyžádá.
- Platí pro přehrávání i nahrávání. Jestli jsou obě strany jednoho zařízení svázané, dokument neříká.
- Ovladače: doložený je jen vestavěný HDAudio, „buffer sizes between 128 samples (2.66ms@48kHz)
  and 480 samples (10ms@48kHz)". Pro `usbaudio.sys` a `usbaudio2.sys` Microsoft minimum neuvádí;
  každý ovladač hlásí své. **Pro USB zvukovku SteelSeries to ukáže až krok 1 návodu.**
- Požadavek na APO při krátké periodě dokument neuvádí.
- `InitializeSharedAudioStream` může selhat s `AUDCLNT_E_ENGINE_PERIODICITY_LOCKED`.

**Kde se bere loopback vůči EFX: nenalezeno.**
[Architektura APO](https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/audio-processing-object-architecture)
říká jen, že EFX je poslední stupeň („after the mix"). Otázka na Microsoft Q&A je bez odpovědi.
DEVLOG (milník 19) píše „post-EFX", ale bez měření. Proto krok 4.4 návodu.

**Snesitelné zpoždění vlastního hlasu** (druhotné zdroje; plné texty studií se nepodařilo otevřít):

| Zdroj                                             | Údaj                                                   |
| ------------------------------------------------- | ------------------------------------------------------ |
| Stone & Moore 2003, naslouchadla, uzavřená vložka | zpoždění nevadí do ~9 ms                               |
| Stone a kol. 2008, otevřená vložka                | do 5–6 ms                                              |
| Stone & Moore 1999 a 2002                         | ruší nad 20 ms; vlastní řeč nad 15 ms (suchá místnost) |
| 3GPP TS 26.131, náhlavní soupravy                 | zpoždění sidetone ≤ 5 ms                               |

Přehled studií: <https://pmc.ncbi.nlm.nih.gov/articles/PMC3513830>. In-ear odpovídá uzavřené
vložce. Horní propust z kapitoly 5 by měla snesitelnost zlepšit (méně překryvu s hlasem vedeným
kostí), ale to je **hypotéza**.

**Jiný software:** nic spolehlivého. „Poslouchat toto zařízení" podle uživatelů kolem 200 ms,
Voicemeeter udává pod 30 ms. O Sonaru ani Equalizer APO se údaj nenašel.
