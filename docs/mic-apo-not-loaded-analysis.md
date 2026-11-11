# APO se u testera nikdy nenačte: analýza logu (2026-11-10)

Vstup: `gamersequalizer.log` od testera (7.–10. 11., 9 startů služby), kód (`apo_link.rs`, `dsp_watch.rs`,
`apo_install.rs`, `scripts/apo-install.ps1`, `gamersequalizer-apo/src/lib.rs`) a zdroják Equalizer APO
(`github.com/mirror/equalizerapo`, stav 2024-09-27, soubory `DeviceAPOInfo.cpp`, `Setup/Setup.nsi`).
Psáno na Macu: **nic z toho není ověřeno na Windows**.

Značení: **Naměřeno** = je v logu. **Z kódu** = ověřeno čtením zdrojáku. **Hypotéza** = potřebuje data z PC testera.

## 1. Závěr

Problém není v připojení k mikrofonu. Instalace efektu proběhla (registr i DLL jsou podle kontroly aplikace
v pořádku), ale **zvukový engine Windows (audiodg) naši APO knihovnu za čtyři dny ani jednou nespustil**,
na mikrofonu ani na výstupu. Aplikace přitom ukazuje zelené „efekt je nainstalovaný a aktivní“, protože ten
stav čte jen z registru. Proč ji Windows ignorují, z logu poznat nejde: chybí v něm stav zařízení.
Podezřelí jsou v kapitole 4, skript pro testera v kapitole 5.

## 2. Co je v logu (Naměřeno)

| Pozorování                                                                 | Kde                                   |
| -------------------------------------------------------------------------- | ------------------------------------- |
| `[ApoLink] connected to the microphone APO` chybí úplně                    | celý log, 9 startů služby             |
| Instalátor spuštěn 5x, pokaždé „for 2 device(s)“ (mikrofon + výstup)       | 7. 11. 16:10 a 22:22, 8., 9., 10. 11. |
| Další start po instalaci (bez novějšího APO) žádný `[Health] problem`      | 7. 11. 17:30, 10. 11. 19:27           |
| Výstupní EQ je zapsaný na výstupu `{479912a5…}` (jiný výstup hlásí „není“) | 8. 11. 20:40 `OutputProcessing`       |
| `[Mic] microphone processing started` jen jednou, a to v režimu mostu      | 9. 11. 20:23:56 (běžel Sonar)         |
| Most 15 minut snímal mikrofon `{a5dcf979…}`, APO se nepřipojilo ani tehdy  | 9. 11. 20:23–20:38                    |
| Se zapnutými debug řádky jen `[Leveler] start … level none`, pak nic       | 10. 11. 19:30:58–19:35                |
| Hra (Anno 117) hrála přes výstup hodiny, APO se nepřipojilo                | 8., 9., 10. 11.                       |

Popředí se od okna instalátoru pokaždé vrátilo aplikaci za 6–9 s. To odpovídá úspěchu (restart zvuku + 3 s);
při chybě okno čeká na Enter. Spolu s chybějícím `[Health] problem` při dalším startu: instalace nespadla.
Chyby `Failed to get capture device … 0x80070490` a `GetBuffer 0x88890004` jsou jen restart zvuku Windows
během instalace; most se z nich sám vzpamatoval.

Jediný úsek, kdy u testera zpracování mikrofonu běželo, je 9. 11. 20:23–20:38: SteelSeries Sonar vytvořil
virtuální mikrofon, takže DSP dělal most uvnitř naší služby (DeepFilterNet načten, latence 55 ms). Ve 20:38:54
tester otevřel SteelSeries GG, virtuální zařízení zmizela a zpracování skončilo. **Řetězec DSP tedy na jeho
PC funguje; nefunguje jen načtení do audiodg.**

### Notebook autora (10. 11. večer, `scripts/apo-diagnose.ps1`, build e41be82)

Windows 11 25H2 (26200.9711), Microphone Array + Speakers (Realtek). Naměřeno:

