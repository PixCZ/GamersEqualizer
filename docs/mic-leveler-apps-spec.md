# Leveler a aplikace, které si samy řídí hlasitost mikrofonu: zadání

Zadání (2026-11-26). Navazuje na rozbor `docs/mic-app-volume-analysis.md`. Z rozboru platí zjištění
o WebRTC, Teams a Zoomu. Jeho krok 4 platí jen napůl: hlasitost za levelerem zmizí, ale fáze 5
zůstane pro digitální zesílení s vypnutým levelerem a pro limiter.

## V kostce

Mikrofon má se zapnutým levelerem dva ovladače. Každý dělá jednu věc a každý má jasného majitele:

| Ovladač                        | Co dělá                                        | Kdo s ním hýbe                                                     |
| ------------------------------ | ---------------------------------------------- | ------------------------------------------------------------------ |
| Vstupní citlivost (Windows)    | jak silný signál přijde z mikrofonu do řetězce | ty, automatika v krajních případech, případně cizí aplikace        |
| Cíl leveleru (-36 až -12 dBFS) | jak hlasitě tě slyší ostatní                   | jen ty: posuvník mikrofonu v mixeru a ve flyoutu, záložka Mikrofon |

Hlavní změna proti dnešku: **hlasitost za levelerem zmizí.** Byl to druhý ovladač na totéž: 80 %
za levelerem zní stejně jako cíl o 2 dB nižší. Posuvník mikrofonu v mixeru proto se zapnutým
levelerem nastavuje cíl, stejně jako už dnes flyout. Když hlasitostí mikrofonu ve Windows pohne
kdokoli jiný, změní se jen vstupní citlivost. Leveler ji vyrovná a ostatní tě slyší pořád stejně.

Druhá změna: **když se s námi nějaká aplikace přetahuje, leveler se do konce jejího nahrávání
zastaví.** Aplikaci si zapamatujeme a příště ho u ní zastavíme hned. Seznam takových aplikací
uvidíš v záložce Mikrofon a můžeš ho promazat.

S vypnutým levelerem zůstane všechno jako dnes.

## Jak signál projde

1. **Mikrofon a převodník.** Vstupní citlivost ve Windows mění zesílení přímo v zařízení, před
   převodníkem. Určuje poměr hlasu a šumu, jestli clippuje a jak „tvrdě“ zní. Proto ji nastavuješ
   ty a automatika ji opraví jen v krajních případech.
2. **Náš řetězec:** potlačení ozvěny, odšumovač a šumová brána. Práh brány je pevný v dBFS, takže
   při vysoké citlivosti se nad něj dostane i šum místnosti.
3. **Leveler.** Učí se tvoji úroveň hlasu bez vstupní citlivosti, takže změnu citlivosti hned
   vyrovná. Srovná hlas na cíl.
4. **Fáze 5:** limiter. S vypnutým levelerem před ním ještě digitální zesílení nad 100 %, jako dnes.
5. **Aplikace** (Discord, Teams, hra). Chrome a WebRTC mají vlastní hlasitost streamu, ta je až za
   námi a nikomu nevadí.

## Kdo pohnul hlasitostí ve Windows

Služba čte hlasitost mikrofonu každých 250 ms, jako dnes. Každou změnu zařadí do jedné ze tří
skupin.

**Naše.** Hodnotu jsme nastavili sami (automatika, posuvník citlivosti v okně). Víme, co jsme
nastavili, takže ji poznáme přesně.

**Uživatel.** Patří sem změna, když je v popředí okno, kterým se hlasitost mikrofonu dá ovládat:
- Nastavení Windows, ovládací panel Zvuk, herní panel Xbox, naše okno;
- aplikace, která právě nahrává, a v posledních 1,5 s přišel vstup z myši nebo klávesnice (posouváš
  posuvník v nastavení Teams).

Patří sem také každá změna, když žádná aplikace nenahrává.

**Aplikace.** Ostatní změny, když nějaká aplikace nahrává. V popředí je hra, prohlížeč nebo nahrávající
aplikace bez tvého vstupu. Nikdo jiný než program to být nemůže, protože bez okna v popředí hlasitost
mikrofonu ručně nezměníš.

