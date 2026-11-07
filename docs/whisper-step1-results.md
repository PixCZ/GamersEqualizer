# Šepot, krok 1: co vidí jednotlivé klíče gate

Odpověď na „Krok 1: co přesně změřit“ z analýzy šepotu (Fable, navazuje na `mic-processing-report.md`).
Změřeno 2026-11-06 testem `whisper_probe` v `crates/gamersequalizer-mic-vst/src/ai_denoiser.rs`:

```
cargo test -p gamersequalizer-mic-vst --release whisper_probe -- --ignored --nocapture
```

Vstupy: `test_data/private/scenario.wav` (1. kanál) a `test_data/private/keys.wav` (vlastní klávesnice,
hlasité psaní bez řeči). **Časy úseků jsou pořád odhad podle úrovní**, nikdo je neověřil poslechem.
Úsek „šepot 25–28 s“ proto obsahuje i pauzy mezi slovy. Výstup DFN3 tam má medián −63 dBFS, tedy
těsně nad dnem, takže šepot zabírá zhruba polovinu rámců, možná méně. Procenta „otevřeno“ u šepotu
nejsou podíl ze slyšitelného šepotu, ale ze všech rámců úseku.

Všechny hodnoty jsou po rámcích 10 ms, modely na výchozí síle 0,75 a bez gate.

## 1. RNNoise detektor na surovém vstupu, zesílení jen do detektoru

Pravděpodobnost řeči: medián / 90. percentil / maximum a podíl rámců nad 0,05, 0,3 a 0,5.

| Zesílení | Úsek               | p50  | p90  | max  | > 0,05 | > 0,3 | > 0,5 |
| -------- | ------------------ | ---- | ---- | ---- | ------ | ----- | ----- |
| 0 dB     | ticho 1–10 s       | 0,00 | 0,00 | 0,03 | 0,0 %  | 0,0 % | 0,0 % |
| 0 dB     | tichý hlas 17–19 s | 0,17 | 1,00 | 1,00 | 57,0 % | 46,0 % | 37,0 % |
| 0 dB     | šepot 25–28 s      | 0,00 | 0,01 | 0,37 | 4,7 %  | 0,7 % | 0,0 % |
| 0 dB     | klávesy 85–110 s   | 0,00 | 0,00 | 0,03 | 0,0 %  | 0,0 % | 0,0 % |
| 0 dB     | hudba 111–115 s    | 0,60 | 0,97 | 1,00 | 93,0 % | 76,8 % | 62,5 % |
| 0 dB     | keys.wav           | 0,00 | 0,01 | 0,39 | 0,2 %  | 0,1 % | 0,0 % |
| +12 dB   | šepot 25–28 s      | 0,00 | 0,02 | 0,45 | 5,3 %  | 1,7 % | 0,0 % |
| +12 dB   | ticho 1–10 s       | 0,00 | 0,00 | 0,16 | 2,0 %  | 0,0 % | 0,0 % |
| +24 dB   | šepot 25–28 s      | 0,00 | 0,03 | 0,53 | 7,7 %  | 3,3 % | 1,0 % |
| +24 dB   | ticho 1–10 s       | 0,01 | 0,02 | 0,58 | 4,4 %  | 2,6 % | 0,3 % |
| +24 dB   | keys.wav           | 0,01 | 0,03 | 0,56 | 6,1 %  | 0,3 % | 0,1 % |

Závěr:
* Šepot leží na ~0, devět rámců z deseti pod 0,01. Hypotéza z 2.2 je potvrzená.
* Zesílení nepomáhá. Při +24 dB stoupne šepot jen na 1 % rámců nad 0,5, ale stejně stoupne i ticho
  a klávesy. Normalizace úrovně před detektorem tedy nemá smysl.
* Ladit prahy taky nemá smysl. Při prahu 0,05 by šepot „slyšel“ jen v 4,7 % rámců.
* Hudba: detektor ji bere jako řeč (62,5 % rámců nad 0,5), což potvrzuje zprávu.

## 2. Výstupní detektor a úroveň výstupu modelu

