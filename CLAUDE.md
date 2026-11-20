# Regole del progetto cloudraid

- Rispondi e scrivi in italiano: messaggi del programma, commenti, README, messaggi di commit.
- **Niente em dash (—)** in nessun file: usa il trattino normale `-`. Controllo: `git grep -n '—'` non deve trovare niente.
- Si compila e si testa **solo in Docker** (`make build`, `make test`): sul server non va installato Rust.
- Ogni modifica che tocca il formato su disco (chiave, indice, frammenti) deve restare compatibile con i repository
  esistenti, oppure aumentare la versione del formato e saper leggere quella vecchia.
- I dati degli utenti sono la priorità: prima di cancellare frammenti va salvato l'indice che non li usa più
  (`pending_deletes` in `src/repo.rs`); le scritture senza ridondanza piena vanno rifiutate, non fatte a metà.
- Ogni correzione o funzionalità nuova ha il suo test in `tests/raid.rs` (cartelle locali al posto degli account).
- Le tappe del progetto sono nella tabella in fondo al README: aggiornala quando una fase avanza.
