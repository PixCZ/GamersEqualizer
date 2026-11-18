# Automatický ChatMix: ověření na Windows

Pro agenta na Windows stroji a pro uživatele (kroky označené **Ručně** dělá člověk). Větev
`feat/chat-ducking`, poslední commit. Příkazy jsou pro PowerShell z kořene repa. Co funkce dělá a jak je
postavená: `DEVLOG.md`, milník 65. **Tento soubor je dočasný: po ověření ho smaž** a výsledky zapiš do
milníku 65 (oddíl „Live test").

## 0. Výchozí stav

- Na Macu prošlo: formát, clippy pro Windows cíl (i s `debug-capture`), testy `gamersequalizer-common`
  (138), simulátor `duck_sim`, testy `silero_vad`.
- **Na Macu neběžely** testy služby, APO a okna (jsou jen typově zkontrolované) a psaly se naslepo.
- Naživo je ověřeno jen jedno (2026-11-18, commit 8b5917b): s vypnutým prostorovým zvukem běžný režim
  hru tlumí; se zapnutým Dolby Atmos nedělal nic. Od té doby přibyl „režim mixu" pro hry s Atmosem;
  ten nikdo neslyšel.
- **APO z 8b5917b zůstává nainstalované.** Rozložení sdílené paměti (verze 24) se nezměnilo, stačí
  sestavit aplikaci. Nové APO neinstaluj, pokud to kapitola 1 nevyžádá.
- Uživatel hraje vždy s Dolby Atmos a nechce ho vypínat. Žádný krok níže to nevyžaduje.

## 1. Brána (agent)

1. Ověř, že se rozložení paměti proti 8b5917b opravdu nezměnilo:
   `git diff 8b5917b -- crates/gamersequalizer-common/src/shm crates/gamersequalizer-common/src/streams.rs`
   smí ukázat jen komentáře. Jinak **stop** a nahlas to (bylo by potřeba nové APO).
2. Sestav a pusť bránu přes subagenta `dev-rebuild`: `scripts/dev-rebuild.ps1 -Test -Start` (bez `-Apo`).
3. Poprvé tu poběží testy v `crates/gamersequalizer-daemon/src/win_audio/ducking/`,
   `win_audio/chatmix/` (hlavně `game_duck/tests.rs`), `win_audio/stream_router.rs`,
   `ui/audio_settings.rs`, test českých textů (`i18n.rs`) a testy APO.
4. Když test spadne: je to nejspíš chyba z psaní naslepo. Najdi příčinu a oprav ji tam, kde je (v kódu,
   nebo v testu, pokud je špatně jeho očekávání). Testy neoslabuj a nemaž. Ke každé opravě napiš jednu
   větu, co bylo špatně.
5. Výsledek: seznam spadlých testů s hláškou, co jsi opravil, a konečný stav brány.

## 2. Příprava (ručně)

- Stránka Log: zaškrtnout „Save debug to disk". Log je `%APPDATA%\GamersEqualizer\logs\gamersequalizer.log`.
- Smazat `%APPDATA%\GamersEqualizer\duck_level_cache.json` (zůstaly v něm úrovně z testu s podcastem).
- Discord: vypnout Attenuation. Windows: Zvuk > Komunikace > „Nedělat nic". Jinak se zeslabení sčítají.
- Zdroj chatu: hovor na Discordu, nebo podcast v prohlížeči přetaženém v mixeru na kanál Chat.
- Dolby Atmos zapnutý, Automatický ChatMix zapnutý, Síla 50 %.

## 3. Test A: hra s Atmosem (režim mixu)

| Krok                                                    | Co má být vidět a slyšet                                                         |
| ------------------------------------------------------- | -------------------------------------------------------------------------------- |
| A1. Hra hraje aspoň 20 s, v chatu ticho                 | V logu `[Ducking] <hra> cannot be heard: spatial sound (...) is on ...` a stavový řádek s `mode mix (spatial sound: Dolby Atmos for Headphones)` |
| A2. Přestřelka a do ní řeč v chatu                      | Hra se ztiší celá, plynule; v okně „Hra zeslabena o X %" a informační text o prostorovém zvuku |
| A3. Řeč skončí                                          | Hra se do zhruba 1,5 s plynule vrátí                                             |
| A4. Řeč nad tichou hrou                                 | Nic se netlumí; v minutovém řádku `none lowered`                                 |
| A5. Několik minut střídání klidu, přestřelek a řeči     | Hloubka zeslabení je po prvních promluvách pořád stejná                          |

Řádky, které agent v logu hledá:

- `[Ducking] on, strength 50 %; mode mix (...); chat <exe>, detector yes; game <exe>; master switch on`
- `[Ducking] last minute (mix): N spurts, M lowered for S s, depth ...`
- debug: `[Ducking] 10 s <hra> (mix): ..., mix level ..., chat band ..., chat sounding N %`

Když stavový řádek říká `mode none: ...`, je v něm i důvod (hra a chat mají vlastní výstupy, nebo k
službě nedoteče výstupní mix, tedy chybí výstupní EQ na tomto výstupu). Když říká `mode stream`, efekt
hru slyší na jejím streamu a platí test C.

**Poslech (ručně), odpovědět slovy:** je náběh zeslabení plynulý, nebo jsou slyšet schůdky? Je hloubka
tak akorát, moc, málo? Je řeči rozumět?

## 4. Test B: hra musí dostat svou hlasitost zpět (ručně, režim mixu)

Sleduj posuvník hry ve směšovači hlasitosti Windows. Posuvník Game v naší aplikaci nech na 100 %,
není-li řečeno jinak.

| Krok                                                                 | Očekávání                                                              |
| -------------------------------------------------------------------- | ---------------------------------------------------------------------- |
| B1. Hru ve směšovači Windows nastav na 60 %, pak jedna promluva      | Během řeči klesne, po ní je přesně 60 %                                |
| B2. Dvacet promluv za sebou                                          | Pořád přesně 60 %, neklesá postupně                                    |
| B3. Během zeslabení pohni naším posuvníkem Game na 50 % a zpět       | Hra sleduje posuvník; po řeči je tam, kde by byla bez zeslabení        |
| B4. Během zeslabení vypni Automatický ChatMix                        | Hra se plynule vrátí na 60 %                                           |
| B5. Během zeslabení ukonči aplikaci i službu (`taskkill /F /IM gamersequalizer.exe`), hra běží dál | Hra zůstane ztlumená; po spuštění aplikace se vrátí na 60 % a v logu je `volume set back to 60 % after an earlier run left it lowered` |
| B6. Během zeslabení ukonči hru, pak ji znovu spusť                   | Po startu hry je na 60 % (stejný řádek v logu)                         |
| B7. Soubor `%APPDATA%\GamersEqualizer\ducking_lowered.json`          | Existuje, dokud režim mixu pro hru běží; po ukončení hry nebo vypnutí funkce zmizí; nezapisuje se při každé promluvě (čas změny souboru) |

Každý krok, kde hra neskončí na původní hlasitosti, je **nejdůležitější nález**: zapiš krok, hodnotu před
a po, a přilož řádky `[Ducking]` z té doby.

## 5. Test C: běžný režim (jen když je po ruce hra bez nativního Atmosu)

Atmos zůstává zapnutý. Hra, která posílá obyčejný stereo nebo 7.1 zvuk, jde přes svůj stream: stavový
řádek říká `mode stream`, tlumí se jen pásmo řeči (basy zůstávají). V debug řádku `start` má být
`gains game 100 % chat 100 %` bez ohledu na polohu posuvníků (oprava dvojího započtení hlasitosti).
S APO z 8b5917b se zeslabení během nekonečné promluvy neuvolní (uvolnění po 3 s klidu je v tomto režimu
až v novém APO); to není chyba tohoto testu.

## 6. Okno (ručně, snímky obrazovky)

- Úvodní stránka, karta ChatMix: přepínač „Automatický ChatMix", text při zeslabení, informační text v
  režimu mixu. Nic nesmí poskočit, když se text objeví a zmizí.
- Stránka „Výstup a sluchátka": karta Automatický ChatMix, posuvník Síla, tlačítko výchozí hodnoty,
  zašednutí při vypnuté funkci.
- Sedí rozestupy, ikona a české texty se sousedními kartami? Co nesedí, vyfoť.

## 7. Dvě otázky z průzkumu (ručně, 5 minut)

1. OBS: přidej zdroj „Application Audio Capture", vyber hru, Atmos zapnutý, zastřílej si. Hýbe se
   ukazatel hlasitosti naplno, jen při menu, nebo vůbec? (Rozhodne, jestli jde hlasitost hry brát
   přímo z Windows místo z výstupního mixu.)
2. Má hra v nastavení zvuku volbu výstupu nebo přepínač Dolby Atmos? Jen zapiš, nic nepřepínej.

## 8. Ladění bez nového APO

`%APPDATA%\GamersEqualizer\duck_tuning.json`, plochý objekt jméno: číslo; služba ho načte do 2 s a zapíše
řádek `[Ducking] tuning from duck_tuning.json: ...`. Pro režim mixu jsou podstatné:

| Jméno                  | Výchozí | K čemu                                                   |
| ---------------------- | ------- | -------------------------------------------------------- |
| `mix_max_depth_db`     | 12      | strop zeslabení v režimu mixu (celé pásmo)               |
| `mix_chat_guard_s`     | 0.3     | jak dlouho po zvuku v chatu se hra ještě neměří          |
| `fast_attack_db_per_s` | 40      | rychlost náběhu; snížit, když jsou slyšet schůdky        |
| `release_db_per_s`     | 12      | rychlost návratu                                         |
| `latch_release_s`      | 3       | po jak dlouhém klidu hry se zeslabení uvolní i během řeči |
| `hold_s`               | 0.5     | přidržení mezi slovy                                     |

## 9. Co poslat zpět

- Výsledek brány (kapitola 1) a případné opravy.
- Tabulky A a B s výsledkem u každého kroku, odpovědi na poslech, snímky okna, odpovědi z kapitoly 7.
- Z logu všechny řádky `[Ducking]`, `[Spatial]` a `[ApoNote]` z doby testu.
- Obsah `duck_level_cache.json` a `ducking_lowered.json` po testu A.
- Pokud se měnilo ladění: konečný `duck_tuning.json` a proč.

## 10. Známá omezení (nehlásit jako chybu)

- Režim mixu tlumí celé pásmo hry, ne jen pásmo řeči, a počítá jako hru vše, co není chat (hudbu,
  systémové zvuky).
- Během řeči platí hlasitost hry z doby před ní; změna se zachytí v nejbližší pauze. Řeč bez jediné
  pauzy (podcast) měření zmrazí.
- První spuštění hry se učí: hloubka roste, dokud nejtišší mluvčí neřekne zhruba deset delších vět.
- Hlasitost hry změněná ve směšovači Windows přímo během zeslabení se přepíše a po řeči se vrátí
  hodnota z doby před ním.
- Hlasový chat vestavěný ve hře oddělit nejde; chat stream mimo 48 kHz se řídí jen hlasitostí.