Pravděpodobnost RNNoise na výstupu modelu (bez gate) a úroveň výstupu v dBFS.

| Model   | Úsek               | p50 pst. | > 0,3  | dBFS p10 | p50    | p90    | max    |
| ------- | ------------------ | -------- | ------ | -------- | ------ | ------ | ------ |
| DFN3    | ticho 1–10 s       | 0,00     | 0,0 %  | −74,9    | −69,7  | −63,2  | −56,1  |
| DFN3    | tichý hlas 17–19 s | 0,05     | 33,0 % | −67,0    | −51,1  | −29,8  | −20,8  |
| DFN3    | šepot 25–28 s      | 0,00     | 0,0 %  | −69,8    | −63,0  | −42,2  | −34,2  |
| DFN3    | klávesy 85–110 s   | 0,00     | 0,0 %  | −80,7    | −72,8  | −63,1  | −53,0  |
| DFN3    | hudba 111–115 s    | 0,00     | 1,5 %  | −78,6    | −69,4  | −61,3  | −58,6  |
| DFN3    | keys.wav           | 0,00     | 0,0 %  | −96,7    | −66,8  | −53,9  | −37,6  |
| DPDFNet | šepot 25–28 s      | 0,00     | 0,0 %  | −108,8   | −101,0 | −84,5  | −62,9  |
| DPDFNet | keys.wav           | 0,00     | 0,0 %  | −102,9   | −94,8  | −84,9  | −65,7  |
| RNNoise | šepot 25–28 s      | 0,00     | 0,7 %  | −76,3    | −68,5  | −51,0  | −42,2  |
| RNNoise | hudba 111–115 s    | 0,90     | 82,8 % | −74,1    | −66,7  | −59,0  | −54,0  |
| RNNoise | keys.wav           | 0,00     | 0,1 %  | −68,5    | −60,3  | −50,4  | −31,3  |

Závěr:
* Výstupní detektor šepot neslyší ani na čistém výstupu DFN3 (0 % nad 0,3). Problém je v síti
  detektoru, ne v šumu na vstupu.
* DFN3 hudbu z místnosti skoro celou odstraní (výstupní detektor 1,5 %), RNNoise ne (82,8 %). U DFN3
  drží gate při hudbě otevřenou jen vstupní detektor.
* DPDFNet šepot maže (p90 −84,5 dBFS), potvrzuje 2.4.
* DFN3 na hlasitém psaní (`keys.wav`) nechá projít špičky až −37,6 dBFS. Na tom závisí cena N2.

## 3. DFN3 `lsnr` (nápad N2b)

Minimum za celý soubor je −15,0 dB (stejné jako `lsnr_min` v upstream `config.ini`; vrací ho i větev
„digitální ticho“). „Klíč“ = podíl rámců, kde `lsnr` > práh aspoň N rámců po sobě.

| Úsek               | p10   | p50   | p90  | max  | > 0 ×5 | > 5 ×5 | > 10 ×5 | > 10 ×10 |
| ------------------ | ----- | ----- | ---- | ---- | ------ | ------ | ------- | -------- |
| ticho 1–10 s       | 9,0   | 14,4  | 20,4 | 27,2 | 99,6 % | 98,0 % | 69,8 %  | 59,6 %   |
| tichý hlas 17–19 s | 10,8  | 27,8  | 35,0 | 35,0 | 98,0 % | 89,0 % | 85,5 %  | 78,5 %   |
| šepot 25–28 s      | 10,3  | 21,8  | 33,6 | 34,9 | 98,7 % | 96,3 % | 78,0 %  | 68,3 %   |
| klávesy 85–110 s   | −7,4  | 3,1   | 12,3 | 26,1 | 54,0 % | 20,9 % | 6,9 %   | 3,2 %    |
| hudba 111–115 s    | −10,4 | −1,0  | 8,5  | 14,8 | 28,5 % | 12,0 % | 0,5 %   | 0,0 %    |
| keys.wav           | −15,0 | −6,4  | 8,8  | 21,8 | 8,8 %  | 3,4 %  | 1,5 %   | 0,7 %    |

