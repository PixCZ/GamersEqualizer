# Hlasitost mikrofonu ve Windows a aplikace s vlastní automatikou

Pracovní rozbor (2026-11-26). Otázka: můžeme hlasitost mikrofonu ve Windows brát čistě jako
vstupní zesílení (řídí ho jen naše automatika, výstup určuje cíl leveleru a za ním už nic není)?
Podmínka: spolehlivě a hned poznat, kdo hlasitostí pohnul, a mít výchozí cíl, se kterým se
automatika aplikací nepere.

Zdroje: zdrojáky WebRTC a Chromia (googlesource, `refs/heads/main`, staženo 2026-11-26),
Microsoft Learn, fóra aplikací. Na Windows zatím nic změřeno.

## Shrnutí

- **Naše vlastní změny se dají poznat přesně a okamžitě.** Každá změna hlasitosti nese GUID, který
  zvolí volající (`pguidEventContext`), a `IAudioEndpointVolumeCallback::OnNotify` ho doručí všem.
  Když budeme posílat vlastní GUID, poznáme svou změnu přesně a ne jen podle hodnoty v toleranci
  1,5 % při dotazování každých 250 ms jako dnes.
- **Kdo jiný hlasitostí pohnul, Windows neřekne.** Žádné PID, žádná událost ETW s volajícím.
  „Neprůstřelná“ detekce aplikace proto není možná. Spolehlivá kombinace je níž: GUID + poslední
  vstup uživatele + aplikace, které právě nahrávají.
- **Chrome (Meet, webový Discord, Teams v prohlížeči) hlasitost mikrofonu ve Windows nemění.** Jeho
  regulace (WebRTC AGC2) mění jen hlasitost vlastního záznamového streamu přes `ISimpleAudioVolume`.
  Navíc má mrtvé pásmo -50 až -12 dBFS, takže hlas srovnaný na náš cíl -24 dBFS vůbec neposouvá.
- **Teams a Zoom podle uživatelů hlasitost ve Windows mění** (a Zoom ji nechává změněnou i po
  ukončení). Cílovou úroveň nikde nezveřejňují. Tu je potřeba změřit.
- **Výchozí cíl -24 dBFS** sedí uprostřed toho, co známe: mrtvé pásmo WebRTC (-50 až -12), výstup
  digitální AGC2 (odhad -17 až -30), ITU-T P.56 -26 dBov.

## Zjištěno ze zdrojů

### WebRTC / Chrome

| Co                                          | Hodnota                                                            | Kde                                                               |
| ------------------------------------------- | ------------------------------------------------------------------ | ----------------------------------------------------------------- |
| Jak Chrome na Windows mění hlasitost        | `ISimpleAudioVolume` vlastního streamu                             | `media/audio/win/audio_low_latency_input_win.cc`, `SetVolume`     |
| Hlasitost zařízení (`IAudioEndpointVolume`) | jen ztlumení u loopbacku, ne hlasitost                             | tamtéž                                                            |
| Jak často čte hlasitost                     | 1 s                                                                | `media/audio/agc_audio_stream.h`, `kIntervalBetweenVolumeUpdates` |
| AGC1 na desktopu                            | vypnutá                                                            | `media/webrtc/helpers.cc`, `ConfigAutomaticGainControl`           |
| AGC2 + regulace vstupní hlasitosti          | zapnuté (`kWebRtcAllowInputVolumeAdjustment`)                      | `media/webrtc/webrtc_features.cc`                                 |
| Mrtvé pásmo úrovně řeči (RMS)               | -50 až -12 dBFS                                                    | `agc2/input_volume_controller.h`                                  |
| Krok podle odchylky                         | max ±15 dB, převod přes `kGainMap`                                 | `agc2/input_volume_controller.cc`                                 |
| Clipping                                    | podíl clipnutých rámců > 0,1, -15/255, za 3 s                      | tamtéž                                                            |
| Nejnižší hlasitost, kterou nastaví          | 20/255                                                             | tamtéž, `min_input_volume`                                        |
| Cizí změna hlasitosti                       | odchylka > 25/255 = ruční změna: převezme ji, chvíli nic nedělá    | tamtéž, `kVolumeQuantizationSlack`                                |
| Digitální AGC2                              | rezerva 5 dB + 12 až 25 dB → řeč zhruba -17 až -30 dBFS (odvozeno) | `agc2/adaptive_digital_gain_controller.cc`                        |

