# cloudraid

Archivio cifrato con ridondanza **Reed-Solomon** distribuito su più storage, come un RAID 5/6
in cui i "dischi" sono account diversi: cartelle locali e qualsiasi storage raggiungibile con rclone
(Google Drive, OneDrive, Dropbox, MEGA, pCloud, SFTP, WebDAV, S3…).

Con lo schema predefinito **4 + 2** ogni blocco di dati diventa 4 frammenti di dati e 2 di parità,
ognuno su un backend diverso: si possono perdere **due backend qualsiasi** senza perdere niente,
occupando 1,5 volte lo spazio dei dati.

> **Stato: fase 2 (prototipo).** Funziona e ha i test, ma è codice giovane: non usarlo come unica copia
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

## Compilazione

Serve solo Docker: `make build` compila in un container `rust:1-bookworm` e produce `target/release/cloudraid`.
`make test` lancia i test.

## Uso

```bash
cp cloudraid.example.toml cloudraid.toml   # poi adatta i backend
export CLOUDRAID_CONFIG=$PWD/cloudraid.toml   # oppure -c cloudraid.toml, predefinito ./cloudraid.toml

cloudraid init                     # chiede la password due volte
cloudraid put ~/foto archivio/foto # file o cartelle
cloudraid ls archivio
cloudraid get archivio/foto /tmp/foto
cloudraid rm archivio/foto/vecchia.jpg
cloudraid status                   # backend disponibili, frammenti per backend
cloudraid scrub                    # verifica tutti i frammenti
cloudraid scrub --repair           # e ricostruisce quelli mancanti o danneggiati
cloudraid unlock                   # cancella i lock scaduti di processi interrotti
```

`get`, `ls`, `status` e `scrub` senza `--repair` non prendono il lock: evita di lanciarli mentre un `rm`
o uno `scrub --repair` stanno cancellando o spostando frammenti.

La password si può passare anche con la variabile `CLOUDRAID_PASSWORD` (utile negli script).
**Senza password i dati non si recuperano in nessun modo**: salvala nel password manager.

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

- Se una scrittura si interrompe a metà possono restare frammenti orfani (innocui, occupano spazio):
  la pulizia arriverà con la fase 4.
- Il lock non è atomico (nessun backend lo permette in modo uniforme): due processi avviati nello stesso
  istante se ne accorgono rileggendo i lock, ma è una protezione dagli errori, non una garanzia assoluta.
- Lo spazio libero dei backend non viene controllato prima di scrivere: un account pieno fa fallire la scrittura,
  che passa su un backend di riserva se c'è.

## Tappe

| Fase | Contenuto | Stato |
|---|---|---|
| 1 | CLI `init/put/get/ls/rm/status/scrub`, cifratura, Reed-Solomon, backend locali, test | fatta |
| 2 | Backend rclone (tutti i provider di rclone), trasferimenti in parallelo, lock, `unlock` | fatta |
| 3 | Mount FUSE in sola lettura con cache locale | |
| 4 | Mount in scrittura, pulizia dei frammenti orfani, ribilanciamento | |
