# Návod: jak provést opravy a refaktoring

Navazuje na `docs/refactoring-audit.md` (nálezy 1 až 20 a menší nálezy). Audit říká **co a proč**,
tento návod říká **jak a v jakém pořadí**. Čísla nálezů odkazují do auditu; cesty a řádky jsou tam.

Kdo to dělá: agent na vývojovém počítači s Windows, kde jde průběžně překládat a pouštět testy.
Kroky označené **Ruční krok** potřebují člověka (práva správce, poslech, nastavení GitHubu).

Co už je hotové (větev `refactoring-plan`): `AGENTS.md` a `.claude/rules/` popisují skutečnou
aplikaci. Před prací si je přečti, pravidla pro zvukové vlákno a sdílenou paměť jsou tam.

## Pravidla práce

1. **Jeden krok, jeden commit.** Krok končí průchodem brány (`scripts/check.ps1`, vznikne v kroku A4;
   do té doby `scripts/dev-rebuild.ps1 -Test`).
2. **Přesun a změna chování nikdy v jednom commitu.** Přesun kódu má být v diffu vidět jako přesun.
3. **Nález „Odvozeno z kódu“ nebo „Hypotéza“ nejdřív potvrď.** Napiš test, který selže, nebo chybu
   předveď. Když to nejde, neopravuj naslepo: zapiš to do tabulky „Nepotvrzené nálezy“ dole a jdi dál.
4. **Před smazáním hledej v celém repu**, i ve `scripts/`, `docs/`, `i18n_cs.rs`, příkladech a testech.
5. **Na `crates/vendor/` nesahej.**
6. **Dokumentaci měň ve stejném commitu** jako kód: `AGENTS.md`, `.claude/rules/`, `README.md`, když
   se mění něco, co uživatel vidí.
7. **Nic navíc.** Žádné nové vrstvy, knihovny ani volby, které krok nepotřebuje. Když krok jde udělat
   smazáním kódu, je to lepší než přidáním.
8. Hotový krok odškrtni v seznamu na konci a commitni to s ním.

## Pořadí a proč

| Fáze | Co                                    | Proč tady                                                         |
| ---- | ------------------------------------- | ----------------------------------------------------------------- |
| A    | Brána kvality: verze, formát, CI      | Priorita. Všechno další už hlídá automat, ne paměť.               |
| B    | Šest malých oprav s testy             | Malé, nezávislé, uživatel je cítí.                                |
| C    | Mazání pozůstatků, přejmenování       | Ubude asi 1 500 řádků, na které by se jinak psaly komentáře.      |
| D    | Edice 2024, lint SAFETY, dělení testů | Priorita. Po mazání, aby se nedokumentoval kód určený ke smazání. |
| E    | Sdílená paměť, jedna změna verze      | Všechny změny rozložení naráz.                                    |
| F    | Instalátor                            | Práva správce, potřebuje ruční zkoušku.                           |
| G    | Zvukový řetězec                       | Potřebuje poslech nebo nahrávku.                                  |
| H    | Dělení souborů podle práce            | Jen přesuny; snadné, když už je kód čistý.                        |
| I    | Slučování kopií, ukládání, formát dat | Změny chování nad už rozdělenými soubory.                         |
| J    | Menší nálezy                          | Cestou.                                                           |
| K    | Úklid dokumentů, DEVLOG               | Nakonec.                                                          |

Fáze B a C jsou krátké. Kdyby bylo potřeba mít edici 2024 dřív, jde D udělat hned po C1 až C3; na
B nezávisí.

---

## Fáze A: brána kvality

Cíl: jedna definice „je to v pořádku“, kterou pouští vývojář, agent i GitHub při každém pushi.

### A1. Připnout verzi překladače

- Na vývojovém PC `rustup update stable`, pak `rustc --version`. Tu verzi zapiš; číslo si nevymýšlej.
- Nový soubor `rust-toolchain.toml` v kořeni:
  ```toml
  [toolchain]
  channel = "<verze, např. 1.99.1>"
  components = ["clippy", "rustfmt"]
  ```
- `.github/workflows/release.yml`: v úlohách `test` a `build` nahraď krok `dtolnay/rust-toolchain`
  krokem, který vezme verzi ze souboru (o jednu cizí akci méně v úloze, která vede k podpisu):
  ```yaml
  - name: Toolchain from rust-toolchain.toml
    run: |
      rustup toolchain install
      rustc --version
  ```
  `rustup toolchain install` bez argumentů umí rustup od verze 1.28. Kdyby na runneru neprošel, nech
  původní akci; připnutou verzi pak doinstaluje první volání `cargo`. V logu musí být vidět správná verze.
- `AGENTS.md`, oddíl „Before a release“: přidej bod „verze v `rust-toolchain.toml`“ k tomu, co se před
  vydáním nabízí ke kontrole.
- **Ruční krok:** spustit workflow Release ručně (Actions > Release > Run workflow; nevydává, jen
  nahraje ZIP) a zkontrolovat verzi v logu.
- Hotovo, když: `cargo --version` v repu hlásí připnutou verzi a ruční běh Release projde.

### A2. Formát

- `cargo fmt` (bez `--all`: to by přeformátovalo i `crates/vendor/`), samostatný commit „Format with
  rustfmt, no code change“. Na Macu `cargo fmt -- --check` hlásí rozdíly asi ve dvaceti souborech
  (`apo/src/lib.rs`, `daemon/src/main.rs`, `health.rs`, příklady v `apo/examples`, …).
- Hash toho commitu zapiš do nového souboru `.git-blame-ignore-revs` (jeden řádek), ať `git blame`
  hromadné formátování přeskakuje.
- Hotovo, když: `cargo fmt -- --check` nic nevypíše.

### A3. Clippy na nulu

