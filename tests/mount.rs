//! Test del mount: lettura a pezzi dal repository, aggiornamento dell'indice e, se il container
//! di test ha /dev/fuse ed è root (`make test`), un mount FUSE vero con il binario.

use cloudraid::mount::{read_range, Cache, ChunkRef, Inodes, Tree};
use cloudraid::{BackendConfig, BackendKind, ChunkConfig, Config, RcloneConfig, Repo};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const PW: &str = "password di prova";
const SMALL: ChunkConfig = ChunkConfig { min: 16 << 10, avg: 64 << 10, max: 256 << 10 };

fn config(tmp: &Path) -> Config {
    Config {
        data_shards: 4,
        parity_shards: 2,
        chunk: SMALL,
        transfers: 4,
        rclone: RcloneConfig::default(),
        backends: (0..6)
            .map(|i| BackendConfig { name: format!("acc{i}"), kind: BackendKind::Local { path: tmp.join(format!("acc{i}")) } })
            .collect(),
    }
}

fn random(seed: &str, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    blake3::Hasher::new().update(seed.as_bytes()).finalize_xof().fill(&mut out);
    out
}

fn put(repo: &mut Repo, tmp: &Path, name: &str, data: &[u8]) {
    let src = tmp.join("src");
    fs::create_dir_all(&src).unwrap();
    let p = src.join(name.replace('/', "_"));
    fs::write(&p, data).unwrap();
    repo.put_file(&p, name).unwrap();
}

/// Legge tutto il file a pezzi di `step` byte, come fa il kernel.
fn read_all(repo: &Repo, cache: &Cache, tree: &Tree, path: &str, step: u64) -> Vec<u8> {
    let file = tree.file(tree.lookup_path(path).unwrap()).unwrap();
    let load = |c: &ChunkRef| cache.get(&c.id, || Ok(repo.read_chunk(&c.id, c.entry.as_ref().unwrap())?.0));
    let mut out = Vec::new();
    let mut off = 0;
    loop {
        let (piece, _) = read_range(&file, off, step, &load).unwrap();
        if piece.is_empty() {
            break;
        }
        off += piece.len() as u64;
        out.extend(piece);
    }
    out
}

#[test]
fn lettura_a_pezzi_anche_con_due_backend_persi() {
    let tmp = TempDir::new().unwrap();
    let mut repo = Repo::init(config(tmp.path()), PW).unwrap();
    let grande = random("grande", 2_500_000);
    let piccolo = b"ciao mount\n".repeat(50);
    put(&mut repo, tmp.path(), "foto/2026/grande.bin", &grande);
    put(&mut repo, tmp.path(), "note.txt", &piccolo);
    put(&mut repo, tmp.path(), "vuoto", b"");
    repo.save().unwrap();

    let tree = Tree::build(repo.index(), &mut Inodes::default());
    let cache = Cache::new(64 << 20);
    assert!(read_all(&repo, &cache, &tree, "foto/2026/grande.bin", 128 << 10) == grande);
    assert!(read_all(&repo, &cache, &tree, "note.txt", 7) == piccolo);
    assert!(read_all(&repo, &cache, &tree, "vuoto", 4096).is_empty());

    // spariscono due account: si legge con la parità, senza cache
    drop(repo);
    fs::remove_dir_all(tmp.path().join("acc0")).unwrap();
    fs::remove_dir_all(tmp.path().join("acc3")).unwrap();
    let repo = Repo::open(config(tmp.path()), PW).unwrap();
    let cache = Cache::new(64 << 20);
    assert!(read_all(&repo, &cache, &tree, "foto/2026/grande.bin", 100_000) == grande);
}

#[test]
fn indice_piu_recente_da_un_altro_processo() {
    let tmp = TempDir::new().unwrap();
    let mut writer = Repo::init(config(tmp.path()), PW).unwrap();
    put(&mut writer, tmp.path(), "a.txt", b"uno");
    writer.save().unwrap();

    let reader = Repo::open(config(tmp.path()), PW).unwrap();
    let generation = reader.index().generation;
    assert!(reader.newer_index(generation).unwrap().is_none(), "niente di nuovo");

    writer.lock().unwrap();
    put(&mut writer, tmp.path(), "b.txt", b"due");
    writer.save().unwrap();
    writer.unlock();
    let newer = reader.newer_index(generation).unwrap().expect("indice nuovo");
    assert!(newer.files.contains_key("b.txt"));
    assert!(reader.newer_index(newer.generation).unwrap().is_none());
}

