# cloudraid

Archivio cifrato con ridondanza **Reed-Solomon** distribuito su più storage, come un RAID 5/6
in cui i "dischi" sono account diversi: cartelle locali, **Google Drive, OneDrive, Dropbox, WebDAV (Nextcloud) e S3**
(Backblaze B2, Wasabi, AWS, MinIO...). Un solo binario: parla direttamente con i servizi, senza programmi esterni.

Con lo schema predefinito **4 + 2** ogni blocco di dati diventa 4 frammenti di dati e 2 di parità,
ognuno su un backend diverso: si possono perdere **due backend qualsiasi** senza perdere niente,
occupando 1,5 volte lo spazio dei dati.

> **Stato: tutte le fasi previste completate, backend nativi senza rclone dalla 0.9 (prototipo).** Funziona e ha i test, ma è codice giovane: non usarlo come unica copia
> di dati importanti.

## Come funziona

```
file → blocchi a lunghezza variabile (FastCDC, ~4 MiB) → deduplicazione
     → cifratura XChaCha20-Poly1305 → Reed-Solomon k + m → un frammento per backend
```

- **Cifratura lato client.** I backend vedono solo frammenti illeggibili con nomi casuali. La chiave principale
  è protetta dalla password con Argon2id e salvata (cifrata) su ogni backend, in `cloudraid.key`, con una copia
  accanto alla configurazione.
- **Credenziali cifrate.** Password WebDAV, chiavi segrete S3 e token degli account stanno in `credenziali.bin`,
  accanto alla configurazione, cifrati con una chiave derivata da quella del repository: senza la password del
  repository non servono a niente. `cloudraid.toml` non contiene segreti.
- **Deduplicazione.** I blocchi uguali, anche in file diversi, vengono salvati una volta sola.
- **Integrità.** Ogni frammento ha il suo hash BLAKE3 nell'indice: un frammento alterato si tratta come
  mancante. Anche ogni blocco e ogni file vengono verificati dopo la ricostruzione.
- **Indice replicato.** L'indice (file, blocchi, posizione dei frammenti) è cifrato e copiato su tutti i backend,
  con le ultime 3 generazioni. All'apertura vince il più recente che si riesce a leggere: per ripartire da zero
  bastano la configurazione e la password.
- **Lettura parsimoniosa.** Si leggono prima i frammenti di dati; la parità solo se ne manca qualcuno.
- **In parallelo.** `transfers` blocchi alla volta (predefinito 4), ognuno con tutti i frammenti in parallelo.
- **Lock.** Chi modifica il repository (`put`, `rm`, `scrub --repair`) prende un lock esclusivo, scritto su tutti
  i backend e rinnovato ogni 5 minuti; un lock non rinnovato da 30 minuti è scaduto e viene ignorato.

## Backend

| Tipo | Che cosa | Configurazione | Credenziale (in `credenziali.bin`) |
|---|---|---|---|
| `local` | cartella o disco montato | `path` | nessuna |
| `webdav` | Nextcloud, ownCloud, NAS... | `url` della cartella, `user` | password (meglio una "password per app") |
| `s3` | AWS, Backblaze B2, Wasabi, MinIO... | `endpoint`, `region`, `bucket`, `access_key`, `prefix`, `path_style` | chiave segreta |
| `dropbox` | la cartella dell'app (`Applicazioni/<app>`) | `folder` facoltativa | token, dal collegamento dell'account |
| `onedrive` | la cartella dell'app (`Applicazioni/<app>`) | `folder` facoltativa | token, dal collegamento dell'account |
| `gdrive` | una cartella nel Drive, scope `drive.file` | `folder`, `folder_id` | token, dal collegamento dell'account |

**Istruzioni passo passo per ogni servizio** (Koofr, Nextcloud, Backblaze B2, Cloudflare R2, Oracle, Google Drive,
OneDrive, Dropbox...), con i piani gratuiti e i servizi non utilizzabili: [docs/servizi.md](docs/servizi.md).