- Změř: `cargo clippy --workspace --all-targets` a totéž s
  `-p gamersequalizer-apo --features debug-capture` (kód za `cfg(feature)` se jinak nekontroluje vůbec).
- Varování oprav. Většina bude `indexing_slicing`:
  - data zvenčí (soubor, registr, sdílená paměť, zvukový blok): `.get(i)` a ošetřit `None`;
  - index dokazatelně v mezích (pevné pole a konstanta): `#[expect(clippy::indexing_slicing, reason = "...")]`
    co nejblíž místu.
- Pak v kořenovém `Cargo.toml` změň `indexing_slicing = "warn"` na `"deny"` a oprav blok lintů
  v `.claude/rules/code-style.md`.
- Když je varování víc než asi 150, jdi po crate (`common`, `mic-vst`, `apo`, `daemon`), commit na
  crate. Krok není hotový, dokud nezmizí poslední.
- Hotovo, když: `cargo clippy --workspace --all-targets -- -D warnings` projde, i s `debug-capture`.

### A4. Jeden skript brány, CI na každý push

- Nový `scripts/check.ps1`. Dělá právě toto a při první chybě skončí nenulovým kódem:
  1. `cargo fmt -- --check`
  2. `cargo clippy --workspace --all-targets -- -D warnings`
  3. `cargo clippy -p gamersequalizer-apo --all-targets --features debug-capture -- -D warnings`
  4. `cargo test --workspace`
  5. `scripts/tests/apo-install.tests.ps1`
  6. `cargo test --release --manifest-path crates/vendor/rnnoise2/Cargo.toml`
  7. `cargo test --release --manifest-path crates/vendor/silero_vad/Cargo.toml`
  8. `cargo clippy --manifest-path crates/vendor/rnnoise2/Cargo.toml --all-targets` (jako dnes: selže
     jen při chybě, varování v převzatém kódu nepočítá)
- Kroky 4 až 8 a clippy dnes dělá `scripts/dev-rebuild.ps1` v části `-Test` (řádky 74-93). Přesuň je
  odtud, `-Test` ať jen zavolá `check.ps1`. Vezmi s sebou i ošetření z řádků 83-85: Windows PowerShell
  dělá z výstupu na stderr chybové záznamy, s `'Stop'` by první řádek clippy krok ukončil.
- Nový `.github/workflows/ci.yml`. Akce připnuté na stejná SHA jako v `release.yml`:
  ```yaml
  name: CI

  on:
    push:
      branches: ['**']
    pull_request:

  permissions:
    contents: read

  concurrency:
    group: ci-${{ github.ref }}
    cancel-in-progress: true

  jobs:
    check:
      runs-on: windows-latest
      timeout-minutes: 60
      steps:
        - uses: actions/checkout@11d5960a326750d5838078e36cf38b85af677262 # v4
          with:
            persist-credentials: false

        - name: Toolchain from rust-toolchain.toml
          run: |
            rustup toolchain install
            rustc --version

        - uses: Swatinem/rust-cache@d1527c8f67ef9af357290475d4af4b0fab8f12e0 # v2

        - name: Check
          shell: pwsh
          run: ./scripts/check.ps1
  ```
  Žádná tajemství, žádná práva k zápisu. Tagy workflow nespouští (ty patří `release.yml`).
- `AGENTS.md`, „Dev loop“: nový první bod „`scripts/check.ps1` je brána; pouští ji `dev-rebuild.ps1 -Test`
  i CI při každém pushi. Hotovo je až po jejím průchodu.“ Stejně uprav `.claude/rules/testing.md`
  („Running“) a `.claude/agents/dev-rebuild.md`.
- **Ruční krok (rozhodnutí uživatele):** CI samo jen hlásí. Skutečné vynucení je pravidlo na GitHubu
  (Settings > Rules > Rulesets) pro `main`: vyžadovat průchod kontroly `CI / check`. Pak se do `main`
  nedá dostat nic neověřeného, ale práce musí chodit přes pull request z větve.
- Hotovo, když: push do větve spustí CI a ten projde.

---

## Fáze B: chyby

U každé nejdřív test, který selže, pak oprava.

### B1. Instalátor (nález 1)

- `scripts/apo-install.ps1:886`. Řádek je v hlavním toku, testy přitom vytahují jen vyjmenované
  funkce a proměnné (`scripts/tests/apo-install.tests.ps1:6-16`). Vyjmi ho proto do funkce, např.
  `Test-BackupCreated([string]$backupFile)`, s tělem
  `try { [bool](Get-Content $backupFile -Raw | ConvertFrom-Json).Created } catch { $false }`.
- Přidej ji do seznamu `$functions` v testech a tři případy: záloha s `Created = true`, bez něj,
  poškozený soubor.
- Musí projít ve Windows PowerShellu 5.1 i v `pwsh`.

### B2. Import ze Sonaru (nález 3)

- `import_sonar` (`sonar_importer/mod.rs:35-55`): když soubor stejného jména existuje a liší se
  obsahem, nejdřív ho přesuň do složky smazaných, pak ulož nový. Stejný obsah: nic nepřesouvej.
- `move_to_deleted` je dnes soukromá metoda `OutputEq` (`output_eq/mod.rs:707`). Přesuň ji tam, kde
  na ni dosáhne i import (nejspíš `ProfileStorage`); nedělej druhou kopii.
- Test s dočasnou složkou: upravený profil A, import A' -> platí A', původní A je ve smazaných.
- Tlačítko má říct, co se stane (popisek po najetí myší, anglicky v `tr(...)`, česky v `i18n_cs.rs`).

### B3. ID zařízení (nález 4)

- `remap` (`win_audio/device_ids.rs:78-93`): kandidát na nové ID nesmí být zařízení, které už je
  v `known`. `heal` počítá `remap` před tím, než připojená zařízení do `known` doplní (řádky 46-53),
  takže podmínka nezablokuje poctivé přesuny. ID porovnávej bez ohledu na velikost písmen, jako zbytek
  funkce.
