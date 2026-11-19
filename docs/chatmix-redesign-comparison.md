# Automatic ChatMix: návrh od nuly (10 konstant) proti současnému (40)

Pracovní dokument z 2026-11-19, větev `feat/chat-ducking`. Až bude ducking na `main`, smazat a podstatné
převést do DEVLOG.md.

## 1. Zadání (zpětně sepsané)

Zadání dostal čerstvý subagent, který neviděl současný kód, DEVLOG ani handoff. Obsahovalo:

**Co chce uživatel**

1. Hlas v chatu je v přestřelce srozumitelný.
2. Nic se neděje, když nikdo nemluví, když je mluvčí zřetelně nad hrou, v klidné hře (lobby) ani při
   jednom náhodném výstřelu.
3. Ztlumení je v každé přestřelce zhruba stejné a necuká s dávkami.
4. Žádné skoky: celá hra se vrací rychlostí kolem 4 dB/s (12 dB/s znělo jako skok), nádech hru nevrátí.
5. Jedno krátké slovo („jo“) hru znatelně neztlumí.
6. Hlasitý i tichý mluvčí v jednom hovoru, tichý musí být srozumitelný.
7. Mluvčí bez pauz (podcast: zvuk 67–87 % času).
8. Dlouhé hraní (20 min lobby, přestřelka, 20 min lobby…), nic se nesčítá.
9. Změna posuvníků Game a Chat, ChatMixu nebo hlavní hlasitosti se projeví hned nebo do pár sekund.
10. Jediný ovladač pro uživatele: Síla (0–100 %).
11. Kašel, klávesnice a šum v chatu hru neztlumí.

**Prostředí**

- stream APO na každém streamu;
- hlasitost relace se aplikuje před APO;
- Silero na chatu;
- mix mód při Atmosu (hra je vidět jen ve výstupním mixu, kde je s chatem s neznámým ziskem a
  zpožděním; ztlumení jde přes hlasitost relace);
- čtyři pozorované pasti (sledovat okamžitou hladinu, „mix − chat“, měřit jen v tichu chatu, práh −60 dBFS).

**Scénáře:** 9 akceptačních scénářů (viz kap. 3) a rozpočet nejvýš 10 konstant.

## 2. Návrh od nuly

Hloubka se počítá z odhadu poměru chat : hra.

- **Cíl:** poměr chat : hra v řečovém pásmu = (Síla − 0,5) × 12 dB. 50 % tedy znamená chat stejně
  hlasitý jako hra.
- **Hladina hry:** průměr v dB s náběhem 1 s a poklesem 6 s.
- **V mix módu** (při řeči) se hladina hry bere jako minimum mixu za poslední 4 s. Chat v mixu =
  mix − očekávaná hra po ztlumení.
- **Poměr:** sleduje se rychle dolů (0,5 s, chrání tichého mluvčího) a pomalu nahoru (4 s).
- **Brána:** aspoň 1 s řeči (s útlumem 4 s), drží 1,5 s po poslední řeči.
- **Pohyb:** hloubka se mění 4 dB/s oběma směry, strop 15 dB.

| Konstanta    | Hodnota | Účel                                         |
| ------------ | ------- | -------------------------------------------- |
| `SPAN_DB`    | 12 dB   | rozsah cíle přes Sílu (−6 až +6 dB)          |
| `D_MAX`      | 15 dB   | strop hloubky                                |
| `RATE`       | 4 dB/s  | rychlost náběhu i návratu                    |
| `GATE_S`     | 1 s     | kolik řeči otevře bránu                      |
| `GATE_TAU`   | 4 s     | útlum čítače řeči                            |
| `HANG`       | 1,5 s   | jak dlouho brána drží po řeči                |
| `G_UP`       | 1 s     | náběh hladiny hry                            |
| `G_DOWN`     | 6 s     | pokles hladiny hry                           |
| `TRACK_DOWN` | 0,5 s   | poměr chat : hra dolů (tichý mluvčí)         |
| `TRACK_UP`   | 4 s     | poměr nahoru; okno minima v mix módu         |

Sám subagent předem označil jako slabé scénáře 2 (bez pauz), 7 (posuvník) a částečně 3 a 8.

## 3. Jak se porovnávalo

- **Simulátor:** scratch kopie `duck_sim` (mix mód, tick 50 ms, Silero na chatu). Návrh od nuly jsem
  přepsal z pseudokódu do Rustu a zapojil místo `MixDucker`. Současné řešení je pracovní strom
  s opravou učení z 2026-11-18.
- **Data:** syntetická hra (růžový šum −55 dBFS, přestřelka s výstřely kolem −22 dBFS) a skutečná řeč
  (`test_data/denoise/speech_clean.wav`, kolem −31 dBFS).
- **Síla:** obě řešení na 50 %, nový navíc na 100 %. Jeho 50 % je „chat stejně hlasitý jako hra“,
  současných 50 % je „hlas 8,5 dB nad hrou“; jeho 100 % (+6 dB) je tomu blíž.
- **Měřilo se:** hloubka po segmentech, podíl času se ztlumením (i bez prvních 30 s segmentu, které
  nesou dozvuk předchozího), počet „cuknutí“ (pokles o víc než 3 dB a znovu nahoru) a poměr hlas : hra
  v řečovém pásmu během řeči (čím víc, tím srozumitelněji).

## 4. Výsledky

