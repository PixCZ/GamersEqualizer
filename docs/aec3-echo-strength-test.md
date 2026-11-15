# Síla echa: test na nahrávkách

Pro agenta (Antigravity) na Windows stroji s `test_data/private/captures/`. Větev `aec3-mic-gain`,
poslední commit. Příkazy jsou pro PowerShell z kořene repa.

**Třetí kolo: udělej jen kapitoly 0 a 6** a výsledky zapiš jako milník 50. Kapitoly 1-5 jsou
hotové (milník 48: test A shodný s milníkem 44, test B bez falešné detekce, test C prošel).

Historie: první kolo (milník 46) neprošlo, měřič bral hlas za echo. Měřič je přepsaný (milník
47): sílu echa uzná jen tehdy, když úroveň mikrofonu sleduje úroveň reference okno po okně
(„potvrzeno"). Jinak hlásí jen horní mez a AEC3 nechá na -40 dB.

Co test **je**: ověření měřiče síly echa a toho, co z něj plyne, na existujících nahrávkách.
Co **není**: živý test (instalace APO, log od testera). Ten přijde potom a dělá ho člověk.

## Pravidla

- Neměň konstanty měřiče (`ECHO_*`, `REF_*`, `FALLBACK_*`, `MIC_GAIN_*`) ani `tuning()`.
  Když něco nevyjde, zapiš čísla a zastav se. Rozhodnutí o změně udělá člověk.
- Smíš opravit chyby překladu v souborech psaných bez překladače. Záměr je v
  `docs/aec3-tuning-guide.md`, kapitola 11.1. Každou takovou opravu vyjmenuj v DEVLOGu.
- Výsledky zapiš do `DEVLOG.md` jako milník 50, commitni a pushni na `aec3-mic-gain`. Nemerguj.

## 0. Přeložit a pustit testy

```powershell
git pull
cargo test --release -p gamersequalizer-mic-chain --lib aec3:: -- --nocapture
cargo test -p gamersequalizer-common
cargo test -p gamersequalizer-daemon chain_log
cargo test -p gamersequalizer-mic-chain --release
cargo clippy --workspace --all-targets
```

Čekaný výsledek: `aec3::` 16 passed, 4 ignored; `chain_log` 5 passed. Sdílená paměť má layout v19
(přibyl `fit` do logu), takže pozor na to, aby APO a aplikace byly ze stejného buildu. Z výpisu si zkontroluj tyto řádky (hodnoty z Macu):

```text
echo -20 dB, music from 2 s: -20.0 dB (bound: false), fit 1.00 x 1.01
echo -50 dB, music from 0 s: -49.6 dB (bound: false), fit 1.00 x 0.85
echo -200 dB, music from 0 s: -16.5 dB (bound: true), fit 0.56 x 0.75
strong echo, first 0.4 s: mic -38.8, out -40.2, remembered -82.7 dBFS
```

Odchylka do 0,5 dB je v pořádku (jiný procesor, jiné SIMD). Větší zapiš.

## 1. Test A: kde se nic měnit nemělo, nic se nezměnilo

Záložní odhad má zůstat na -40 dB všude, kde měřič nepotvrdí echo silnější než asi 32 dB pod
referencí. Výsledky AEC3 pak mají být stejné jako v milníku 44.

```powershell
$env:VARIANTS = ""
cargo test --release -p gamersequalizer-mic-chain --lib capture_tuning_report -- --ignored --nocapture
Remove-Item Env:VARIANTS
```

Porovnej s řádky „winner" v tabulce milníku 44 (sloupce `echo`, `left`, `voice`, `>6dB`, `>10dB`,
`onset`, `4-8k`).

| Výsledek                                             | Verdikt                                    |
| ---------------------------------------------------- | ------------------------------------------ |
| `fallb` -40 a čísla shodná (do 0,1 dB / 1 bodu %)    | prošlo                                     |
| `fallb` -40 a čísla jiná                             | **neprošlo**: zapiš nahrávku a oba řádky   |
| `fallb` jiný než -40                                 | zapiš oba řádky; jestli je to oprávněné, rozhodne test B |

Jiný `fallb` než -40 se dá čekat nejvýš u `101152` a `102403` (monitor přes HDMI, `check` kolem
-29 a -28 dB v milníku 46). Jinde by to byla chyba.

## 2. Test B: čte měřič správně?

```powershell
$env:SCENES = ""
cargo test --release -p gamersequalizer-mic-chain --lib capture_echo_report -- --ignored --nocapture
```

Report pro každou nahrávku vypíše řádek a pod ním stopu (`trace`): kdy se změnila hlášená síla
echa, záložní odhad a zesílení mikrofonu. Totéž by ukázal debug log.