- Test: dvě známá zařízení se stejným jménem, jedno odpojené -> žádný přesun. Stávající testy musí
  projít dál.

### B4. Volba „(žádný)“ (nález 7)

- `ui/tab_eq.rs:166`: `eq.bind_exe(&exe, choice, shm)` místo `Some(choice.unwrap_or_default())`.
- Pokud jde logiku volby vytáhnout do čisté funkce bez egui, přidej test; jinak stačí zkouška v okně.

### B5. Varování se špatným jménem (nález 13)

- `health.rs:500-505`: jméno i klíč brát ze `snap.output` (to, co se opravdu kontroluje), ne ze
  `snap.default_output`.
- Test v `health.rs` vedle ostatních: výchozí výstup je jiné zařízení než kontrolované -> varování
  jmenuje kontrolované.

### B6. Test překladů hlídá všechny soubory

- Test v `i18n.rs:126-151` prochází ručně psaný seznam `SOURCES` (22 souborů). Chybí v něm např.
  `output_eq/genre.rs`, a každý nový soubor s textem z kontroly tiše vypadne. Fáze H nové soubory
  přidává, proto se to opravuje už tady.
- Seznam nahraď procházením složky `src/` za běhu testu (`env!("CARGO_MANIFEST_DIR")`, `std::fs`,
  jen `.rs`). Chybějící české texty, které se tím objeví, doplň do `i18n_cs.rs`; dva nepoužité klíče smaž.
- Pak oprav větu o `SOURCES` v `.claude/rules/code-style.md` (oddíl UI).

---

## Fáze C: mazání a jména

### C1. Starý most mikrofonu (nález 2)

Mění chování: zmizí posílání mikrofonu do VB-Cable / „Sonar - Microphone“. Patří do poznámek k vydání.

- `MicStreamWorker` (`win_audio/mic_stream.rs`) dnes drží dvě vlákna: most a `loopback`, které nese
  i spojení s APO (`ApoLink`). Zůstává jen `loopback`.
- `StreamResources` a `init_client_48k_float` přesuň do `loopback.rs`, `loopback` spouštěj přímo
  z `main.rs` (dnes řádky 195-200). `mic_stream.rs` smaž.
- Tím osiří a jdou také pryč: `PluginMode`, `SpeakerTap`, `speaker_tap`, `tap_frames`, `process()`
  a parametr `sample_rate` v `plugin.rs` (řetězec je jen 48 kHz); `NO_VIRTUAL_MIC`;
  `set_output_device` / `output_device_id`; `ShmRingBuffer::push_frames` bez časové značky, pokud ji
  pak volají jen testy; texty v `i18n_cs.rs`, které už nikdo nepoužívá.
- Dál se smazání dotkne:
  - `input_device_id` / `set_input_device` a jejich volání (`main.rs:195-197, 295`, `ui/app.rs:128`,
    `ui/tab_mic.rs:64`): ověř, co z toho po mostu ještě někdo čte;
  - pole `MicStreamWorker` ve `win_audio/mod.rs:27, 36, 46, 53`;
  - `set_reference_device` (`main.rs:199, 317`, `ui/app.rs:138`) musí dál plnit `Arc`, který si bere
    `loopback::spawn` (`loopback.rs:34-38`), jinak potlačení ozvěny přijde o referenci;
  - test v `plugin.rs:1097` volá `process()`: přepsat na `process_timed`.
- Pozor na kontrolu zdraví: most zvyšoval `aec_telemetry_seq` v místní sdílené paměti
  (`plugin.rs:776`), podle kterého `RunWatch` (`health.rs:768`) a `DspWatch` poznají, že řetězec
  běží. Ověř, že po smazání ten údaj dál přichází z APO přes `ApoLink`.
- **Ruční krok:** nainstalovat sestavení, promluvit do mikrofonu v nějaké aplikaci a v logu ověřit:
  `[ApoLink]` spojeno, potlačení ozvěny má referenci, žádné nové varování „nainstalováno, ale nic nedělá“.

### C2. `dsp_apo`

- `auto_preamp.rs` přesuň do `output_eq/` a oprav pět importů (`ui/eq_editor.rs`, `ui/eq_graph.rs`,
  `ui/tab_eq.rs`, `output_eq/autoeq.rs`, `output_eq/mod.rs`). Jeho první řádek bere
  `super::biquad::BiquadCoefficients`; změň na `gamersequalizer_common::biquad::BiquadCoefficients`.
- Smaž `apo_writer.rs`, `biquad.rs` (dvouřádkový přeposílač), `mod.rs` a řádek `pub mod dsp_apo`
  v `main.rs:7`.
- `apo_abbreviation` v `common` zůstává: volá ji `output_eq/share_text.rs:35`.

### C3. Pokusy a drobnosti v řetězci mikrofonu

- Smaž `whisper_probe` a `whisper_sweep` (`ai_denoiser.rs:2918-3346`) a odkaz v `DEVLOG.md:566` na
  neexistující `docs/whisper-step1-results.md` nahraď větou, že zprávy byly odstraněny.
- Pole `rescue_soft_prob` nahraď konstantou. `test_digital_boost_saturation` (`plugin.rs:1037-1049`)
  zkouší cizí funkci: přesuň k `soft_limit` v `common`, nebo smaž, když tam takový test už je.
- Oprav komentáře, které lžou: tract 0.21 -> 0.23 (`dpdfnet.rs:5-6`, `ai_denoiser.rs:313-319`),
  „VST host“ (`plugin.rs:246`), alokace v `dpdfnet.rs:34-35`.

### C4. Ostatní mrtvý kód (nález 15, bez polí sdílené paměti)