| Kontrola                                   | Výsledek                                                         |
| ------------------------------------------ | ---------------------------------------------------------------- |
| `DisableProtectedAudioDG`                  | není nastaveno                                                   |
| DLL, COM, registrace APO                   | v pořádku                                                        |
| příznak vypnutých vylepšení                | není ani v `FxProperties`, ani v `Properties`                    |
| režimy EFX (`modes,7`)                     | mikrofon: DEFAULT + SPEECH; reproduktory: DEFAULT                |
| DLL načtená v audiodg                      | **ano**                                                          |
| jméno sdílené paměti DLL / služba / okno   | `v19` / `v19` / `v19`                                            |
| `Global\GamersEqualizer_Apo_v19` existuje  | **ano**                                                          |
| Defender                                   | jen starý exe 960ff9c ve Stažených + jeho položka Run, ne DLL    |
| code integrity, pády audiodg               | nic                                                              |

Tím na notebooku padají H1, H2, H3 a H6 (pro APO). Paměť ale vytvoří kterékoli naše APO, i výstupní
při hraní zvuku, takže to nedokazuje, že běží APO na mikrofonu. Zbývá z logu zjistit, jestli se služba
připojila (`[ApoLink] connected`) a jestli při nahrávání přišla telemetrie mikrofonu
(`[Mic] microphone processing started`).

### Notebook autora (11. 11. ráno, build 10dd4cd s hlášeními z audiodg)

Naměřeno (výstup skriptu + log služby):

| Čas      | Co se stalo                                                                           |
| -------- | ------------------------------------------------------------------------------------- |
| 08:24:06 | aktualizace APO (nová DLL, restart zvuku)                                             |
| 08:24:20 | audiodg pid 6836: `Output EQ` i `Stream EQ` „started“ (48 kHz, 2 ch, float)           |
| 08:24:22 | Záznam zvuku nahrává z `Microphone Array` (`[Tray] microphone in use`)                |
| 08:24:32 | služba správně hlásí `NotRunning { flow: Capture }` + upozornění Windows              |
| potom    | od mikrofonního APO **žádné hlášení**: ani „started“, ani formát, ani paměť, ani stop |

- Výstupní APO běží ve stejném audiodg a zapisovat do protokolu umí, takže mikrofonní APO se k
  `LockForProcess` vůbec nedostalo. Windows ho buď pro capture graf nevytvořily, nebo ho vyřadily
  už při vyjednávání formátu (tam build 10dd4cd nic nezapisoval).
- Mikrofon: MFX = 3 APO Realteku (režimy DEFAULT, RAW, SPEECH, COMMUNICATIONS), EFX = Realtek + naše
  (režimy DEFAULT + SPEECH). Dokumentace Microsoftu u `PKEY_EFX_ProcessingModes_Supported_For_Streaming`
  povoluje pro EFX jen DEFAULT. Na vývojovém PC (kde to funguje) má mikrofon režimy jen DEFAULT + jeden
  další a žádný RAW.
- `mode 00000000-…` u výstupních APO: implementujeme jen `IAudioSystemEffects`, takže Windows
  (hypotéza) předávají `APOInitSystemEffects` v1 bez režimu. Nový build zapisuje velikost bloku.

Další krok (build po 10dd4cd): APO zapisuje každý krok, kterým ho Windows spouští: požadavek na
třídu, vytvoření instance, `Initialize`, každý dotaz na vstupní a výstupní formát s odpovědí a
odmítnutý `LockForProcess`. Poslední hlášení mikrofonu ukáže, kde se to zastaví. Skript vypisuje
každý audiodg zvlášť. Pokus bez kompilace: přidat mikrofon i do slotu MFX
(`apo-install.ps1 -Flow Capture -Slot MFX`) a podívat se, jestli pak přijde „Mic Chain: stream … started“.

**Pokus s MFX (11. 11. 08:54, stejný build 10dd4cd, Naměřeno):** mikrofon přidaný i do slotu MFX
(`fx,14`) se při nahrávání spustil: „Mic Chain: stream 48000 Hz, 2 ch, float 4 bytes … started“, pak
„stream stopped after 1687 blocks, 1679 processed“. Služba poprvé hlásí `[Mic] microphone processing
started` a změřila latenci 54.9 ms. **Na tomhle mikrofonu Windows EFX nespouští, MFX ano.**

