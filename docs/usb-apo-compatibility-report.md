# USB Audio APO kompatibilita, záloha a bezriziková instalace (2026-11-11)

Vstup: log od testera (`Arctis Nova 3`, build `0.1.0 (dfcaeeb)`), živé experimenty na lokálním stroji se 3 současně připojenými USB audio zařízeními (`ASUS AI Noise-Cancelling Mic Adapter`, `EPOS H3PRO`, `Jabra EVOLVE LINK MS`, celkem 6 aktivních endpointů) a čtení [scripts/apo-install.ps1](../scripts/apo-install.ps1), [scripts/apo-uninstall.ps1](../scripts/apo-uninstall.ps1), [crates/gamersequalizer-daemon/src/win_audio/apo_install.rs](../crates/gamersequalizer-daemon/src/win_audio/apo_install.rs), [crates/gamersequalizer-daemon/src/win_audio/mic_probe.rs](../crates/gamersequalizer-daemon/src/win_audio/mic_probe.rs) a [crates/gamersequalizer-apo/src/lib.rs](../crates/gamersequalizer-apo/src/lib.rs).

Značení:
- **Změřeno**: přímo pozorováno v živém experimentu (`test-usb-apo.ps1`, log `usb-apo-20261111-200018-experiment-6usb.txt`) nebo v logu testera (`2026-11-11 18:58:16`).
- **Z kódu**: ověřeno čtením zdrojáků / ACL v registru.
- **Neověřeno / Otevřené**: navrženo nebo odvozeno, před mergem do `main` je potřeba ověřit v další session.

---

## 1. Exekutivní shrnutí (4 potvrzené příčiny)

Na Windows 11 s výchozím inbox ovladačem pro USB audio (`usbaudio2.sys` + `VocaEffectPack` na Capture + `Microsoft Audio Home Theater Effects` na Render) nešlo jen o „slabou USB zvukovku bez EFX/MFX“. Všech 6 testovaných USB endpointů dokáže APO spolehlivě spouštět (**0 pádů `audiodg.exe`, 0 selhání otevření**), ale stávající instalátor a odinstalátor mají **4 konkrétní chyby**:

| ID    | Oblast                           | Popis příčiny (Změřeno / Z kódu)                                                                                                                                          | Důsledek                                                                                           |
| ----- | -------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------- |
| Bug A | Capture + Render (`EFX` / `MFX`) | `apo-install.ps1` zapisuje CLSID do `,13`, `,14`, `,15`, ale **nikdy nezapisuje `processing modes`** (`{d3993a3f-99c2-4402-b5ec-a92a0367664b},5/6/7` = `{C18E2F7E-...}`). | USB Capture `Mic Chain` a USB Render `Output EQ` se v `audiodg.exe` vůbec nenačtou.                |
| Bug B | Render (`Stream EQ` a `MFX`)     | `apo-install.ps1` přečte legacy `REG_SZ` `,5` / `,6`, zkopíruje je do kompozitních `,13` / `,14`, ale **legacy hodnoty `,5` / `,6` v registru nechá**.                    | Windows Audio Engine ignoruje kompozitní `,13` (a `,14`), takže `Stream EQ` ani `MFX` neběží.      |
| Bug C | `PKEY_FX_Association` (`,0`)     | Na testovaných USB zařízeních `,0` (`{00000000-...}`) chybí v `FxProperties`. Experiment (`C1`, `R1`, `R3`) dokázal, že **bez `,0` vše běží**; `,0` sám nestačí.          | Zbytečné přidávat `,0` naslepo (může zůstat jen jako případná poslední záchrana pro 1 % ovladačů). |
| Bug D | Rollback / `apo-uninstall.ps1`   | `FxProperties` dává `BUILTIN\Administrators` pouze `SetValue, ReadKey` (nikoliv `CreateSubKey`). `.reg` export obsahuje podklíče (`Volatile`/`User`/`Default`).           | `reg.exe import` při `-RestoreBackup` (i v rollbacku) selže s `Error accessing the registry`.      |

---

## 2. Inventář testovaných zařízení a surový stav registru

Všechna 4 sledovaná USB zařízení vykazují na Windows 11 **totožný registr vzor** (srovnáno ještě s lokálním Realtek HDAudio a AMD HDMI):

### 2.1 Capture endpointy (mikrofony)