Proč podle okna v popředí a ne podle psaní: při hraní píšeš a klikáš pořád. Kdyby rozhodoval jen
vstup, automatiku aplikace bychom při hraní nepoznali nikdy.

Změny, mezi kterými nejsou víc než 2 s, jsou jedna změna, ať trvají jakkoli dlouho: táhnutí
posuvníkem, opakované mačkání tlačítka, plynulý krok aplikace. Změnu ukončí až 2 s klidu. Dvě
změny milisekundu od sebe jsou tedy vždycky jedna.

Když hlasitost postupně zvedáš v Nastavení Windows, v ovládacím panelu Zvuk nebo v našem okně, je to
skupina „uživatel“. Do přetahování se to nepočítá vůbec, ani po několika krocích s pauzami.

## Přetahování a zastavení leveleru

**Jedna změna od aplikace není přetahování.** Zoom a Skype hlasitost na začátku hovoru jednou
nastaví. Bereme ji jako novou vstupní citlivost a leveler ji vyrovná. Nic dalšího se neděje.

**Druhá změna od aplikace do 1 minuty od té první, během jednoho nahrávání, je přetahování.** Proč druhá přijde:
regulace v aplikaci hlasitost zvýší, aby tě slyšela hlasitěji. Leveler to ale vyrovná a ona nic
nepozná, takže zvyšuje dál, až na 100 %. Tam to dnes stáhne naše automatika a celé se to opakuje.

Při přetahování služba **zastaví leveler**:
- leveler drží svůj současný zisk v dB, neučí se a změny vstupní citlivosti nevyrovnává;
- automatika vstupní citlivost nechá být;
- tvůj posuvník (cíl) funguje dál: posune držený zisk o stejný počet dB;
- limiter funguje dál.

Teď změna aplikace opravdu projde až k ní, takže její regulace se ustálí a přestane hlasitostí
hýbat. Hlasitost, jakou tě slyší ostatní, určuje během hovoru ona, ne náš cíl. Je to cena za klid.
Hovor zní jako s aplikací bez nás, jen s naším odšumem a ozvěnou.

**Konec.** Leveler se zase rozjede, až aplikace, která se přetahovala, přestane nahrávat. Když ji
neznáme, rozjede se, až přestanou nahrávat všechny aplikace, které tehdy nahrávaly. Vstupní
citlivost pak vrátíme na hodnotu před hovorem (Zoom ji jinak nechá, kde skončil). To neplatí, když
jsi ji během hovoru sám změnil (skupina „uživatel“): pak zůstane tvoje.

**Omyl je levný.** Zastavení jen podrží leveler do konce nahrávání a nic se neztratí. Proto stačí
dvě změny za minutu. Zápis do seznamu platí napříště, a proto chce víc (níž).

Zastavení trvá do konce nahrávání, nebo do restartu služby. Po restartu se zastaví znovu, jen když
je aplikace v seznamu.

## Seznam aplikací

- **Kdo to byl.** Když při přetahování nahrává jen jedna aplikace, je to ona.
- **Zápis až napodruhé.** První přetahování se u aplikace jen poznamená (počet a datum v souboru,
  v seznamu ještě není vidět). Když se ta samá aplikace přetahuje i v jiném nahrávání, třeba
  v dalším hovoru, zapíše se do seznamu s názvem (`Teams`, `Zoom.exe`) a datem. Náhodné dvě
  změny v jednom hovoru (kolečko na headsetu, bod 16) ji tedy do seznamu nedostanou.
- Když nahrává víc aplikací, nezapíše se žádná. Leveler se jen zastaví do konce nahrávání.
- **Příště.** Jakmile aplikace ze seznamu začne nahrávat, leveler se zastaví hned na začátku, na
  zisku naučeném pro tvůj hlas. Zníš tedy stejně hlasitě jako normálně, jen se s ní nic nepřetahuje.
  Platí to i tehdy, když spolu s ní nahrává jiná aplikace.
