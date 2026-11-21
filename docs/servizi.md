# Servizi utilizzabili con cloudraid

Come configurare ogni servizio come spazio (backend) di cloudraid. Gli spazi si aggiungono dall'interfaccia web, pagina
**Spazi** → **Aggiungi uno spazio**. Prima di salvare, cloudraid prova lo spazio: crea la cartella, scrive, rilegge,
elenca e cancella un file di prova. Se qualcosa non va, l'interfaccia spiega cosa e non salva niente.

La prova cancella il file e la cartella che ha creato: sul servizio resta solo la cartella del repository. Le versioni
fino alla 0.9 lasciavano una cartella vuota `prova`: la toglie **Pulisci** (pulizia dei frammenti orfani, nella
pagina Manutenzione), oppure si può cancellare a mano.

Il nome di uno spazio si può cambiare in ogni momento con **Rinomina**: cambia solo il nome con cui cloudraid lo chiama,
i dati restano dove sono e la cartella sul servizio non cambia nome.

Password, chiavi segrete e token non finiscono mai in `cloudraid.toml`. Vanno in `config/credenziali.bin`, cifrati con
la chiave del repository.

Piani gratuiti verificati il **21/11/2026**. Cambiano spesso, quindi controllali sul sito del servizio prima di contarci.

## Prima di cominciare

- **Lo spazio più piccolo limita gli altri.** Ogni blocco diventa 6 frammenti (4 di dati e 2 di parità), ciascuno su
  uno spazio diverso. Con 6 spazi esatti, la capacità utile è circa 4 volte quella dello spazio più piccolo. Con più di
  6 spazi il ribilanciamento distribuisce meglio, e quelli piccoli pesano meno.
- **Servizi che chiedono la carta di credito** (Cloudflare R2, Oracle, IDrive e2, Google Cloud Storage). Oltre la quota
  gratuita fanno pagare, e oggi cloudraid **non ha ancora un limite di spazio per servizio**: tieni d'occhio
  l'occupazione nella pagina Spazi.
- **Un solo account per servizio.** Più account sullo stesso provider spesso violano le condizioni d'uso. Soprattutto,
  non danno ridondanza vera: se quel provider ha un guasto, li perdi tutti insieme.
- **Non toccare i file sugli spazi.** Nella cartella `cloudraid` di ogni servizio ci sono frammenti cifrati con nomi
  incomprensibili: non vanno modificati, spostati o cancellati a mano. Per svuotare uno spazio si usa **Dismetti**.
- **Dopo aver aggiunto uno spazio** premi **Ribilancia ora**: una parte dei dati esistenti viene spostata sullo spazio
  nuovo.

## Riepilogo

