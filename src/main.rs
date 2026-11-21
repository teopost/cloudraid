use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use cloudraid::repo::{human, LOCK_STALE_SECS};
use cloudraid::{Config, ReadStats, Repo};

#[derive(Parser)]
#[command(version, about = "Archivio cifrato con ridondanza Reed-Solomon distribuito su più storage")]
struct Cli {
    /// File di configurazione
    #[arg(short, long, env = "CLOUDRAID_CONFIG", default_value = "cloudraid.toml")]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Crea un repository nuovo su tutti i backend
    Init,
    /// Carica un file o una cartella
    Put {
        /// File o cartella locale
        src: PathBuf,
        /// Percorso nel repository (predefinito: il nome di SRC)
        dest: Option<String>,
    },
    /// Estrae un file o una cartella
    Get {
        /// Percorso nel repository
        path: String,
        /// Destinazione locale (predefinito: il nome nel repository, nella cartella corrente)
        dest: Option<PathBuf>,
    },
    /// Elenca i file
    Ls {
        /// Mostra solo questo file o questa cartella
        #[arg(default_value = "")]
        path: String,
    },
    /// Toglie un file o una cartella
    Rm { path: String },
    /// Stato dei backend e del repository
    Status,
    /// Verifica tutti i frammenti (ed eventualmente li ripara)
    Scrub {
        /// Ricostruisce i frammenti mancanti o danneggiati
        #[arg(long)]
        repair: bool,
    },
    /// Avvia l'interfaccia web di amministrazione
    Serve {
        /// Indirizzo e porta di ascolto
        #[arg(long, default_value = "0.0.0.0:8080")]
        listen: String,
        /// File con la cronologia delle operazioni
        #[arg(long, env = "CLOUDRAID_STATE", default_value = "/config/web-state.json")]
        state: PathBuf,
        /// Cookie di sessione anche senza HTTPS (solo per provare in locale)
        #[arg(long)]
        insecure_cookie: bool,
    },
    /// Monta il repository come cartella (FUSE); resta attivo fino a Ctrl-C o SIGTERM
    Mount {
        /// Cartella in cui montare
        mountpoint: PathBuf,
        /// Ogni quanti secondi cercare un indice più recente (modifiche da web o riga di comando)
        #[arg(long, default_value_t = 30)]
        refresh: u64,
        /// Memoria per i blocchi già letti, in MB
        #[arg(long, default_value_t = 256)]
        cache_mb: usize,
        /// Proprietario di file e cartelle
        #[arg(long, default_value_t = 0)]
        uid: u32,
        #[arg(long, default_value_t = 0)]
        gid: u32,
        /// Accesso anche agli altri utenti, secondo i permessi (file 0440, cartelle 0550)
        #[arg(long)]
        allow_other: bool,
        /// Richieste servite in parallelo
        #[arg(long, default_value_t = 16)]
        threads: usize,
        /// Anche in scrittura: i file scritti passano da una copia locale e vengono caricati alla chiusura
        #[arg(long, env = "CLOUDRAID_MOUNT_RW")]
        rw: bool,
        /// Cartella di appoggio per i file in scrittura (con --rw; predefinita: cloudraid-staging nella cartella temporanea)
        #[arg(long)]
        staging: Option<PathBuf>,
    },
    /// Cancella i frammenti orfani (scritture interrotte, copie vecchie dopo una riparazione)
    Gc {
        /// Conta soltanto, senza cancellare
        #[arg(long)]
        dry_run: bool,
    },
    /// Sposta frammenti dai backend più pieni ai più vuoti (per esempio dopo averne aggiunto uno)
    Rebalance {
        /// Mostra il piano, senza spostare niente
        #[arg(long)]
        dry_run: bool,
    },
    /// Cancella i lock lasciati da processi interrotti
    Unlock {
        /// Cancella anche i lock non ancora scaduti (solo se sei sicuro che nessun processo stia lavorando)
        #[arg(long)]
        all: bool,
    },
}

fn password(confirm: bool) -> Result<String> {
    if let Ok(p) = std::env::var("CLOUDRAID_PASSWORD") {
        return Ok(p);
    }
    let p = rpassword::prompt_password("Password del repository: ")?;
    if confirm {
        if p.len() < 8 {
            bail!("la password deve avere almeno 8 caratteri");
        }
        if rpassword::prompt_password("Ripeti la password: ")? != p {
            bail!("le password non coincidono");
        }
    }
    Ok(p)
}