Závěr: **N2b zamítnuto.** `lsnr` měří odstup od šumu, ne přítomnost řeči. Tichá místnost má vysoké
`lsnr` (medián 14,4 dB), takže klíč by gate držel otevřenou skoro pořád, i když nikdo nemluví.
Vedlejší nález: `DF_MIN_DB_THRESH = −20` opravdu leží pod minimem −15, takže stupeň „nulová maska“
je vypnutý (kapitola 3 analýzy platí). Komentář u konstanty v kódu to popisuje nepřesně.

## 4. Energetický klíč na výstupu DFN3 (nápad N2)

Algoritmus přesně podle náčrtu v N2 (dno stoupá 2 dB/s, klesá hned, nejníž −75 dBFS). Podíl rámců,
kde by klíč otevřel gate. „+hold“ = s holdem 200 ms jako skutečná gate (bez lookahead a bez
ostatních detektorů).

| Úsek               | 12 dB ×5 | 9 dB ×5 | 15 dB ×5 | 12 dB ×10 | +hold 12 ×5 | +hold 9 ×5 | +hold 15 ×5 | +hold 12 ×10 |
| ------------------ | -------- | ------- | -------- | --------- | ----------- | ---------- | ----------- | ------------ |
| ticho 1–10 s       | 1,1 %    | 3,8 %   | 0,0 %    | 0,0 %     | 9,6 %       | 25,9 %     | 0,0 %       | 0,0 %        |
| tichý hlas 17–19 s | 39,5 %   | 44,0 %  | 39,0 %   | 27,5 %    | 78,0 %      | 88,0 %     | 68,0 %      | 63,5 %       |
| šepot 25–28 s      | 30,7 %   | 36,3 %  | 27,3 %   | 27,0 %    | 43,7 %      | 60,7 %     | 37,7 %      | 38,3 %       |
| klávesy 85–110 s   | 3,2 %    | 6,0 %   | 1,0 %    | 1,0 %     | 12,8 %      | 22,2 %     | 6,3 %       | 4,1 %        |
| hudba 111–115 s    | 5,2 %    | 14,2 %  | 0,0 %    | 0,0 %     | 31,0 %      | 54,8 %     | 0,0 %       | 0,0 %        |
| keys.wav           | 13,2 %   | 21,5 %  | 8,5 %    | 5,3 %     | 45,6 %      | 57,0 %     | 32,6 %      | 17,9 %       |

Pro srovnání: dnešní gate (oba RNNoise detektory) se v šepotu otevře v 0 % rámců.

Závěr:
* **N2 šepot vidí.** V šepotu otevírá 27–31 % všech rámců úseku (bez holdu), v tichu 0–1 %. Protože
  šepot zabírá jen část úseku, je skutečné pokrytí šepotu vyšší. Bez přesných časů ho nejde spočítat.
* **Cena je hlasité psaní.** Na `keys.wav` by výchozí návrh (12 dB ×5) s holdem držel gate otevřenou
  ve 46 % času. Dnešní gate tam drží výstupní detektor skoro zavřenou. Přísnější varianta 12 dB ×10
  cenu sníží na 18 % a šepot skoro neztratí (27,0 % místo 30,7 % bez holdu).
* Kandidát pro krok 3: **12 dB ×10** (nebo 15 dB ×5). Ticho a hudba 0 %, klávesy v nahrávce 1 %,
  hlasitá klávesnice 18 % s holdem.

## 5. Nová nahrávka podle scénáře pro šepot

Kapitoly 1–4 jsou ze `scenario.wav`, kde se úseky jen odhadovaly. Pak vznikla nahrávka přímo pro
šepot: `test_data/private/whisper_scenario.wav`, 84 s, bez přebuzení (špička −8,5 dBFS), šum
místnosti kolem −66 dBFS. Byla nahraná podle tohoto scénáře (každý úsek ~10 s, mezi úseky tlesknutí
a pauza):