Rozhodnutí (stejně jako Equalizer APO, který má výchozí SFX/EFX a přepnutí na SFX/MFX):
- Výchozí slot mikrofonu zůstává EFX. Na vývojovém PC funguje a běží i pro RAW streamy.
- Když služba vidí, že efekt v EFX nic nezpracoval, nabídne „Zkusit jiné zapojení efektu“ (karta
  Mikrofon i menu v liště). Instalátor dostane `Capture:{guid}:MFX`, přesune efekt a z druhého slotu
  ho odebere, aby neběžel dvakrát. Bez slotu (aktualizace) ho nechá tam, kde je.
- Omezení MFX: běží jen v režimech z `modes,6` (tady DEFAULT, RAW, SPEECH, COMMUNICATIONS). Aplikace v
  jiném režimu by dostala neupravený mikrofon.
- Automaticky při instalaci (pořád jeden dotaz Windows): instalátor po restartu zvuku spustí
  `gamersequalizer.exe --probe-mic {guid}`. Ten otevře mikrofon na 3 s jako běžná aplikace a instalátor
  v protokolu čeká na „Mic Chain: stream …“. Když nepřijde, přesune efekt do druhého slotu, restartuje
  zvuk a zkusí to znovu. Když nefunguje ani jeden, napíše to a okno nechá otevřené.
- Hlášení „started“ u mikrofonu uvádí, kolik mikrofonních řetězců v audiodg právě běží. V MFX vzniká
  jeden na každý režim, který se zrovna používá, takže je vidět, kdy běží dva najednou.

**Diagnostika jen v logu (11. 11.):** testeři posílají jen log aplikace, žádný další nástroj.
`scripts/apo-diagnose.ps1` je pryč a log obsahuje to, co skript uměl bez práv správce:
- `[ApoCheck]`: build Windows, `DisableProtectedAudioDG`, DLL (čas, značka „staženo z internetu“),
  registrace COM všech tří tříd, všechna zařízení s našimi efekty (sloty, stav) a zařízení v použití.
- `[ApoNote]`: hlášení APO s procesem a buildem DLL (dokazuje, že audiodg DLL načetl, a kterou verzi),
  pády audiodg, nálezy Defenderu a události Code Integrity o našich souborech. Když Windows některý z
  těchto protokolů nedovolí číst, log to napíše.

Vývojové PC se týden jen uspává do hibernace, restart neproběhl. Než ho prohlásíme za referenci,
je potřeba ho jednou opravdu restartovat (Restartovat, ne Vypnout: rychlé spuštění je taky
hibernace) a ověřit, že mikrofon funguje dál.

## 3. Z kódu

- Sdílenou paměť `Global\GamersEqualizer_Apo_v19` vytváří APO v `LockForProcess`, a to každá ze tří tříd
  (mikrofon, výstupní EQ, EQ streamů): `lib.rs:602`, `:607`, `:918`. Služba ji jen otevírá a při úspěchu
  zaloguje `[ApoLink] connected…` (`apo_link.rs:162`). Neúspěch se neloguje („expected“).
- Most otevírá mikrofon v běžném režimu, ne RAW (v kódu není `AUDCLNT_STREAMOPTIONS_RAW`), takže 9. 11. měl
  audiodg důvod APO na mikrofonu vytvořit.
- `ApoStatus::Active` znamená jen: DLL je v Program Files a naše CLSID jsou v seznamech efektů zařízení
  (`apo_install.rs:94-101`). Jestli efekt opravdu běží, se nikde nekontroluje a nehlásí.
- Aplikace nekontroluje vypnutá „Vylepšení zvuku“. Varuje jen instalační skript, v okně, které se za 3 s
  zavře. `CTI-ME.txt` bod 4 se odvolává na hlášení aplikace, které neexistuje.
- Skript čte příznak vypnutých vylepšení z klíče `Properties` (`apo-install.ps1:235`). Equalizer APO čte tutéž
  hodnotu `{1da5d803-…},5` z `FxProperties` (`DeviceAPOInfo.cpp:236`). Pokud má pravdu upstream, naše
  varování se nezobrazí nikdy.

## 4. Hypotézy

H1–H3 vysvětlují, proč selžou obě zařízení zároveň a proč to na vývojovém PC funguje. Tester používá USB
sluchátka (doplněno 10. 11.), což posiluje H2 a přidává H5. Pořadí po 10. 11. večer (Defender na
notebooku autora smazal exe): **H6**, pak H2, H3, H4, H5. H1 v původní podobě padá.