fn file_name(p: &Path) -> Result<String> {
    let canon = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    canon
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .with_context(|| format!("impossibile ricavare il nome di {}", p.display()))
}

fn cmd_put(repo: &mut Repo, src: &Path, dest: Option<String>) -> Result<()> {
    let dest = match dest {
        Some(d) => d,
        None => file_name(src)?,
    };
    let meta = std::fs::metadata(src).with_context(|| format!("{} non trovato", src.display()))?;
    let mut files = Vec::new();
    if meta.is_dir() {
        for entry in walkdir::WalkDir::new(src).sort_by_file_name() {
            let entry = entry?;
            if entry.file_type().is_file() {
                let rel = entry.path().strip_prefix(src)?;
                let rel = rel.to_str().with_context(|| format!("nome non UTF-8: {}", rel.display()))?;
                files.push((entry.path().to_path_buf(), format!("{dest}/{rel}")));
            }
        }
    } else {
        files.push((src.to_path_buf(), dest));
    }
    for (path, dest) in files {
        let new = repo.put_file(&path, &dest)?;
        println!("+ {dest} ({new} blocchi nuovi)");
    }
    Ok(())
}

fn cmd_get(repo: &Repo, path: &str, dest: Option<PathBuf>) -> Result<()> {
    let norm = cloudraid::repo::normalize(path)?;
    let files = repo.matching(&norm)?;
    if files.is_empty() {
        bail!("{path}: niente da estrarre");
    }
    let base = norm.rsplit('/').next().unwrap_or("").to_string();
    let mut stats = ReadStats::default();
    if files.len() == 1 && files[0] == norm {
        let mut out = dest.unwrap_or_else(|| PathBuf::from(&base));
        if out.is_dir() {
            out.push(&base);
        }
        repo.get_file(&norm, &out, &mut stats)?;
        println!("{} -> {}", norm, out.display());
    } else {
        let root = dest.unwrap_or_else(|| PathBuf::from(if base.is_empty() { "." } else { &base }));
        let prefix = if norm.is_empty() { String::new() } else { format!("{norm}/") };
        for f in &files {
            let out = root.join(f.strip_prefix(&prefix).unwrap_or(f));
            repo.get_file(f, &out, &mut stats)?;
            println!("{f} -> {}", out.display());
        }
    }
    if stats.degraded > 0 {
        eprintln!(
            "attenzione: {} blocchi su {} ricostruiti con la parità: lancia `cloudraid scrub --repair`",
            stats.degraded, stats.chunks
        );
    }
    Ok(())
}

