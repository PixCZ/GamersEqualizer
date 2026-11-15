# Audit kódu a plán refaktoringu (2026-11-15)

Stav: commit `3547694` na `main`. Nic v kódu se nezměnilo, tohle je jen zpráva a postup.

U každého nálezu je uvedeno, jak pevný je:

- **Ověřeno**: přečetl jsem to v kódu sám, případně spustil.
- **Odvozeno z kódu**: plyne ze čtení kódu, ale nikdo to nespustil (workspace se na Macu nepřeloží).
- **Hypotéza**: závisí na chování Windows, je potřeba zkouška na Windows.

## Co aplikace dělá

Aplikace pro Windows, která upravuje zvuk bez virtuální zvukové karty. Skládá se ze dvou částí:
služba s oknem (`gamersequalizer.exe`, egui) a DLL, kterou si Windows načtou do svého zvukového
procesu `audiodg.exe` (tzv. APO, zvukový efekt na zařízení). DLL čistí mikrofon (potlačení ozvěny,
šumu, hlídání hlasitosti) a ekvalizuje výstup; služba jí posílá nastavení přes sdílenou paměť (kus
paměti, který vidí oba procesy).

Předpokládaná zátěž: jeden uživatel, jeden počítač. Proces ale běží celé dny, zařízení se připojují
a odpojují, služba zvuku se restartuje a starší běžící kopie se může potkat s novější.

Velikost: 43 tisíc řádků vlastního Rustu ve čtyřech crate, 5,5 tisíce řádků převzatých (`crates/vendor`),
1 700 řádků PowerShellu v instalátoru, 364 testů.

## Platí „clean code“ i pro Rust?

Ano, jádro je stejné: funkce dělá jednu věc, nic se neopisuje, jména říkají pravdu. V Rustu se ale
velká část nehlídá kázní, nýbrž nástroji: co jde vynutit překladačem nebo lintem (automatická kontrola
kódu), to se nepíše do pravidel pro lidi. Projekt to z velké části už dělá: `unwrap`/`panic` zakazuje
lint, formát řeší `rustfmt`, akce v CI jsou připnuté na SHA, testy jsou u kódu.

Co dnešní praxe přidává a tady chybí:

- **Kontroly běží jen ručně.** `clippy` pouští jen `dev-rebuild.ps1 -Test`, testy v CI jen při tagu
  (`.github/workflows/release.yml`). Commit, který něco rozbije, se pozná až v den vydání. Řešení:
  druhý workflow na každý push se třemi kroky: `cargo fmt --check`, `cargo clippy -- -D warnings`,
  `cargo test`.
- **Pravidlo „každý `unsafe` má komentář SAFETY“ nikdo nehlídá.** V kódu je zhruba 246 bloků `unsafe`
  a 108 komentářů `SAFETY:` (hrubý počet, jeden komentář může krýt víc bloků). Lint
  `clippy::undocumented_unsafe_blocks` to vynutí sám.
- **Edice 2021.** Edice 2024 (Rust 1.85, únor 2025) mimo jiné zakazuje odkazy na `static mut`. Projekt
  má jedno: `HOOK_SENDER` v `win_audio/game_detector.rs:25`. Nahradit `OnceLock`/`Mutex`, pak
  `cargo fix --edition`.
- **Verze překladače není připnutá.** CI bere `stable`, takže nový Rust může v den vydání přinést nová
  varování. Stačí `rust-toolchain.toml` s číslem verze.
- **`#[expect(...)]` místo `#[allow(...)]`** (Rust 1.81): asi 40 výjimek v kódu; `expect` ohlásí, když už
  výjimka není potřeba, takže po refaktoringu nezůstanou viset.
- **Dlouhé testy ve vlastním souboru.** `ai_denoiser.rs` má 1 399 řádků kódu a 2 233 řádků testů,
  `aec3.rs` 629 a 1 033. Běžný postup: `#[cfg(test)] mod tests;` a testy v sousedním `tests.rs`.
  Soubor s kódem je pak třetinový, nic dalšího se nemění.
- **Chyby jako text.** 93 funkcí vrací `Result<_, String>`. To je v pořádku, dokud se text jen zapíše
  a ukáže. Problém je, když se podle textu rozhoduje: `mic_stream.rs:160` porovnává chybu s konstantou
  `NO_VIRTUAL_MIC`. Tam, kde volající větví, patří `enum`; jinde nechat být a nezavádět knihovnu.