| #  | Hypotéza                                                         | Opora                                                                                                     |
| -- | ---------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------- |
| H1 | Windows odmítnou nepodepsanou DLL v audiodg                      | Equalizer APO kvůli tomu nastavuje `DisableProtectedAudioDG=1` (`Setup.nsi:269`); my ne                   |
| H2 | Zařízení nemá uvedené režimy zpracování pro slot EFX             | Equalizer APO dopisuje `{d3993a3f-…},5/6/7` = DEFAULT, když chybí (`DeviceAPOInfo.cpp:578-606`)           |
| H3 | Vylepšení zvuku jsou na zařízení vypnutá                         | Equalizer APO tu hodnotu při instalaci maže (`:611`); my jen varujeme, a nejspíš ze špatného klíče        |
| H4 | DLL se načte, ale APO odmítne formát nebo nevytvoří paměť        | paměť vzniká až v `LockForProcess`; chyba se v audiodg nemá kam zalogovat                                 |
| H5 | Efekt je zapsaný na jiném zařízení, než které aplikace používají | kontrola čte jen registr zařízení, které má aplikace za používané; názvy zařízení v logu nejsou           |
| H6 | Antivir blokuje naši DLL nebo exe                                | notebook autora: Defender smazal exe jako `Trojan:Win32/Cloxer`; u testera okno Bitdefenderu po instalaci |

- **H6** (doplněno 10. 11. večer, teď hlavní podezřelý):
  - Naměřeno: na notebooku autora Defender smazal `gamersequalizer.exe` s nálezem `Trojan:Win32/Cloxer`.
    Předtím se tam aplikace podle autora chovala nejspíš stejně jako u testera (neověřeno logem). U testera
    je v logu 9. 11. 20:25:01 okno `bdagent.exe` (Bitdefender), minutu po instalaci.
  - Sedí to na symptom: antivir, který soubor označil, k němu zablokuje přístup. DLL v Program Files pak
    existuje (kontrola aplikace projde, stav „aktivní“), ale audiodg ji neotevře a efekt tiše vynechá.
  - Proč nás heuristika chytá (z kódu): nepodepsané binárky bez informací o verzi (`build.rs` vkládá jen
    ikonu, DLL nemá žádné prostředky), skripty PowerShellu uvnitř exe, spuštění PowerShellu jako správce
    s `-ExecutionPolicy Bypass -EncodedCommand` (`apo_install.rs:182`), odpojený proces na pozadí
    (`main.rs:473`), automatické spouštění, sledování okna v popředí, nahrávání mikrofonu a DLL zapsaná do
    systémového procesu. Každá z těch věcí je legitimní, dohromady vypadají jako malware.
  - Nevíme: jestli Defender označil i DLL, a jestli je nález planý. Odsud to potvrdit nejde.
  - Co se dá o `Trojan:Win32/Cloxer` dohledat (web, 10. 11.): oficiální popis od Microsoftu se nenašel. Trend
    Micro vede `Trojan:Win32/Cloxer.D!cl` jako Microsoftí jméno pro obecnou rodinu „dropper“
    (`Trojan.Win32.CLOXER.AA`). Na fórech je řada hlášení téhož nálezu na legitimních instalátorech a hrách
    (TMS Web Core, ASP.NET Zero, INVERSUS Deluxe), která vývojáři řešili nahlášením planého poplachu
    Microsoftu; nález se liší stroj od stroje. Je to tedy obecný nález, ne otisk konkrétního viru, ale samo
    o sobě to čistotu našeho souboru nedokazuje. Zapsat přesný název i s příponou z Historie ochrany.
- **H1** (oslabena 10. 11.): na vývojovém PC hodnota `DisableProtectedAudioDG` neexistuje
  (`reg query "HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Audio" /v DisableProtectedAudioDG` nic nenašel)
  a nepodepsaná DLL se tam přesto načítá. Windows ji tedy samy o sobě neodmítají a příznak není potřeba.
  Zbývá jen varianta specifická pro PC testera: jiný build Windows nebo antivir. V logu je 9. 11. 20:25:01
  okno `bdagent.exe` (Bitdefender), minutu po instalaci; čeho se týkalo, z logu nevíme.
  Navíc aplikace o víkendu 7.–8. 11. podle autora fungovala i na notebooku (druhé PC). Neověřeno logem:
  zelené „aktivní“ je jen registr, důkazem je až řádek `[ApoLink] connected…` v logu notebooku.