1. ticho,
2. šepot věty „Dneska večer hrajeme ranked, potřebuju, abys kryl levou stranu, já půjdu na bod B.“,
3. tišší šepot,
4. šepot a normální psaní,
5. šepot a mlácení do klávesnice,
6. tichý normální hlas,
7. tichý hlas a mlácení do klávesnice,
8. jen klávesnice,
9. střídání šepotu a hlasu po slovech,
10. ticho.

Tlesknutí v nahrávce nejsou poznat, takže hranice úseků určují pauzy (test `vad_timeline`, po 0,5 s).
Úseků se zvukem je 7, scénář jich měl 8. Přiřazení podle úrovně a detektoru řeči
(**zatím nepotvrzené poslechem**):

| Úsek (s)  | Pravděpodobně bod scénáře   | Proč                                                    |
| --------- | --------------------------- | ------------------------------------------------------- |
| 1–18      | 1 ticho                     | −66 dBFS, detektor 0 %                                  |
| 20–24     | 2 šepot                     | detektor 0 %; 19–20 s je znělé, a proto vynechané       |
| 26,5–31,5 | 3 tišší šepot               | o ~6 dB tišší, detektor 0 %                             |
| 36–41     | 4 šepot + psaní             | šepot, špičky −29 dBFS                                  |
| 45–49     | 6 tichý hlas                | detektor 66 %                                           |
| 53,5–57,5 | 6 nebo 7                    | detektor 70 %, úroveň stejná jako 45–49                 |
| 59,5–68,5 | 8 jen klávesy               | detektor 0 %, ostré špičky                              |
| 75–82     | 9 střídání                  | znělé záblesky 76, 78, 80 s, mezi nimi šepot            |

Bod 5 (šepot a mlácení do klávesnice) v nahrávce nejspíš chybí. Klávesy v úseku 59,5–68,5 s jsou
spíš tiché (špičky −25 dBFS). Hlasité psaní pokrývá jen starší `keys.wav`.

### Výsledky (`whisper_probe` na nové nahrávce)

RNNoise detektor na surovém vstupu, podíl rámců nad 0,5 (v závorce +24 dB jen do detektoru):

| Úsek            | > 0,5         |
| --------------- | ------------- |
| šepot 20–24     | 0,0 % (1,5 %) |
| tišší šepot     | 0,0 % (2,8 %) |
| šepot + psaní   | 0,0 % (2,0 %) |
| hlas 45–49      | 65,5 %        |
| jen klávesy     | 0,0 %         |

DFN3 `lsnr`: šepot má medián 3,5–5,8 dB, tichá místnost 10,9 dB. **N2b je zamítnuté podruhé**,
šepot má nižší `lsnr` než ticho.

Energetický klíč N2 na výstupu DFN3 s holdem 200 ms, tedy podíl času, kdy by byla gate otevřená:

| Úsek            | 12 dB ×5 | 9 dB ×5 | 15 dB ×5 | 12 dB ×10 |
| --------------- | -------- | ------- | -------- | --------- |
| ticho 1–18      | 14,7 %   | 30,7 %  | 3,6 %    | 1,2 %     |
| šepot 20–24     | 84,5 %   | 94,8 %  | 81,0 %   | 48,8 %    |
| tišší šepot     | 65,8 %   | 83,2 %  | 38,4 %   | 27,4 %    |
| šepot + psaní   | 59,2 %   | 75,4 %  | 36,8 %   | 29,6 %    |
| hlas 45–49      | 93,5 %   | 95,5 %  | 91,8 %   | 87,8 %    |
| jen klávesy     | 9,8 %    | 26,8 %  | 2,2 %    | 0,0 %     |
| střídání        | 72,3 %   | 76,4 %  | 48,9 %   | 40,7 %    |
| keys.wav        | 45,6 %   | 57,0 %  | 32,6 %   | 17,9 %    |

Dnešní gate: šepot 0 % (žádný z obou detektorů nepřekročí práh).

Závěr:
* **Na skutečném šepotu je přísná varianta 12 dB ×10 moc hluchá** (49 % místo 85 %). Doporučení
  z kapitoly 4 tím padá, protože vycházelo z odhadnutých úseků.