Každou položku nejdřív vyhledej v celém repu.

- `apo`: `SIDETONE_INSTANCE` (duplikát `APO_INSTANCE_IDS`), druhé volání `endpoint_device_id`
  v Initialize, dva stejné testy CLSID (`lib.rs:2077-2102`) sloučit do jednoho cyklu, režim „on“
  v příkladu `sidetone_probe`, `CHAIN_RATE` v `rate_bridge.rs:23` (duplikát `REF_SAMPLE_RATE`).
- `common`: `ShmRingBuffer::header_mut`, `SidetoneChain::gate_gain_db`, jednopásmový režim expandéru
  (`wide`, `wide_buf`, `single_band`, `set_single_band`).
- `daemon`: `applied_mica`, `get_erle_db`, alias `ShmHeaderT`, obal `on_off_button`, nepoužitá
  velikost z `file_stamp`, `let capture_id = endpoint_id` (`apo_install.rs:141`), složka profilů `aux`.
- Testy, které nic netvrdí (`endpoint_lock`, autostart): doplnit tvrzení, nebo smazat.

### C5. Jména z prototypu

- `CLSID_GAIN_APO` -> `CLSID_MIC_APO` (nebo podle toho, co třída dnes dělá), `GainApoFactory` ->
  `ApoFactory`, popis „(gain only)“ v `crates/gamersequalizer-apo/Cargo.toml`, komentář „In-place gain“.
- Jméno je i ve dvou komentářích: `scripts/apo-install.ps1:140` a `win_audio/apo_install.rs:29`.
- **Hodnota GUID se nesmí změnit.** Je zapsaná v registru na počítačích testerů a ve skriptech
  instalátoru. Mění se jen jméno konstanty v Rustu.

### C6. Přejmenovat crate `gamersequalizer-mic-vst`

- Nové jméno `gamersequalizer-mic-chain` (knihovna `gamersequalizer_mic_chain`); `GamersEqualizerPlugin`
  -> `MicChain`, `plugin.rs` -> `chain.rs`. Přes `git mv`, jeden commit, žádná jiná změna.
- Kde všude: složka a `Cargo.toml` crate; kořenový `Cargo.toml` (členové workspace);
  `crates/build-support/version_info.rs:15`; `Cargo.toml` a `src/lib.rs` v `apo` (závislost
  i předání funkce `debug-capture`) a čtyři příklady v `apo/examples`; `Cargo.toml` v `daemon`
  a soubory `takeover.rs`, `win_audio/{loopback,dsp_watch,apo_link,stream_router,sidetone}.rs`,
  `ui/{state,tab_mic,tab_sidetone}.rs`; `.claude/agents/dev-rebuild.md`; `AGENTS.md` (tabulka
  Structure); `docs/aec3-*.md`; `test_data/denoise/README.md`; `README.md:196`; řádky
  `cargo test -p ...` v komentářích testů.
- Schválně beze změny: `DEVLOG.md` (historie, 13 výskytů) a `SPECIFICATION.md:41` (původní zadání).
- **Nejsnáze se přehlédne `app_log.rs:24`**: filtr logu `gamersequalizer_mic_vst=debug`. Když zůstane
  staré jméno, řádky z řetězce mikrofonu z logu tiše zmizí. Po sestavení ověř, že v logu jsou.
- Kontrola exportu `VSTPluginMain` v `scripts/package.ps1:196` ať zůstane, nic nestojí.

---

## Fáze D: moderní Rust

### D1. Edice 2024

1. `static mut HOOK_SENDER` (`win_audio/game_detector.rs:25`) nahraď `static HOOK_SENDER: Mutex<Option<Sender<isize>>>`
   (dá se spojit s `HOOK_LOCK` vedle). Samostatný commit, funguje i v edici 2021.
2. Čistý strom, pak `cargo fix --edition --workspace --all-targets` a znovu s
   `-p gamersequalizer-apo --features debug-capture`. `cargo fix` upraví jen kód, který se zrovna
   překládá, proto ten druhý běh.
3. Projdi, co `cargo fix` udělal (kód je teď platný v obou edicích), a commitni. Na co se dívat:
   - `extern` bloky jsou teď `unsafe extern`, `#[no_mangle]` je `#[unsafe(no_mangle)]`. Exporty DLL
     se nesmí změnit: po sestavení projde kontrola v `scripts/package.ps1`.
   - V `unsafe fn` obalil celá těla do `unsafe { }` (od edice 2024 se to vyžaduje). Kde to jde snadno,
     zmenši blok jen na opravdu nebezpečné operace; krok D2 chce ke každému bloku komentář.
   - Změny pořadí uvolnění dočasných hodnot v `if let` a na konci bloku: zkontroluj místa se zámky,
     ať se zámek nedrží déle ani kratší dobu než dřív.
4. Teprve teď přepni edici, jednou pro všechny: v kořenovém `Cargo.toml` `[workspace.package] edition = "2024"`
   a v každém crate `edition.workspace = true`. Ve stejném commitu `cargo fmt` (edice 2024 formátuje
   trochu jinak; zvlášť by mezi commity neprošla brána). Hash přidej do `.git-blame-ignore-revs`.
5. Crate ve `vendor` nech na jejich edici.
- **Ruční krok:** nainstalovat APO sestavené v nové edici a krátce vyzkoušet mikrofon, výstup a odposlech.

### D2. Lint na komentáře SAFETY

- Jdi po crate: `common` (asi 8 bloků), `mic-chain` (21), `apo` (62), `daemon` (155 před mazáním). Commit
  na crate; hotový crate dostane na začátek `lib.rs` / `main.rs` řádek
  `#![deny(clippy::undocumented_unsafe_blocks)]`, takže brána prochází po každém commitu.