- **H2**: milník 16 testoval Realtek mikrofon, který už vlastní EFX měl (i s režimy). Zařízení bez EFX
  (typicky USB sluchátka na obecném ovladači) má režimy jen pro SFX/MFX a skript přidá EFX bez nich.
- **H3**: nejlevnější na ověření, tester to vidí v Nastavení > Systém > Zvuk > zařízení > „Vylepšení zvuku“.
- **H5** (sluchátka odpojená při instalaci, nebo efekt na jiném mikrofonu):
  - Pro: stav „aktivní“ je jen registr mikrofonu, který aplikace považuje za používaný (vybraný, jinak výchozí
    ve Windows v roli „zařízení“, `eConsole`). Z logu nevíme, že `{a5dcf979…}` je mikrofon sluchátek. Roli
    „komunikační zařízení“ aplikace u mikrofonu nesleduje; pokud ji Signal používá a ukazuje jinam, nahrává
    z mikrofonu bez efektu a aplikace nic nehlásí.
  - Proti: skript na odpojené zařízení neinstaluje (`Resolve-Job` chce `DeviceState` 1) a skončí chybou dřív,
    než cokoli změní. 9. 11. jsou obě zařízení v logu vidět těsně před instalací i po ní (most na
    `{a5dcf979…}`, loopback na `{479912a5…}`). Kdyby se po připojení sluchátek změnil výchozí mikrofon, log by
    měl `NotOnThisMic` nebo `NotDefaultMic`; nemá ani jedno.
  - Výstup tím vysvětlit nejde: výstupní EQ je na výchozím výstupu `{479912a5…}`, přes který hrála hra, a
    paměť APO by vytvořil i on. H5 tedy může být nanejvýš druhý problém vedle H1–H4.

## 5. Postup

0. **Notebook autora (H6), nejdřív tohle**:
   - Zabezpečení Windows > Ochrana před viry a hrozbami > Historie ochrany: které soubory nález uvádí (jen
     exe, nebo i `C:\Program Files\GamersEqualizer\apo\gamersequalizer_apo.dll`) a přesný název nálezu.
     Totéž v PowerShellu jako správce: `Get-MpThreatDetection | Format-List InitialDetectionTime, ThreatID, Resources, ProcessName`.
   - Pokus: přidat výjimku Defenderu pro složku s aplikací a pro `C:\Program Files\GamersEqualizer`, exe
     obnovit, efekt znovu nainstalovat, otevřít mikrofon. Když se pak v logu objeví `[ApoLink] connected…`
     a bez výjimek ne, je H6 potvrzená.
   - Tester: v Bitdefenderu Oznámení a Karanténa, jestli tam je `gamersequalizer`.
1. Hotovo: `reg query` z H1 na vývojovém PC nic nenašel, H1 je oslabena. Hlavní podezřelí jsou H2 a H3.
   - Notebook: najít v `%APPDATA%\GamersEqualizer\gamersequalizer.log` řádek `[ApoLink] connected to the
     microphone APO` a zapsat, přes jaký mikrofon a výstup se testovalo (vestavěné, nebo USB).
   - **H2 jde zkusit bez testera**: na notebooku nebo vývojovém PC připojit libovolná USB sluchátka nebo USB
     mikrofon, nainstalovat na ně efekt, otevřít mikrofon a sledovat, jestli se řádek `[ApoLink] connected…`
     objeví. Skript níže (s GUID toho zařízení v `$eps`) ukáže, jestli je `rezimy,7` prázdné.
2. Tester zkontroluje „Vylepšení zvuku“ na mikrofonu i na sluchátkách (nesmí být „Vypnuto“).
3. Tester pustí hudbu, otevře Záznam zvuku (ať mikrofon i výstup běží) a v PowerShellu **jako správce**
   spustí skript níže. Jen čte, nic nemění. Výstup pošle zpátky. Skript nebyl spuštěn (psáno na Macu).