| Zařízení                                   | VID / PID           | GUID                                     | RegFmt / LiveMix                                     | Výchozí `FxProperties` (klíčové položky)                                                                     |
| ------------------------------------------ | ------------------- | ---------------------------------------- | ---------------------------------------------------- | ------------------------------------------------------------------------------------------------------------ |
| `Arctis Nova 3` (log testera)              | neevidováno v logu  | `{10f82ecb-25f2-4304-8fdd-e49798a171b7}` | 48 kHz (probe načetl `143520` frames)                | po instalaci: `,15 = Mic Chain`; `SFX/MFX/EFX modes = []`; `enhancements-off = -`                            |
| `ASUS AI Noise-Cancelling Mic Adapter`     | `VID_0B05&PID_194E` | `{4563517b-c096-431f-8925-9261078e8d08}` | `48000 Hz, 2 ch, 16bit` / `48000Hz 2ch 32bit 0xFFFE` | pouze `{9e6136e0-...},100 = SWD\DRIVERENUM\{96bedf2c-...}#VocaEffectPack&1&813678c&0`; bez `,0`, bez `modes` |
| `EPOS H3PRO`                               | `VID_1395&PID_0101` | `{5b424cd0-3095-4e27-8c22-d3e91d10eb3f}` | `16000 Hz, 1 ch, 16bit` / `16000Hz 1ch 32bit 0xFFFE` | `VocaEffectPack` + předchozí `,15 = {17e8d42e-...}` (po stávajícím `apo-install.ps1`); bez `,0`, bez `modes` |
| `Jabra EVOLVE LINK MS`                     | `VID_0B0E&PID_0305` | `{6a3e2730-351e-403b-bf27-3929058547cb}` | `48000 Hz, 1 ch, 16bit` / `48000Hz 2ch 32bit 0x0003` | pouze `VocaEffectPack`; bez `,0`, bez `modes`                                                                |
| `Realtek(R) Audio` (lokální, pro srovnání) | `VEN_10EC&DEV_1220` | `{cf0a2270-1885-4dd6-82a5-f636e9efadd5}` | `48000 Hz, 2 ch` (`audiodg` log `18:58:48`)          | má `,0 = {00000000-...}`, `,14` (OEM MFX), `,15` (OEM + `Mic Chain`), `modes,6` a `modes,7`                  |

Poznámka k **ASUS USB-C adaptéru**: jde o zařízení s detekcí 3,5mm jacku — dokud do něj není připojený jack, endpoint zůstává v `MMDevices` neaktivní / skrytý (důležité pro UX diagnostiku u uživatelů s USB-C dongly).

### 2.2 Render endpointy (sluchátka / výstupy)

| Zařízení                               | GUID                                     | LiveMix                        | `FxProperties` (výchozí hodnoty před fixem)                                                                                             |
| -------------------------------------- | ---------------------------------------- | ------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------- |
| `Arctis Nova 3` (log testera)          | `{a5cbd075-7a11-4177-bcc5-aa1af4cfebdf}` | —                              | `SFX modes [{C18E2F7E-...}]`, `MFX modes [{C18E2F7E-...}]`, **`EFX modes []`**                                                          |
| `ASUS AI Noise-Cancelling Mic Adapter` | `{2ca1f6e1-0996-40e9-b866-292049b6a51c}` | `48000Hz 2ch 32bit tag=0xFFFE` | `Microsoft Audio Home Theater Effects`: `,3`, `,5 = {C9453E73...}`, `,6 = {13AB3EBD...}`, `modes,5`, `modes,6`; bez `,0`, bez `modes,7` |
| `EPOS H3PRO`                           | `{e9fa77d5-58ec-4c45-aa8b-5e0467d96412}` | `48000Hz 2ch 32bit tag=0xFFFE` | stejné + předchozí `,13 = {C9453E73...}; {Stream EQ}`, `,15 = {Output EQ}`; bez `,0`, bez `modes,7`                                     |
| `Jabra EVOLVE LINK MS`                 | `{42b05aac-7b95-4cc8-8481-b940a20fe8bd}` | `48000Hz 2ch 32bit tag=0xFFFE` | `Microsoft Audio Home Theater Effects`: `,3`, `,5`, `,6`, `modes,5`, `modes,6`; bez `,0`, bez `modes,7`                                 |
| `AMD HDMI` (lokální, srovnání)         | `{09fc23e8-60ea-4ac7-88b5-19d3141dd651}` | —                              | `Microsoft Audio Home Theater Effects`: má `,0`, `,3`, `,5`, `,6`, `,13`, `,15`, `modes,5`, `modes,6`, ale **také chybí `modes,7`**!    |
| `Realtek(R) Audio` (lokální, srovnání) | `{b45cef56-a56b-49b0-88c0-4af057026718}` | —                              | má `,0`, čistě kompozitní `,13`, `,14`, `,15` (**žádné legacy `,5`, `,6`, `,7`**) + `modes,5/6/7/11/12`                                 |