* **Rozumný kompromis je 12 dB ×5**, tedy přesně Fableův návrh. Normální šepot 85 %, tišší 66 %.
  Cenou je gate otevřená v 15 % ticha, v 10 % tichých kláves a ve 46 % hlasitého psaní. Zavřená
  gate u DFN3 jen dorovnává 1–6 dB šumu místnosti, takže otevírání v tichu stojí málo. Psaní stojí
  víc.
* Kritérium „aspoň 90 % rámců šepotu“ nesplní žádná varianta bez falešného otevírání. Procenta jsou
  ale ze všech rámců úseku, včetně nádechů a pauz mezi větami, takže skutečné pokrytí šepotu je
  vyšší.
* Možné vylepšení pro krok 3: krátký klíč (×5) jen tehdy, když výstup DFN3 nese energii hlavně nad
  ~2 kHz (šepot je šumový a vysoký, klávesy mají širší spektrum), nebo kratší hold pro rámce otevřené
  jen klíčem N2. Nic z toho zatím není změřené.

## 7. Druhé kolo podle posouzení (`whisper_sweep`)

Test `whisper_sweep` v `ai_denoiser.rs` obsahuje kroky 1b–1d, krok 2 a kapitolu 7 posouzení. Gate je
simulovaná po rámcích 10 ms (otevřeno = 0 dB, zavřeno = −12 dB), bez RNNoise detektorů.

**1c, rozptyl ticha po rámcích:** surový vstup p10 −74,0 / p50 −67,9 / p90 −61,4 / p99 −55,5 dBFS,
výstup DFN3 −76,2 / −69,8 / −62,5 / −58,0. Vstup kolísá stejně jako výstup (13 dB), takže dno ze
vstupu falešné otevírání neodstraní.

**1d, sweep klíče N2 (48 variant):** žádná varianta nesplní „ticho 0 dB“. I nejpřísnější
(dno ze vstupu, 12 dB, bez odpuštění, hold 0) pustí v tichu 3,8 dB šumu z 12 a přitom ubere
šepotu 2,7 dB, tiššímu 4,9 dB. Klíč se otevírá právě na nejhlasitějších výkyvech šumu, které nesou
většinu jeho energie.

**Krok 2 a hlavní nález: šepot ztrácí už samotný model.** Energie výstupu vůči vstupu bez gate, obojí
za dolní propustí 100 Hz (jako výchozí EQ):

| Model   | Šepot  | Tišší šepot | Šepot + psaní | Hlas   | Ticho (šum) |
| ------- | ------ | ----------- | ------------- | ------ | ----------- |
| DFN3    | −8,4   | −10,2       | −11,3         | −0,5   | −2,9        |
| DPDFNet | −11,1  | −32,9       | −32,8         | −0,2   | −42,3       |
| RNNoise | −4,5   | −1,0        | −0,7          | −0,0   | −11,7       |

Předpoklad „DFN3 šepot zachová, stačí opravit gate“ pocházel ze sekund 26–27 staré nahrávky, které
nejspíš šepot nebyly. Na skutečném šepotu ho DFN3 ztlumí o 8–11 dB, víc než šum místnosti (3 dB),
takže odstup šepotu od šumu dokonce zhorší. Pod `keys.wav` přijde DFN3 o 29–36 % hlasitějších rámců
šepotu (o víc než 15 dB pod čistým šepotem), ale už bez kláves je to 31 %.

Síla DFN3 na to nemá vliv: při 0,25 / 0,5 / 0,75 je ztráta šepotu −5,0 / −5,3 / −5,3 dB (bez
dolní propusti). U RNNoise ztráta se silou klesá, ale klesá i potlačení šumu:

| RNNoise, síla | Šepot / tišší / + psaní (dB) | Ubere ticha / `keys.wav` (dB) |
| ------------- | ---------------------------- | ----------------------------- |
| 0,25          | −1,7 / −1,2 / −0,9           | 2,5 / 2,5                     |
| 0,5           | −3,7 / −2,3 / −1,7           | 6,0 / 6,0                     |
| 0,75          | −5,7 / −3,2 / −2,3           | 12,0 / 12,0                   |