| Scénář                         | Současné (50 %)                                              | Od nuly 50 %                                              | Od nuly 100 %                          |
| ------------------------------ | ------------------------------------------------------------ | --------------------------------------------------------- | -------------------------------------- |
| 1 Lobby, jeden mluvčí          | lobby 0 %, přestřelky 18 dB bez cukání, nástup 4–11 s        | lobby 7 % (8–12 cuknutí), přestřelky 11–12 dB, cuká       | lobby **76 % ztlumeno**, 178 cuknutí   |
| 2 Lobby, hovor bez pauz        | stejně dobré jako 1                                          | přestřelky jen 5–6 dB, nástup 15–21 s                     | lobby **50 % ztlumeno**                |
| 3 Dva mluvčí (12 dB od sebe)   | 18 dB stále; hlas : hra hlasitý 12–16, tichý 4–9 dB          | první dvojice nesrozumitelná (−1 a −4,5 dB), pak 10 / 4   | 5 / 1,5, pak 13 / 6 dB                 |
| 4 Krátká slova v přestřelce    | bez propadu (slova zaniknou, −4,4 dB)                        | stejně                                                    | stejně                                 |
| 5 Jeden výstřel v klidu        | **18 dB na asi 25 s**                                        | do 6 dB, 60 % zbytku                                      | do 11 dB, 100 %                        |
| 6 Mluvčí nad střední hrou      | 0 % (po dozvuku)                                             | **100 % ztlumeno, do 12 dB**                              | 100 %, do 15 dB                        |
| 7 Hra o 10 dB níž posuvníkem   | přestřelka 2 přetlumená: hlas : hra 17,9 proti 6,1 dB        | 11,3 proti 7,0 dB, blíž                                   | 13,9 proti 7,2 dB                      |
| 8 Přestřelka s mezerami 3–6 s  | 18 dB stále, bez cukání                                      | **obráceně: v mezerách hlouběji než v dávkách**, hlas −5 až +2 dB | totéž                          |
| 9 Konec řeči                   | návrat do 7,4 s                                              | 4,6 s                                                     | 5,3 s                                  |

**Proč návrh od nuly selhává (strukturálně, ne laděním):**

- **Scénáře 1, 2, 6** (tlumí klidnou hru i mluvčího nad hrou): v mix módu během řeči odhaduje hru
  z mixu, ve kterém je i chat. Minimum za 4 s plynulé řeči je pořád chat, takže „hra“ vyjde hlasitá
  a „chat = mix − hra“ skoro nula. Je to past b ze zadání v jiném převleku.
- **Scénář 8** (v mezerách hlouběji než v dávkách): v mezeře hladina hry klesá pomalu (6 s), takže
  odhad chatu (mix − nadhodnocená hra) spadne na podlahu a poměr vyjde −30 dB.

Návrh dostal nula iterací a ladění by ho zlepšilo. Tyhle dvě chyby ale plynou z odečítání chatu
z mixu, ne z hodnot. Současné řešení měří hru jen v tichu chatu právě proto.

## 5. Co z toho plyne

**Provedeno 2026-11-19** (necommitnuto). Stejné scénáře, síla 50 %, před → po:

- **Jeden výstřel:** 18 dB na 25 s → 0 dB.
- **Hra o 10 dB níž:** další přestřelka 16 dB → 8 dB; dozvuk po přestřelce 56 % → 0 % (po 30 s).
- **Lobby beze změny.** Přestřelky v lobby a mezery v přestřelce dál bez cukání, hloubka 15–17 dB
  místo stropu 18 dB (během ztlumení se hlasitá hladina učí jen do úrovně změřené před ním).
- **Cena:** v hovoru skoro bez pauz se přestřelka pozná později (17 s místo 10 s) a první
  dvojice mluvčích ve scénáři 3 je ztlumená později.

1. **Méně parametrů samo o sobě nepomůže.** Učení a zámek zajišťují scénáře 1, 3, 6 a 8; deset
   konstant bez učení je v nich výrazně horší. Doporučení zůstává: současný návrh + zjednodušení
   ze skupiny 3 (pryč 6 parametrů: zpomalování učení a zahřívání).
2. **Porovnání odhalilo tři slabiny současného řešení:**
   - **Jeden výstřel v klidné hře při řeči = 18 dB na asi 25 s (scénář 5).** Změřená hladina hry
     z pauzy s výstřelem se drží a klesá pomalu (`mix_game_fall_s` 6 s). Návrh od nuly zde pomáhá
     pomalejším náběhem hladiny hry (1 s). V mix módu by šlo dávat měření z krátké pauzy menší váhu,
     nebo vyžadovat, aby hru „hlasitou“ ukázaly aspoň dvě pauzy.
   - **Ztišení hry posuvníkem přetlumí další přestřelku (scénář 7):** hlas : hra 18 dB místo 6 dB.
     Řešení z dřívějška: posunout naučenou hlasitou hladinu o změnu hlasitosti relace, kterou služba zná.
   - **Dozvuk po přestřelce:** při plynulé řeči zůstává hra ztlumená ještě desítky sekund po konci
     přestřelky (scénář 7 „calm“: 56 % i po 30 s). Je to cena za to, že mezery v přestřelce necukají
     (scénář 8). Pro posouzení by stálo za to poslechnout `mix_after.wav`.
3. **Krátká slova** zaniknou v obou řešeních (−4,4 dB). Zadání si tu protiřečí („slovo nemá hru
   ztlumit“ proti „hlas srozumitelný“) a obě řešení volí klid hry.

## 6. Omezení

- Syntetická hra a jeden klip řeči. Stream mód se nesimuloval (v obou návrzích je jednodušší).
- Návrh od nuly jsem přepsal z pseudokódu já, takže drobná odchylka od záměru autora je možná.
  Strukturální chyby z kap. 4 ale plynou přímo z jeho vzorců.
- Nic z toho není slyšené; jsou to čísla ze simulace.

Skripty (scénáře, měření, běh) byly v `.claude/tmp/chatmix-redesign/`, simulátor v
`.claude/tmp/duck-lobby/simcrate/`; obojí je dočasné.