```powershell
$eps = @(@('Capture', '{a5dcf979-3cfc-4d0b-b464-2b515ffda9f2}'), @('Render', '{479912a5-6d7b-4273-bd30-7761a00fc044}'))
$root = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio'
$fx = '{d04e05a6-594b-4fb6-a80d-01af5eed7d1d}'; $modes = '{d3993a3f-99c2-4402-b5ec-a92a0367664b}'
$sys = '{1da5d803-d492-4edd-8c23-e0c0ffee7f0e},5'
$nt = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion'
"Windows $($nt.DisplayVersion) build $($nt.CurrentBuild).$($nt.UBR)"
"DisableProtectedAudioDG = $((Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Audio' -ErrorAction SilentlyContinue).DisableProtectedAudioDG)"
foreach ($e in $eps) {
  $key = "$root\$($e[0])\$($e[1])"
  $p = Get-ItemProperty "$key\Properties" -ErrorAction SilentlyContinue
  $f = Get-ItemProperty "$key\FxProperties" -ErrorAction SilentlyContinue
  "`n== $($e[0]) $($e[1]): $($p.'{a45c254e-df1c-4efd-8020-67d146a850e0},2') ($($p.'{b3f8fa53-0004-438e-9003-51a46e139bfc},6'))"
  "vylepseni vypnuta: FxProperties=$($f.$sys) Properties=$($p.$sys)"
  foreach ($n in 1, 2, 5, 6, 7, 13, 14, 15) { "fx,$n = $($f."$fx,$n" -join ' | ')" }
  foreach ($n in 5, 6, 7) { "rezimy,$n = $($f."$modes,$n" -join ' | ')" }
}
"`n== Vsechna zarizeni (stav 1 = pripojene)"
foreach ($flow in 'Capture', 'Render') {
  foreach ($k in Get-ChildItem "$root\$flow") {
    $p = Get-ItemProperty "$($k.PSPath)\Properties" -ErrorAction SilentlyContinue
    $f = Get-ItemProperty "$($k.PSPath)\FxProperties" -ErrorAction SilentlyContinue
    $lists = (13, 14, 15 | ForEach-Object { $f."$fx,$_" }) -join ' '
    "$flow $($k.PSChildName) stav=$((Get-ItemProperty $k.PSPath).DeviceState) nase APO=$($lists -match '17e8d42e|6b0f3c1e|3c9a5e21') : $($p.'{a45c254e-df1c-4efd-8020-67d146a850e0},2') ($($p.'{b3f8fa53-0004-438e-9003-51a46e139bfc},6'))"
  }
}
$dll = Join-Path ([Environment]::GetFolderPath('ProgramFiles')) 'GamersEqualizer\apo\gamersequalizer_apo.dll'
"`nDLL: $(Test-Path $dll) $((Get-Item $dll -ErrorAction SilentlyContinue).LastWriteTime)"
try {
  $mods = (Get-Process audiodg -ErrorAction Stop).Modules | Where-Object ModuleName -eq 'gamersequalizer_apo.dll'
  "DLL nactena v audiodg: $([bool]$mods)"
} catch { "DLL nactena v audiodg: nelze zjistit ($_)" }
Get-WinEvent -LogName 'Microsoft-Windows-CodeIntegrity/Operational' -MaxEvents 500 -ErrorAction SilentlyContinue |
  Where-Object { $_.Message -match 'gamersequalizer|audiodg' } | Select-Object -First 5 TimeCreated, Id, Message | Format-List