// ---------------------------------------------------------------------------------------------
// Mount FUSE vero.

fn fuse_usable() -> bool {
    // SAFETY: geteuid non ha precondizioni
    let root = unsafe { libc::geteuid() } == 0;
    root && fs::OpenOptions::new().read(true).write(true).open("/dev/fuse").is_ok()
}

fn mounted(p: &Path) -> bool {
    fs::read_to_string("/proc/mounts").unwrap().lines().any(|l| l.split(' ').nth(1) == Some(p.to_str().unwrap()))
}

fn wait(what: &str, secs: u64, mut ok: impl FnMut() -> bool) {
    let start = Instant::now();
    while !ok() {
        assert!(start.elapsed() < Duration::from_secs(secs), "timeout: {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

struct Mount {
    child: Child,
    point: PathBuf,
}

impl Drop for Mount {
    fn drop(&mut self) {
        // SAFETY: kill su un pid nostro
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let _ = self.child.wait();
    }
}

/// Avvia `cloudraid mount` sul repository di `config(tmp)` e aspetta che la cartella sia montata.
fn start_mount(tmp: &Path, extra: &[&str]) -> Mount {
    let toml: String = std::iter::once(format!(
        "data_shards = 4\nparity_shards = 2\n[chunk]\nmin = {}\navg = {}\nmax = {}\n",
        SMALL.min, SMALL.avg, SMALL.max
    ))
    .chain((0..6).map(|i| {
        format!("[[backend]]\nname = \"acc{i}\"\ntype = \"local\"\npath = \"{}\"\n", tmp.join(format!("acc{i}")).display())
    }))
    .collect();
    let cfg_path = tmp.join("cloudraid.toml");
    fs::write(&cfg_path, toml).unwrap();
    let point = tmp.join("mnt");
    let mut args = vec!["-c", cfg_path.to_str().unwrap(), "mount", point.to_str().unwrap(), "--refresh", "1", "--uid", "1000"];
    args.extend_from_slice(extra);
    let child = Command::new(env!("CARGO_BIN_EXE_cloudraid")).args(&args).env("CLOUDRAID_PASSWORD", PW).spawn().unwrap();
    let m = Mount { child, point: point.clone() };
    wait("mount", 20, || mounted(&point));
    m
}

/// Smonta con SIGTERM e controlla che il processo esca bene.
fn stop_mount(mut m: Mount) {
    // SAFETY: kill su un pid nostro
    unsafe { libc::kill(m.child.id() as i32, libc::SIGTERM) };
    let status = m.child.wait().unwrap();
    assert!(status.success(), "{status}");
    assert!(!mounted(&m.point));
}

#[test]
fn mount_fuse_vero() {
    if !fuse_usable() {
        eprintln!("/dev/fuse non disponibile o non root: test saltato");
        return;
    }
    let tmp = TempDir::new().unwrap();
    let cfg = config(tmp.path());
    let mut repo = Repo::init(cfg.clone(), PW).unwrap();
    let grande = random("fuse", 1_500_000);
    put(&mut repo, tmp.path(), "cartella/grande.bin", &grande);
    put(&mut repo, tmp.path(), "leggimi.txt", b"ciao dal mount\n");
    repo.save().unwrap();

    let mut m = start_mount(tmp.path(), &[]);
    let point = m.point.clone();

    // cartelle, attributi e contenuto
    let mut names: Vec<String> = fs::read_dir(&point).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    names.sort();
    assert_eq!(names, ["cartella", "leggimi.txt"]);
    let meta = fs::metadata(point.join("cartella/grande.bin")).unwrap();
    assert_eq!(meta.len(), grande.len() as u64);
    use std::os::unix::fs::MetadataExt;
    assert_eq!((meta.uid(), meta.mode() & 0o777), (1000, 0o440));
    assert!(fs::read(point.join("cartella/grande.bin")).unwrap() == grande);
    assert_eq!(fs::read_to_string(point.join("leggimi.txt")).unwrap(), "ciao dal mount\n");

    // sola lettura
    let err = fs::write(point.join("nuovo.txt"), b"x").unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EROFS), "{err}");
    assert!(fs::remove_file(point.join("leggimi.txt")).is_err());
    assert!(fs::OpenOptions::new().append(true).open(point.join("leggimi.txt")).is_err());

    // un file aggiunto da un altro processo compare da solo
    repo.lock().unwrap();
    put(&mut repo, tmp.path(), "arrivato.txt", b"dopo");
    repo.save().unwrap();
    repo.unlock();
    wait("file nuovo visibile", 15, || point.join("arrivato.txt").exists());
    assert_eq!(fs::read(point.join("arrivato.txt")).unwrap(), b"dopo");

    // SIGTERM: smonta in modo pulito ed esce con successo
    // SAFETY: kill su un pid nostro
    unsafe { libc::kill(m.child.id() as i32, libc::SIGTERM) };
    let status = m.child.wait().unwrap();
    assert!(status.success(), "{status}");
    assert!(!mounted(&m.point));
}

fn errno_of(r: std::io::Result<impl Sized>) -> Option<i32> {
    r.err().and_then(|e| e.raw_os_error())
}

#[test]
fn mount_fuse_in_scrittura() {
    if !fuse_usable() {
        eprintln!("/dev/fuse non disponibile o non root: test saltato");
        return;
    }
    let tmp = TempDir::new().unwrap();
    let mut repo = Repo::init(config(tmp.path()), PW).unwrap();
    put(&mut repo, tmp.path(), "esistente/vecchio.txt", b"contenuto vecchio\n");
    repo.save().unwrap();
    drop(repo);
    let staging = tmp.path().join("appoggio");
    let m = start_mount(tmp.path(), &["--rw", "--staging", staging.to_str().unwrap()]);
    let p = m.point.clone();
    // un repository aperto a parte: vede quello che il mount ha salvato. L'indice si salva entro due
    // secondi; fsync sulla cartella (come `sync`) lo salva subito
    let check = || {
        fs::File::open(&p).unwrap().sync_all().unwrap();
        Repo::open(config(tmp.path()), PW).unwrap()
    };
    let content = |repo: &Repo, path: &str| {
        let out = tmp.path().join("estratto");
        repo.get_file(path, &out, &mut Default::default()).unwrap();
        let data = fs::read(&out).unwrap();
        fs::remove_file(out).unwrap();
        data
    };

    // file nuovi, piccoli e grandi
    let grande = random("scrittura", 2_300_000);
    fs::write(p.join("nuovo.txt"), b"ciao").unwrap();
    fs::write(p.join("grande.bin"), &grande).unwrap();
    use std::os::unix::fs::MetadataExt;
    assert_eq!(fs::metadata(p.join("nuovo.txt")).unwrap().mode() & 0o777, 0o640);
    assert_eq!(fs::read(p.join("nuovo.txt")).unwrap(), b"ciao");
    assert!(fs::read(p.join("grande.bin")).unwrap() == grande);
    let r = check();
    assert_eq!(content(&r, "nuovo.txt"), b"ciao");
    assert!(content(&r, "grande.bin") == grande, "salvato nel repository");
    assert_eq!(fs::read_dir(&staging).unwrap().count(), 0, "copie locali cancellate");

    // aggiunta in coda, sovrascrittura più corta, troncamento, scrittura in mezzo
    use std::io::Write;
    fs::OpenOptions::new().append(true).open(p.join("nuovo.txt")).unwrap().write_all(b" mondo").unwrap();
    assert_eq!(fs::read(p.join("nuovo.txt")).unwrap(), b"ciao mondo");
    fs::write(p.join("esistente/vecchio.txt"), b"nuovo").unwrap();
    assert_eq!(fs::read(p.join("esistente/vecchio.txt")).unwrap(), b"nuovo");
    fs::OpenOptions::new().write(true).open(p.join("grande.bin")).unwrap().set_len(1000).unwrap();
    assert_eq!(fs::metadata(p.join("grande.bin")).unwrap().len(), 1000);
    {
        use std::os::unix::fs::FileExt;
        let f = fs::OpenOptions::new().read(true).write(true).open(p.join("nuovo.txt")).unwrap();
        f.write_all_at(b"C", 0).unwrap();
        // prima della chiusura si legge già il contenuto nuovo
        assert_eq!(fs::read(p.join("nuovo.txt")).unwrap(), b"Ciao mondo");
    }
    let r = check();
    assert_eq!(content(&r, "nuovo.txt"), b"Ciao mondo");
    assert_eq!(content(&r, "esistente/vecchio.txt"), b"nuovo");
    assert!(content(&r, "grande.bin") == grande[..1000]);

    // data di modifica (touch -d, cp -p)
    let t = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    fs::File::options().write(true).open(p.join("nuovo.txt")).unwrap().set_modified(t).unwrap();
    assert_eq!(fs::metadata(p.join("nuovo.txt")).unwrap().modified().unwrap(), t);
    assert_eq!(check().index().files["nuovo.txt"].mtime, Some(1_700_000_000));

    // cartelle: vuote, annidate, rinominate, tolte
    fs::create_dir_all(p.join("a/b/c")).unwrap();
    assert!(p.join("a/b/c").is_dir());
    assert_eq!(errno_of(fs::create_dir(p.join("a"))), Some(libc::EEXIST));
    fs::rename(p.join("nuovo.txt"), p.join("a/b/spostato.txt")).unwrap();
    assert!(!p.join("nuovo.txt").exists());
    assert_eq!(fs::read(p.join("a/b/spostato.txt")).unwrap(), b"Ciao mondo");
    fs::rename(p.join("a"), p.join("z")).unwrap();
    assert_eq!(fs::read(p.join("z/b/spostato.txt")).unwrap(), b"Ciao mondo");
    assert!(p.join("z/b/c").is_dir());
    assert_eq!(errno_of(fs::remove_dir(p.join("z/b"))), Some(libc::ENOTEMPTY));
    fs::create_dir(p.join("vuota")).unwrap();
    assert_eq!(errno_of(fs::rename(p.join("vuota"), p.join("z"))), Some(libc::ENOTEMPTY));
    fs::rename(p.join("z/b/c"), p.join("vuota")).unwrap(); // su una cartella vuota: la sostituisce
    fs::remove_dir(p.join("vuota")).unwrap();
    // togliendo l'ultimo file la cartella resta
    fs::remove_file(p.join("z/b/spostato.txt")).unwrap();
    assert!(p.join("z/b").is_dir());
    assert_eq!(fs::read_dir(p.join("z/b")).unwrap().count(), 0);
    // sovrascrittura con rinomina (come fanno gli editor e rsync)
    fs::write(p.join("esistente/.tmp"), b"versione 2").unwrap();
    fs::rename(p.join("esistente/.tmp"), p.join("esistente/vecchio.txt")).unwrap();
    assert_eq!(fs::read(p.join("esistente/vecchio.txt")).unwrap(), b"versione 2");

    let r = check();
    let mut files: Vec<&String> = r.index().files.keys().collect();
    files.sort();
    assert_eq!(files, ["esistente/vecchio.txt", "grande.bin"]);
    assert!(r.index().dirs.contains("z/b"), "cartella vuota salvata nell'indice");
    assert_eq!(content(&r, "esistente/vecchio.txt"), b"versione 2");
    fs::remove_file(p.join("grande.bin")).unwrap();

    // il lock si lascia dopo qualche secondo senza scritture
    // anche senza fsync l'indice arriva sui backend entro pochi secondi
    fs::write(p.join("senza-sync.txt"), b"x").unwrap();
    wait("salvataggio ritardato", 10, || Repo::open(config(tmp.path()), PW).unwrap().index().files.contains_key("senza-sync.txt"));
    fs::remove_file(p.join("senza-sync.txt")).unwrap();
    let mut other = Repo::open(config(tmp.path()), PW).unwrap();
    wait("lock lasciato dal mount", 15, || other.lock().is_ok());
    assert!(!other.index().files.contains_key("senza-sync.txt"), "la cancellazione salvata prima di lasciare il lock");
    // e mentre un altro processo lo tiene, una scrittura aspetta invece di fallire
    let writer = {
        let p = p.clone();
        std::thread::spawn(move || fs::write(p.join("in_attesa.txt"), b"aspettato"))
    };
    std::thread::sleep(Duration::from_secs(3));
    assert!(!writer.is_finished(), "la scrittura deve aspettare il lock");
    other.unlock();
    writer.join().unwrap().unwrap();

    // dopo uno smontaggio tutto è ancora lì
    stop_mount(m);
    let m = start_mount(tmp.path(), &["--rw", "--staging", staging.to_str().unwrap()]);
    let p = m.point.clone();
    assert_eq!(fs::read(p.join("in_attesa.txt")).unwrap(), b"aspettato");
    assert_eq!(fs::read(p.join("esistente/vecchio.txt")).unwrap(), b"versione 2");
    assert!(p.join("z/b").is_dir());
    assert!(!p.join("grande.bin").exists());
    stop_mount(m);
    // i blocchi dei file tolti sono stati cancellati: la verifica è pulita
    let mut r = check();
    let report = r.scrub(false).unwrap();
    assert_eq!((report.shards_missing, report.shards_corrupt), (0, 0));
}