- **V aplikaci.** Záložka Mikrofon, pod levelerem: „Aplikace, které si řídí hlasitost mikrofonu
  samy“. U každé je název, datum a tlačítko Odebrat. U Teams, Zoomu a Skypu přidáme tip, kterou
  volbu v nich vypnout (názvy z rozboru). Pak ji můžeš ze seznamu odebrat a leveler bude u ní
  fungovat zase celý.
- **Uložení:** `audio_settings.json`, seznam s výchozí hodnotou prázdný. Starší verze aplikace ho
  přeskočí a při uložení zachová.

## Co uvidíš v okně

- **Záložka Mikrofon, leveler zapnutý:**
  - cíl leveleru jako dnes;
  - řádek „Citlivost“ bude opravdu citlivost: hlasitost ve Windows 0–100 %, ukazuje i změny
    z Windows a od automatiky;
  - pod ním věta, proč na ní záleží: šum, clipping, tvrdý zvuk při 100 %.
- **Stavový řádek při zastavení:** „Leveler stojí: Teams si sám řídí hlasitost mikrofonu. Rozjede
  se po skončení hovoru.“
- **Mixer:** posuvník mikrofonu se zapnutým levelerem nastavuje cíl leveleru, stejně jako flyout.
  Windows se nedotkne.
- **Flyout:** beze změny, řádek mikrofonu nastavuje cíl leveleru.
- **Vypnutí leveleru:** posuvník mikrofonu zase ovládá Windows (0–100 %) a nad 100 % digitální
  zesílení, jako dnes. Vstupní citlivost každého mikrofonu si při tom zapamatujeme a po zapnutí ji
  vrátíme.

## Všechny situace

### Běžné používání

1. **Nic nepere** (Discord s vypnutou AGC, Meet v Chrome, hra, OBS): leveler srovnává na cíl.
   Automatika citlivost opraví jen v krajních případech: při clippingu, nebo když je hlas 20 s mluvení
   slabý nebo moc silný.
2. **Změníš citlivost v Nastavení Windows nebo v našem okně.** Je to nová vstupní citlivost. Leveler
   ji vyrovná, takže pro ostatní zníš stejně, jen se změní šum a čistota. Automatika 30 s počká.
3. **Pohneš posuvníkem mikrofonu v mixeru nebo ve flyoutu:** změní se cíl leveleru, Windows ne.
4. **Chceš být tišší než -36 dBFS:** nejde. Je to dolní hranice cíle a pro úplné ticho je ztlumení.
5. **Nastavíš vysokou citlivost a šumí to:** automatika sama nezasáhne, protože hlas není „moc
   silný“. Je to tvoje volba a věta u posuvníku řekne, že ji máš snížit. Když to clippuje, automatika
   citlivost stáhne hned v nejbližší pauze, o 15 %.
6. **Citlivost 100 % a tvrdý zvuk bez clippingu:** dnes to nepoznáme. Nápad do budoucna: počítat,
   jak často pracuje limiter.

### Aplikace s vlastní automatikou

7. **Aplikace hlasitost jednou nastaví** (Zoom na začátku hovoru): nová citlivost, leveler ji
   vyrovná, nic se nezastaví.
8. **Aplikace hlasitost řídí průběžně** (Teams s „Automatically adjust mic sensitivity“):
   - druhou změnou se leveler do 250 ms zastaví;
   - když nahrává jen Teams, poznamená se; při druhém takovém hovoru se zapíše do seznamu;
   - po hovoru se leveler rozjede a citlivost se vrátí na hodnotu před hovorem.
9. **Totéž, ale nahrává víc aplikací** (Teams + Discord): leveler se zastaví do konce nahrávání
   všech a nikdo se nezapíše. Log jmenuje všechny, které nahrávaly.
10. **Teams po zápisu do seznamu:** leveler stojí od začátku hovoru, zníš jako obvykle a nic se nepřetahuje.
11. **Aplikace ze seznamu a k tomu další** (Teams + OBS): leveler stojí kvůli Teams. Leveler je jen
    jeden, pro všechny aplikace společný.