### Ostatní aplikace

| Aplikace          | Automatika                                        | Mění hlasitost ve Windows?       | Cíl           |
| ----------------- | ------------------------------------------------- | -------------------------------- | ------------- |
| Teams             | „Automatically adjust mic sensitivity“            | ano (hlášení uživatelů)          | nezveřejněn   |
| Zoom              | „Automatically adjust microphone volume“          | ano, i po ukončení (hlášení)     | nezveřejněn   |
| Skype             | „Automatically adjust microphone settings“        | ano (hlášení), i když je vypnutá | nezveřejněn   |
| Discord (desktop) | „Automatic Gain Control“ (WebRTC)                 | neověřeno                        | nezveřejněn   |
| Slack, Steam      | AGC v nastavení                                   | neověřeno                        | nezveřejněn   |
| TeamSpeak         | AGC, SDK úroveň 16000 (~-6 dBFS, měřítko nejisté) | ne (digitální)                   | jen digitální |
| Mumble            | Speex AGC, cíl 30000, max +29 dB                  | ne (digitální)                   | jen digitální |

Nastavení na disku se nám podařilo najít jen u Mumble (`HKCU\Software\Mumble\Mumble`). U Teams,
Zoomu a Discordu ne, takže z disku nepoznáme, jestli mají automatiku zapnutou.

### Windows

- `AUDIO_VOLUME_NOTIFICATION_DATA.guidEventContext` = GUID, který poslal volající. `NULL` dává
  `GUID_NULL`. Neověřený, takže dokazuje jen „naše / cizí“.
- Jaký GUID posílá Windows (Nastavení, ovládací panel Zvuk, okno hlasitosti, klávesy hlasitosti),
  se nikde nepíše. Je potřeba změřit.
- Nástroje typu MicLock nebo MicVolumeLocker nic neblokují, jen hlasitost vracejí zpátky.
- „Povolit aplikacím převzít výhradní řízení“ s hlasitostí podle všeho nesouvisí (lidová rada).

## Odvozeno z kódu

- Náš mikrofonní efekt sedí v MFX nebo EFX (`win_audio/apo_install/`), tedy ve společné části pro
  všechny streamy. Hlasitost jednoho streamu (Chrome) se proto nejspíš uplatní až za ním. Leveler o ní
  neví, ale ani nemusí: je to výstupní hlasitost té jedné aplikace.
- Náš limiter drží špičky pod plnou úrovní. Clipnuté rámce, kvůli kterým WebRTC hlasitost stahuje,
  za naším řetězcem nevzniknou.
- Kdo mikrofon právě používá, už víme (`mic_usage.rs`, ConsentStore: název exe a čas začátku).

## Hypotézy k ověření na Windows

1. **H1:** Hlasitost streamu Chrome se uplatní za naším efektem (MFX/EFX). Ověření: Meet v Chrome,
   chvíli mluvit potichu a sledovat, jestli se mění `mic_input_gain_db` a zisk leveleru. Neměly by.
2. **H2:** Windows UI posílá pevný GUID nebo `GUID_NULL`. Klávesy hlasitosti a posuvník se dají
   poznat podle vstupu uživatele těsně předtím (`GetLastInputInfo`, do ~1,5 s).
3. **H3:** Teams a Zoom posílají každý svůj stálý GUID (nebo `GUID_NULL`) a hlasitost mění bez
   vstupu uživatele, jen když nahrávají.
4. **H4:** Teams a Zoom mají mrtvé pásmo podobně jako WebRTC. Při hlasu na -24 dBFS po leveleru
   hlasitost po ustálení nechají být.
5. **H5:** Teams nebo Zoom, kterým hlasitost hned vrátíme, to převezmou jako ruční změnu (jako
   WebRTC) a přestanou. Nebudou ji tlačit pořád dokola.

## Návrh detekce