- Poslední commit: ty čtyři řádky nahraď jedním `undocumented_unsafe_blocks = "deny"` v kořenovém
  `Cargo.toml` (`[workspace.lints.clippy]`) a stejný řádek přidej do bloku lintů
  v `.claude/rules/code-style.md`.
- Jak má komentář vypadat: říká, **proč to platí** (odkud ukazatel je, jak dlouho žije, které vlákno
  ho drží). „Volání FFI“ nebo „je to bezpečné“ není zdůvodnění.
- Řada volání COM nad jedním objektem: jeden blok a jeden komentář, ne deset bloků.
- Když zdůvodnění napsat nejde, je to nález a opraví se hned:
  - `capture_time_hns` (`apo/src/lib.rs:699, 1775-1778`) čte pole V2 přes odkaz na V1: předat surový
    ukazatel a volat jen pro `ApoKind::Mic`.
  - `mic-chain/src/shared_memory.rs:58-68, 87-92`: doplnit; `layout_mut` udělat soukromé.

### D3. `#[expect]` místo `#[allow]`

- Asi 40 výskytů `#[allow(...)]` v našem kódu přepiš na `#[expect(..., reason = "...")]`. Překladač pak
  sám ohlásí výjimku, která už není potřeba; takovou smaž.
- `allow`, který platí jen pro některou konfiguraci (`cfg`), nech jako `allow`.

### D4. Dlouhé testy do vlastního souboru

- Platí pro soubory, kde testy mají přes 300 řádků: `ai_denoiser.rs` (2 233), `aec3.rs` (1 033),
  `common/src/sidetone.rs` (489), `apo/src/lib.rs` (438).
- Postup: v souboru zůstane `#[cfg(test)] mod tests;`, tělo jde do `<soubor>/tests.rs` (u `lib.rs` do
  `src/tests.rs`). Pak `cargo fmt`. `use super::*` funguje dál.
- Čistý přesun, commit na soubor. Před a po porovnej počet testů: `cargo test -p <crate> -- --list`.
- Pravidlo zapiš do `.claude/rules/testing.md` („Layout“): testy delší než 300 řádků patří do
  sousedního `tests.rs`.

---

## Fáze E: sdílená paměť, jedna změna verze

Všechny změny rozložení v jedné fázi, verze 21 -> 22 jen jednou.

### E1. Jedno číslo verze (nález 9)

- `SHM_NAME` a `APO_SHM_NAME` slož z jednoho čísla (makro s `concat!`), ze stejného čísla ber `SHM_VERSION`.
- Vedle toho `const _: () = assert!(size_of::<ShmLayout>() == N);`. Změna rozložení bez změny verze
  pak neprojde překladem.
- Při otevření mapování kontroluj kromě `magic` i `version` (`mic-chain/src/shared_memory.rs:36, 51`
  a strana služby). Neshoda = „mapování není“, jeden řádek do logu se dvěma čísly verzí.

### E2. `Default` bez opisování (nález 9)

- `impl Default for ShmHeader` = vynulovaná struktura + `init_defaults()`. Všechna pole jsou atomická
  nebo celá čísla, nula je platná hodnota; blok `unsafe` s komentářem SAFETY.
- Úrovně `aec_{ref,mic,out}_level_db_x10 = -1200` přesuň do `init_defaults`. Mění se tím to, co okno
  ukáže před první telemetrií (ticho místo 0 dB), což je správně.

### E3. Pole, která nikdo nečte (nález 15)

- Telemetrie po starém potlačení ozvěny, jen tato tři pole: počet tapů (`filter_taps`), `double_talk`
  a impuls. Kde jsou: `aec3.rs:43, 206-208, 319-327`; `plugin.rs:758-770, 861-862`; `shm.rs` kolem
  řádků 184-192, 562-569, 886-893, 994; `ui/state.rs:40-47, 193, 511-528`; `chain_log.rs:57, 134, 363`.
- **Ve stejných úsecích `shm.rs` jsou i živá pole, ta zůstávají:** `aec_{ref,mic,out}_level_db_x10`
  (potřebuje je E2) a `aec_telemetry_seq` (čte ho `health.rs:768`, `dsp_watch.rs:46`, `apo_link.rs:198`).
- Pozor na `DEBUG_VARS`: sloupce v `vars.tsv` jsou dané pořadím. Kdo čte staré nahrávky, musí o změně vědět.
- Dále `read_frame_index`, `sample_rate`, `channels`, `capacity_frames`.

### E4. Začátek zvuku bez zpracování (nález 10)

- Poslední `owner_beat` a čas jeho změny drž jednou pro celý proces (statické atomické hodnoty
  v `common`), nový `OwnerWatch` z nich začne. Druhý `OwnerWatch` v `SidetoneTap` zruš.
- Testy: nový `OwnerWatch` vytvořený po tom, co jiný viděl živý tep, je živý hned; tep, který se
  nemění déle než `OWNER_TIMEOUT`, živý není ani pro nový.

### E5. Když už se tu sahá (menší nálezy)

- Po přečtení pásem EQ znovu načíst verzi; když se změnila, číst příští blok znovu (`apo/src/lib.rs:1075-1081`).
- `read_bands` ať vrací iterátor, `BiquadCascade::set_bands` ať bere `impl IntoIterator`: zmizí
  alokace na zvukovém vlákně (`apo/src/lib.rs:1002, 1078, 1420`). Pak ji škrtni ze známých výjimek
  v `.claude/rules/architecture.md`.

- **Ruční krok:** stará DLL (verze 21) s novým exe. Aplikace má ukázat, že je potřeba aktualizace,
  a zvuk má procházet; nesmí číst cizí data.

---

## Fáze F: instalátor

Po celé fázi **ruční krok** se čtyřmi zkouškami: čistá instalace, aktualizace přes nainstalovanou
verzi, odinstalace, `--install --silent`.