```

Jak výstup číst:

| Výstup                                                     | Znamená                  |
| ---------------------------------------------------------- | ------------------------ |
| událost CodeIntegrity s `gamersequalizer_apo.dll`          | H1                       |
| `fx,15` obsahuje naše CLSID, `rezimy,7` je prázdné         | H2                       |
| `vylepseni vypnuta` = 1 (v kterémkoli klíči)               | H3, a ukáže správný klíč |
| `DLL nactena v audiodg: True` a služba se přesto nepřipojí | H4                       |
| `DLL nactena v audiodg: False`, registr v pořádku          | H1 nebo H2               |
| `nase APO=True` má jiné zařízení než sluchátka testera     | H5                       |

K H5 ať tester ještě napíše, co má v Nastavení > Systém > Zvuk > Další nastavení zvuku > Záznam jako
„Výchozí zařízení“ a co jako „Výchozí komunikační zařízení“.

## 6. Nápady na změny (až po datech od testera)

1. **Log, který to příště řekne sám** (implementováno ve větvi `diag/apo-not-running`, nepřeloženo):
   - `[ApoCheck]`: build Windows, `DisableProtectedAudioDG`, čas instalované DLL a pro mikrofon i výstup
     seznamy efektů, režimy slotů a příznak vylepšení; zapíše se při startu a při každé změně;
   - APO zapisuje z audiodg do protokolu Windows (Application, zdroj `GamersEqualizer`): start streamu
     (formát, režim, jestli se řetězec postavil), odmítnutý formát, nedostupnou sdílenou paměť a při konci
     mikrofonního streamu, kolik bloků přišlo a kolik se zpracovalo a proč ne; služba je kopíruje do logu
     jako `[ApoNote]`;
   - `[ApoLink] APO not reachable yet: …` s kódem chyby, první verdikt `[ConsoleOwner]`, verze a hash buildu
     v úvodním řádku služby.
2. **Nový stav „nainstalováno, ale neběží“** (implementováno 10. 11., nepřeloženo, neověřeno na Windows):
   `health.rs` `RunWatch`. Každou sekundu: zařízení se používá (mikrofon: nahrává jiná aplikace a není
   „vše vypnuto“; výstup: špička na výstupu > -66 dBFS) a čítač našeho efektu ve sdílené paměti stojí
   (mikrofon `aec_telemetry_seq`, výstup `out_apo_blocks`). Po 10 takových kontrolách problém
   `NotRunning`: řádek `[Health] problem: NotRunning …` v logu, upozornění Windows a položka v menu
   v oznamovací oblasti (Otevřít nastavení zvuku / Ignorovat). Hlídá se jen zařízení, na kterém registr
   efekt uvádí; jiná zvukovka = jiné zařízení, tu dál řeší `NotOnThisMic` / `OutputProcessing`.
   Stavový řádek na kartě Mikrofon říká „běží“ jen podle telemetrie, ne podle registru.
3. **Instalační skript** podle toho, co se potvrdí:
   - H3: číst příznak ze správného klíče a rozhodnout, jestli ho mazat jako upstream, nebo jen hlásit v aplikaci;
   - H2: dopsat DEFAULT režim pro sloty, které přidáváme (EFX, na výstupu i SFX), a při odinstalaci ho vrátit;
   - H1: `DisableProtectedAudioDG=1` je obchůzka s cenou (upstream varuje, že aplikace vyžadující chráněnou
     zvukovou cestu se pak mohou chovat jinak). Čisté řešení je podepsaná DLL.
4. Opravit `CTI-ME.txt` bod 4 podle toho, co aplikace opravdu hlásí.
5. **H6, planý poplach antiviru** (od nejlevnějšího):
   - nahlásit soubory Microsoftu jako planý poplach (`microsoft.com/wdsi/filesubmission`) a Bitdefenderu;
     platí jen pro daný build, každý další je nový neznámý soubor;
   - přidat do exe i DLL informace o verzi (název produktu, popis, verze); `embed-resource` už v projektu je;
   - instalaci dělat bez PowerShellu: stejné exe spuštěné jako správce s parametrem a zápis do registru
     přímo z Rustu. Odpadne `-EncodedCommand`, `-ExecutionPolicy Bypass` i skripty vložené v exe;
   - podpis kódu: jediné, co buduje důvěru napříč buildy; zároveň řeší SmartScreen a Smart App Control;
   - do `CTI-ME.txt` napsat, co má tester udělat, když antivir soubor smaže;
   - bod 2 (stav „nainstalováno, ale neběží“) by tohle uživateli ukázal místo zeleného „aktivní“.

## 7. Mimo téma (taky v logu)

- Poznámkový blok a f.lux jsou rozpoznané jako hry („installed by a launcher“, id `10_MICROSOFT.WINDOWSNOTEPAD…`:
  aplikace z Microsoft Store). f.lux držel „aktivní hru“ přes dvě hodiny (9. 11. 22:30 – 10. 11. 00:49).
- Instalátor češtiny `anno117_cz_1.10.tmp` se spároval s profilem Anno 117 podle jména.
- `Possible clipping detected (2.372)` v režimu mostu (9. 11. 20:24–20:30), řádek nemá značku modulu.
