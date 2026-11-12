# AEC se stereo referencí – co je hotové a jak to ověřit

Navazuje na plán `docs/mic-aec-multichannel-analysis.md`. Větev `feat/aec-stereo-reference`.

## Co se změnilo

- AEC3 dostává levý a pravý kanál reproduktorů zvlášť a učí se pro každý vlastní filtr
  (`aec3.rs`, `process_block_stereo`). Dřív dostával jen jejich mono mix.
- Stereo režim se zapne sám, když se kanály liší: 1 s souvislého slyšitelného stereo obsahu
  (rozdíl kanálů nad -30 dB, hlasitost nad -60 dBFS). Zpět na mono mix jde po 5 minutách mono obsahu.
  Pauzy se nepočítají ani do jednoho.
- Přepnutí znamená přestavbu AEC3. Čeká se na pauzu v mikrofonu stejně jako u změny zesílení
  (jinak by vypadlo 9 ms zvuku), takže stereo naběhne 3–7 s po začátku stereo obsahu.
- Stejný zvuk v obou kanálech (hovor, mono video) běží po staré cestě. Všech 16 dřívějších testů
  AEC dává před změnou i po ní stejná čísla.
- Log aplikace (debug): `AEC stereo reference off -> on`. Debug záznam má navíc
  `03_ref_aec_left.wav`, `03_ref_aec_right.wav` a sloupec `aec_stereo`.
- Bez změny formátu sdílené paměti (`SHM_VERSION` zůstává): ring už L/R držel, stav jde přes nový
  bit v `mic_chain_flags`. Mapování debug záznamu má novou verzi (v4, ~122 MB místo ~99 MB).

## Oprava proti plánu

Plán tvrdil, že loopback u 5.1/7.1 zahazuje center a surroundy. To platí jen pro nouzovou cestu:
`init_client_48k_float` si běžně žádá stereo s `AUTOCONVERTPCM` a kanály do něj smíchá Windows.
Nouzová cesta (Windows převod odmítne) teď 5.1/7.1 skládá sama (`StereoFrame::from_channels`).

## Naměřeno (Mac, syntetické scény, 40 s)

Reproduktory -18 dBFS; levý blíž, pravý o 0,9 ms dál a o 8 dB slabší, každý jiný dozvuk; ozvěna na
mikrofonu -30 dBFS; hlas -30 dBFS. Ozvěna měřena ve 20–30 s (jen ozvěna), hlas ve 30–40 s.

| Scéna                                 | Odstraněno: mix | Odstraněno: stereo | Nejhorších 100 ms: mix → stereo | Hlas: mix → stereo |
| ------------------------------------- | --------------- | ------------------ | ------------------------------- | ------------------ |
| stejný zvuk v obou kanálech           | 43,5 dB         | 43,5 dB (mono)     | -70,2 → -70,2 dBFS              | -10,3 → -10,3 dB   |
| hudba (střed + nezávislé strany)      | 16,5 dB         | 43,8 dB            | -42,2 → -73,0 dBFS              | -3,3 → -11,6 dB    |
| nezávislé kanály                      | 9,9 dB          | 43,4 dB            | -35,5 → -72,2 dBFS              | -3,2 → -12,0 dB    |
| jeden zdroj putuje L↔R (0,25 Hz)      | 29,2 dB         | 44,0 dB            | -46,6 → -71,3 dBFS              | -4,9 → -10,5 dB    |
| jeden zdroj skáče L↔R po 3 s          | 16,0 dB         | 44,3 dB            | -30,1 → -73,3 dBFS              | -10,7 → -11,9 dB   |
| pevné panorama 70/30                  | 43,7 dB         | 43,6 dB            | -71,1 → -72,2 dBFS              | -9,9 → -9,8 dB     |

- Hlas: na mixu „přežije“ jen proto, že ozvěna zůstane a potlačovač to vzdá. Ve stereu ztrácí tolik
  jako dnes u mono obsahu ve stejné scéně (-10,3 dB; hlas je tu stejně hlasitý jako ozvěna).
- Výpočet AEC3: 0,46 → 0,52 % reálného času (+13 %).
- Obava z plánu (stereo filtr se po změně panoramatu rozladí) se nepotvrdila: putující i skákající
  zdroj vychází ve stereu lépe než na mixu.

## Odvozeno z kódu

- **Chyba v sonora 0.3.0:** s `detect_stereo_content = false` a dvěma kanály zůstane AEC3 na levém
  kanálu (konstruktor začíná mono a přepíná jen detektor). Odstraní pak 2,5 dB místo 35 dB. Proto
  detektor knihovny zůstává zapnutý s hysterezí 0 s a bez návratu; o režimu rozhoduje náš obal,
  který si ho pamatuje přes přestavby.
- Vlastní vícekanálové nastavení AEC3 (`create_default_multichannel_config`) nepoužíváme: ozvěny
  neodstranilo víc (43,6 dB) a hlasu vzalo o 10 dB víc (-21,5 dB).
- Záložní odhad ozvěny: ve stereu AEC3 sčítá výkony kanálů, síla ozvěny se měří proti mixu. Zisk jde
  proto o 3 dB dolů; u nesouvisejících kanálů je odhad o 3 dB nad skutečností, nikdy pod.

## Ověřit na Windows

Na Macu nejde přeložit `plugin.rs`, `loopback.rs`, `chain_log.rs` ani APO. Přeloženo a otestováno je
jen `gamersequalizer-common` a `aec3.rs`.

1. `scripts/dev-rebuild.ps1 -Apo -Test`: první překlad zbytku, testy, clippy.
2. Reproduktory, stereo hudba, otevřený mikrofon, debug log zapnutý. Do ~7 s má přijít
   `AEC stereo reference off -> on`. Při hovoru (mono) se nesmí objevit.
3. `-Apo -DebugCapture`, 60 s záznam: prvních 20 s jen hudba, pak mluvit. Porovnání na témže záznamu:
   ```powershell
   $env:CAP = "<záznam>"; $env:VARIANTS = "mono;"
   cargo test --release -p gamersequalizer-mic-vst --lib capture_tuning_report -- --ignored --nocapture
   ```
   První řádek je mono mix, druhý stereo. Sledovat `echo` (odstraněno), `left` (co zbylo) a ztrátu hlasu.
4. Totéž se hrou s pozičním zvukem a se sluchátky (kontrola: nic se nesmí zhoršit).
5. V souhrnu logu `load avg` a `late blocks` před a po zapnutí sterea.

## Co chybí

- Přepínač v nastavení (stereo / mono). Záměrně vynechán: A/B jde offline na záznamu a přepínač by
  znamenal změnu sdílené paměti, UI a překladů.
- Plných N kanálů (varianta C plánu). Až podle měření na 5.1 sestavě.