1. **Callback místo dotazování.** Služba zaregistruje `IAudioEndpointVolumeCallback` na mikrofonu,
   který se používá. Každá naše změna (automatika, okno, flyout, oprava ze zdraví) pošle vlastní
   konstantní GUID. `OnNotify` s naším GUID = naše, vždy a hned.
2. **Cizí změnu rozdělit** (čistá funkce, testovatelná):
   - **uživatel:** vstup z klávesnice nebo myši v posledních ~1,5 s (`GetLastInputInfo`), nebo GUID,
     který jsme změřili u Windows UI;
   - **aplikace:** bez vstupu a nějaká aplikace právě nahrává. Když nahrává jen jedna, je to ona a
     její GUID si zapamatujeme. Příště ji poznáme hned, i když nahrává víc aplikací;
   - **neznámo:** ostatní (bere se jako uživatel, tedy opatrně).
3. **Reakce, když hlasitost Windows je jen vstup:**
   - aplikace → hned vrátit naši hodnotu a jednou ukázat varování s jejím jménem a názvem volby
     („Teams mění hlasitost mikrofonu. Vypni v Teams ‚Automatically adjust mic sensitivity‘.“);
   - když aplikace vrací hlasitost víc než ~5× za minutu (H5 neplatí), přestat vracet, nechat varování
     a hlasitost nechat jí;
   - uživatel → nová vstupní citlivost, automatika 30 s počká (jako dnes).
4. **Limity:** bez PID se aplikace s `GUID_NULL` od sebe neodliší. Uživatel, který zrovna píše, když
   aplikace mění hlasitost, se započítá jako uživatel. Další změnu aplikace bez psaní ale chytíme.

## Výchozí cíl

Nechat -24 dBFS. Je uvnitř mrtvého pásma WebRTC (-50 až -12) a blízko výstupu jeho digitální AGC
(-17 až -30), takže aplikace postavené na WebRTC hlasitost neposouvají. Pro Teams a Zoom rozhodne
měření H4. Pokud tam vyjde užší pásmo, výchozí cíl posuneme do něj.

## Postup

1. **Jen logování (žádná změna chování):** callback + vlastní GUID u našich změn. Každá změna hlasitosti
   mikrofonu dá jeden řádek `[MicVol]`: hodnota předtím a potom, GUID, „naše / cizí“, kolik ms od
   posledního vstupu uživatele, které aplikace nahrávají. Jen při změně, žádný periodický výpis.
2. **Měření na Windows (člověk):**
   - Windows: posuvník v Nastavení, ovládací panel Zvuk, okno hlasitosti, klávesy hlasitosti → H2;
   - naše okno a flyout → vše „naše“;
   - Teams hovor s automatikou zapnutou, 5 min mluvení, cíl -30 / -24 / -18 → H3, H4;
   - totéž Zoom; Discord desktop s AGC zapnutou; Meet v Chrome → H1, H3;
   - u Teams a Zoomu jednou ručně vrátit hlasitost a sledovat, co udělají → H5.
3. **Vyhodnocení logu:** tabulka GUID → zdroj, pásmo, kde Teams a Zoom nechají hlasitost být.
4. **Rozhodnutí:** jestli H3 a H5 platí, přestavět na „hlasitost Windows = jen vstup“: rozdělení změn
   (krok 2 návrhu), vracení s varováním, odstranit přání za levelerem (`mic_volume_permille` ve fázi 5,
   `mic_hw_auto`, `mic_auto_wish_permille`, `adopt_mic_auto`), mixer a flyout: posuvník mikrofonu =
   cíl leveleru. Jinak nechat dnešní návrh a jen nahradit poznávání našich změn callbackem a GUID.
5. **Testy:** rozdělení změn jako čistá funkce (vstup, GUID, nahrávající aplikace → uživatel / aplikace
   / neznámo), vracení a jeho limit za minutu.

## Co nevíme

- Kde přesně Windows uplatní hlasitost streamu při záznamu vůči MFX/EFX (H1).
- Jestli Discord desktop mění hlasitost ve Windows.
- Cíle Teams a Zoomu a jak reagují na vrácenou hlasitost.
- Jestli klávesy hlasitosti a ovládání přes vzdálenou plochu `GetLastInputInfo` vidí.