Co bych naopak **nedělal**: přepis na `async`, vlastní vrstvu nad COM, jednotný typ chyby přes celý
projekt, dělení funkcí jen kvůli délce. Dlouhá funkce, která dělá jednu věc (kreslí jednu kartu),
je v pořádku.

## Nálezy

### Musí se opravit

1. **Instalátor spadne na jedné vzácné cestě** (`scripts/apo-install.ps1:886`) — *Ověřeno, spuštěno v `pwsh`*
   - **Co to je:** Řádek zjišťuje, jestli klíč v registru vytvořila dřívější instalace, aby ho šlo smazat.
   - **Problém:** `[bool](try { … } catch { … })` není platný výraz; PowerShell hlásí „výraz try nebyl
     rozpoznán“. Nastane přesně v případě, pro který řádek vznikl: klíč zůstal prázdný a existuje záloha.
     Instalace skončí chybou.
   - **Oprava:** `$made = [bool]$(try { … } catch { $false })` a jeden případ do
     `scripts/tests/apo-install.tests.ps1`.
   - **Když to necháme:** Uživatel po nepovedené odinstalaci na takovém zařízení už nenainstaluje.

2. **Starý „most“ mikrofonu běží pořád a bez podmínky** (`win_audio/mic_stream.rs:118-176, 255-293`, spouští `main.rs:195-200`) — *Ověřeno v kódu, slyšitelný dopad nezkoušen*
   - **Co to je:** Pozůstatek z doby před APO: služba sama nahrává mikrofon, prožene ho celým řetězcem
     a hraje do zařízení, jehož jméno obsahuje „vb-audio“, „cable input“, „virtual audio cable“ nebo
     „sonar - microphone“.
   - **Problém:** Kdo má VB-Cable nebo Voicemeeter (jméno obsahuje „vb-audio“), tomu služba posílá
     mikrofon do toho zařízení nepřetržitě. Most navíc zapisuje do sdílené paměti stejný „tep“ jako APO,
     takže kontrola „nainstalováno, ale nic nedělá“ u těchto lidí nikdy nezabere. README o mostu nemluví.
   - **Oprava:** Most smazat. `StreamResources` a `init_client_48k_float` přesunout do `loopback.rs`
     a to spouštět z `main`. Tím zmizí i `PluginMode`, `SpeakerTap`, parametr `sample_rate` (jediný
     volající s jinou frekvencí než 48 kHz) a porovnávání chyb podle textu. Asi −450 řádků.
   - **Když to necháme:** Část testerů slyší vlastní mikrofon a log jim tvrdí, že je vše v pořádku.

3. **Import ze Sonaru přepíše úpravy uživatele** (`sonar_importer/mod.rs:35-55`, tlačítko `ui/tab_eq.rs:307-321`) — *Ověřeno v kódu*
   - **Co to je:** Tlačítko „Import from Sonar DB“ uloží každý profil ze Sonaru pod jeho jménem.
   - **Problém:** Profil stejného jména nahradí bez dotazu. Kdo si upravil „Apex Legends“ a klikne na
     import kvůli novým presetům, o úpravy přijde. Komentář u funkce to přiznává.
   - **Oprava:** Před zápisem zavolat existující `move_to_deleted` na soubor, který se liší (jde vrátit),
     nebo existující přeskočit jako `sync_new`.
   - **Když to necháme:** Tichá ztráta ruční práce uživatele.

4. **Oprava ID zařízení může smazat nastavení jiného zařízení** (`win_audio/device_ids.rs:78-96`) — *Odvozeno z kódu*
   - **Co to je:** Když zařízení dostane od Windows nové ID, služba ho v souborech s nastavením přepíše.
   - **Problém:** Dvě zařízení se stejným jménem („Speakers (USB Audio Device)“), jedno odpojené:
     staré ID se přepíše na ID druhého, soubor má dva stejné klíče a při čtení jeden vypadne.
   - **Oprava:** V `remap` nebrat za cíl zařízení, které už je v `known` pod vlastním ID (zapamatované
     zařízení nemůže být „novým ID“ jiného). Jeden test.
   - **Když to necháme:** Občas zmizí profil sluchátek nebo páru pro odposlech.

