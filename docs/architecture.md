# Architecture et feuille de route

## Ce qu'on sait du protocole

Sources : le fil « FLIR ONE (gen2) USB protocol » du forum EEVblog, le
pilote Linux `flirone-v4l2` qui en est issu, les notes de Miso98/hw-flir-one-gen3,
et nos propres captures sur une Gen 3 USB-C (confirmé : même ID, même
séquence de démarrage, mais format thermique différent).

**Identifiant USB** : `09cb:1996`.

**Interfaces**

| interface | endpoints          | rôle                                      |
|-----------|--------------------|-------------------------------------------|
| 0         | 0x81 in, 0x02 out  | configuration / état (JSON)               |
| 1         | 0x83 in, 0x04 out  | fichiers (dont `CameraFiles.zip`)         |
| 2         | 0x85 in, 0x06 out  | flux de trames                            |

Particularité : les interfaces 1 et 2 déclarent l'alt 1 (sans endpoints)
**avant** l'alt 0 (avec endpoints).

**Démarrage** : requêtes standard SET_INTERFACE (bmRequestType 0x01,
bRequest 0x0B). Séquence : (alt 0, iface 2), (0, 1), (1, 1), puis (1, 2)
avec 2 octets de données `[0, 0]`, puis lire 0x85 en continu. La caméra
n'émet sur 0x85 que si elle se croit en alt 1, alors que l'hôte doit rester
sur l'alt 0 (le seul qui a des endpoints). Il faut donc envoyer la requête
en transfert de contrôle **brut**, sans que l'OS change réellement d'alt.

**Trame** (0x85) : en-tête de 28 octets little-endian commençant par
`EF BE 00 00`, avec à l'offset 8 la taille utile, 12 la taille thermique,
16 la taille JPEG, 20 la taille du texte d'état. Puis :

1. bloc thermique :
   - **Gen 3** : 10 332 octets = 63 paquets Lepton VoSPI de 164 octets
     (2 octets d'ID dont `id & 0x0fff` = numéro de ligne, 2 octets de CRC,
     80 pixels `u16` LE). Paquets 0–59 = lignes de l'image 80×60,
     60–62 = télémétrie. Valeurs brutes typiques 3400–3950 à température
     ambiante.
   - Gen 2 : 160×120, lignes de 164 mots avec un trou de 2 mots entre les
     deux moitiés de 80 pixels (non testé ici, décodage conservé) ;
2. image visible JPEG 1440×1080 (~46 Ko) ;
3. texte d'état JSON (`shutterState`, `ffcState`, `shutterTemperature`…).

**Radiométrie** : modèle de Planck de FLIR,
`T = B / ln(R1 / (R2·(S+O)) + F) − 273,15`, avec `S = 4,91 × brut + c`
corrigé de l'émissivité et de la température réfléchie. Le décalage `c` est
recalculé à chaque trame à partir du « spotmeter » du Lepton (voir plus bas).

Les constantes propres à la caméra sont dans `CameraFiles.zip`, lu par
`Camera::read_file` : commande `openFile` sur l'endpoint de config (0x02),
réponse `openFileStatus` sur 0x81 avec l'identifiant de flux, puis
`readFile` répété ; les données arrivent sur 0x83 avec un en-tête de
6 mots (magie 0x5510). Le zip contient `system/calib.rsc` (texte). Le cas
actif est `.calib.ccase.0.ID` (sur la Gen 3 testée :
`ds120C_weRB_apNOA_fiNOF_leExFOL2`) et ses constantes sont sous
`.calib.extRBF.<ID>` : R, B, F, J0, J1, avec R1 = R, R2 = 1/J1, O = −J0.
La convention de signe de J0 est confirmée par la mesure ci-dessous.

**Télémétrie Lepton et référence de température** (analyse du 11/10/2026
sur 10 trames enregistrées) : les 3 paquets VoSPI 60–62 sont la télémétrie
du Lepton (mots 16 bits LE). Ligne A : mot 22 = moyenne de l'image, mot 24 =
température du capteur (K × 100), mot 26 = température du boîtier. Ligne C
(paquet 62) : mots 50/51/52 = **température moyenne/max/min mesurée par le
Lepton lui-même** sur une petite zone (K × 100), mots 54–57 = cette zone
(lignes 29–30, colonnes 39–40, le centre). Paquet 61, mots 19–26 : paramètres
radiométriques du Lepton (émissivité 1,0, fond 22 °C).

Vérifications :
- obturateur fermé (FFC) : le Lepton lit 34,4 °C, le capteur
  `shutterTemperature` dit 34,9 °C ; l'ancien calcul « brut × 4 » donnait
  41,4 °C ;
- doigt au centre : Lepton 35,5 °C, ancien calcul 42,3 °C ;
- ajustement brut → Lepton sur 26–35 °C (avant et après FFC) : facteur
  **4,91** (et non 4), écart résiduel < 0,01 °C.

Les comptes bruts dérivent avec la température du capteur et sautent à
chaque FFC (−270 comptes sur une même scène). Le logiciel cale donc le
décalage `c` à chaque trame pour que les pixels du centre donnent la valeur
du Lepton (`Calibration::anchored`), lissé, appliqué d'un coup après un
saut. Reste à vérifier le facteur 4,91 loin de 35 °C : eau glacée (0 °C) et
eau bouillante (≈ 98 °C, émissivité 0,96).