### F1. `DeviceState` (nález 17)

- `scripts/apo-install.ps1:549`: `-band 0xF` místo `-eq 1`, stejně jako Rust (`apo_install.rs:72`).
- Test, že skript obsahuje všechna CLSID a jména hodnot z konstant v Rustu (test v Rustu nad
  vloženým textem skriptu).

### F2. Návrat zpět při aktualizaci (nález 5)

- Nejdřív test `Resolve-ProbeOpen` / `Restore-FxSnapshot` se snímkem, který naše CLSID už obsahuje:
  po návratu v seznamech být nesmí.
- Oprava: ze snímku pro návrat (`apo-install.ps1:857-889`) vyjmout naše tři CLSID z hodnot MultiString.
  Záloha původního stavu na disku se nemění.

### F3. `Add-Type` v cizí složce (nález 11, hypotéza)

- Nejdřív ověř: v povýšeném Windows PowerShellu 5.1 sleduj, kam `Add-Type` zapisuje (např.
  Process Monitor, nebo výpis `$env:TEMP` v povýšeném běhu).
- Když se potvrdí: `elevated_bootstrap` (`apo_install.rs:611`) vytvoří složku
  `%ProgramFiles%\GamersEqualizer.tmp.<guid>` (zapisovat tam smí jen správce), nastaví na ni
  `$env:TEMP` a `$env:TMP` a na konci ji v `finally` smaže. Mimo složku aplikace, aby odinstalaci
  nepřekážela.

### F4. Společný začátek skriptů (nález 17)

- Asi 200 stejných řádků (`apo-uninstall.ps1:48-255`) přesuň do `scripts/apo-common.ps1`.
- Oba skripty ho načtou jedním řádkem, který funguje i při ručním spuštění z disku:
  `. "$PSScriptRoot\apo-common.ps1"  # @common`.
- Rust (`win_audio/apo_install.rs`, kde je `include_str!`) ten řádek před zápisem a výpočtem otisku
  nahradí textem společného souboru. Povýšený běh je dál jeden text s jedním otiskem.
- Testy (`apo-install.tests.ps1`) vytahují funkce ze souboru; přidej k prohledávaným i společný soubor.

### F5. Dokumentace

- `AGENTS.md` tvrdí, že se záloha uloží „jednou“; kód ji přepíše, kdykoli zařízení žádný náš efekt
  nenese (`apo-install.ps1:891`). Rozhodni, co je správně (nejspíš kód), a sjednoť.
- Komentář u `apo-install.ps1:820` o spouštění přes `-File` už neplatí.

---

## Fáze G: zvukový řetězec

Před G2 a G3 si pořiď pojistku: test, který pustí pevný vstup z `test_data/denoise` celým řetězcem
a porovná výstup s výstupem před změnou (stačí součet nebo pár vzorků). G2 a G3 nemají výstup změnit.

### G1. Zvuk z doby před pauzou (nález 6)

- Rozšiř test `plugin.rs:1229-1246` na dva bloky po 480 vzorcích. Má selhat.
- Oprava: `flush()` nastaví počitadlo na `AEC3_LATENCY_SAMPLES` a `process_frame` tolik vzorků výstupu
  ztlumí, jako `mute_left` v `ai_denoiser.rs`.

### G2. Jedno čištění stavu (nález 19)

- Stav spánku, převzetí a podlahy šumu do jedné struktury s jedním `clear()`. `flush` ať volá
  i `rescue.reset()`. `reset`, `flush`, `reset_engines`, `reset_timing`, `start_takeover` pak říkají,
  co navíc dělají, ne co všechno čistí.

### G3. Nastavení čtené jednou (nález 19)

- Jedna `MicSettings::read(header)`; použije ji `mic_process`, `LevelerInputs` i kód pro nahrávání
  (`BlockSettings`).

### G4. Drobnosti

- `aec3.rs:354-358`: `warn!` jen jednou (příznak), ne na každý rámec.
- `plugin.rs:500-523`: `leveler_seed_blocked` vynulovat, když `leveler_saved_x10 == 0`.
- Skládání rámců jednou: `frame_fifo.rs` místo čtyř kopií (`ai_denoiser.rs:1058-1208`,
  `dpdfnet.rs:61-87`); `I16_SCALE` jen jednou.
- Testy, které chybí: přepnutí `aec_reference_seq`, znaménko síly ozvěny (`plugin.rs:743-751`).

### G5. Vstupní body COM bez ochrany

- `catch_unwind` dnes obaluje volání do řetězce a stavbu procesorů, ale ne vstupní body
  `DllGetClassObject` (`apo/src/lib.rs:385`), `CreateInstance` (:419), `apo_initialize` (:594)
  a `cfg_lock_for_process` (:718). Pád v nich ukončí `audiodg.exe`, tedy zvuk celého systému.
- Obal je stejně jako ostatní (při pádu vrátit chybový `HRESULT` a zapsat poznámku přes `event_log`).
  Pak uprav větu o nechráněných vstupních bodech v `.claude/rules/architecture.md`.

- **Ruční krok:** nahrávka přes `-DebugCapture` před fází a po ní, stejná situace (řeč, hudba
  z reproduktorů, přepnutí modelu, vypnutí a zapnutí).

---

## Fáze H: dělení souborů

Cílové rozdělení je v auditu v oddílu „Navržené dělení souborů“. Pravidla:

- Jen přesun. Žádné přejmenování, přeskládání ani „když už jsem tady“.
- Jeden původní soubor na commit. Viditelnost jen tak velkou, jak je potřeba (`pub(super)`, `pub(crate)`).
- Testy jdou s kódem, který zkouší.
- Pořadí: `common/shm` -> `common/sidetone` -> `apo/lib.rs` -> řetězec mikrofonu (`denoise/`,
  `chain_*`) -> `daemon` (`main.rs`, `health.rs`, `apo_install.rs`, `chatmix.rs`) -> `ui/state.rs` ->
  `output_eq/mod.rs` -> `ui/autoeq_wizard.rs`.