5. **Návrat zpět při aktualizaci nevrátí nic** (`scripts/apo-install.ps1:857-889, 1012-1024`) — *Odvozeno z kódu*
   - **Co to je:** Před změnou si instalátor uloží stav efektů, aby se při chybě vrátil.
   - **Problém:** Při aktualizaci ten stav už naše efekty obsahuje. Když nová DLL padá, „návrat“ je tam
     nechá a aplikace zařízení označí za vrácené.
   - **Oprava:** Ze snímku pro návrat vyjmout naše tři CLSID. Test `Resolve-ProbeOpen` s takovým snímkem.
   - **Když to necháme:** Pojistka proti „počítači bez zvuku“ nefunguje právě při aktualizaci.

6. **Po pauze zazní kousek zvuku z doby před pauzou** (`mic-vst/src/aec3.rs:296-301, 360-368`) — *Odvozeno z kódu*
   - **Co to je:** `flush()` má při vypnutí a zapnutí řetězce zahodit rozpracovaný zvuk.
   - **Problém:** Vyprázdní frontu, ale ne vnitřní zpoždění AEC3 (430 vzorků, 9 ms). Po zapnutí vyjde
     nejdřív zvuk z doby před vypnutím. Test (`plugin.rs:1229-1246`) hlídá jen první blok, kde to není.
   - **Oprava:** Po `flush` ztlumit prvních `AEC3_LATENCY_SAMPLES` výstupu, stejně jako to dělá
     `mute_left` v `ai_denoiser.rs`. Test rozšířit na dva bloky po 480.
   - **Když to necháme:** Krátké lupnutí nebo slabika navíc při přepnutí.

### Mělo by se opravit

7. **Volba „(žádný)“ u rozpoznané hry znamená „není hra“** (`ui/tab_eq.rs:162-167`) — *Ověřeno v kódu*
   - **Co to je:** Výběr profilu pro právě rozpoznanou hru.
   - **Problém:** `choice.unwrap_or_default()` uloží prázdný text, což je vazba „Not a game“
     (`tab_eq.rs:232`). Hra pak dostane záložní profil a její zvuk přejde do kanálu Média.
   - **Oprava:** Předat volbu rovnou: `eq.bind_exe(&exe, choice, shm)`. `None` vazbu zruší.
   - **Když to necháme:** Uživatel chce jen odebrat profil a přeřadí si hru jinam.

8. **Starší verze zahodí data novější verze** (`output_eq/settings.rs:11-13`, `ui/audio_settings.rs:19-21`, `common/profile.rs:4-13, 72-83`, `sonar_importer/storage.rs:86-101`) — *Odvozeno z kódu*
   - **Co to je:** Soubory s nastavením a profily (JSON).
   - **Problém:** Neznámá pole se při uložení starší verzí ztratí. `FilterType` a `VadType` nemají
     záložní hodnotu: profil z novější verze se přeskočí celý, a protože není v paměti, nový profil
     stejného jména ho bez varování přepíše.
   - **Oprava:** `#[serde(flatten)] extra: serde_json::Map` do těch struktur, varianta `Unknown(String)`
     u obou výčtů (hodnota musí přežít uložení), a kontrola jména i proti souboru na disku.
   - **Když to necháme:** Návrat ke starší verzi (tester s dvěma ZIPy) tiše poškodí nastavení.

9. **Verze sdílené paměti se hlídá ručně na třech místech** (`common/src/shm.rs:11-17, 517-660, 681-707`) — *Ověřeno v kódu*
   - **Co to je:** Rozložení paměti, kterou čte služba i DLL; mohou být z různých sestavení.
   - **Problém:** `SHM_NAME`, `APO_SHM_NAME` a `SHM_VERSION` se musí změnit zároveň. Pole `version` se
     zapisuje, ale nikdo ho nečte, a velikost se nekontroluje. Navíc `impl Default` (142 řádků) opisuje
     `init_defaults` a už se rozešel: úrovně −1200 jsou jen v `Default`, který používají jen testy.
   - **Oprava:** Obě jména složit `concat!` z jednoho čísla, přidat `const _: () = assert!(size_of::<ShmLayout>() == N)`,
     při otevření zkontrolovat `version`. `Default` = vynulovaná paměť + `init_defaults()`. −140 řádků.
   - **Když to necháme:** Jedno zapomenuté číslo a stará DLL s novou službou čtou pole na jiných místech.

