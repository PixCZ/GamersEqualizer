# APO na Windows 10: analýza a stav

Otázka: jde aplikaci nainstalovat i na Windows 10, kde je „méně možností registrace APO“?

Krátce: **ano, od Windows 10 verze 1803 (build 17134) stejnou cestou jako na Windows 11.** Instalátor to
od větve `feat/win10-apo` dovolí, na starších Windows skončí dřív, než cokoli změní. Na skutečném
Windows 10 to zatím nikdo nezkoušel.

## Zjištěno

| Tvrzení                                                              | Zdroj                                                                                                                                         |
| -------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------- |
| Kompozitní seznamy `,13` / `,14` / `,15` (víc efektů v jednom slotu) fungují od Windows 10 verze 1803. | Dokumentace Microsoftu k `PKEY_CompositeFX_*` („In Windows 10, version 1803 and later“). Původní verze tohoto dokumentu tvrdila „až Windows 11“, to byla chyba. |
| Starší Windows 10 (před 1803) mají jen jeden efekt na slot (`,5` / `,6` / `,7`). | Tamtéž, `PKEY_FX_*` „Windows 8.1 and later“.                                                                                       |
| Naše DLL nepoužívá nic, co je jen ve Windows 11.                      | [lib.rs](../crates/gamersequalizer-apo/src/lib.rs): jen `IAudioProcessingObject`, `...RT`, `...Configuration`, `IAudioSystemEffects` (v1), `APOInitSystemEffects2`. |
| Legacy hodnoty zastíní kompozitní seznamy.                            | Naměřeno na Windows 11 ([usb-apo-compatibility-report.md](usb-apo-compatibility-report.md), `R2` vs `R3`). Equalizer APO na tom staví i na Windows 10 (zapisuje jen legacy hodnoty). |
| Ovladač jen se starými efekty LFX `,1` / GFX `,2` je zvláštní režim.  | Equalizer APO (`DeviceAPOInfo.cpp`, „only use LFX/GFX if the audio driver supplied only those APOs“).                                          |
| Aplikace sama Windows 11 nepotřebuje.                                 | [theme.rs](../crates/gamersequalizer-daemon/src/ui/theme.rs): chybějící písma Windows 11 nahradí vestavěná.                                    |

## Co je hotové (větev `feat/win10-apo`)

1. **Kontrola verze Windows**: build pod 17134 → aplikace instalátor nespustí (žádný dotaz správce) a
   skript sám skončí hned na začátku. Nic se nezmění.
2. **Předběžná kontrola každého zařízení** (`Get-EndpointBlocker` v
   [apo-install.ps1](../scripts/apo-install.ps1)): když ovladač používá jen staré efekty LFX/GFX a má GFX,
   instalace by ho vyřadila. Takové zařízení instalátor přeskočí, nesáhne na něj a žlutě napíše
   `Skipped <zařízení>: ... It was not changed.` Ostatní zařízení nainstaluje. Když nezbude žádné, skončí
   a nezmění nic. Hláška jde i do logu aplikace (`installer: ...`) a okno zůstane otevřené.
3. **Mikrofon po migraci**: když instalátor přesunul efekty výrobce do kompozitních seznamů a náš efekt
   se nespustí v EFX ani v MFX, vrátí registr toho mikrofonu do původního stavu. Dřív nechal náš efekt
   na místě. Bez migrace se chování nemění.
4. **Test pravidla**: `scripts/tests/apo-install.tests.ps1` (9 případů, běží i přes `pwsh` na Macu, součást
   `dev-rebuild.ps1 -Test`).

Zbytek ochrany už ve větvi `diag/usb-apo-compatibility` byl: kontrola práv k registru a snímek
`FxProperties` před změnou, po instalaci zkušební otevření zařízení, návrat změn při pádu `audiodg.exe`,
při neotevřeném zařízení a u výstupu i tehdy, když se nespustí žádný z našich efektů.

## Co se neví (ověřit na Windows 10)

- **Nepodepsaná DLL v `audiodg.exe`.** Equalizer APO si vždy zapíná `DisableProtectedAudioDG = 1`, my ne
  (na Windows 11 to potřeba není). Jestli to Windows 10 vyžaduje, se z dostupných zdrojů nedá zjistit.
  Hodnotu loguje `[ApoCheck]` (`describe_system`). Kdyby byla potřeba, zkušební otevření selže a instalátor
  změny vrátí. Zapínat ji automaticky nechci: vypíná chráněnou zvukovou cestu pro celý systém (DRM).
- **Jeden vadný efekt umí umlčet celé zařízení.** Když Windows 10 některý efekt v řetězci nenačte, zvukový
  engine stream vůbec nesestaví. Přesně to hlídá zkušební otevření s návratem změn.
- **Starší ovladače** (Realtek HDA bez DCH, starší USB zvukovky) mají legacy hodnoty častěji než stroje
  s Windows 11, takže migrace do kompozitních seznamů poběží častěji než dosud.

## Postup testu na Windows 10

1. Stroj nebo VM s Windows 10 22H2 (VM stačí na výstup, mikrofon chce skutečné zařízení nebo USB passthrough).
2. Před instalací exportovat `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio` (`reg export`).
3. Nainstalovat z aplikace. Čekaný výsledek: zelené „Works: ...“ u každého zařízení.
4. Z logu poslat řádky `[ApoCheck]` (build Windows, `DisableProtectedAudioDG`, sloty) a `[ApoNote]`
   (`installer: ...`, `stream ... started`).
5. Odinstalovat z aplikace a porovnat `FxProperties` s exportem: efekty výrobce musí být zpátky
   (po migraci v kompozitních seznamech místo legacy hodnot, to je v pořádku).
6. Pokud instalátor skončí červeně, v logu je důvod. Zařízení má být ve stavu před instalací.

## Záchrana, kdyby něco nehrálo

1. Aplikace → Nastavení → „Odinstalovat z Windows“, nebo `apo-uninstall.ps1` jako správce.
2. Bez správce a hned: Nastavení Windows → Zvuk → zařízení → vypnout **Vylepšení zvuku**.
3. Krajní možnost: Obnovení systému nebo přeinstalace ovladače zvukovky.