- Na konci `npm run graph:sync` a aktualizovat tabulky Structure v `AGENTS.md`.

Jediná změna chování (samostatný commit po přesunu, nález 18): služba si místo celého `AppState`
vytvoří jen mapování sdílené paměti a `OutputEq`. Nejdřív sepiš, co všechno služba z `AppState`
dnes opravdu používá (`main.rs:191` a dál), ať nic nevypadne.

---

## Fáze I: slučování kopií, ukládání, formát dat

- **I1. Mixér (nález 16):** `Channel` se čtyřmi hodnotami a metodami místo pěti opsaných kanálů
  (`chatmix.rs:329-628`); jedno `write_mixer(header, &chatmix)` pro obě strany.
- **I2. Vrstvy EQ:** `ProfileStorage::dir_for(layer)` a jedno čtení, uložení a smazání pro všechny
  čtyři vrstvy (`output_eq/mod.rs:156-493`).
- **I3. Prolínání EQ v APO:** `EqFade { cascade, preamp_gain, wet, step, scratch }` pro výstup i stream.
  Přidej chybějící test přepnutí kanálu Chat -> Média (`apo/src/stream.rs`).
- **I4. Ovladače:** `theme::percent_volume` a `theme::level_text` místo tří kopií.
- **I5. Zařízení přes COM:** `win_audio/mmdevice.rs` místo asi patnácti opsaných výčtů; při tom
  uvolnit text z `GetId` (`chatmix.rs:261`).
- **I6. Ukládání (nález 14):** jedno `pending_save` vyřízené v `app.rs` a v `on_exit`; obě
  `process::exit(0)` v `ui/tab_settings.rs` nahradit zavřením okna.
- **I7. Dopředná kompatibilita (nález 8):** nejdřív testy se souborem „z novější verze“ (neznámé
  pole, neznámý typ filtru): načíst, uložit, nic se neztratí. Pak `#[serde(flatten)] extra` do
  struktur nastavení a profilů, `Unknown(String)` u `FilterType` a `VadType` (zvuk s neznámým filtrem
  prochází beze změny, okno řekne, že profil je z novější verze), a kontrola jména nového profilu
  i proti souboru na disku.
- **I8. Dlouhý běh (nález 12):** Master hledat i při `devices_changed`; chybu v `loopback` zapsat jen
  při změně; `loopback` a `sidetone` po neúspěšném otevření sdílené paměti zkoušet znovu s čekáním.

---

## Fáze J: menší nálezy

Seznam je v auditu („Menší nálezy“). Vezmi každý ve fázi, která na dané místo sahá; co zbyde, tady.
Jeden je hypotéza a potřebuje **ruční krok**: APO výstupu na „tichém“ bloku (`process_connection` v `apo/src/entry.rs`),
zkouška příkladem `sidetone_probe`, když nic jiného nehraje.

---

## Fáze K: úklid

- `DEVLOG.md`: jeden stručný milník anglicky. Co se změnilo ve struktuře, nová verze sdílené paměti,
  změny chování (most mikrofonu pryč), nálezy, které se nepotvrdily. Žádné výpisy testů.
- Poznámky k vydání v `packaging/release-notes/`.
- Smazat `docs/refactoring-audit.md` a tento návod.
- Před sloučením do `main` nabídnout kontrolu závislostí (`AGENTS.md`, „Before a release“).

---

## Kontrolní seznam