10. **Každý začátek zvuku projde až 100 ms bez zpracování** (`common/src/shm.rs:1014-1027`, použití `apo/src/lib.rs:952, 1073, 1310, 1415`) — *Ověřeno v kódu, dopad neměřen*
    - **Co to je:** `OwnerWatch` hlídá, jestli služba žije (tep každých 100 ms).
    - **Problém:** Nový procesor začíná ve stavu „nežije“, dokud neuvidí změnu tepu. Do té doby jde do
      aplikací syrový mikrofon a výstup hraje bez EQ; s předzesílením −6 dB je to skok hlasitosti.
    - **Oprava:** Poslední tep a čas jeho změny držet jednou pro celý proces; nový `OwnerWatch` z nich začne.
    - **Když to necháme:** Začátek každého hovoru a každé skladby zní krátce jinak.

11. **Povýšený skript překládá C# ve složce, kam smí zapisovat uživatel** (`scripts/apo-install.ps1:324`, `scripts/apo-uninstall.ps1:228`) — *Hypotéza*
    - **Co to je:** `Add-Type` v PowerShellu 5.1 přeloží kód do `%TEMP%` a výslednou DLL načte.
    - **Problém:** `%TEMP%` patří uživateli. Program běžící pod stejným účtem může soubor vyměnit
      a spustit kód jako správce. Proti stejnému útočníkovi už skript chrání kontrola SHA-256.
    - **Oprava:** V `elevated_bootstrap` (`apo_install.rs:611`) nastavit `$env:TEMP`/`$env:TMP` na složku
      jen pro správce.
    - **Když to necháme:** Kontrola otisku skriptu má vedle sebe otevřená zadní vrátka.

12. **Odpojené zařízení a tiše ukončená vlákna** (`main.rs:303-315`, `win_audio/loopback.rs:45-51, 63-70`, `win_audio/sidetone.rs:484-490`) — *Odvozeno z kódu*
    - **Co to je:** Služba sleduje hlavní výstup („Master“) a dvě vlákna na pozadí.
    - **Problém:** Master se znovu hledá jen při změně výchozího zařízení. Po odpojení náhlavní soupravy
      drží mrtvé ID a `loopback` zapisuje chybu každou vteřinu; log se zkracuje jen při startu. A když
      se vláknu nepodaří otevřít sdílenou paměť, skončí navždy; `loopback` přitom nese spojení s APO.
    - **Oprava:** Master hledat i při `devices_changed`, chybu v `loopback` opakovat jen při změně (jako
      `mic_stream`), u obou vláken smyčka s čekáním místo `return`.
    - **Když to necháme:** Log roste celé dny a po jedné chybě APO běží naprázdno do restartu.

13. **Varování jmenuje jiné zařízení, než které kontroluje** (`health.rs:500-505`) — *Ověřeno v kódu*
    - **Co to je:** Upozornění „EQ výstupu není nastaven na …“.
    - **Problém:** Stav čte z Master, jméno a klíč bere z výchozího výstupu Windows. Se Sonarem jako
      výchozím napíše „… na SteelSeries Sonar - Gaming“ a „Ignorovat“ uloží špatné ID.
    - **Oprava:** Použít `snap.output`.
    - **Když to necháme:** Uživatel opravuje zařízení, které v pořádku je.

14. **Ukončení z nastavení přeskočí ukládání** (`ui/tab_settings.rs:137, 307`, `ui/app.rs:80-85`, `ui/tab_mic.rs:744-748`, `ui/tab_sidetone.rs:467-470`) — *Ověřeno v kódu (`process::exit`), zbytek odvozen*
    - **Co to je:** Tři různá místa „ulož po puštění myši“ a dvě volání `process::exit(0)`.
    - **Problém:** `exit` obejde `on_exit`, takže se neuloží poloha okna ani rozdělané úpravy. Uložení
      mikrofonu a odposlechu proběhne, jen když je jejich karta zrovna vykreslená.
    - **Oprava:** Jedno `pending_save` vyřízené v `app.rs` a v `on_exit`; `exit` nahradit zavřením okna.
    - **Když to necháme:** Občas se „neuloží, co jsem nastavil“ a nejde to zopakovat.

