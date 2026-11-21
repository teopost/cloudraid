# cloudraid

Archivio cifrato con ridondanza **Reed-Solomon** distribuito su più storage, come un RAID 5/6
in cui i "dischi" sono account diversi: cartelle locali e qualsiasi storage raggiungibile con rclone
(Google Drive, OneDrive, Dropbox, MEGA, pCloud, SFTP, WebDAV, S3…).

Con lo schema predefinito **4 + 2** ogni blocco di dati diventa 4 frammenti di dati e 2 di parità,
ognuno su un backend diverso: si possono perdere **due backend qualsiasi** senza perdere niente,
occupando 1,5 volte lo spazio dei dati.

> **Stato: fasi 1-4 complete: interfaccia web, mount in lettura e scrittura, pulizia e ribilanciamento (prototipo).** Funziona e ha i test, ma è codice giovane: non usarlo come unica copia
> di dati importanti.

## Come funziona

```
file → blocchi a lunghezza variabile (FastCDC, ~4 MiB) → deduplicazione
     → cifratura XChaCha20-Poly1305 → Reed-Solomon k + m → un frammento per backend
```

- **Cifratura lato client.** I backend vedono solo frammenti illeggibili con nomi casuali. La chiave principale
  è protetta dalla password con Argon2id e salvata (cifrata) su ogni backend, in `cloudraid.key`.
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

| Tipo | Configurazione | Note |
|---|---|---|
| `local` | `path = "/mnt/disco1/cloudraid"` | cartella o disco montato |
| `rclone` | `remote = "gdrive:cloudraid"` | qualsiasi remote di `rclone config` più la cartella del repository |

Per i backend rclone cloudraid avvia **un solo** `rclone rcd`, raggiungibile solo da 127.0.0.1 con credenziali
casuali, e lo chiude all'uscita. Login OAuth, rinnovo dei token, limiti di traffico dei provider e cache delle cartelle
restano a rclone: ogni account si configura una volta con `rclone config` (vedi la sezione `[rclone]` di
`cloudraid.example.toml` per scegliere eseguibile, file di configurazione e opzioni). Le operazioni fallite per
errori temporanei vengono ripetute fino a 4 volte.

Un backend è **disponibile** se la sua cartella del repository esiste. Una cartella sparita (account perso, disco
smontato) si tratta come un disco guasto.

**Aggiungere un backend** a un repository esistente: aggiungi la sezione `[[backend]]` alla configurazione, crea a mano
la cartella del repository (`mkdir` oppure `rclone mkdir nuovoaccount:cloudraid`) e riavvia i servizi. Alla prima
scrittura il backend nuovo riceve l'indice e la chiave cifrata del repository (`cloudraid.key`), e da lì in poi i
frammenti nuovi si distribuiscono anche su di lui; per spostarci anche una parte di quelli già scritti usa
`cloudraid rebalance` (vedi sotto).
Se la cartella contiene già la chiave di un **altro** repository, cloudraid non la tocca e avvisa.

## Interfaccia web

`cloudraid serve` avvia un'interfaccia di amministrazione: stato dei backend e della ridondanza, file (sfoglia, cerca,
carica anche trascinando, scarica, elimina), verifica e riparazione con l'avanzamento, lock, cronologia delle operazioni.

- **Accesso con la password del repository.** Il server parte bloccato e non salva la password da nessuna parte: il primo
  accesso apre il repository (con i backend cloud può richiedere qualche decina di secondi), che poi resta aperto in memoria
  fino al riavvio del container. Sessioni con cookie `HttpOnly`, `Secure`, `SameSite=Strict`: scadono dopo 1 ora di inattività
  e comunque dopo 12 ore. I tentativi sbagliati vengono rallentati.
- **Un'operazione alla volta**, in background (scrub, riparazione, caricamenti, eliminazioni), con il lock del repository come
  la riga di comando. Prima di scrivere il server rilegge l'indice: le modifiche fatte nel frattempo dalla riga di comando non
  vanno perse. «Ricontrolla» rilegge backend e indice anche a mano.
- Lo scaricamento ricostruisce e decifra il file sul server; durante un'operazione lunga viene rifiutato (409).
- Protezioni: richieste di modifica solo con l'intestazione `X-Cloudraid: 1` (CSRF), Content-Security-Policy senza script né stili
  inline, nessuna libreria esterna.
- Cronologia delle operazioni in `config/web-state.json` (niente segreti).

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
in `/mnt/cloudraid/archivio` (`CLOUDRAID_MOUNT_DIR` cambia la cartella madre), in lettura e scrittura
(`CLOUDRAID_MOUNT_RW=false` per la sola lettura). Il container ha bisogno di `/dev/fuse`, del permesso `SYS_ADMIN`
e di uscire dal profilo AppArmor `docker-default`, che vieta `mount`.
Attenzione: `docker kill` è uno stop manuale, quindi il container non riparte da solo e lascia il mount appeso
finché non lanci `docker compose up -d`. Un crash vero, invece, fa ripartire il container da solo.

## Installazione sul server (Docker)

Il progetto sta in `/srv/docker/cloudraid`, come le altre app. L'immagine contiene cloudraid e rclone 1.71;
`compose.yaml` monta la configurazione (`./config`) e i backend locali (`/srv/cloudraid`).
cloudraid è un comando, non un servizio: si lancia con lo script `./cloudraid`, che monta anche la cartella corrente,
così i percorsi relativi funzionano come con un programma installato.