**Kapitola 7 posouzení:** šum místnosti zesílený o +20 dB před DFN3 (a zeslabený za ním) potlačí
DFN3 o 3,9 dB místo 2,1 dB. Pomůže to málo.

Závěr:
* Klíč gate šepot nezachrání: ten nejvíc ztrácejí samotné modely DFN3 a DPDFNet.
* Ze všech tří modelů šepot nejlépe zachovává RNNoise (−1 až −4,5 dB) a přitom ubere 12 dB šumu.
* Nejjednodušší funkční „režim šepot“: RNNoise s vypnutou gate. Šepot ztratí 1–4,5 dB, šum místnosti
  i klávesy zůstanou o 12 dB tišší. Klávesy ale ztiší jen o 12 dB, takže psaní bude slyšet víc
  než s DFN3.

## 8. Učící se normalizér hlasitosti (implementováno)

Místo dalšího ladění gate přibyla nová fáze řetězce `voice_leveler.rs`: denoiser → EQ →
**normalizér** → zesílení/limiter. Ve výchozím stavu je zapnutý, v UI je to karta „Fáze 4:
Vyrovnání hlasitosti hlasu“ s přepínačem a s aktuálním zesílením. Ve sdílené paměti je to verze 10
(`mic_leveler_enabled`, `mic_leveler_gain_db_x10`).

Jak funguje:
* Učí se jen z rámců, kde vstupní RNNoise detektor slyší řeč. Šum, klávesy ani šepot ho nic nenaučí.
  Příznak detektoru se zpozdí o latenci modelu a lookahead, aby seděl na zvuk, který právě prochází.
* Úroveň řeči je výkonový průměr rámců řeči. Prvních 30 s řeči je to běžný průměr (rychlost 1/n,
  tedy na začátku rychle a pak čím dál pomaleji), potom pevná rychlost 1/3000, takže si pamatuje
  zhruba posledních 30 s řeči.
* Po 3 s řeči se jednotlivý rámec počítá nejvýš 6 dB od odhadu, takže výkřik ani kašel úroveň
  nepřestaví.
* Zesílení = cíl −24 dBFS minus naučená úroveň, v rozsahu −6 až +24 dB, vyhlazené asi na 100 ms.
  Mezi slovy se nemění, takže šum v pauzách „nepumpuje“.

Měření (`leveler_report`, `whisper_scenario.wav`, výchozí gate, výstup v dBFS):

| Model   | Vstup  | Zesílení na konci | Hlas 45–49 | Hlas 53–57 | Střídání | Ticho  |
| ------- | ------ | ----------------- | ---------- | ---------- | -------- | ------ |
| RNNoise | 0 dB   | +7,5              | −21,7      | −24,1      | −23,0    | −88,7  |
| RNNoise | −12 dB | +19,6             | −23,3      | −24,0      | −23,0    | −100,6 |
| RNNoise | −24 dB | +24 (strop)       | −33,5      | −32,9      | −31,3    | −112,4 |
| DFN3    | 0 dB   | +8,3              | −22,5      | −24,4      | −23,4    | −78,5  |
| DFN3    | −12 dB | +24 (strop)       | −27,8      | −24,1      | −20,2    | −92,3  |

Hlas z mikrofonu ztlumeného o 12 dB vyjde stejně hlasitě jako z normálně nastaveného (±1,5 dB).
Šepot normalizér zesílí spolu s hlasem (RNNoise: −33,9 → −32,7 dBFS), sám se z něj ale neučí.

## 9. Co chybí k rozhodnutí

1. Potvrdit poslechem přiřazení úseků v kapitole 5 a jestli bod 5 opravdu chybí.
2. Otázky 1, 2 a 4 z analýzy (model a nastavení, kde šepot mizel, Discord, DPDFNet v UI) jsou na tobě.
3. `whisper_report` (krok 2) a sweep N2 se skutečnou gate (krok 3) zatím nejsou udělané.