fn cmd_status(repo: &Repo, cfg: &Config) {
    let ix = repo.index();
    let total: u64 = ix.files.values().map(|f| f.size).sum();
    let stored: u64 = ix.chunks.values().map(|c| c.plain_len).sum();
    println!(
        "schema: {} dati + {} parità (si possono perdere {} backend), indice generazione {}",
        cfg.data_shards, cfg.parity_shards, cfg.parity_shards, ix.generation
    );
    println!(
        "file: {}, {} ({} dopo la deduplicazione, {} blocchi)",
        ix.files.len(),
        human(total),
        human(stored),
        ix.chunks.len()
    );
    for b in repo.status() {
        println!(
            "  {:<12} {:<15} {:>8} frammenti {:>10}  {}",
            b.name,
            if b.available { "disponibile" } else { "NON DISPONIBILE" },
            b.shards,
            human(b.bytes),
            b.location
        );
    }
    for l in repo.locks() {
        let age = now().saturating_sub(l.time);
        let state = if age >= LOCK_STALE_SECS { "scaduto" } else { "attivo" };
        println!("lock {state}: {} pid {}, rinnovato {age} s fa", l.host, l.pid);
    }
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn run(cli: Cli) -> Result<()> {
    // il server web legge la configurazione al primo login: può partire anche prima che esista
    if let Cmd::Serve { listen, state, insecure_cookie } = cli.cmd {
        return cloudraid::web::serve(cloudraid::web::ServeOptions { listen, config: cli.config, state, insecure_cookie });
    }
    let cfg = Config::load(&cli.config)?;
    if let Cmd::Init = cli.cmd {
        let repo = Repo::init(cfg.clone(), &password(true)?)?;
        println!("repository creato su {} backend", cfg.backends.len());
        cmd_status(&repo, &cfg);
        return Ok(());
    }
    let mut repo = Repo::open(cfg.clone(), &password(false)?)?;
    if let Cmd::Mount { mountpoint, refresh, cache_mb, uid, gid, allow_other, threads, rw, staging } = cli.cmd {
        let staging = rw.then(|| staging.unwrap_or_else(|| std::env::temp_dir().join("cloudraid-staging")));
        return cloudraid::mount::mount(
            repo,
            cloudraid::mount::MountOptions {
                mountpoint,
                refresh: std::time::Duration::from_secs(refresh.max(1)),
                cache_bytes: cache_mb.max(16) << 20,
                uid,
                gid,
                allow_other,
                threads,
                staging,
            },
        );
    }
    // chi modifica il repository prende il lock esclusivo
    if matches!(cli.cmd, Cmd::Put { .. } | Cmd::Rm { .. } | Cmd::Scrub { repair: true } | Cmd::Gc { .. } | Cmd::Rebalance { .. }) {
        repo.lock()?;
    }
    let result = match cli.cmd {
        Cmd::Init | Cmd::Serve { .. } | Cmd::Mount { .. } => unreachable!(),
        Cmd::Put { src, dest } => cmd_put(&mut repo, &src, dest),
        Cmd::Get { path, dest } => cmd_get(&repo, &path, dest),
        Cmd::Ls { path } => {
            let files = repo.matching(&path)?;
            for f in &files {
                println!("{:>10}  {f}", human(repo.index().files[f].size));
            }
            let total: u64 = files.iter().map(|f| repo.index().files[f].size).sum();
            println!("{} file, {}", files.len(), human(total));
            Ok(())
        }
        Cmd::Rm { path } => match repo.remove(&path) {
            Ok(removed) if removed.is_empty() => Err(anyhow::anyhow!("{path}: file non presente")),
            Ok(removed) => {
                for f in removed {
                    println!("- {f}");
                }
                Ok(())
            }
            Err(e) => Err(e),
        },
        Cmd::Status => {
            cmd_status(&repo, &cfg);
            Ok(())
        }
        Cmd::Gc { dry_run } => repo.gc(dry_run, &mut |_, _| {}).map(|r| {
            println!("{}", r.summary(dry_run));
        }),
        Cmd::Rebalance { dry_run } => repo.rebalance(dry_run, &mut |d, t| eprint!("\r{d}/{t} spostamenti")).map(|r| {
            if r.planned > 0 && !dry_run {
                eprintln!();
            }
            println!("{}", r.summary(dry_run));
            for ((name, before), (_, after)) in r.before.iter().zip(&r.after) {
                println!("  {name:<12} {:>10} -> {:>10}", human(*before), human(*after));
            }
        }),
        Cmd::Unlock { all } => {
            let removed = repo.break_locks(all);
            for l in &removed {
                println!("lock cancellato: {} pid {}", l.host, l.pid);
            }
            let left = repo.locks().len();
            println!("{} lock cancellati, {} rimasti", removed.len(), left);
            if left > 0 {
                println!("i lock rimasti sono attivi (rinnovati negli ultimi {} minuti): usa --all solo se sei sicuro", LOCK_STALE_SECS / 60);
            }
            Ok(())
        }
        Cmd::Scrub { repair } => repo.scrub(repair).and_then(|r| {
            println!(
                "blocchi: {}, frammenti integri: {}, mancanti: {}, danneggiati: {}",
                r.chunks, r.shards_ok, r.shards_missing, r.shards_corrupt
            );
            println!("blocchi degradati ma leggibili: {}", r.degraded_chunks);
            if repair {
                println!("frammenti riparati: {}, non riparabili: {}", r.repaired_shards, r.unrepaired_shards);
            } else if r.degraded_chunks > 0 {
                println!("lancia `cloudraid scrub --repair` per ricostruirli");
            }
            if !r.lost_chunks.is_empty() {
                eprintln!("PERSI {} blocchi, file danneggiati:", r.lost_chunks.len());
                for f in &r.lost_files {
                    eprintln!("  {f}");
                }
                bail!("dati persi: {} blocchi irrecuperabili", r.lost_chunks.len());
            }
            Ok(())
        }),
    };
    // salva comunque il lavoro fatto prima di un eventuale errore
    let saved = repo.save();
    result?;
    saved
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("errore: {e:#}");
            ExitCode::FAILURE
        }
    }
}