12. **Aplikace vrátí krok naší automatiky:** je to změna od aplikace, takže platí body 7 a 8. Dnešní
    pojistka (dvě vrácení za sebou automatiku vypnou) zůstává jako druhá záchrana.
13. **Upravíš hlasitost v nastavení Teams během hovoru:** okno Teams je v popředí a hýbeš myší,
    takže je to tvoje změna. Nic se nezastaví ani nezapíše.
14. **Teams nebo Zoom s vypnutou automatikou:** hlasitostí nehýbou, takže leveler funguje celý.
    Když jsou v seznamu, odeber je.

### Okrajové případy

15. **Hlasitost mění software ke sluchátkům** (G Hub, Synapse, GG, NVIDIA Broadcast):
    - jeho okno v popředí a ty v něm klikáš → tvoje změna;
    - mění ji na pozadí, když něco nahrává → změna od aplikace;
    - sám nenahrává, takže se do seznamu nezapíše, místo něj by se zapsala ta nahrávající. Viz limity.
16. **Kolečko nebo tlačítka hlasitosti mikrofonu na USB headsetu**, která ovládají hlasitost ve
    Windows: během hovoru vypadají jako aplikace. Dvě otočení s pauzou přes 2 s do minuty leveler
    zastaví. Tady je to vlastně správně: jinak by leveler tvoje otočení vyrovnal a nic bys
    neslyšel. Nahrávající aplikace se tím jen poznamená. Do seznamu by se dostala, až kdyby se to
    stalo i v dalším hovoru, a pak ji odebereš. Viz limity.
17. **Mikrofon bez hardwarové hlasitosti** (Windows ji jen dopočítá): automatika citlivosti je
    vypnutá jako dnes. Poznání přetahování funguje stejně.
18. **Leveler vypnutý, bypass nebo ztlumený mikrofon:** nic z tohoto se neděje. Změna ve Windows je
    změna posuvníku, jako dnes.
19. **Přepneš mikrofon:** zastavení a automatika začnou znovu, seznam aplikací platí dál (je podle
    aplikace, ne podle mikrofonu).
20. **Restart služby nebo počítače během hovoru:** zastavení se zapomene. Aplikace ze seznamu se
    zastaví hned znovu. Jiná aplikace se zastaví po další druhé změně.
21. **Aktualizace aplikace se starým efektem v audiodg:** změna rozvržení sdílené paměti. Starý
    efekt novou paměť nenajde a zvuk pustí beze změny. Banner nabídne aktualizaci efektů, jako při
    každé takové změně.

## Log

Jen při události, žádné periodické výpisy:

- `[MicVol] Windows mic volume 62 % -> 80 %: app (foreground Game.exe, recording Teams)`, a obdobně
  `user (foreground SystemSettings.exe)` nebo `ours`;
- `[MicVol] Teams moved the mic volume again: leveler held until it stops recording (first time for Teams: noted)` nebo `(second time for Teams: added to the held list)`;
- `[MicVol] Teams stopped recording: leveler runs again, Windows mic volume back to 62 %`;
- `[MicVol] Teams is on the held list: leveler held from the start`.

## Implementace

- **`win_audio/mic_hold.rs`** (nový): čistá funkce, která změnu zařadí jako naše, uživatel nebo
  aplikace. Vstup: okno v popředí, čas posledního vstupu, nahrávající aplikace. Dále stav
  přetahování (sloučení do 2 s, druhá změna do 1 min, konec, hodnota před hovorem) a zápis do
  seznamu po dvou nahráváních.
- **`win_audio/chatmix/mod.rs`:** se zapnutým levelerem cizí změna nemění `mic.volume`, jen vstupní
  citlivost. `mic.volume` platí jen pro vypnutý leveler. `mic_hw_auto`, `mic_auto_wish_permille`
  a `adopt_mic_auto` zmizí: Windows je teď vstup, žádné přání vedle něj.