| Sloupec | Význam                                                                                |
| ------- | ------------------------------------------------------------------------------------- |
| `meter` | co hlásí měřič na konci, dB pod referencí; `<` = nepotvrzeno, jen horní mez (může to být hlas) |
| `fit`   | jak dobře úroveň mikrofonu sleduje úroveň reference na konci: korelace x sklon. Potvrzeno je od 0.90 a sklonu 0.7-1.3 |
| `check` | totéž jinou cestou: medián (úroveň mikrofonu - úroveň reference) v oknech, kde Silero slyší jen echo a reference je nad -30 dBFS |
| `fallb` | záložní odhad echa v AEC3 na konci (dB pod referencí)                                 |
| `start` | o kolik dB ubylo echo v prvních 0,1-0,5 s hlasité reference, kdy AEC3 ještě nemá filtr |
| ostatní | jako v `capture_tuning_report`, od 3. sekundy                                         |

Každá nahrávka dostane jeden ze tří verdiktů:

| Verdikt          | Kdy                                                                                 |
| ---------------- | ----------------------------------------------------------------------------------- |
| **prošlo**       | potvrzeno (`meter` bez `<`) a `check - 6 <= meter <= check + 2`; nebo bez reference a `meter` je `-` |
| **nepotvrzeno**  | `meter` je mez (`<...`) a `fallb` je -40. Není to chyba: AEC3 se chová jako dřív. Zapiš `fit` |
| **NEPROŠLO**     | cokoli z tabulky níž                                                                |

Co je chyba:

| Nahrávky                                        | NEPROŠLO, když                                                  |
| ----------------------------------------------- | --------------------------------------------------------------- |
| sluchátka (`115931`, `122520`)                  | `meter` je potvrzený (bez `<`)                                   |
| řeč do hudby bez oken s echem (`124950`, `125717`, `144824`) | `fallb` není -40                                   |
| reproduktory a monitor                          | potvrzeno, ale `meter > check + 2` nebo `meter < check - 6`     |
| `205013`, `210655`                              | potvrzeno a `meter` nad -45                                     |
| všechny                                         | `fallb` není -40, ačkoli `meter` je mez nebo je pod -33         |

Stopa (`trace`), chyba je:

- potvrzená hodnota (`echo X (fit ...)`) během řeči **stoupne o 4 dB a víc** (hlas asi:
  `130319` 6,4-10,4 s, 16,5-20,3 s, 26,6-30 s; `125717` od 12. s; `205013` 9-29 s),
- víc než 3 změny potvrzené hodnoty za nahrávku,
- víc než jedna změna `fallback` za nahrávku.

Řádek `no echo confirmed (at most X)` se smí měnit libovolně: je to jen horní mez a nic neřídí.

U nahrávek s verdiktem „nepotvrzeno", které **mají** okna s echem o samotě (reproduktory,
monitor), zapiš do tabulky `fit`. Podle těch čísel se rozhodne, jestli je práh 0.90 pro skutečné
místnosti moc přísný. Sám ho neměň.

Když je aspoň jedna nahrávka NEPROŠLO, **zastav se** a zapiš tabulku a stopy. Test C pak nemá
smysl. Samá „prošlo" a „nepotvrzeno" znamenají pokračovat.

## 3. Test C: silné echo, nasimulované z nahrávek

Žádná nahrávka nemá echo silnější než 37 dB pod referencí, a právě pro silné echo je změna
určená. Scéna `micscale=<dB>` zesílí surový mikrofon: echo je pak o tolik silnější vůči referenci,
jako s reproduktory těsně u mikrofonu. Místnost, mikrofon i hlas zůstávají skutečné.
`seed=auto` předá AEC3 sílu echa změřenou v předchozí scéně, stejně jako to v další relaci
udělá cache daemonu.

```powershell
$env:SCENES = ";micscale=20;micscale=20 seed=auto;micscale=20 seed=auto gain=0.01"
$env:CAP = "20261109-122712,20261109-130319,20261109-205013,20261109-210655,20261107-101152,20261107-102403"
cargo test --release -p gamersequalizer-mic-chain --lib capture_echo_report -- --ignored --nocapture
```

Názvy složek uprav podle skutečných. Když report u scény napíše „samples of the scaled mic are
over full scale", použij pro tu nahrávku `micscale=12` (případně 6) a zapiš, které to byly.

Test C má smysl jen pro nahrávky, které v testu B vyšly „prošlo" (potvrzeno): `micscale` na
tom, jestli mikrofon sleduje referenci, nic nemění. U ostatních zapiš jen „nepotvrzeno, test C
nic neukáže".

Čtyři scény a co znamenají:

| Scéna                                   | Co simuluje                                                |
| --------------------------------------- | ---------------------------------------------------------- |
| `(as recorded)`                         | nahrávka jak je                                            |
| `micscale=20`                           | silné echo, první relace: měřič nic neví, změří a přestaví AEC3 |
| `micscale=20 seed=auto`                 | silné echo, další relace: síla echa je zapamatovaná        |
| `micscale=20 seed=auto gain=0.01`       | totéž, ale záložní odhad natvrdo na starých -40 dB (sloupec `fallb` tu ukazuje, co by měřič chtěl, ne co běží) |

Kritéria:

| Co                                                                 | Prošlo, když                                              |
| ------------------------------------------------------------------ | --------------------------------------------------------- |
| linearita: `meter` u `micscale=20` proti `(as recorded)`           | o 20 dB výš, ±2 dB (pokud původní nebyl mez)              |
| `fallb` u `micscale=20`                                            | mezi `meter - 8` a `meter + 2`, nejméně -40, nejvýš 0     |
| stopa u `micscale=20`                                              | právě jedno `fallback -40 -> X`, 5-11 s po začátku hudby  |
| stopa u `seed=auto`                                                | `0.0 s fallback -40 -> X` a žádná další změna `fallback`  |
| `start`: `seed=auto` proti `seed=auto gain=0.01`                   | aspoň o 6 dB víc, pokud `fallb` není -40                  |
| hlas: `seed=auto` proti `seed=auto gain=0.01` (`voice`, `>10dB`, `onset`) | `voice` a `onset` nejvýš o 0,5 dB horší, `>10dB` nejvýš o 3 body |
| echo: `seed=auto` proti `seed=auto gain=0.01` (`echo`, `left`)     | `echo` nejvýš o 1 dB horší, `left` nejvýš o 1 dB výš      |

Poslední tři řádky jsou jádro: zapamatovaná síla echa má pomoct na začátku (`start`) a nic
nestát potom. Řádek `micscale=20` (první relace) s ostatními v hlasových sloupcích
neporovnávej: obsahuje přestavbu AEC3 uprostřed nahrávky.

Pokud u některé nahrávky vyjde `fallb` -40 i s `micscale=20` (echo je pořád slabší než 32 dB pod
referencí), přidej pro ni `micscale=30`, pokud se nepřebudí.

## 4. Co zapsat do DEVLOGu (milník 48)

1. Které soubory se nepřeložily a co jsi v nich opravil.
2. Test A: verdikt; nahrávky, které se liší, s oběma řádky.
3. Test B: tabulka nahrávka / `meter` / `fit` / `check` / `fallb` / verdikt (prošlo, nepotvrzeno,
   NEPROŠLO), a stopy tam, kde je víc než jedna změna.
4. Test C: pro každou nahrávku čtyři řádky a stopy, pod tím verdikt podle sedmi kritérií.
5. Jedna věta na konec: prošlo všechno / neprošlo X, a co z toho plyne pro živý test.

## 5. Co nedělat

- Neladit podle výsledků konstanty ani `tuning()`.
- Nepouštět `capture_replay_report` denoiseru na výstup scén s `micscale`: zesílení leveleru
  z nahrávky k zesílenému mikrofonu nepatří a čísla by nic neznamenala.
- Nebalit ani neinstalovat nic. Živý test (přeinstalace APO kvůli layoutu v18, log se zapnutým
  „Ukládat debug na disk") je další krok a popisuje ho `docs/aec3-tuning-guide.md`, kapitola 11.3.

## 6. Třetí kolo: jak nízko smí práh korelace

Milník 48 ukázal, že práh 0.90 potvrdí echo jen na 2 z 11 nahrávek s reproduktory. Tabulka ale
má `fit` jen z konce každé nahrávky. Než se práh sníží, je potřeba vědět, **kdy by nižší práh
potvrdil něco, co echo není**. Na to je scéna `fit=<r>`: měřič potvrzuje už od korelace `r`.
V kódu se nic nemění, konstanta `ECHO_FIT_MIN_CORRELATION` zůstává 0.90.

```powershell
$env:SCENES = ";fit=0.8;fit=0.7;fit=0.6;fit=0.5"
Remove-Item Env:CAP -ErrorAction SilentlyContinue
cargo test --release -p gamersequalizer-mic-chain --lib capture_echo_report -- --ignored --nocapture
```

Pro každou nahrávku s referencí a každý práh zapiš: potvrzeno ano/ne, `meter`, `check`, `fallb`,
a ze stopy čas prvního potvrzení a největší potvrzenou hodnotu.

Každý řádek ohodnoť:

| Verdikt             | Kdy                                                                              |
| ------------------- | -------------------------------------------------------------------------------- |
| **správně**         | potvrzeno a po celou stopu `check - 6 <= hodnota <= check + 2`                   |
| **nepotvrzeno**     | `meter` je mez                                                                   |
| **FALEŠNĚ**         | potvrzeno na sluchátkách (`115931`, `122520`)                                    |
| **FALEŠNĚ**         | potvrzeno a některá hodnota ve stopě je nad `check + 2` (u `124950`, `125717`, `144824` bez `check`: nad -33) |
| **FALEŠNĚ**         | potvrzená hodnota během řeči stoupne o 4 dB a víc                                |
| **FALEŠNĚ**         | víc než jedna změna `fallback` ve stopě                                          |

Souhrnná tabulka na konec:

| Práh | správně (počet nahrávek) | FALEŠNĚ (které nahrávky) |
| ---- | ------------------------ | ------------------------ |
| 0.90 | ...                      | ...                      |
| 0.80 | ...                      | ...                      |
| 0.70 | ...                      | ...                      |
| 0.60 | ...                      | ...                      |
| 0.50 | ...                      | ...                      |

Doporuč nejnižší práh, který nemá **žádný** řádek FALEŠNĚ. Konstantu v kódu neměň, jen napiš
doporučení a tabulky do DEVLOGu jako milník 50, commitni a pushni.
