use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

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

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
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
            "  {:<16} {:<15} {:>8} frammenti  {:>10}",
            b.name,
            if b.available { "disponibile" } else { "NON DISPONIBILE" },
            b.shards,
            human(b.bytes)
        );
    }
}

fn run(cli: Cli) -> Result<()> {
    let cfg = Config::load(&cli.config)?;
    if let Cmd::Init = cli.cmd {
        let repo = Repo::init(cfg.clone(), &password(true)?)?;
        println!("repository creato su {} backend", cfg.backends.len());
        cmd_status(&repo, &cfg);
        return Ok(());
    }
    let mut repo = Repo::open(cfg.clone(), &password(false)?)?;
    let result = match cli.cmd {
        Cmd::Init => unreachable!(),
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
