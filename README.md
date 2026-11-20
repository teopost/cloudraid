# cloudraid

Archivio cifrato con ridondanza **Reed-Solomon** distribuito su più storage, come un RAID 5/6
in cui i "dischi" sono account diversi (per ora cartelle locali, poi Google Drive, OneDrive, Dropbox…).

Con lo schema predefinito **4 + 2** ogni blocco di dati diventa 4 frammenti di dati e 2 di parità,
ognuno su un backend diverso: si possono perdere **due backend qualsiasi** senza perdere niente,
occupando 1,5 volte lo spazio dei dati.

> **Stato: fase 1 (prototipo).** Funziona e ha i test, ma è codice giovane: non usarlo come unica copia
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
```

La password si può passare anche con la variabile `CLOUDRAID_PASSWORD` (utile negli script).
**Senza password i dati non si recuperano in nessun modo**: salvala nel password manager.

### Quando un backend si guasta

- Le letture continuano a funzionare finché i backend persi sono al massimo `parity_shards`;
  `get` avvisa quando ha dovuto usare la parità.
- Le **scritture** vengono rifiutate se i backend disponibili sono meno di `data_shards + parity_shards`:
  meglio non scrivere che scrivere senza ridondanza.
- Sostituisci il backend con uno nuovo e vuoto (stesso nome nella configurazione) e lancia
  `cloudraid scrub --repair`: i frammenti mancanti vengono ricostruiti lì. Con un backend in più del necessario,
  `scrub --repair` sposta i frammenti sui backend liberi anche senza sostituzione.
- `scrub` esce con errore se trova blocchi irrecuperabili, ed elenca i file danneggiati: si può usare da cron.

## Formato sui backend

```
cloudraid.key                  chiave principale cifrata con la password (JSON)
index/00000000000000000042.bin indice cifrato, ultime 3 generazioni
data/ab/abcd…ef.0 … .5         frammenti: <id del blocco>.<numero del frammento>
```

## Limiti della fase 1

- Solo backend locali (cartelle o dischi montati).
- Un solo processo alla volta sullo stesso repository: non c'è ancora un lock.
- Se una scrittura si interrompe a metà possono restare frammenti orfani (innocui, occupano spazio):
  la pulizia arriverà con la fase 4.
- I file vengono letti e scritti un blocco alla volta, senza parallelismo.

## Tappe

| Fase | Contenuto | Stato |
|---|---|---|
| 1 | CLI `init/put/get/ls/rm/status/scrub`, cifratura, Reed-Solomon, backend locali, test | fatta |
| 2 | Backend cloud (OpenDAL: Google Drive, OneDrive, Dropbox, S3, WebDAV, SFTP), scritture in parallelo, lock | |
| 3 | Mount FUSE in sola lettura con cache locale | |
| 4 | Mount in scrittura, pulizia dei frammenti orfani, ribilanciamento | |