Závěr z tabulky: `AMD HDMI` a kamarádova `Arctis Nova 3` Render mají úplně stejný podpis `Microsoft Audio Home Theater Effects` jako `ASUS`, `EPOS` a `Jabra`, takže fix vyřeší nejen USB sluchátka/dongly, ale i HDMI/DisplayPort audio a podobné inbox ovladače.

---

## 3. Výsledky živých experimentů (Změřeno: 2026-11-11 20:00–20:03)

Běh `test-usb-apo.ps1` (v repu uložen jeho vylepšený nástupce [scripts/verify-usb-apo.ps1](../scripts/verify-usb-apo.ps1)), záloha `apo-backup\usb-exp-20261111-200018`, každý endpoint testován izolovaně s čerstvým restartem `Audiosrv` (`audiodg.exe` nový PID pro každý krok) a po skončení vrácen do původního stavu.

### 3.1 Capture matice

| Krok                       | Popis změny                                         | ASUS (`48k 2ch`)                         | EPOS (`16k 1ch`)                                      | Jabra (`48k 1ch` mix `0x0003`)                        |
| -------------------------- | --------------------------------------------------- | ---------------------------------------- | ----------------------------------------------------- | ----------------------------------------------------- |
| `C0` baseline              | výchozí stav (`EPOS` už měl `,15`)                  | ne (exit 0)                              | ne (exit 0)                                           | ne (exit 0)                                           |
| `C0a` stock `,15`          | simulace současného `apo-install.ps1` (bez `modes`) | ne (exit 0, pid `19520`)                 | přeskočeno (`EPOS` už `,15` měl v `C0`)               | ne (exit 0, pid `24076`)                              |
| **`C1` `,15` + `modes,7`** | **EFX `,15 = Mic Chain` + `modes,7 = {C18E2F7E}`**  | **ANO** (pid `24828`, `300` blk, `OPEN`) | **ANO** (`16kHz 1ch`, pid `20272`, `226` blk, `OPEN`) | **ANO** (`48kHz 1ch`, pid `24228`, `301` blk, `OPEN`) |

Pozorování ke Capture:
- U všech 3 USB mikrofonů stačilo přidat **`{d3993a3f-99c2-4402-b5ec-a92a0367664b},7 = [REG_MULTI_SZ] {C18E2F7E-933D-4965-B7D1-1EEF228D2AF3}`**.
- Nebylo nutné psát `,0` (`PKEY_FX_Association`) ani vypínat `VocaEffectPack` (`{9e6136e0-...},100`).
- `GamersEqualizer Mic Chain` bez problému přijal `48000 Hz 2ch` (`ASUS`), `16000 Hz 1ch` (`EPOS`) i `48000 Hz 1ch` (`Jabra`, jehož WASAPI endpoint je `tag=0x0003`, v `audiodg` `48000 Hz 1ch float 4 bytes`).
- Poznámka pro `[pre-probe]`: při startu `Audiosrv` si Windows ověřují `Realtek Capture` (`48000 Hz 2ch` — `class requested`, `aggregated`, `initialized`, `output format ... accepted` — bez `stream started`), proto musí kontrola vždy sledovat **`stream ... started`** a oddělit eventy vzniklé před samotnou sondou.

### 3.2 Render matice

U `Jabra Render` šlo o výchozí přehrávací zařízení Windows, takže ihned po `Start-Service Audiosrv` (během 1,8 s pauzy před spuštěním `UsbAudioProbe`) jej začal přehrávat samotný systém — `audiodg.exe` tak zalogoval `stream started` už do sekce `[pre-probe]`, a kvůli deduplikaci zpráv na PID v [event_log.rs](../crates/gamersequalizer-apo/src/event_log.rs) se stejná hláška po startu `UsbAudioProbe` podruhé nezapsala (`SUMMARY` proto u `Jabra` vypsalo `Stream=False Output=False`, což je opraveno v [scripts/verify-usb-apo.ps1](../scripts/verify-usb-apo.ps1)). Surové řádky `AUDIODG.EXE` v logu nicméně jednoznačně ukazují skutečný stav na každém PIDu:

| Krok                                   | Změny nad baseline / předchozím krokem                                                                       | ASUS (`{2ca1...}`)                           | EPOS (`{e9fa...}`)                           | Jabra (`{42b0...}` — dle EventLog PIDu)                          |
| -------------------------------------- | ------------------------------------------------------------------------------------------------------------ | -------------------------------------------- | -------------------------------------------- | ---------------------------------------------------------------- |
| `R0` baseline                          | původní stav                                                                                                 | Stream=ne, Output=ne (`shm=MISSING`)         | Stream=ne, Output=ne (`shm=MISSING`)         | Stream=ne, Output=ne (pid `31716`, `shm=MISSING`)                |
| `R0a` stock `,13+,15`                  | bez `modes,7`, legacy `,5/,6` ponecháno                                                                      | Stream=ne, Output=ne (jen not-aggr.)         | přeskočeno (už v `R0`)                       | Stream=ne, Output=ne (pid `32756`, `shm=MISSING`)                |
| `R1` `+ modes,7`                       | přidáno `modes,7` (legacy `,5/,6` stále v registru)                                                          | Stream=ne, **Output=ANO** (pid `18528`)      | Stream=ne, **Output=ANO** (pid `21464`)      | Stream=ne, **Output=ANO** (pid `11796`, `shm=OPEN`)              |
| `R1b` `+ ,0` bez `modes,7`             | `modes,7` odebráno, přidáno `,0`                                                                             | Stream=ne, **Output=ne** (`MISSING`)         | Stream=ne, **Output=ne** (`MISSING`)         | Stream=ne, **Output=ne** (pid `18904`, `shm=MISSING`)            |
| `R2` `+ modes,7 + ,0`                  | oba klíče přítomny, legacy `,5/,6` stále zůstaly                                                             | Stream=ne, **Output=ANO** (pid `18964`)      | Stream=ne, **Output=ANO** (pid `29628`)      | Stream=ne, **Output=ANO** (pid `17512`, `shm=OPEN`)              |
| **`R3` smazání legacy `,5/,6`**        | **odstraněny legacy `,5` a `,6` (ponechán `,13` s MS SFX + `Stream EQ`, `,15` s `Output EQ`, `modes,5/,7`)** | **Stream=ANO, Output=ANO** (`22252`, `OPEN`) | **Stream=ANO, Output=ANO** (`19992`, `OPEN`) | **Stream=ANO, Output=ANO** (`32900`, `20:01:55.425`, `OPEN`)     |
| `R4` `Output EQ` do `,14` + `,5/,6`    | jen na Jabra: vráceno `,5/,6`, `Output` v `,14` MFX                                                          | —                                            | —                                            | **NE** (pid `22780`, `shm=MISSING` — `,6` zastínil `,14`!)       |
| `R4b` `Output EQ` do `,14` bez `,5/,6` | jen na Jabra: znovu smazáno `,5/,6`                                                                          | —                                            | —                                            | **Stream=ANO, Output=ANO** (pid `4212`, `20:02:03.654`)          |
| `R5` legacy `,5=Stream, ,6=Output`     | jen na Jabra: bez `,13/,14/,15`                                                                              | —                                            | —                                            | **Stream=ANO, Output=ANO** (pid `35880`, ale vyřadí MS SFX/MFX!) |

Klíčový důkaz z `R3` a `R4/R4b`:
1. **`R1` vs `R1b`**: `Output EQ` v `,15` (EFX) **vyžaduje `modes,7`** a bez něj nejede ani s `,0`.
2. **`R2` vs `R3` a `R4` vs `R4b`**: Pokud v `FxProperties` zůstanou legacy `REG_SZ` hodnoty `,5` / `,6` (např. od `Microsoft Audio Home Theater Effects`), Windows **zastíní a ignoruje** odpovídající kompozitní pole `,13` / `,14`! Jakmile je legacy `,5` / `,6` přeneseno do `,13` / `,14` a smazáno, rozjede se **jak `Stream EQ`, tak `Output EQ`** na všech 3 USB zařízeních.

---

## 4. ACL zjištění v registru: proč `reg.exe import` selhává

Při prvním pokusu o rollback na `ASUS Capture {4563517b...}` selhal příkaz `reg.exe import` chybou `ERROR: Error accessing the registry. (exit 1)`. Výpis `Get-Acl` ukázal důvod:

- `NT SERVICE\TrustedInstaller`: `FullControl`
- `NT SERVICE\Audiosrv` / `AudioEndpointBuilder`: `Delete, ReadKey, ...`
- **`BUILTIN\Administrators`: pouze `SetValue, ReadKey`** (bez `CreateSubKey`!)

Protože `reg.exe export` ukládá do `.reg` souboru i podklíče `FxProperties\{...}\Default`, `\User`, `\Volatile`, snaží se `reg.exe import` při obnově otvírat/vytvářet podklíče, na které nemá ani účet Správce oprávnění. To se týká i stávající volby `-RestoreBackup` v [scripts/apo-uninstall.ps1](../scripts/apo-uninstall.ps1) (řádky 84–96) a vysvětluje, proč obnova ze zálohy nemusela fungovat.

**Ověřené řešení (nasazené v [scripts/verify-usb-apo.ps1](../scripts/verify-usb-apo.ps1)):**
1. Před úpravou zálohovat přímé hodnoty `FxProperties` (`Name`, `RegistryValueKind`, `Value`, u `Binary` přes Base64) do JSON souboru + ponechat `.reg` jen jako čitelnou referenci.
2. Při obnově otevřít klíč striktně s právy `RegistryRights 'SetValue, QueryValues'`, smazat nově přidané hodnoty (`DeleteValue($name, $false)`) a obnovit původní hodnoty přes `RegistryKey.SetValue($name, $typedVal, $kind)` (pozor na PowerShell 5.1: musí dostat přesné `.NET` typy `[byte[]]` a `[string[]]`, ne `@(...)` `Object[]`!).
3. Ověřeno na všech 6 USB endpointech v `test-usb-apo.ps1` i zpětným `-InspectOnly`: po testu zůstal registr bajt po bajtu totožný s výchozím stavem.

---

## 5. Architektura bezrizikové instalace a návratu (cíl pro produkční úpravu)

Aby uživatel v žádném případě (ani u 1% edge-case ovladačů) nepřišel o zvuk a nemusel přeinstalovávat Windows:

1. **Vrstva 1 — Pre-flight + JSON záloha:**
   - Před zápisem zkontrolovat existenci `FxProperties` a oprávnění `SetValue, QueryValues`.
   - Uložit typovou JSON zálohu hodnot (`apo-backup/<guid>-FxProperties-original.json`, nikdy nepřepisovat existující `-original`), pro zpětnou kompatibilitu volitelně zachovat i `.reg`.
2. **Vrstva 2 — Čistý zápis kompozitního řetězce + `modes`:**
   - Při přidání CLSID do kompozitního slotu (`SFX` `,13`, `MFX` `,14`, `EFX` `,15`) zajistit odpovídající `modes` klíč (`{d3993a3f-...},5` / `,6` / `,7`): pokud chybí nebo je prázdný, zapsat `REG_MULTI_SZ` `@('{C18E2F7E-933D-4965-B7D1-1EEF228D2AF3}')`.
   - **Migrace legacy klíčů**: pokud endpoint používá legacy `REG_SZ` `,5` / `,6` / `,7` (nebo `,1` LFX), přenést všechny stávající ne-GE CLSIDy do kompozitních `,13` / `,14` / `,15` (plus doplnit příslušné `modes,5` / `,6` / `,7`, pokud chyběly) a migrované legacy hodnoty smazat. Zachovají se tak vestavěné efekty Microsoftu/OEM a Windows nezastíní náš `Stream EQ` ani `MFX`.
   - `PKEY_FX_Association` (`,0`) nepřidávat automaticky (u 3 testovaných USB karet nebylo potřeba); případně zkusit až jako volitelný krok pro specifické ovladače, pokud ani `EFX` ani `MFX` nenastartují.
3. **Vrstva 3 — Povinný self-test Capture i Render + okamžitý rollback:**
   - **Capture**: kromě stávajícího `--probe-mic` (`EFX` → fallback `MFX`) kontrolovat navíc EventID `1000` pro `audiodg.exe` po restartu `Audiosrv`. Pokud `--probe-mic` selže při otevření mikrofonu nebo `audiodg.exe` spadne, **okamžitě obnovit zálohu** (nenechat na zařízení konfiguraci, která rozbila otevření zvuku).
   - **Render**: doplnit obdobný 1,5s tichý WASAPI probe (`--probe-render {guid}` v Rustu), který zkontroluje otevření výstupu, `Stream EQ` + `Output EQ` start v EventLogu a absenci pádu `audiodg.exe`. Pokud selže `Output EQ` v `EFX`, zkusit fallback do `MFX` (`,14` + `modes,6`); pokud selže samotné otevření výstupu nebo spadne `audiodg.exe`, **okamžitě obnovit zálohu**.
