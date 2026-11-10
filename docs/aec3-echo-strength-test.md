# Síla echa: test na nahrávkách

Pro agenta (Antigravity) na Windows stroji s `test_data/private/captures/`. Větev `aec3-mic-gain`,
poslední commit (report `capture_echo_report` je až v něm). Co se testuje a proč: DEVLOG milník 45. Příkazy jsou pro PowerShell
z kořene repa.

Co test **je**: ověření měřiče síly echa a toho, co z něj plyne, na existujících nahrávkách.
Co **není**: živý test (instalace APO, log od testera). Ten přijde potom a dělá ho člověk.

## Pravidla

- Neměň konstanty měřiče (`ECHO_*`, `REF_*`, `FALLBACK_*`, `MIC_GAIN_*`) ani `tuning()`.
  Když něco nevyjde, zapiš čísla a zastav se. Rozhodnutí o změně udělá člověk.
- Smíš opravit chyby překladu v souborech psaných bez překladače. Záměr je v
  `docs/aec3-tuning-guide.md`, kapitola 11.1. Každou takovou opravu vyjmenuj v DEVLOGu.
- Výsledky zapiš do `DEVLOG.md` jako milník 46, commitni a pushni na `aec3-mic-gain`. Nemerguj.

## 0. Přeložit a pustit testy

```powershell
git pull
cargo test --release -p gamersequalizer-mic-vst --lib aec3:: -- --nocapture
cargo test -p gamersequalizer-common
cargo test -p gamersequalizer-daemon chain_log
cargo test -p gamersequalizer-mic-vst --release
cargo clippy --workspace --all-targets
```

Čekaný výsledek: `aec3::` 16 passed, 4 ignored. Z výpisu si zkontroluj tyto řádky (hodnoty z Macu):

```text
echo -20 dB, music from 2 s: measured -20.0 dB (bound: false)
echo -50 dB, music from 0 s: measured -49.6 dB (bound: false)
echo -200 dB, music from 0 s: measured -60.5 dB (bound: true)
strong echo, first 0.4 s: mic -38.8, out -40.2, remembered -82.7 dBFS
```

Odchylka do 0,5 dB je v pořádku (jiný procesor, jiné SIMD). Větší zapiš.

## 1. Test A: kde se nic měnit nemělo, nic se nezměnilo

Síla echa je na všech dosavadních nahrávkách slabší než asi 32 dB pod referencí, takže záložní
odhad má zůstat na -40 dB a výsledky AEC3 mají být stejné jako v milníku 44.

```powershell
$env:VARIANTS = ""
cargo test --release -p gamersequalizer-mic-vst --lib capture_tuning_report -- --ignored --nocapture
Remove-Item Env:VARIANTS
```

Porovnej s řádky „winner" v tabulce milníku 44 (sloupce `echo`, `left`, `voice`, `>6dB`, `>10dB`,
`onset`, `4-8k`).

| Výsledek                                             | Verdikt                                    |
| ---------------------------------------------------- | ------------------------------------------ |
| `fallb` -40 a čísla shodná (do 0,1 dB / 1 bodu %)    | prošlo                                     |
| `fallb` -40 a čísla jiná                             | **neprošlo**: zapiš nahrávku a oba řádky   |
| `fallb` jiný než -40                                 | zapiš; ta nahrávka má silnější echo, než se čekalo. Pokračuj testem B, tam se ukáže, jestli měřič čte správně |

## 2. Test B: čte měřič správně?

```powershell
$env:SCENES = ""
cargo test --release -p gamersequalizer-mic-vst --lib capture_echo_report -- --ignored --nocapture
```

Report pro každou nahrávku vypíše řádek a pod ním stopu (`trace`): kdy se změnila hlášená síla
echa, záložní odhad a zesílení mikrofonu. Totéž by ukázal debug log.