## Ce qu'on a appris sur la Gen 3 (tests du 10/10/2026)

- Message `sledInformation` reçu sur 0x81 : firmware « Rosebud » 1.0.27,
  Lepton 1.0.30, `formFactor: dongle`, **`thermalWidth: 80`,
  `thermalHeight: 60`**, little-endian. Le décodage doit lire la taille
  dans ce message au lieu de supposer 160×120.
- En-têtes des messages (d'après Miso98/hw-flir-one-gen3, `NOTES.txt`) :
  mots 32 bits LE, magie `0x1cc` (config), `0x5510` (fichiers), `0xbeef`
  (trames) ; 2e mot = 1 ; 3e = taille ; dernier = CRC-32 standard des mots
  précédents (vérifié sur nos captures).
- Même séquence de démarrage que la Gen 2.
- Windows, comportement des pilotes face au SET_INTERFACE brut (vérifié
  avec USBPcap) :
  - WinUSB : avale la requête (répond OK, rien ne part sur le câble) ;
  - libusb-win32 : la convertit en SELECT_INTERFACE, ce qui supprime les
    pipes de l'hôte ;
  - WSL2/usbipd : même problème côté Windows, 0x85 ne renvoie que des zéros ;
  - **UsbDk : transmet la requête telle quelle. C'est la solution retenue**
    (10 trames reçues le 10/10/2026, ~3 i/s dans l'outil de test).
- Bloc thermique Gen 3 = paquets VoSPI (voir plus haut), décodage validé
  visuellement : `docs/premieres-images-gen3.png`.
- Bug libusb Windows corrigé dans `vendor/libusb1-sys` : l'alt était cherché
  par index au lieu de `bAlternateSetting` (la caméra liste l'alt 1 avant
  l'alt 0).

## Découpage du code

```
flir-viewer (egui)          ← UI, palettes, mesures
    │  canal (dernière trame seulement)
thread caméra ── flir-usb   ← rusb/libusb (UsbDk) : ouverture, SET_INTERFACE, lecture 0x85
                     │
                 flir-proto ← trames, décodage thermique, °C (sans I/O)
```

Choix techniques :

- **`rusb`** (libusb compilée avec le projet, patch dans `vendor/`) :
  c'est la seule voie qui donne accès au backend UsbDk sous Windows.
  `nusb` (pur Rust) a été essayé puis abandonné car il passe par WinUSB.
- **`eframe`/`egui`** : une seule base de code pour les trois OS, simple
  pour afficher des textures mises à jour à chaque trame.
- **`flir-proto` sans I/O** : tout ce qui est délicat se teste sans
  caméra, à partir de trames enregistrées avec l'exemple `dump`.

## Jalons

1. ~~**Premier contact matériel (Windows)**~~ : fait (UsbDk, décodage
   VoSPI 80×60).
2. ~~**Visionneuse utilisable**~~ : fait (palettes, point chaud, points de
   mesure, déconnexion propre).
3. ~~**Calibration réelle**~~ : fait (constantes lues dans
   `CameraFiles.zip`, émissivité, offset).
4. ~~**Analyse d'images**~~ : fait pour l'essentiel (captures PNG/CSV,
   vidéo, diagnostic par injection, superposition sur la photo).
5. ~~**Multiplateforme**~~ : Windows et Linux validés, paquets `.exe` et
   `.deb`. macOS abandonné : il ne peut pas recevoir le flux (voir ci-dessous).
6. **Pont réseau (plus tard)** : un intermédiaire branché à la caméra
   (Raspberry Pi ou PC Linux, ou PC Windows avec UsbDk) lit le flux et
   l'envoie par le réseau à la visionneuse, qui peut alors tourner sur un
   Mac. Demandé par Cyrille le 11/10/2026, à faire quand un Raspberry Pi
   sera disponible.

### macOS : pourquoi le flux ne passe pas (tests du 11/10/2026)

La caméra n'émet que dans le réglage alternatif 1 des interfaces 1 et 2,
que ses descripteurs déclarent sans endpoint. Sous Linux et avec UsbDk, la
requête SET_INTERFACE part telle quelle et l'hôte garde les endpoints du
réglage 0. macOS l'intercepte à tous les niveaux : libusb/IOKit bascule
réellement l'interface (les pipes 0x83/0x85 disparaissent), et un pilote
DriverKit (essai avec SIP désactivé) se voit refuser la requête brute
(kIOReturnNotPermitted), tandis que `SelectAlternateSetting` détruit les
pipes. Un compte Apple Developer n'y changerait rien.

## Questions résolues

- Faut-il vider les endpoints 0x81/0x83 pour que le flux 0x85 continue ?
  Non pour 0x83 : la visionneuse ne le lit jamais pendant le flux et tourne
  sans interruption depuis le 10/10/2026 (sessions longues, vidéo). 0x81 est
  lu en continu par le fil d'état (batterie, `sledInformation`), donc le cas
  « 0x81 jamais lu » n'a pas été testé, mais il n'a pas d'intérêt.
- Le facteur ×4 sur les valeurs brutes vaut-il pour la Gen 3 ? Non : 4,91,
  avec un décalage calé sur la mesure du Lepton (voir « Télémétrie Lepton »).
  À confirmer à 0 °C et vers 100 °C.