- **`chain.rs`, fáze 5:** se zapnutým levelerem žádná hlasitost, jen limiter.
- **Mixer (`ui/tab_mixer.rs`):** posuvník mikrofonu se zapnutým levelerem = cíl, přes
  `loudness_slider` jako flyout.
- **Starší nastavení:** kdo měl mikrofon v mixeru pod 100 %, bude po aktualizaci hlasitější o tolik,
  kolik to dělalo. Nepřevádíme to, jen věta v poznámkách k vydání („uber cíl“).
- **`win_audio/mic_gain.rs`:** se zastaveným levelerem nic nedělá.
- **Okno v popředí:** služba ho už sleduje kvůli hrám (`game_detector.rs`). Čas posledního vstupu
  dá `GetLastInputInfo`. Nahrávající aplikace dá `mic_usage::recording_apps()`.
- **Sdílená paměť:** nové pole `mic_leveler_hold` a zvýšení `shm_version!`. Po aktualizaci je proto
  potřeba přeinstalovat efekty (člověk).
- **`voice_leveler.rs` a `chain.rs`:** režim „drž“: zisk se nemění, neučí se a vstupní citlivost se
  nevyrovnává.
- **`ui/audio_settings.rs`:** seznam aplikací a zapamatovaná citlivost pro vypnutý leveler.
- **`ui/tab_mic.rs`:** posuvník citlivosti, seznam s tlačítkem Odebrat, stavový řádek. Texty přes
  `tr()` a do `i18n_cs.rs`.
- **`DEVLOG.md`.**

## Testy a zkouška

- **Unit testy:**
  - zařazení změny: popředí, vstup a nahrávající aplikace → naše, uživatel nebo aplikace;
  - jedna změna nezastaví, druhá do minuty ano, druhá po minutě ne;
  - změny s mezerami pod 2 s jsou jedna, i když trvají déle;
  - konec po skončení nahrávání;
  - vrácení citlivosti, nebo nevrácení po tvé změně;
  - zápis jen při jedné nahrávající aplikaci a až při druhém nahrávání, zastavení od začátku pro
    aplikaci ze seznamu;
  - režim „drž“ v leveleru (zisk se nehne při změně vstupu);
  - seznam v JSON: načtení starého souboru bez seznamu a zachování neznámých polí.
- **Zkouška bez druhého člověka.** Stačí jedna aplikace s automatikou a její zkušební hovor:
  - Teams „Udělat zkušební hovor“;
  - Zoom testovací schůzka (zoom.us/test);
  - Discord „Test mikrofonu“.

  V logu se pak ukáže, jestli přišla druhá změna a leveler se zastavil.

## Limity

- Windows neřekne, kdo hlasitost změnil. Při víc nahrávajících aplikacích proto víme jen „nějaká“,
  a nikoho nezapíšeme.
- Software, který hlasitost mění, ale sám nenahrává (bod 15), a tlačítka na headsetu (bod 16) mohou
  zapsat nevinnou nahrávající aplikaci, když se to stane ve dvou hovorech. Proto je seznam vidět
  a dá se promazat.
- Aplikace, která hlasitost mění pomaleji než jednou za minutu, se za přetahování nepočítá. Leveler
  každou její změnu vyrovná a ona se může dostat až na 100 %. Tam ji stáhne naše automatika, a když
  to aplikace dvakrát vrátí, automatika se zastaví (dnešní pojistka).
- Během zastavení tě ostatní slyší podle regulace té aplikace, ne podle našeho cíle.
- Chrome a webové aplikace hlasitost ve Windows nemění, takže je tohle vůbec nezahrne. Ani nemusí,
  s levelerem se nepřou.

## K potvrzení

1. **Aplikace ze seznamu:** leveler zastavit na naučeném zisku (navrhuji: zníš jako normálně), nebo
   vypnout úplně (zisk 0 dB, tedy hlasitost jen podle citlivosti)?
2. **Řádek „Citlivost“ v záložce Mikrofon** se zapnutým levelerem ovládá hlasitost ve Windows
   (navrhuji), nebo ji nechat jen ve Windows?