```bash
cd /srv/docker/cloudraid
make build                                   # costruisce l'immagine cloudraid:local
( umask 077; cp .env.example .env )          # poi controlla LAN_IP
install -d -m 700 config
cp cloudraid.example.toml config/cloudraid.toml   # poi adatta i backend
# solo i remote di rclone che servono (rclone ci riscrive i token rinnovati):
( umask 077; awk '/^\[/{p=($0=="[gdrive]"||$0=="[dropbox]")} p' /root/.config/rclone/rclone.conf > config/rclone.conf )
./cloudraid init
./cloudraid status
docker compose up -d web                     # interfaccia web: https://<LAN_IP>/cloudraid/
```

Facoltativo: `CLOUDRAID_PASSWORD` in `.env` (`chmod 600`, modello in `.env.example`) per gli usi non interattivi,
per esempio uno `scrub` periodico da cron.

`config/` e `.env` restano solo sul server (fuori da git) ed entrano nel backup orario di `/srv/docker`.

## Sviluppo

`make test` lancia i test in un container `rust:1-bookworm`. La cache di compilazione sta in `~/.cache/cloudraid-build`,
fuori da `/srv/docker`, così non finisce nei backup. I test dei backend rclone usano il rclone dell'host, se c'è.
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
cloudraid mount /mnt/cloudraid --rw  # il repository come cartella (vedi sopra)
```

`get`, `ls`, `status` e `scrub` senza `--repair` non prendono il lock: evita di lanciarli mentre un `rm`
o uno `scrub --repair` stanno cancellando o spostando frammenti.

La password si può passare anche con la variabile `CLOUDRAID_PASSWORD` (utile negli script).
**Senza password i dati non si recuperano in nessun modo**: salvala nel password manager.

### Pulizia e ribilanciamento

- `cloudraid gc` elenca tutti gli oggetti nelle cartelle `data/` dei backend (su rclone con una sola chiamata
  ricorsiva) e cancella quelli che l'indice non usa: frammenti di caricamenti interrotti, file temporanei, copie
  vecchie rimaste su un backend tornato disponibile dopo una riparazione. Prende il lock, così nessun altro
  processo sta caricando frammenti non ancora nell'indice. I backend non raggiungibili vengono saltati.
- `cloudraid rebalance` sposta frammenti dai backend sopra la media a quelli più vuoti, per esempio dopo averne
  aggiunto uno. I frammenti di un blocco restano sempre su backend diversi, e uno spostamento si fa solo se riduce
  davvero lo squilibrio. Ogni frammento viene verificato prima di copiarlo; la copia vecchia si cancella solo dopo il
  salvataggio dell'indice, che avviene ogni 64 spostamenti, così un'interruzione perde poco lavoro e lascia solo
  orfani innocui. Con i cloud costa traffico: prima guarda il piano con `--dry-run`.
- Tutte e due sono anche nell'interfaccia web (Manutenzione), con simulazione, conferma e avanzamento.

### Quando un backend si guasta

- Le letture continuano a funzionare finché i backend persi sono al massimo `parity_shards`;
  `get` avvisa quando ha dovuto usare la parità.
- Le **scritture** vengono rifiutate se i backend disponibili sono meno di `data_shards + parity_shards`:
  meglio non scrivere che scrivere senza ridondanza.
- Sostituisci il backend con uno nuovo e vuoto (stesso nome nella configurazione, cartella creata a mano,
  per esempio `rclone mkdir nuovoaccount:cloudraid`) e lancia
  `cloudraid scrub --repair`: i frammenti mancanti vengono ricostruiti lì. Con un backend in più del necessario,
  `scrub --repair` sposta i frammenti sui backend liberi anche senza sostituzione.
- `scrub` esce con errore se trova blocchi irrecuperabili, ed elenca i file danneggiati: si può usare da cron.

## Formato sui backend

```
cloudraid.key                  chiave principale cifrata con la password (JSON)
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
- Un backend lento rallenta tutto: le letture aspettano i frammenti di dati anche se la parità su un backend veloce
  basterebbe. Nella prova reale (21/11/2026, 33 MB, 4 cartelle locali + Google Drive + Dropbox) `get` ha impiegato
  99 s con Drive e 8 s senza: Google Drive col client_id condiviso di rclone risponde `RATE_LIMIT_EXCEEDED` e rclone
  rallenta. Con Google Drive serve un **client_id proprio** (<https://rclone.org/drive/#making-your-own-client-id>).
- Lo spazio libero dei backend non viene controllato prima di scrivere: un account pieno fa fallire la scrittura,
  che passa su un backend di riserva se c'è.

## Tappe

| Fase | Contenuto | Stato |
|---|---|---|
| 1 | CLI `init/put/get/ls/rm/status/scrub`, cifratura, Reed-Solomon, backend locali, test | fatta |
| 2 | Backend rclone (tutti i provider di rclone), trasferimenti in parallelo, lock, `unlock`, immagine Docker | fatta |
| 3a | Interfaccia web di amministrazione (`serve`) dietro Traefik, solo LAN | fatta |
| 2b | Letture "a gara": chiedere anche la parità e usare i primi k frammenti che arrivano, così un backend lento non blocca | |
| 3 | Mount FUSE in sola lettura con cache in memoria, servizio `mount` nel compose | fatta |
| 4a | Mount in scrittura (copia locale caricata alla chiusura), rinomine e cartelle nell'indice | fatta |
| 4 | Pulizia dei frammenti orfani (`gc`) e ribilanciamento (`rebalance`), da riga di comando e interfaccia web | fatta |