15. **Pozůstatky starších návrhů** — *Ověřeno v kódu (hledáno volání v celém repu)*
    - **Co to je:** Kód a jména z verzí, které už neexistují.
    - **Problém:** Čtenář neví, co platí. Seznam:
      - `daemon/src/dsp_apo/apo_writer.rs`: zapisovač konfigurace pro Equalizer APO, nikdo ho nevolá.
        `auto_preamp` přesunout do `output_eq`, složku `dsp_apo/` smazat.
      - Telemetrie po starém potlačení ozvěny (MDF): `aec3.rs:43, 206-208, 319-327` vrací stále stejné
        hodnoty, okno pro ně drží časovač (`ui/state.rs:40-47, 511-528`).
      - Pokusy `whisper_probe` a `whisper_sweep` (`ai_denoiser.rs:2918-3346`, asi 430 řádků) odkazují na
        `docs/whisper-step1-results.md`, který už není.
      - Jména z prvního prototypu APO: `CLSID_GAIN_APO`, `GainApoFactory`, popis „(gain only)“.
      - Crate `gamersequalizer-mic-vst`, `GamersEqualizerPlugin`: VST to už není. Přejmenování sahá na
        20 míst; pozor na filtr logu `gamersequalizer_mic_vst=debug` v `app_log.rs:24`.
      - Nepoužité: `applied_mica`, `get_erle_db`, `set_output_device`, `ShmRingBuffer::header_mut`,
        `SidetoneChain::gate_gain_db`, jednopásmový režim expandéru, složka profilů `aux` (ve Windows
        vyhrazené jméno, uložení vždy selže).
    - **Oprava:** Smazat. Pole ve sdílené paměti odebrat najednou při jedné změně verze (viz 9).
    - **Když to necháme:** Každá další úprava se musí vyhýbat kódu, který nic nedělá.

16. **Kopie, které se musí měnit společně** — *Odvozeno z kódu*
    - **Co to je:** Stejná logika opsaná vícekrát.
    - **Problém:** Přidání kanálu mixéru znamená zásah na 8 místech, přidání vrstvy EQ 12 funkcí:
      - Mixér: 5 kanálů × 4 hodnoty opsané v `chatmix.rs:329-628`; zápis do sdílené paměti čtyřikrát
        (`main.rs:179-188, 274-280, 327-338`, `ui/state.rs:332`).
      - `OutputEq`: čtení, zápis a mazání zvlášť pro každou ze čtyř vrstev (`output_eq/mod.rs:156-493`).
      - Prolínání EQ v APO dvakrát (`apo/src/lib.rs:965-991` a `1084-1135`).
      - Ovladač hlasitosti v procentech třikrát (`tab_mic.rs:217-233`, `tab_sidetone.rs:219-235`,
        `tab_mixer.rs:443-447`), proti vlastnímu pravidlu projektu.
      - Skládání rámců čtyřikrát (`ai_denoiser.rs:1058-1208`, `dpdfnet.rs:61-87`).
      - Vytvoření výčtu zařízení přes COM asi patnáctkrát (`chatmix`, `loopback`, `stream_router`, …).
    - **Oprava:** `Channel` s metodami a jedno `write_mixer`; `ProfileStorage::dir_for(layer)`; `EqFade`;
      `theme::percent_volume`; `frame_fifo.rs`; `mmdevice.rs`. Asi −400 řádků.
    - **Když to necháme:** Chyby typu „v jednom kanálu to funguje, v druhém ne“.

17. **Instalátor a odinstalátor opisují jeden druhého a liší se od Rustu** (`scripts/apo-uninstall.ps1:48-255`, `apo-install.ps1:549`, `apo_install.rs:72`) — *Ověřeno v kódu*
    - **Co to je:** Asi 200 řádků pomocných funkcí je v obou skriptech.
    - **Problém:** Rust bere zařízení jako aktivní podle `DeviceState & 0xF`, PowerShell podle `-eq 1`.
      Zařízení s vyššími bity aplikace nabídne a skript pak celou instalaci zastaví.
    - **Oprava:** `-band 0xF`. Společný začátek do `apo-common.ps1`, který Rust vloží do textu před
      výpočtem otisku. Test, že skript obsahuje všechna CLSID a jména hodnot z konstant v Rustu.
    - **Když to necháme:** Oprava v jednom skriptu se do druhého nedostane.