Il modo semplice per aggiungere un backend è l'interfaccia web, pagina **Spazi**, "Aggiungi uno spazio": si sceglie il
servizio, si compilano pochi campi (o si collega l'account), cloudraid crea la cartella, scrive, rilegge, elenca e
cancella un file di prova, e **solo se tutto va bene** salva le credenziali cifrate e aggiunge il backend a
`cloudraid.toml` (conservandone i commenti; la versione precedente resta in `cloudraid.toml.bak`). Web e mount
passano alla configurazione nuova senza riavvio e senza chiedere di nuovo la password.

Come parla con ciascun servizio, tutto implementato in cloudraid (client HTTPS con rustls e certificati inclusi):

- **WebDAV**: PUT/GET/DELETE, MKCOL per le cartelle (create quando servono), PROPFIND con profondità 1 (Nextcloud non
  permette la profondità infinita), autenticazione Basic.
- **S3**: PutObject, GetObject, DeleteObject, ListObjectsV2 a pagine, firma **AWS Signature V4** (verificata con
  l'esempio della documentazione AWS e contro un server che controlla le firme).
- **Dropbox**: API v2, upload semplice (fino a 150 MB), `list_folder` anche ricorsivo; collegamento con codice da
  incollare e PKCE, senza segreti.
- **OneDrive**: Microsoft Graph nella cartella dell'app, indirizzi per percorso, download seguendo a mano il
  reindirizzamento (il token non va al sito di download); collegamento con codice dispositivo.
- **Google Drive**: API v3 con lo scope `drive.file` (cloudraid vede solo i file che crea lui), cache percorso →
  identificativo, cartelle create una alla volta, caricamento multipart fino a 4 MB e "resumable" oltre;
  collegamento con codice dispositivo.
- Per tutti: nuovi tentativi con attesa crescente (1, 2, 4, 8 s) su errori temporanei, 429 e limiti di velocità,
  rispettando `Retry-After`; token d'accesso rinnovati da soli, e salvato il refresh token nuovo quando il provider lo
  cambia (Microsoft lo fa a ogni rinnovo).

Un backend è **disponibile** se risponde e la cartella (o il bucket) del repository esiste. Uno irraggiungibile si
tratta come un disco guasto. Alla prima scrittura un backend nuovo riceve anche la chiave cifrata del repository;
se ne contiene già una di un **altro** repository, cloudraid non la tocca e avvisa.

### Registrare cloudraid presso Google, Microsoft e Dropbox (una volta sola)

Per collegare account di questi tre servizi, cloudraid deve essere registrato come applicazione: lo fai tu, una volta,
dal tuo account, e metti i valori in Spazi → "Collegamento con Google, Microsoft e Dropbox" (finiscono nella sezione
`[apps]` di `cloudraid.toml`; non sono password). La stessa finestra contiene queste guide passo passo:

- **Google Drive**: console.cloud.google.com → nuovo progetto → abilita *Google Drive API* → schermata di consenso
  OAuth di tipo *Esterno* con l'ambito `.../auth/drive.file` → **pubblica** l'app (con il solo drive.file non serve la
  verifica, e i collegamenti non scadono dopo 7 giorni come in modalità test) → Credenziali → ID client OAuth di tipo
  **TV e dispositivi con input limitato** → `google_client_id` e `google_client_secret`.
- **OneDrive**: entra.microsoft.com → Registrazioni app → nuova, account Microsoft personali → Autenticazione:
  **Consenti flussi client pubblici = Sì** → autorizzazioni Graph `Files.ReadWrite.AppFolder` e `offline_access` →
  `onedrive_client_id` (ID applicazione).
- **Dropbox**: dropbox.com/developers/apps → *Scoped access*, **App folder** → permessi `files.content.write`,
  `files.content.read`, `files.metadata.read` → `dropbox_app_key` (l'App secret non serve: PKCE).

In alternativa le variabili d'ambiente `CLOUDRAID_DROPBOX_APP_KEY`, `CLOUDRAID_ONEDRIVE_CLIENT_ID`,
`CLOUDRAID_GOOGLE_CLIENT_ID` e `CLOUDRAID_GOOGLE_CLIENT_SECRET`.

**Da rclone (fino alla 0.8)**: i backend `type = "rclone"` e la sezione `[rclone]` non sono più accettati (cloudraid lo dice
chiaramente all'avvio). Toglili dalla configurazione e riaggiungi gli stessi account dall'interfaccia; `rclone.conf` non
serve più.

## Interfaccia web

`cloudraid serve` avvia un'interfaccia di amministrazione: stato dei backend e della ridondanza, file (sfoglia, cerca,
carica anche trascinando, scarica, elimina), verifica e riparazione con l'avanzamento, lock, cronologia delle operazioni.

- **Accesso con la password del repository.** Il server parte bloccato e non salva la password da nessuna parte: il primo
  accesso apre il repository (con i backend cloud può richiedere qualche decina di secondi), che poi resta aperto in memoria
  fino al riavvio del container. Sessioni con cookie `HttpOnly`, `Secure`, `SameSite=Strict`: scadono dopo 1 ora di inattività
  e comunque dopo 12 ore. I tentativi sbagliati vengono rallentati.
- **Un'operazione alla volta**, in background (scrub, riparazione, caricamenti, eliminazioni), con il lock del repository come
  la riga di comando. Prima di scrivere il server rilegge l'indice: le modifiche fatte nel frattempo dalla riga di comando non
  vanno perse. Mentre qualcuno usa l'interfaccia, ogni 15 s controlla se il mount o la riga di comando hanno salvato un indice
  più recente: i file copiati nella cartella montata compaiono da soli. «Ricontrolla» rilegge backend e indice anche a mano.
- Lo scaricamento ricostruisce e decifra il file sul server; durante un'operazione lunga viene rifiutato (409).
- Protezioni: richieste di modifica solo con l'intestazione `X-Cloudraid: 1` (CSRF), Content-Security-Policy senza script né stili
  inline, nessuna libreria esterna.
- Cronologia delle operazioni in `config/web-state.json` (niente segreti).
- **Spazi**: aggiungere (con la prova, che alla fine cancella il file e la cartella `prova`), dismettere, togliere e
  **rinominare**. La rinomina cambia il nome nella configurazione, nelle credenziali e nell'indice. I frammenti restano
  dove sono, perché sui servizi il nome non compare. Mount e riga di comando rileggono la configurazione quando prendono
  il lock, così scrivono sempre con i nomi dell'indice.

Nel compose il servizio `web` sta dietro Traefik, **solo dalla LAN** (`lan-only@file`), su `https://<LAN_IP>/cloudraid/`
(certificato non valido per un IP: il browser avvisa, come per la dashboard di Traefik) e, se impostato, su
`https://<CLOUDRAID_HOST>/` (serve un nome che lo risolva: record DNS o file hosts).

## Cartella montata (FUSE)

```bash
cloudraid mount /mnt/cloudraid/archivio --allow-other --uid 1000 --gid 1000          # sola lettura
cloudraid mount /mnt/cloudraid/archivio --allow-other --uid 1000 --gid 1000 --rw     # anche scrittura
```

Il repository compare come una cartella normale: `ls`, `cp`, `mv`, `rm`, `mkdir`, file manager, lettori video,
editor, condivisione in rete.

**Lettura**
- Cartelle e dimensioni vengono dall'indice in memoria: `ls` e `find` non toccano i backend.
- Leggendo un file si scaricano solo i blocchi che servono, ognuno verificato con la sua impronta; con backend
  persi (fino a `parity_shards`) si legge con la parità, come `get`.
- I blocchi letti restano in una cache in memoria (`--cache-mb`, predefinito 256) e il blocco successivo viene chiesto
  in anticipo. Le richieste sono servite in parallelo (`--threads`), così una lettura lenta non blocca `ls`.
- Ogni `--refresh` secondi (predefinito 30) controlla se interfaccia web o riga di comando hanno salvato un indice
  più recente: i file nuovi compaiono da soli.
- Un file aperto in lettura resta com'era all'apertura anche se nel frattempo viene sostituito.

**Scrittura (`--rw`)**
- Un file aperto per scrivere vive in una **copia locale** nella cartella di appoggio (`--staging`, predefinita
  `cloudraid-staging` nella cartella temporanea): ci vuole spazio locale pari ai file in scrittura in quel momento.
  La copia è **in chiaro** e viene cancellata appena il file è caricato.
- Alla **chiusura** del file (o con `fsync`) la copia viene spezzata in blocchi, cifrata e caricata come fa `put`
  (con deduplicazione). La chiusura aspetta la fine del caricamento: con backend lenti `cp` resta fermo sull'ultimo
  file finché non è al sicuro, e un errore arriva al programma invece di perdersi.
- Modificare un file esistente senza riscriverlo da capo (aggiunta in coda, scrittura in mezzo, troncamento) lo
  scarica prima per intero nella copia locale.
- Rinominare e spostare (anche cartelle intere), cancellare, creare cartelle, cambiare la data di modifica: cambiano
  **solo l'indice**, i dati restano dove sono. Le cartelle vuote vengono salvate nell'indice e compaiono anche
  nell'interfaccia web.
- Ogni modifica prende il **lock** del repository. L'indice si salva **entro 2 secondi** dall'ultima modifica,
  come fa ext4 con i metadati: le modifiche vicine fanno un salvataggio solo (copiare centinaia di file piccoli non
  costa un giro su tutti i backend per file). `fsync` su un file o su una cartella (`sync -f`) lo salva subito.
  Se il processo muore in quella finestra, le ultime modifiche all'indice si perdono e i loro blocchi restano orfani.
- Il lock resta preso mentre le scritture continuano e viene lasciato dopo 5 secondi di calma (salvando prima),
  così interfaccia web e riga di comando possono tornare a scrivere.
  Se il lock è di un altro processo (per esempio una riparazione dall'interfaccia), la scrittura aspetta fino a un
  minuto e poi fallisce con "risorsa occupata".
- Permessi e proprietario non si salvano: `chmod` e `chown` vengono accettati e ignorati. Niente collegamenti
  (simbolici o fisici).
- Se il processo muore con file aperti in scrittura, quelle modifiche si perdono (al riavvio le copie locali
  rimaste vengono cancellate): il programma che scriveva non ha mai ricevuto la conferma della chiusura.

**In generale**
- Permessi: file 0440 e cartelle 0550 (0640 e 0750 con `--rw`) del proprietario scelto con `--uid`/`--gid`.
  Con `--allow-other` il kernel fa entrare anche gli altri utenti secondo questi permessi (root entra comunque).
- SIGTERM o Ctrl-C smontano in modo pulito; se il processo muore male, all'avvio successivo il mount appeso viene
  smontato da solo.

Nel compose c'è il servizio `mount`, facoltativo (profilo `mount`): metti in `.env` `COMPOSE_PROFILES=mount` e
`CLOUDRAID_PASSWORD` (all'avvio nessuno può digitarla), poi `docker compose up -d`. Il repository compare sull'host
in `/mnt/cloudraid/archivio`, accanto ai dischi locali in `/mnt/cloudraid/dischi`, in lettura e scrittura
(`CLOUDRAID_MOUNT_RW=false` per la sola lettura). Il container ha bisogno di `/dev/fuse`, del permesso `SYS_ADMIN`
e di uscire dal profilo AppArmor `docker-default`, che vieta `mount`.
Attenzione: `docker kill` è uno stop manuale, quindi il container non riparte da solo e lascia il mount appeso
finché non lanci `docker compose up -d`. Un crash vero, invece, fa ripartire il container da solo.

## Installazione sul server (Docker)

Il progetto sta in `/srv/docker/cloudraid`, come le altre app. L'immagine contiene solo cloudraid;
`compose.yaml` monta la configurazione (`./config`) e i backend locali (`/mnt/cloudraid/dischi`, fuori dal backup orario di `/srv/docker`).
cloudraid è un comando, non un servizio: si lancia con lo script `./cloudraid`, che monta anche la cartella corrente,
così i percorsi relativi funzionano come con un programma installato.

```bash
cd /srv/docker/cloudraid
make build                                   # costruisce l'immagine cloudraid:local
( umask 077; cp .env.example .env )          # poi controlla LAN_IP
install -d -m 700 config
cp cloudraid.example.toml config/cloudraid.toml   # per cominciare bastano cartelle locali
./cloudraid init
./cloudraid status
docker compose up -d web                     # interfaccia web: https://<LAN_IP>/cloudraid/
```

Poi gli account cloud si aggiungono dall'interfaccia (Spazi), e le cartelle locali si possono dismettere.
`init` crea il repository solo su backend che non chiedono credenziali (cartelle locali): le credenziali sono cifrate con
la chiave del repository, che ancora non esiste. Le cartelle che l'interfaccia può creare devono stare dentro
`/mnt/cloudraid/dischi` (`CLOUDRAID_LOCAL_ROOT`), l'unica cartella dell'host per i dischi montata nel container.
Per usare un altro disco fisico, montalo in una sottocartella (es. `/mnt/cloudraid/dischi/usb1`).

Facoltativo: `CLOUDRAID_PASSWORD` in `.env` (`chmod 600`, modello in `.env.example`) per gli usi non interattivi,
per esempio uno `scrub` periodico da cron.

`config/` e `.env` restano solo sul server (fuori da git) ed entrano nel backup orario di `/srv/docker`.

## Sviluppo

`make test` lancia i test in un container `rust:1-bookworm`. La cache di compilazione sta in `~/.cache/cloudraid-build`,
fuori da `/srv/docker`, così non finisce nei backup. I backend di rete si provano contro server finti in memoria che
seguono le API vere (`tests/cloud.rs`): WebDAV, S3 con la verifica della firma di ogni richiesta, Dropbox, OneDrive e
Google Drive con collegamento dell'account, rinnovo dei token, reindirizzamenti e limiti di velocità; sopra di loro un
repository vero, costruito come fa l'interfaccia web.
Il test del mount FUSE vero gira se c'è `/dev/fuse` (il Makefile passa al container dispositivo e permessi),
altrimenti viene saltato.

## Uso

```bash
# con Docker: /srv/docker/cloudraid/cloudraid al posto di cloudraid
cloudraid init                     # chiede la password due volte
cloudraid put ~/foto archivio/foto # file o cartelle
cloudraid ls archivio
cloudraid get archivio/foto /tmp/foto
cloudraid rm archivio/foto/vecchia.jpg
cloudraid status                   # backend disponibili, frammenti per backend
cloudraid scrub                    # verifica tutti i frammenti
cloudraid scrub --repair           # e ricostruisce quelli mancanti o danneggiati
cloudraid unlock                   # cancella i lock scaduti di processi interrotti
cloudraid gc --dry-run             # conta i frammenti orfani; senza --dry-run li cancella
cloudraid rebalance --dry-run      # piano di ribilanciamento; senza --dry-run sposta i frammenti
cloudraid retire account3          # dismissione: sposta altrove i frammenti di account3 (--undo per annullare)
cloudraid mount /mnt/cloudraid --rw  # il repository come cartella (vedi sopra)
```

`get`, `ls`, `status` e `scrub` senza `--repair` non prendono il lock: evita di lanciarli mentre un `rm`
o uno `scrub --repair` stanno cancellando o spostando frammenti.

La password si può passare anche con la variabile `CLOUDRAID_PASSWORD` (utile negli script).
**Senza password i dati non si recuperano in nessun modo**: salvala nel password manager.

### Letture "a gara"

Per ricostruire un blocco bastano k frammenti qualsiasi, di dati o di parità. cloudraid misura quanto ci mette ogni
backend a consegnare un frammento (media mobile, visibile nelle schede dei backend dell'interfaccia web) e chiede i
k frammenti ai più rapidi. Se una risposta tarda (il doppio del previsto, oppure tre volte quanto hanno impiegato le
risposte già arrivate) chiede un frammento in più a un altro backend e usa i primi k che arrivano; il backend che
tardava viene segnato subito come lento e lasciato per ultimo finché non migliora. Le richieste rimaste indietro finiscono
in sottofondo. Allo stesso modo la chiave e l'indice, all'apertura, si prendono dal primo backend che risponde.

Misura sul repository dimostrativo (7 cartelle, 27 MB) con una cartella rallentata di 1,5 s a lettura (`delay_ms`):
`get` di tutto in 0,7 s, come senza rallentamenti; quando il backend lento serve per forza (due cartelle spente)
15,8 s. Prima di questa versione ogni blocco con un frammento di dati sul backend lento lo aspettava.

`delay_ms` nella sezione di un backend (solo per prove) rallenta apposta ogni sua lettura.

### Spazi di capienza diversa

Ogni blocco va sui backend con **più spazio libero**, così quelli grandi lavorano di più e uno piccolo pieno non ferma
niente:

- **Spazio libero:** cloudraid lo legge dal servizio:
  - Dropbox `users/get_space_usage`;
  - OneDrive `/me/drive` (quota);
  - Google Drive `about.storageQuota`;
  - WebDAV con le proprietà RFC 4331 (`quota-available-bytes`);
  - per le cartelle locali, lo spazio libero del disco.

  Su ogni servizio lascia libero un margine di 64 MiB. S3 non dichiara lo spazio. Lo si richiede ogni 5 minuti e, nel
  frattempo, si contano le scritture fatte.
- **Limite facoltativo per backend:** `limit_gb = 9` in `cloudraid.toml`, oppure "Limite" in Spazi. È il massimo che
  cloudraid usa su quel servizio. Serve per S3 e per restare dentro una quota gratuita.
- **Cartelle sullo stesso disco:** si dividono la capienza del disco in parti uguali. Un blocco mette **al massimo m
  frammenti sullo stesso disco**, se si può: così un disco guasto non costa più della parità. Con 3 cartelle su un disco
  e 3 servizi cloud i blocchi diventano 3 + 2, 2 frammenti sul disco e 3 sul cloud, e sopravvivono al guasto del disco.
  Se non si può (per esempio, solo cartelle sullo stesso disco) si scrive senza questo vincolo. Il ribilanciamento
  sposta per primi i frammenti di troppo su un disco, quando c'è un backend su cui metterli. L'interfaccia avvisa delle
  cartelle sullo stesso disco.
- **Meno di k + m backend con posto** (per esempio uno spazio piccolo pieno, o uno spento): il blocco usa meno frammenti
  di dati, cioè 3 + 2, poi 2 + 2, poi 1 + 2 con lo schema 4 + 2. La parità resta la stessa, quindi si possono **sempre
  perdere m backend qualsiasi**; costa solo un po' di spazio in più (1,67x, 2x, 3x invece di 1,5x).

  L'indice registra k e m di ogni blocco, e lettura, verifica e riparazione li usano blocco per blocco: anche le
  versioni precedenti leggono questi blocchi.
- **Più di k + m spazi:** la capacità cresce meglio. Ogni blocco usa i 6 con più posto, e i servizi grandi ricevono
  più frammenti.
- **Quanto si può ancora caricare:** `status` e l'interfaccia mostrano una stima, calcolata simulando la distribuzione.
- **Ribilanciamento:** sposta il frammento di ogni blocco dal backend con meno spazio libero a quello con più spazio
  libero, se la differenza vale il traffico. Un backend oltre il suo limite cede frammenti per primo. Anche dismissione
  e riparazione scelgono il backend con più spazio libero.

### Pulizia e ribilanciamento

- `cloudraid gc` elenca tutti gli oggetti nelle cartelle `data/` dei backend (su Dropbox e S3 con elenchi
  ricorsivi, sugli altri cartella per cartella) e cancella quelli che l'indice non usa: frammenti di caricamenti interrotti, file temporanei, copie
  vecchie rimaste su un backend tornato disponibile dopo una riparazione. Prende il lock, così nessun altro
  processo sta caricando frammenti non ancora nell'indice. I backend non raggiungibili vengono saltati.
- `cloudraid rebalance` sposta frammenti dai backend con meno spazio libero a quelli con più spazio libero (vedi sopra),
  per esempio dopo averne aggiunto uno o dopo aver abbassato un limite. I frammenti di un blocco restano sempre su
  backend diversi, e uno spostamento si fa solo se la differenza supera il 5% del carico medio. Ogni frammento viene verificato prima di copiarlo; la copia vecchia si cancella solo dopo il
  salvataggio dell'indice, che avviene ogni 64 spostamenti, così un'interruzione perde poco lavoro e lascia solo
  orfani innocui. Con i cloud costa traffico: prima guarda il piano con `--dry-run`.
- Tutte e due sono anche nell'interfaccia web (Manutenzione), con simulazione, conferma e avanzamento.

### Dismettere o sostituire un backend

`cloudraid retire NOME` (o "Dismetti" sulla scheda del backend nell'interfaccia web) mette il backend **in
dismissione**: non riceve più frammenti nuovi (né dalle scritture, né dalla riparazione, né dal ribilanciamento) e i
suoi frammenti vengono spostati sugli altri, ognuno sul meno carico fra quelli che non hanno già un frammento dello
stesso blocco. La ridondanza resta piena per tutto il tempo; se il backend non risponde più, i suoi frammenti vengono
ricostruiti dalla parità. Lo stato sta nell'indice, quindi vale anche per il mount e per la riga di comando; il
lavoro si può interrompere e riprendere. Quando il backend è vuoto (la scheda dice "dismesso") lo si toglie dalla
configurazione, oppure lo si spegne: non conta più come guasto. `--undo` (o "Rimetti in servizio") lo riattiva e
`rebalance` gli ridà una parte dei frammenti.

Per **sostituire** un account: aggiungi alla configurazione quello nuovo (con la sua cartella creata), riavvia i
servizi, dismetti quello vecchio. Senza il vecchio devono restare almeno `data_shards + parity_shards` backend:
altrimenti cloudraid rifiuta e chiede di aggiungere prima il sostituto.

### Quando un backend si guasta

- Le letture continuano a funzionare finché i backend persi sono al massimo `parity_shards`;
  `get` avvisa quando ha dovuto usare la parità.
- Le **scritture** vengono rifiutate se i backend disponibili sono meno di `data_shards + parity_shards`:
  meglio non scrivere che scrivere senza ridondanza.
- Il modo più semplice: aggiungi un account nuovo dall'interfaccia e "Dismetti" quello guasto, i suoi frammenti
  vengono ricostruiti dalla parità sugli altri. Oppure, a mano, rimetti al suo posto un backend nuovo e vuoto con lo
  stesso nome e lancia `cloudraid scrub --repair`: i frammenti mancanti vengono ricostruiti lì. Con un backend in più del necessario,
  `scrub --repair` sposta i frammenti sui backend liberi anche senza sostituzione.
- `scrub` esce con errore se trova blocchi irrecuperabili, ed elenca i file danneggiati: si può usare da cron.

## Formato sui backend

```
cloudraid.key                  chiave principale cifrata con la password (JSON; copia anche accanto alla configurazione)
index/00000000000000000042.bin indice cifrato, ultime 3 generazioni
locks/<id>.json                lock di chi sta modificando il repository (host, pid, ora)
data/ab/abcd…ef.0 … .5         frammenti: <id del blocco>.<numero del frammento>
```

## Limiti attuali

- Mount in scrittura, misure sul repository dimostrativo (6 cartelle sullo stesso SSD): 50 MB in 0,5 s, 634 file
  piccoli (5 MB) in 12 s con `cp -r` o `rsync -a`, circa 18 ms a file (la scrittura dei frammenti con fsync).
  Con backend cloud ogni file costa almeno un giro di rete sul backend più lento.
- Se una scrittura si interrompe a metà possono restare frammenti orfani (innocui, occupano spazio):
  `cloudraid gc` li cancella.
- Il lock non è atomico (nessun backend lo permette in modo uniforme): due processi avviati nello stesso
  istante se ne accorgono rileggendo i lock, ma è una protezione dagli errori, non una garanzia assoluta.
- Le **scritture** vanno alla velocità del backend più lento: ogni blocco ha un frammento su ognuno di k + m backend.
- Dropbox, OneDrive e Koofr (WebDAV) sono provati anche con account veri; Google Drive solo contro un server finto
  fedele alle API. WebDAV e S3 sono provati anche contro server veri (hacdias/webdav e Versity S3 Gateway, che
  controlla le firme).
- I blocchi scritti con meno frammenti di dati (3 + 2...) restano così: il ribilanciamento sposta frammenti, non
  ricodifica i blocchi.
- Su OneDrive, con il solo permesso sulla cartella dell'app, alcuni account potrebbero non mostrare la quota: allora lo
  spazio risulta "non noto" ed è meglio impostare un limite.
- `init` solo su cartelle locali (vedi Installazione).
- Anche l'elenco delle versioni dell'indice, all'apertura, aspetta tutti i backend: serve a non perdere l'indice
  più recente se è rimasto solo su un backend.
- Più processi che scrivono non si scambiano le prenotazioni di spazio: ognuno conta le sue scritture e rilegge lo
  spazio dai servizi ogni 5 minuti. Un servizio che rifiuta per mancanza di spazio viene escluso fino al controllo
  successivo, e il frammento va su un altro.

## Tappe

| Fase | Contenuto | Stato |
|---|---|---|
| 1 | CLI `init/put/get/ls/rm/status/scrub`, cifratura, Reed-Solomon, backend locali, test | fatta |
| 2 | Backend rclone, trasferimenti in parallelo, lock, `unlock`, immagine Docker | fatta, rclone tolto nella 6 |
| 3a | Interfaccia web di amministrazione (`serve`) dietro Traefik, solo LAN | fatta |
| 5 | Dismissione di un backend (`retire`), anche dall'interfaccia web | fatta |
| 2b | Letture "a gara": i k frammenti dai backend più rapidi, uno in più se qualcuno tarda; chiave e indice dal primo che risponde | fatta |
| 3 | Mount FUSE in sola lettura con cache in memoria, servizio `mount` nel compose | fatta |
| 4a | Mount in scrittura (copia locale caricata alla chiusura), rinomine e cartelle nell'indice | fatta |
| 4 | Pulizia dei frammenti orfani (`gc`) e ribilanciamento (`rebalance`), da riga di comando e interfaccia web | fatta |
| 6 | Backend nativi (WebDAV, S3, Dropbox, OneDrive, Google Drive) senza rclone, credenziali cifrate, pagina Spazi con aggiunta guidata | fatta |