4. **Vrstva 4 — Nezávislá obnova a uživatelský fallback:**
   - Opravit [scripts/apo-uninstall.ps1](../scripts/apo-uninstall.ps1) (`-RestoreBackup`) na obnovu z `.json` zálohy přes `SetValue, QueryValues` (včetně smazání přidaných `modes` klíčů, které dnešní odinstalátor nemaže).
   - Pokud by uživatel měl problém bez spuštění odinstalátoru, stačí dočasně vypnout **Vylepšení zvuku (Audio enhancements)** v Nastavení Windows u konkrétního zařízení (okamžitý bypass APO bez restartu).

---

## 6. Návod k ověření pro další sessiony a otevřené body

### Jak ověřit nálezy skriptem v tomto repozitáři

- Pouze čtení (bez práv správce, bez restartu zvuku):
  ```powershell
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\verify-usb-apo.ps1 -InspectOnly
  ```
- Pasivní WASAPI test (bez zásahu do registru):
  ```powershell
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\verify-usb-apo.ps1 -BaselineOnly
  ```
- Plný izolovaný experiment na aktivních USB zařízeních (**PowerShell jako Správce**; automaticky zálohuje a vrátí vše do původního stavu):
  ```powershell
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\verify-usb-apo.ps1
  ```

### Co zbývá udělat a ověřit před mergem produkční opravy

1. **Rozsah migrace legacy `,5` / `,6` (Neověřeno izolovaně):** v experimentu `R3` v `test-usb-apo.ps1` se mazaly `,5` i `,6` bez přenesení `{13AB3EBD-...}` do `,14` (nový [scripts/verify-usb-apo.ps1](../scripts/verify-usb-apo.ps1) už přenos do `,14` v `R3` testuje). Ověřit, zda při čisté migraci `,5` -> `,13` a `,6` -> `,14` (+ vymazání `,5` i `,6`) zůstane funkční OEM `Microsoft Audio Home Theater Effects` MFX a zda zůstane bez regresí `Realtek` i `AMD HDMI`.
2. **Test živého průchodu audia se spuštěnou aplikací (Neověřeno):** experiment ověřil start všech APO v `audiodg.exe` bez spuštěného GUI (`0 processed: GamersEqualizer did not drive it`). Po nasazení opravy do `apo-install.ps1` je třeba pustit samotnou aplikaci `gamersequalizer.exe`, změřit `Mic Chain`, `Stream EQ` i `Output EQ` se skutečným EQ/filtrem na `ASUS`, `EPOS` (`16 kHz`) a `Jabra` (`0x0003`) a potvrdit nezkreslený zvuk.
3. **Implementace opravy:**
   - [scripts/apo-install.ps1](../scripts/apo-install.ps1): zápis `modes` (`,5`, `,6`, `,7`), migrace a smazání legacy `,5`, `,6`, `,7`, JSON záloha hodnot, event-log check bez falešných pozitiv při startu `Audiosrv`, rollback při chybě probe nebo pádu `audiodg`.
   - [scripts/apo-uninstall.ps1](../scripts/apo-uninstall.ps1): oprava `-RestoreBackup` přes JSON + `.NET RegistryKey` (`SetValue, QueryValues`) a úklid přidaných hodnot (`modes`).
   - [crates/gamersequalizer-daemon/src/win_audio/mic_probe.rs](../crates/gamersequalizer-daemon/src/win_audio/mic_probe.rs) / [apo_install.rs](../crates/gamersequalizer-daemon/src/win_audio/apo_install.rs): přidat `--probe-render {guid}`, případně do diagnostiky `[ApoCheck]` doplnit i přítomnost `,0` (`PKEY_FX_Association`) a legacy `,5`, `,6`, `,7`.
4. **Drobné vylepšení logování v `gamersequalizer-apo`:** do EventLog zpráv APO (aspoň do `started` / `format rejected`) časem doplnit identifikátor endpointu, aby se při vícero otevřených zařízeních se stejným formátem (`48000 Hz 2ch`) zprávy neduplikovaly mezi Realtekem a USB zařízením na stejném `audiodg` PIDu.