18. **`AppState` dělá všechno a staví si ho i služba** (`ui/state.rs`, `main.rs:191`) — *Ověřeno v kódu*
    - **Co to je:** Jedna struktura se 40 poli: vlastní sdílenou paměť, telemetrii mikrofonu, EQ, profily
      i stav každé karty.
    - **Problém:** Služba bez okna ji vytváří celou, takže při startu dělá i práci okna (hledání presetů,
      čtení registru, synchronizaci se Sonarem). Kdo chce změnit práci se sdílenou pamětí, edituje soubor
      v `ui/`.
    - **Oprava:** Vlastnictví sdílené paměti vyjmout do `shm_mapping.rs` mimo `ui/`; služba potřebuje jen
      to a `OutputEq`. Celé dělení souborů je níže.
    - **Když to necháme:** Každá nová karta dál zvětšuje strukturu, kterou sdílí dva režimy běhu.

19. **Řetězec mikrofonu: pět různých „resetů“ a nastavení čtené třikrát** (`ai_denoiser.rs:817-857, 890-907, 985-995, 1352`; `plugin.rs:351-391, 506-512, 571-581`) — *Odvozeno z kódu*
    - **Co to je:** `reset`, `flush`, `reset_engines`, `reset_timing` a `start_takeover` čistí každý jinou
      část stavu.
    - **Problém:** Nové pole stavu se musí zkontrolovat proti všem pěti. Už teď `flush` nečistí
      `rescue.net.fifo` a `reset_engines` nechá `mute_left`.
    - **Oprava:** Stav spánku a převzetí do jedné struktury s jedním `clear()`; jedno `MicSettings::read(header)`.
    - **Když to necháme:** Chyby „po přepnutí modelu to chvíli zní divně“, které se špatně hledají.

20. **Pravidla pro agenty popisují jinou aplikaci** (`AGENTS.md` část Project a Structure, `.claude/rules/architecture.md`, `code-style.md`, `testing.md`) — *Ověřeno*
    - **Co to je:** Soubory, které si každý agent načte na začátku práce.
    - **Problém:** Mluví o Tauri, Reactu, SQLite, `AnyhowResult`, `tokio` a „jednom příkazu na soubor“.
      Nic z toho v repu není a `SPECIFICATION.md` Tauri výslovně zakazuje. Zakazují také `unsafe`, přitom
      `Cargo.toml` vysvětluje, proč to nejde.
    - **Oprava:** Přepsat podle skutečnosti: egui, dva procesy, sdílená paměť, pravidla pro zvukové
      vlákno (žádná alokace, zámek ani log na blok), `Result<_, String>` tam, kde se chyba jen zapíše.
    - **Když to necháme:** Agent se řídí pravidly, která kódu odporují, a refaktoring podle nich by škodil.

### Menší nálezy (bez rozboru)

Dalších 23 drobností, které se vyplatí vzít cestou, když se na dané místo sahá:

- K ověření na Windows: APO výstupu se nespustí na „tichém“ bloku (`apo/src/lib.rs:1758-1763`), takže
  odposlech nemusí hrát, když nic jiného nehraje. Zkouška: `sidetone_probe` bez jiného zvuku.
- Vstupní body COM v APO nejsou obalené `catch_unwind` (`apo/src/lib.rs:385, 419, 594, 718`); pád
  v nich ukončí `audiodg.exe`.
- Okno se může otevřít mimo obrazovku při měřítku 150 % (`ui/window_state.rs:50-63`): poloha je
  v bodech, kontrola v pixelech.