| Sloupec | Význam                                                                                |
| ------- | ------------------------------------------------------------------------------------- |
| `meter` | co čte měřič na konci, dB pod referencí; `<` = jen horní mez (echo je v šumu mikrofonu) |
| `check` | totéž jinou cestou: medián (úroveň mikrofonu - úroveň reference) v oknech, kde Silero slyší jen echo a reference je nad -30 dBFS. Šum se neodečítá, takže u slabého echa vychází výš než `meter` |
| `fallb` | záložní odhad echa v AEC3 na konci (dB pod referencí)                                 |
| `start` | o kolik dB ubylo echo v prvních 0,1-0,5 s hlasité reference, kdy AEC3 ještě nemá filtr |
| ostatní | jako v `capture_tuning_report`, od 3. sekundy                                         |

Kritéria:

| Nahrávky                                   | Prošlo, když                                                         |
| ------------------------------------------ | -------------------------------------------------------------------- |
| reproduktory a monitor (hudba hraje)       | `meter` není mez a platí `check - 6 <= meter <= check + 2`           |
| `210655`                                   | navíc `meter` kolem -50 (echo -68 dBFS, hudba -18 dBFS, milník 42)   |
| sluchátka (`115931`, `122520`)             | `meter` je mez (`<...`), nebo je pod -50                             |
| bez reference                              | `meter` je `-`                                                       |
| všechny                                    | `fallb` je -40, pokud `meter` není nad -33                           |

Stopa (`trace`):

- První `echo ...` se smí objevit nejdřív po 5 s hlasité reference.
- Při stálé hlasitosti nejvýš 2 další změny za 30 s. Víc znamená, že měřič kolísá.
- Žádné střídání „echo X" a „echo within the noise" tam a zpět.
- Žádný řádek `fallback`, pokud `meter` není nad -33.

Zvlášť se podívej na nahrávky, kde se hodně mluví do hudby (`125717` od 12. s, `130319`:
hlas asi 6,4-10,4 s, 16,5-20,3 s, 26,6-30 s). Hlášená síla echa během řeči **nesmí stoupnout
o 4 dB a víc**. Kdyby stoupla, měřič bere hlas za echo: zapiš čas a hodnoty.

Když test B neprojde, **zastav se** a zapiš tabulku a stopy. Test C pak nemá smysl.

## 3. Test C: silné echo, nasimulované z nahrávek

Žádná nahrávka nemá echo silnější než 37 dB pod referencí, a právě pro silné echo je změna
určená. Scéna `micscale=<dB>` zesílí surový mikrofon: echo je pak o tolik silnější vůči referenci,
jako s reproduktory těsně u mikrofonu. Místnost, mikrofon i hlas zůstávají skutečné.
`seed=auto` předá AEC3 sílu echa změřenou v předchozí scéně, stejně jako to v další relaci
udělá cache daemonu.

```powershell
$env:SCENES = ";micscale=20;micscale=20 seed=auto;micscale=20 seed=auto gain=0.01"
$env:CAP = "20261109-122712,20261109-130319,20261109-205013,20261109-210655,20261107-101152,20261107-102403"
cargo test --release -p gamersequalizer-mic-vst --lib capture_echo_report -- --ignored --nocapture
```

Názvy složek uprav podle skutečných. Když report u scény napíše „samples of the scaled mic are
over full scale", použij pro tu nahrávku `micscale=12` (případně 6) a zapiš, které to byly.

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

## 4. Co zapsat do DEVLOGu (milník 46)

1. Které soubory se nepřeložily a co jsi v nich opravil.
2. Test A: verdikt; nahrávky, které se liší, s oběma řádky.
3. Test B: tabulka nahrávka / `meter` / `check` / `fallb` / verdikt, a stopy tam, kde je víc než
   jedna změna.
4. Test C: pro každou nahrávku čtyři řádky a stopy, pod tím verdikt podle sedmi kritérií.
5. Jedna věta na konec: prošlo všechno / neprošlo X, a co z toho plyne pro živý test.

## 5. Co nedělat

- Neladit podle výsledků konstanty ani `tuning()`.
- Nepouštět `capture_replay_report` denoiseru na výstup scén s `micscale`: zesílení leveleru
  z nahrávky k zesílenému mikrofonu nepatří a čísla by nic neznamenala.
- Nebalit ani neinstalovat nic. Živý test (přeinstalace APO kvůli layoutu v18, log se zapnutým
  „Ukládat debug na disk") je další krok a popisuje ho `docs/aec3-tuning-guide.md`, kapitola 11.3.