| Servizio | Gratis | Tipo nell'interfaccia | Carta | Provato con cloudraid |
|---|---|---|---|---|
| [Disco o cartella locale](#disco-o-cartella-locale) | dipende dal disco | Cartella su questo server | no | sì |
| [Koofr](#koofr) | 10 GB | Nextcloud o WebDAV | no | da provare (WebDAV provato con un altro server) |
| [Nextcloud / ownCloud](#nextcloud-e-owncloud) | dipende dal fornitore | Nextcloud o WebDAV | - | WebDAV provato con un altro server |
| [Backblaze B2](#backblaze-b2) | 10 GB | Archivio S3 | no | da provare (S3 provato con un altro server) |
| [Cloudflare R2](#cloudflare-r2) | 10 GB al mese | Archivio S3 | sì | da provare |
| [Oracle Cloud](#oracle-cloud-always-free) | 20 GB (da confermare) | Archivio S3 | sì | da provare |
| [IDrive e2](#idrive-e2) | 10 GB | Archivio S3 | sì | da provare |
| [Google Cloud Storage](#google-cloud-storage) | 5 GB | Archivio S3 | sì | da provare |
| [Filebase](#filebase) | 5 GB | Archivio S3 | no | da provare |
| [Google Drive](#google-drive) | 15 GB, in comune con Gmail e Foto | Google Drive | no | solo contro un server finto |
| [OneDrive](#onedrive) | 5 GB | OneDrive | no | solo contro un server finto |
| [Dropbox](#dropbox) | 2 GB | Dropbox | no | solo contro un server finto |

Legenda della colonna "Provato con cloudraid":
- **sì**: provato dal vivo.
- **WebDAV / S3 provato con un altro server**: il protocollo è stato provato dal vivo, quel servizio in particolare no.
- **solo contro un server finto**: provato nei test automatici, contro un server che imita le API del servizio, ma mai
  con un account vero.

---

## Disco o cartella locale

Tipo **Cartella su questo server** (`type = "local"`).

- La cartella deve stare dentro `/mnt/cloudraid/dischi`, l'unica cartella dell'host per i dischi montata nei container.
  Esempio: `/mnt/cloudraid/dischi/disco1`.
- Per usare un altro disco fisico (USB o un secondo SSD), montalo in una sottocartella, per esempio
  `/mnt/cloudraid/dischi/usb1`, e aggiungilo come cartella.
- Più cartelle sullo stesso disco fisico **non** proteggono da un guasto di quel disco. Vanno bene per le prove. Per una
  protezione vera, ogni spazio deve stare su un disco diverso o su un servizio diverso.
- Questi dati non entrano nel backup orario di `/srv/docker`: la loro protezione è la ridondanza di cloudraid.

## Koofr

Europeo (Slovenia), 10 GB gratis per sempre, accesso WebDAV ufficiale.

1. **Crea una password per app.** Koofr non accetta la password dell'account per WebDAV (dal 2021).
   1. Entra su <https://app.koofr.net>.
   2. **Preferenze** → **Password**, nel menu a sinistra.
   3. In fondo, nella sezione **App passwords**, scrivi un nome (es. `cloudraid`) e premi **Generate**.
   4. Copia la password: Koofr la mostra una volta sola.
2. **Aggiungi lo spazio.** Spazi → Aggiungi uno spazio → **Nextcloud o WebDAV**:

   | Campo | Valore |
   |---|---|
   | Nome dello spazio | `koofr` |
   | Indirizzo della cartella | `https://app.koofr.net/dav/Koofr/cloudraid` |
   | Utente | l'email dell'account Koofr |
   | Password | la password per app del punto 1 |

Nell'indirizzo, `Koofr` (con la K maiuscola) è lo spazio da 10 GB dell'account; `cloudraid` è la cartella che cloudraid
crea da solo. Se compare "accesso negato", hai usato la password dell'account invece di quella per app, oppure l'hai
copiata male.

Configurazione equivalente in `cloudraid.toml`:

```toml
[[backend]]
name = "koofr"
type = "webdav"
url = "https://app.koofr.net/dav/Koofr/cloudraid"
user = "tua-email@esempio.it"
```

## Nextcloud e ownCloud

Tipo **Nextcloud o WebDAV**. Vale per un Nextcloud tuo, di un amico o di un fornitore.

1. In Nextcloud: **Impostazioni personali** → **Sicurezza** → **Crea nuova password per app**. Copiala.
2. Compila così:
   - **Indirizzo della cartella:** `https://<server>/remote.php/dav/files/<utente>/cloudraid`;
   - **Utente:** il nome utente di Nextcloud (lo stesso che compare nell'indirizzo);
   - **Password:** la password per app.

Per ownCloud e altri server WebDAV (NAS Synology o QNAP) l'indirizzo cambia: usa quello che il server indica per WebDAV,
seguito da `/cloudraid`. La cartella che contiene `cloudraid` deve esistere già: se manca, cloudraid lo segnala.

## Backblaze B2

10 GB gratis, senza carta di credito. Download gratis fino a 3 volte lo spazio occupato al mese.

1. Crea l'account su <https://www.backblaze.com/sign-up/cloud-storage>.
2. **Buckets** → **Create a Bucket**:
   - **nome:** unico al mondo, per esempio `cloudraid-` seguito da qualcosa di tuo;
   - **file privati** (*Private*);
   - **cifratura e Object Lock** disattivati.
3. Nella scheda del bucket copia l'**Endpoint** (es. `s3.eu-central-003.backblazeb2.com`). La regione è la parte dopo
   `s3.`: `eu-central-003`.
4. **Application Keys** → **Add a New Application Key**:
   - accesso solo al bucket appena creato;
   - **Read and Write**.

   Copia **keyID** e **applicationKey**: la seconda viene mostrata una volta sola.
5. Spazi → Aggiungi uno spazio → **Archivio S3**:

   | Campo | Valore |
   |---|---|
   | Endpoint | `https://s3.eu-central-003.backblazeb2.com` (con `https://`) |
   | Regione | `eu-central-003` |
   | Bucket | il nome del bucket |
   | Access key | il **keyID** |
   | Secret key | la **applicationKey** |

## Cloudflare R2

10 GB al mese gratis, download sempre gratis. **Serve la carta di credito** per attivare R2.

1. Nella dashboard Cloudflare: **R2** → attiva il servizio → **Create bucket** (es. `cloudraid`).
2. Copia l'**Account ID** (nella pagina principale di R2).
3. **Manage R2 API Tokens** → **Create API token**:
   - permesso **Object Read & Write**, limitato al bucket;
   - copia **Access Key ID** e **Secret Access Key**.
4. Compila così:
   - **Endpoint:** `https://<ACCOUNT_ID>.r2.cloudflarestorage.com`;
   - **Regione:** `auto`;
   - **Bucket, Access key, Secret key:** quelli dei punti precedenti.

## Oracle Cloud (Always Free)

Spazio di Object Storage gratuito per sempre: 20 GB secondo fonti non ufficiali, da confermare sulla pagina Always Free
di Oracle. **Serve la carta di credito** all'iscrizione. Oracle ha un'API compatibile S3 ufficiale.

1. Crea l'account su <https://www.oracle.com/cloud/free/> e scegli la **regione di casa**, per esempio `eu-milan-1`.
   Non si cambia dopo.
2. **Storage** → **Buckets** → **Create Bucket**, per esempio `cloudraid`, livello *Standard*.
3. Trova il **namespace**: menu del profilo → **Tenancy** → *Object Storage Namespace*.
4. Crea le chiavi: menu del profilo → **My profile** → **Customer secret keys** → **Generate secret key**. Copia la
   chiave segreta, mostrata una volta sola, e l'**Access key** che compare nell'elenco.
5. Compila così:
   - **Endpoint:** `https://<namespace>.compat.objectstorage.<regione>.oraclecloud.com`;
   - **Regione:** la regione di casa, es. `eu-milan-1`;
   - **Bucket, Access key, Secret key:** quelli dei punti precedenti.

## IDrive e2

10 GB gratis con un piano mensile attivo. **Serve la carta di credito.** Download gratis fino a 3 volte lo spazio
occupato.

1. Nella console di IDrive e2, attiva una regione europea (per esempio Francoforte o Parigi) e crea un bucket.
2. **Access Keys** → **Create Access Key**, con accesso al bucket. Copia Access Key ID e Secret Access Key.
3. Compila così:
   - **Endpoint:** l'indirizzo che la console mostra per la regione (con `https://`);
   - **Regione:** il codice della regione mostrato dalla console;
   - **Bucket, Access key, Secret key:** quelli dei punti precedenti.

## Google Cloud Storage

5 GB gratis al mese. Non è Google Drive: è l'archivio a oggetti di Google Cloud. **Serve la carta di credito.** La quota
gratuita vale solo in alcune regioni statunitensi (per esempio `us-east1`, `us-west1` e `us-central1`, da verificare
sulla pagina dei prezzi). Non ancora provato con cloudraid: in teoria funziona, perché Google offre un'API compatibile
S3 con chiavi HMAC.

1. In <https://console.cloud.google.com>: **Cloud Storage** → **Buckets** → **Create**, in una regione con quota
   gratuita, classe *Standard*.
2. **Cloud Storage** → **Settings** → **Interoperability**:
   - imposta il progetto predefinito;
   - crea una **chiave HMAC** (meglio per un service account con accesso solo a quel bucket);
   - copia Access key e Secret.
3. Compila così:
   - **Endpoint:** `https://storage.googleapis.com`;
   - **Regione:** `auto`;
   - **Bucket, Access key, Secret key:** quelli dei punti precedenti.

## Filebase

5 GB gratis, senza carta, ma con soli **5 GB di download al mese**: superata quella soglia, i download vengono rifiutati
fino al mese dopo. Va bene come spazio di riserva, non per un repository letto spesso. Un solo bucket nel piano
gratuito.

1. Su <https://filebase.com>: crea un bucket e, in **Access Keys**, copia Key e Secret.
2. Compila così:
   - **Endpoint:** `https://s3.filebase.com`;
   - **Regione:** `us-east-1`;
   - **Bucket, Access key, Secret key:** quelli del punto precedente.

## Altri servizi S3

Per AWS, Wasabi, MinIO e altri servizi compatibili S3 servono l'endpoint del servizio, la regione, il bucket e una coppia
di chiavi con permessi di lettura e scrittura su quel bucket. Per MinIO e simili come regione va bene `us-east-1`.

Due opzioni si impostano solo in `cloudraid.toml`:
- `prefix`: per usare solo una parte del bucket;
- `path_style = false`: per i servizi che vogliono indirizzi nella forma `bucket.endpoint`. Quasi tutti accettano
  quella predefinita, `endpoint/bucket`.

---

## Google Drive, OneDrive e Dropbox

Questi tre servizi usano un login (OAuth), quindi cloudraid deve essere registrato come applicazione presso ciascuno.
Lo fai una volta sola, dal tuo account. Poi metti i valori in Spazi → **Collegamento con Google, Microsoft e Dropbox**:
finiscono nella sezione `[apps]` di `cloudraid.toml` e non sono password. La stessa finestra dell'interfaccia contiene
le guide qui sotto.

Ti consiglio di cominciare da **Dropbox**: bastano 5 minuti e nessuna verifica.

### Dropbox

2 GB gratis. cloudraid vede solo la propria cartella `Applicazioni/<nome app>`.

1. Registrazione dell'app (una volta):
   1. Apri <https://www.dropbox.com/developers/apps> → **Create app**.
   2. Scegli **Scoped access** e **App folder**, con un nome per esempio `cloudraid-casa`.
   3. Scheda **Permissions**: spunta `files.content.write`, `files.content.read` e `files.metadata.read`, poi premi
      **Submit** in fondo alla pagina. Ricarica la pagina e controlla che le spunte siano rimaste: senza Submit non
      vengono salvate.
   4. Scheda **Settings**: copia la **App key** nell'interfaccia. L'App secret non serve.
2. Aggiunta dello spazio:
   1. Spazi → Aggiungi uno spazio → **Dropbox** → **Collega l'account**.
   2. Si apre la pagina di Dropbox: acconsenti e copia il codice che compare.
   3. Incollalo nella finestra di cloudraid.

| Errore | Causa e rimedio |
|---|---|
| `does not have the required scope 'files.content.write'` | all'app manca il permesso. Abilitalo nella scheda Permissions, premi **Submit**, poi ricollega l'account dall'inizio, con un codice nuovo: i permessi vengono fissati nel token al momento del collegamento |

### OneDrive

5 GB gratis. cloudraid vede solo la propria cartella `App/<nome app>` (in inglese `Apps/<nome app>`).

**Prima di cominciare: serve una "directory".** Microsoft non permette più di registrare app nuove con un account
personale "da solo": l'app deve stare in una directory (tenant) Microsoft Entra
([Microsoft Learn](https://learn.microsoft.com/en-us/entra/identity-platform/reference-breaking-changes)).
- Se il tuo account ne ha già una (per esempio perché hai usato Azure), la registrazione funziona e basta.
- Se compare un errore del tipo *"creating applications outside of a directory..."*, crea una directory gratuita
  attivando un account Azure gratuito (azure.microsoft.com) con lo stesso account Microsoft. Chiede la carta di
  credito solo per verificare l'identità.
- Non usare la directory del lavoro: l'app finirebbe sotto il controllo dell'azienda, e spesso gli utenti non possono
  crearne.

1. Registrazione dell'app (una volta):
   1. Apri <https://entra.microsoft.com> (o portal.azure.com) con il tuo account Microsoft personale.
   2. **Registrazioni app** → **Nuova registrazione**:
      - **Nome:** `cloudraid`. È il nome della cartella che comparirà nel tuo OneDrive: controlla di averlo scritto
        giusto (si può cambiare dopo, vedi sotto);
      - **Tipi di account supportati:** **"Account in qualsiasi directory organizzativa e account Microsoft
        personali"**. Senza "account Microsoft personali" non puoi collegare il tuo OneDrive;
      - **URI di reindirizzamento:** lascialo vuoto.
   3. **Autenticazione**: in fondo, alla voce **Consenti flussi client pubblici**, scegli **Sì** e premi **Salva**.
      Senza questo il collegamento con il codice non funziona.
   4. **Autorizzazioni API** → **Aggiungi un'autorizzazione** → **Microsoft Graph** → **Autorizzazioni delegate**:
      aggiungi `Files.ReadWrite.AppFolder` e `offline_access`. Non serve il "consenso amministratore".
   5. Dalla pagina **Panoramica** copia l'**ID applicazione (client)** (una stringa come `1a2b3c4d-...`) in Spazi →
      **Collegamento con Google, Microsoft e Dropbox**, campo OneDrive, poi **Salva**.
2. Aggiunta dello spazio:
   1. Spazi → Aggiungi uno spazio → **OneDrive**, nome per esempio `onedrive`, poi **Collega l'account**.
   2. cloudraid mostra un indirizzo (`microsoft.com/devicelogin`) e un codice (es. `ABCD-EFGH`). Aprilo da qualsiasi
      dispositivo, anche dal telefono, inserisci il codice ed entra con l'account **che possiede il OneDrive da
      usare**.
   3. Accetta i permessi: cloudraid vedrà solo la sua cartella `App/<nome app>`.
   4. La finestra si accorge da sola quando hai finito, fa la prova di scrittura e salva. Poi premi **Ribilancia ora**.

**Nome sbagliato?** Si sistema senza toccare cloudraid:
- **nome dell'app:** Registrazioni app → la tua app → **Personalizzazione e proprietà** (*Branding & properties*) →
  **Nome** → **Salva**. L'ID applicazione resta uguale, quindi non serve ricollegare niente;
- **cartella in OneDrive:** non cambia nome da sola. Rinominala da onedrive.live.com, per esempio da `App/cloudrain` ad
  `App/cloudraid`: Microsoft la riconosce dall'ID dell'app, non dal nome, quindi cloudraid continua a trovarla. Non
  spostarla e non toccare i file che contiene.

| Errore | Causa e rimedio |
|---|---|
| *"creating applications outside of a directory..."* durante la registrazione | manca una directory: vedi "Prima di cominciare" |
| `AADSTS7000218` o "client_assertion or client_secret" | manca **Consenti flussi client pubblici: Sì** (punto 3) |
| `AADSTS50020`, oppure "l'account non esiste nel tenant" | tra i tipi di account mancano gli "account Microsoft personali" (punto 2). Si cambia in **Manifesto** (`signInAudience` = `AzureADandPersonalMicrosoftAccount`) o rifacendo la registrazione |
| accesso negato o 403 in scrittura | manca `Files.ReadWrite.AppFolder` (punto 4): aggiungilo e ricollega l'account |

### Google Drive

15 GB gratis, in comune con Gmail e Google Foto. Con l'ambito `drive.file`, cloudraid vede solo i file che crea lui.

1. Registrazione dell'app (una volta):
   1. Apri <https://console.cloud.google.com/> e crea un progetto (es. `cloudraid`).
   2. **API e servizi** → **Libreria**: cerca **Google Drive API** e premi **Abilita**.
   3. **Schermata consenso OAuth** (o *Google Auth Platform*):
      - tipo **Esterno**, nome `cloudraid`, la tua email;
      - tra gli ambiti aggiungi `.../auth/drive.file`.
   4. **Pubblica** l'app (stato *In produzione*). Con il solo ambito `drive.file` non serve la verifica di Google. In
      modalità test, invece, i collegamenti scadono dopo 7 giorni.
   5. **Credenziali** → **Crea credenziali** → **ID client OAuth**, tipo **TV e dispositivi con input limitato**. Copia
      nell'interfaccia ID client e client secret.
2. Aggiunta dello spazio:
   1. Spazi → Aggiungi uno spazio → **Google Drive**.
   2. Scegli il nome della cartella nel Drive (predefinito `cloudraid`).
   3. Collega l'account con indirizzo e codice, come per OneDrive.

---

## Servizi non utilizzabili oggi

| Servizio | Gratis | Perché no |
|---|---|---|
| pCloud | fino a 10 GB | WebDAV solo nei piani Premium (dal 2022). Si potrebbe aggiungere con la sua API nativa, vedi sotto |
| Infomaniak kDrive | 15 GB | WebDAV solo nei piani a pagamento |
| Yandex Disk | 5 GB | WebDAV solo con Yandex 360 a pagamento, dal 22/06/2026 |
| Icedrive | 10 GB | WebDAV solo nei piani a pagamento |
| Box | 10 GB | API propria, non WebDAV né S3. Si potrebbe aggiungere, vedi sotto |
| MEGA | 20 GB | protocollo proprio con cifratura sua e limiti di trasferimento |
| Filen | 10 GB | SDK con cifratura propria; il suo WebDAV gira solo in locale, tramite il loro programma |
| Storj | - | il piano gratuito ora è una prova di 30 giorni |
| Scaleway | - | i 75 GB gratuiti valgono per il traffico in uscita, non per lo spazio |
| Amazon S3 | 5 GB | gratis solo per 12 mesi (funziona come S3, ma poi si paga) |
| Proton Drive, iCloud | 5 GB | nessuna API pubblica |
| Jottacloud, TeraBox, Internxt | | nessuna API affidabile o documentata, oppure piano gratuito troppo piccolo |

## Servizi che si potrebbero aggiungere

In ordine di convenienza:

1. **pCloud** (fino a 10 GB): API REST documentata con OAuth. Lavoro paragonabile a Dropbox.
2. **Box** (10 GB): API documentata, accessibile gratis solo con il *Free Developer Plan*. Il limite di 250 MB per file
   non conta, perché i frammenti sono di pochi MB.
3. **SFTP**: non è un servizio gratuito, ma permette di usare il server di un amico, un'altra macchina di casa o uno
   spazio di backup a pagamento come una Hetzner Storage Box.
4. **MEGA** (20 GB): molto spazio, ma il protocollo è il più complesso e i limiti di trasferimento lo rendono poco
   prevedibile.

Prima di questi, conviene aggiungere a cloudraid un **limite di spazio per servizio** (es. "B2: massimo 9 GB"): così
non si supera mai una quota gratuita, e i servizi con la carta di credito diventano sicuri da usare.