- Průvodce AutoEQ hlásí úspěch, i když uložení selhalo (`tab_headphones.rs:461-471`).
- Čtení vybraného souboru bez limitu velikosti (`tab_headphones.rs:578`, `profile_bar.rs:406-419`).
- „Reset“ rozpoznané hry z okna, které není vlastníkem, rozejde okno a zvuk (`output_eq/mod.rs:600-606`).
- Rozpoznání hry čte na vlákně okna i `appinfo.vdf` ze Steamu, často přes 100 MB (`output_eq/mod.rs:523-534`).
- Karta odposlechu volá službu zvuku při každém snímku (`tab_sidetone.rs:28-33`).
- Zbytečná práce při každém snímku: `bus_result` dvakrát, `apply_mix` dvakrát, kopie logu 256 kB.
- `warn!` při každém nepovedeném rámci AEC3 na zvukovém vlákně (`aec3.rs:354-358`), 100 řádků za vteřinu.
- `leveler_seed_blocked` se nikdy nevynuluje (`plugin.rs:67-68, 500-523`).
- `capture_time_hns` čte pole V2 přes odkaz na V1 (`apo/src/lib.rs:699, 1775-1778`).
- Pásma EQ se mohou jeden blok přečíst napůl stará (`apo/src/lib.rs:1075-1081`).
- Alokace na zvukovém vlákně při změně EQ (`apo/src/lib.rs:1002, 1078, 1420`).
- Vlastník slotu streamu se na zvukovém vlákně nekontroluje (`apo/src/lib.rs:931-944`).
- Telemetrie výstupu: poslední zápis vyhrává mezi zařízeními (`apo/src/lib.rs:1069-1071`).
- `daemon_running` používá `CreateMutexW` místo `OpenMutexW` (`main.rs:612`).
- `find_physical_microphone` při každém volání neuvolní text z `GetId` (`chatmix.rs:261`).
- `autostart.rs:49`: `?` vrátí před `RegCloseKey`.
- Dva testy nic netvrdí (`endpoint_lock`, autostart), dva testy CLSID jsou stejné.
- Test překladů má ruční seznam souborů a chybí v něm `genre.rs`; dva české klíče se nepoužívají.
- `unsafe` bez SAFETY v `mic-vst/src/shared_memory.rs:58-68, 87-92`; `layout_mut` má být soukromé.
- `AGENTS.md` říká, že záloha efektů se uloží „jednou“; kód ji přepisuje (`apo-install.ps1:891`).

## Navržené dělení souborů

Dělí se podle práce, ne podle délky. Každý bod je čistý přesun bez změny chování.

- **`apo/src/lib.rs`** (1 858 řádků kódu)
  - `com.rs`: objekt COM, tabulky funkcí, továrna, exporty DLL.
  - `entry.rs`: funkce `apo_*`/`rt_*`/`cfg_*`, `ApoState`, kontrola formátu.
  - `mic.rs`, `output.rs`, `stream.rs`: tři procesory; `output` a `stream` sdílí `EqFade`.
  - `sidetone_tap.rs`, `init_block.rs`, `notes.rs`, `clock.rs`.
- **`common/src/shm.rs`** → `shm/header.rs`, `shm/eq.rs`, `shm/mirror.rs`, `shm/reference_ring.rs`, `shm/owner.rs`.
- **`common/src/sidetone.rs`** → `sidetone/pairs.rs` (soubor služby, nic pro zvukové vlákno),
  `sidetone/ring.rs`, `sidetone/expander.rs`, `sidetone/mod.rs`.
- **`mic-vst/src/ai_denoiser.rs`** → složka `denoise/`: `mod.rs` (`AiVoiceIsolator`), `loader.rs`,
  `rnnoise.rs`, `deepfilter.rs`, `voice.rs`, `gate.rs`, `rescue.rs`, `sleep.rs`, testy zvlášť.
- **`mic-vst/src/plugin.rs`**: řetězec zůstává; ven jde `chain_settings.rs`, `chain_telemetry.rs`,
  `chain_capture.rs`.
- **`mic-vst/src/aec3.rs`**: nechat, jen `EchoMeter` do `echo_meter.rs` a testy zvlášť.
- **`daemon/src/main.rs`** → `daemon.rs` (smyčka služby rozdělená na `follow_mic`, `follow_output`,
  `sync_mixer`), `lifecycle.rs` (předání, import, mutex); v `main.rs` zůstane rozbor argumentů a okno.
- **`daemon/src/health.rs`** → `health/issue.rs` (texty), `health/apo_diag.rs` (`[ApoCheck]`, `[ApoNote]`);
  jádro zůstává.