- [x] A1 verze překladače 1.100.0 (zbývá ruční běh Release)
- [x] A2 formát
- [x] A3 clippy na nulu
- [x] A4 `check.ps1`, CI (zbývá: push, ověřit běh CI; pravidlo na GitHubu ruční)
- [x] B1 instalátor, řádek 886
- [x] B2 import ze Sonaru
- [x] B3 ID zařízení (jinak než návod: kontrola srážky v souborech, viz commit)
- [x] B4 volba „(žádný)“ (zbývá zkouška v okně)
- [x] B5 varování se špatným jménem
- [x] B6 test překladů
- [x] C1 most mikrofonu (ručně ověřeno 15. 11.: `[ApoLink]` spojeno, AEC Locked; poznámka v `packaging/release-notes/next.md`)
- [x] C2 `dsp_apo`
- [x] C3 pokusy v řetězci mikrofonu (komentář „VST host“ zmizel už s `process()` v C1)
- [x] C4 ostatní mrtvý kód (bez složky `aux`, viz Nepotvrzené nálezy)
- [x] C5 jména z prototypu (`CLSID_MIC_APO`, `ApoFactory`; GUID beze změny)
- [x] C6 přejmenování crate (`gamersequalizer-mic-chain`, `MicChain`, `chain.rs`; pole `plugin` v APO je teď `chain`)
- [x] D1 edice 2024
- [x] D2 lint SAFETY (nálezy opravené cestou: `capture_time_hns`, `IPolicyConfig` s holým `PCWSTR`, únik textu z `GetId` v `get_endpoint_info`)
- [x] D3 `#[expect]` (30 zbytečných smazáno, z toho 28 na testových modulech, které pokrývá `clippy.toml`; `allow` zůstal jen u `unused_mut` v `aec3.rs`)
- [x] D4 dlouhé testy zvlášť
- [x] E1 jedno číslo verze (`init_defaults` píše `version` před `magic`, jinak by souběžně startující APO viděla verzi 0)
- [x] E2 `Default`
- [x] E3 nepoužitá pole (tady verze 21 -> 22; `vars.tsv` bez `double_talk` a `aec_taps`, 55 sloupců, debug mapování v5: patří do milníku v K)
- [x] E4 `OwnerWatch`
- [x] E5 čtení pásem (bez nového čtení verze, viz Nepotvrzené nálezy)
- [x] F1 `DeviceState`
- [x] F2 návrat zpět
- [x] F3 `Add-Type` (potvrzeno: zapisuje `.cs`, `.cmdline` a `.dll` do `$env:TEMP`; TEMP se mění jen kolem `Add-Type`, ne v bootstrapu, protože odinstalace do TEMP přesouvá svůj zamčený exe)
- [x] F4 společný začátek skriptů (jen doslova stejné kusy; `Say`, `Set-FxValue` a obnova se liší chováním, zůstávají)
- [x] F5 dokumentace instalátoru (záloha: platí kód, přepíše se, dokud zařízení nenese náš efekt)
- [x] G1 AEC3 po pauze (ztlumen celý první rámec po `flush`, ne jen 430 vzorků: za nimi je ~25 vzorků dozvuku filtrů dělení pásem)
- [x] G2 jedno čištění stavu (`Sleep::clear`; podlaha šumu místnosti mimo: nečistí ji nikdo a pauza místnost nemění; `reset_engines` nuluje `mute_left`)
- [x] G3 nastavení jednou
- [x] G4 drobnosti řetězce (`frame_fifo.rs` s `FrameInput`/`FrameOutput`, i pro DPDFNet s překryvem oken)
- [x] G5 vstupní body COM (`guarded`; zbývá ruční nahrávka `-DebugCapture` před fází a po ní)
- [x] H dělení souborů (hotovo: `common/sidetone`, `apo/lib.rs`, `ai_denoiser/`, `chain/`, `aec3/echo_meter.rs`, `health/`, `win_audio/apo_install/`, `win_audio/chatmix/`, `ui/state/` + `shm_mapping.rs`, `output_eq/`, `ui/tab_headphones/autoeq_wizard.rs`, `daemon/main.rs` (smyčka služby zůstala jedna funkce: rozdělit ji na `follow_mic` a spol. není přesun), `common/shm/`; změna chování hotová: služba bez `AppState`)
- [x] I1 až I8 (hotovo: I1 `Channel` a `ChatMixController::write_levels` pro službu i okno; vyvážení dál zapisuje jen okno; I2 `ProfileStorage::dir_for`, `save_layer_profile`, `load_layer_profiles`, jedno uložení a smazání vrstvy v `OutputEq`; I3 `apo/src/eq_fade.rs` a test přepnutí Chat -> Média; I4 `theme::percent_volume`, `volume_text`, `level_text`: údaj nad 100 % všude krátce „250 % (+8.0 dB)“, bez „(nativně)“ a „zesílení“, aby se vešel do pásu mixéru; I5 `win_audio/mmdevice.rs` (bez `CoInitializeEx`: vlákno okna si apartmán volí samo), text z `GetId` v `find_physical_microphone` už neuniká; I6 `AppState::save_pending` z každého snímku a z `on_exit`, obě `process::exit` nahradilo zavření okna (úklid dat při odinstalaci čeká na konec procesu); bez testu: chce celý `AppState`; I7 `FilterType::Unknown(String)` (APO pásmo přeskočí, editor ho ukáže jako „Filtr z novější verze“), `extra` u profilu, `OutputEqSettings`, `UiPrefs` a `SidetonePair`, `AudioSettings::save` nechá neznámé klíče souboru (struktura se staví ze sdílené paměti), `refuse_unreadable`: soubor profilu, který tahle verze nepřečte, se nepřepíše (kontrola při uložení, ne v kreslení); `VadType` bez `Unknown`: profil s neznámým typem se při načtení přeskočí a `refuse_unreadable` ho chrání, `Unknown` by vzal `Copy` na 95 místech; `BiquadBand` bez `extra` (64 literálů), neznámé pole pásma se uložením ztratí, doplnit, až pásmo dostane nové pole; I8 Master se hledá i při `devices_changed`, `win_audio/worker.rs`: `RepeatFilter` (stejná chyba za sebou v logu jednou; `loopback` i `sidetone`) a `open_shm` se zkoušením znovu po 1 až 30 s místo konce vlákna)
- [ ] J menší nálezy (hotovo: průvodce AutoEQ hlásí chybu uložení a zůstane na kroku, vybraný soubor nad 1 MB nečte; `set_autostart` zavře klíč i při chybě; `daemon_running` mutex jen otevírá; karta odposlechu se ptá Windows na výchozí zařízení dvakrát za vteřinu, ne v každém snímku; `bus_result` a `apply_mix` jednou za snímek, log se skládá jen při změně už dřív; „Reset“ hry v okně převezme vlastník, bez nového pole ve sdílené paměti; okno jako následovník bere žánr jen z cache služby (`GenreLookup::cached`), `appinfo.vdf` čte jen vlastník, okno tedy jen bez běžící služby)
- [ ] K úklid

## Nepotvrzené nálezy

Sem zapiš nález, který se nepodařilo předvést, a proč.

| Nález                               | Co se zkoušelo                                                                              | Výsledek                                                                                                                                                                                             |
| ----------------------------------- | ------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Složka profilů `aux` (nález 15, C4) | `profiles/aux` v `%APPDATA%` na Windows 11 (26300)                                          | Složka existuje, drží 9 importovaných presetů Sonaru: ukládání funguje. Smazání by vzalo data, zůstává. Windows 10 nezkoušeno.                                                                       |
| Pásma EQ napůl stará (E5)           | Kód: zapisovatel uloží pásma, pak zvýší verzi (Release); APO čte verzi (Acquire), pak pásma | Zápis během čtení změní verzi, takže příští blok čte znovu sám. Nejvýš jeden blok (~10 ms) má směs starých a nových pásem, každé platné (ořezané). Druhé čtení verze nic nepřidá, zůstává bez změny. |