- **`win_audio/apo_install.rs`** → `elevated.rs` (příprava skriptu, otisk, spuštění: bezpečnostní jádro),
  `registry.rs`, `location.rs`, `mod.rs`.
- **`win_audio/chatmix.rs`**: seznamy programů do `app_channels.rs`, hlasitosti přes COM do
  `endpoint_volume.rs`.
- **`ui/state.rs`** → `shm_mapping.rs` (mimo `ui/`), `ui/mic_status.rs`; práce s `mic_active.json` do
  `mic_presets.rs`.
- **`output_eq/mod.rs`** → `profiles.rs`, `detection.rs`, `publish.rs`.
- **`ui/tab_headphones.rs`**: průvodce AutoEQ (řádky 198-658) do `ui/autoeq_wizard.rs`.
- **Nechat být:** `tab_mic.rs`, `tab_mixer.rs`, `ui/app.rs`, `apo-install.ps1` (jeden text s jedním
  otiskem), `streams.rs`, `biquad.rs`, `rate_bridge.rs`.

## Postup

Podrobný postup krok za krokem je v `docs/refactoring-guide.md`; kde se pořadí liší, platí návod
(brána kvality jde první). Tady je jen přehled.

Každý krok je vlastní commit a končí `dev-rebuild.ps1 -Test` na Windows. Kroky na sobě závisí jen
tam, kde je to napsáno.

1. **Chyby (nálezy 1, 3, 4, 7, 13).** Malé opravy, každá s testem, který před opravou selže.
2. **Pravidla a hlídání (nález 20, oddíl o moderním Rustu).** Přepsat `AGENTS.md` a `.claude/rules/`,
   přidat workflow na push a `rust-toolchain.toml`. Dělá se brzy, aby další kroky hlídalo CI.
3. **Mazání (nálezy 2, 15).** Most mikrofonu, `dsp_apo`, pokusy, stará jména. Přejmenování crate
   jedním commitem. Největší zisk čitelnosti za nejmenší riziko.
4. **Sdílená paměť jednou změnou verze (nálezy 9, 10, pole z 15).** Všechny změny rozložení naráz,
   jedno zvýšení verze.
5. **Instalátor (nálezy 5, 11, 17).** Dotýká se práv správce a restartu zvuku, takže po něm ruční
   zkouška instalace, aktualizace a odinstalace.
6. **Zvukový řetězec (nálezy 6, 19).** Po úpravě nahrávka přes `-DebugCapture` pro porovnání.
7. **Dělení souborů (oddíl výše, nález 18).** Jeden soubor na commit, jen přesuny.
8. **Slučování kopií (nálezy 14, 16), ukládání a dopředná kompatibilita (nález 8).**
9. **Edice 2024 a lint `undocumented_unsafe_blocks`.** Až nakonec, protože doplnění komentářů SAFETY
   projde přes všechny soubory a kolidovalo by s přesuny.

## Závěr

- **Verdikt:** Kód je v dobrém stavu na to, jak rychle vznikl: lint proti pádům, hodně testů, promyšlený
  instalátor. Nejdřív opravit nálezy 1 až 6, pak mazat. Dělení souborů je až třetí v pořadí.
- **Úspora:** asi −2 000 řádků, žádná závislost navíc ani méně.
- **Nezkontrolováno:**
  - Nic se nepřekládalo ani nespouštělo kromě řádku PowerShellu z nálezu 1.
  - Nečtené soubory: `chain_log.rs`, `leveler_log.rs`, `mic_gain.rs`, `dolby_atmos.rs`, `app_channels.rs`,
    `aec_delay_cache.rs`, `dsp_watch.rs`, `policy_config.rs`, `notify.rs`, `console_owner.rs`, `theme.rs`
    (jen hlavičky funkcí), `winsqlite.rs`, těla `game_match.rs`, `launchers.rs` a parserů v `genre.rs`.
  - Jen zčásti: `tray.rs`, `win_audio/sidetone.rs`, `stream_router.rs`, `apo_link.rs`, těla dlouhých testů
    v `ai_denoiser.rs` a `aec3.rs`, příklady v `apo/examples`, `package.ps1`, `dev-rebuild.ps1`.
  - `crates/vendor/` (převzatý kód) vůbec.
