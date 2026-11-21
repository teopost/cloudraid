//! Test di integrazione: cartelle locali al posto degli account cloud.

use cloudraid::{AppsConfig, BackendConfig, BackendKind, ChunkConfig, Config, ReadStats, Repo};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const PW: &str = "password di prova";

/// Blocchi piccoli, così anche file di pochi MB diventano molti blocchi.
const SMALL: ChunkConfig = ChunkConfig { min: 16 << 10, avg: 64 << 10, max: 256 << 10 };

struct Lab {
    tmp: TempDir,
    cfg: Config,
}

impl Lab {
    fn new(backends: usize, k: usize, m: usize) -> Lab {
        let tmp = TempDir::new().unwrap();
        let cfg = Config {
            data_shards: k,
            parity_shards: m,
            chunk: SMALL,
            transfers: 4,
            apps: AppsConfig::default(),
            dir: None,
            file: None,
            backends: (0..backends)
                .map(|i| {
                    let path = tmp.path().join(format!("acc{i}"));
                    BackendConfig {
                        name: format!("acc{i}"),
                        kind: BackendKind::Local { path },
                        delay_ms: 0,
                        limit_gb: None,
                    }
                })
                .collect(),
        };
        Lab { tmp, cfg }
    }

    fn backend_dir(&self, i: usize) -> PathBuf {
        self.tmp.path().join(format!("acc{i}"))
    }

    /// Simula la perdita totale di un account.
    fn lose(&self, i: usize) {
        fs::remove_dir_all(self.backend_dir(i)).unwrap();
    }

    /// Simula un account nuovo e vuoto al posto di quello perso.
    fn replace(&self, i: usize) {
        fs::create_dir_all(self.backend_dir(i)).unwrap();
    }

    fn init(&self) -> Repo {
        Repo::init(self.cfg.clone(), PW).unwrap()
    }

    fn open(&self) -> Repo {
        Repo::open(self.cfg.clone(), PW).unwrap()
    }

    fn src(&self) -> PathBuf {
        let p = self.tmp.path().join("src");
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn out(&self, name: &str) -> PathBuf {
        self.tmp.path().join("out").join(name)
    }

    /// Frammenti presenti su un backend.
    fn shards_on(&self, i: usize) -> usize {
        let data = self.backend_dir(i).join("data");
        if !data.exists() {
            return 0;
        }
        walk(&data)
    }
}

fn walk(p: &Path) -> usize {
    fs::read_dir(p)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            if e.file_type().unwrap().is_dir() {
                walk(&e.path())
            } else {
                1
            }
        })
        .sum()
}

/// Dati pseudo-casuali riproducibili.
fn random(seed: &str, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    blake3::Hasher::new().update(seed.as_bytes()).finalize_xof().fill(&mut out);
    out
}

/// Crea i file di prova e li carica: vuoto, 1 byte, piccolo, medio, grande.
fn load_samples(lab: &Lab, repo: &mut Repo) -> Vec<(String, Vec<u8>)> {
    let samples = vec![
        ("vuoto.bin".to_string(), Vec::new()),
        ("uno.bin".to_string(), b"x".to_vec()),
        ("piccolo.txt".to_string(), b"ciao cloudraid\n".repeat(100)),
        ("medio.bin".to_string(), random("medio", 700_000)),
        ("cartella/grande.bin".to_string(), random("grande", 3_000_000)),
    ];
    for (name, data) in &samples {
        let p = lab.src().join(name.replace('/', "_"));
        fs::write(&p, data).unwrap();
        repo.put_file(&p, name).unwrap();
    }
    repo.save().unwrap();
    samples
}

fn check_all(lab: &Lab, repo: &Repo, samples: &[(String, Vec<u8>)]) -> ReadStats {
    let mut stats = ReadStats::default();
    for (name, data) in samples {
        let out = lab.out(&name.replace('/', "_"));
        repo.get_file(name, &out, &mut stats).unwrap_or_else(|e| panic!("{name}: {e:#}"));
        assert!(fs::read(&out).unwrap() == *data, "{name}: contenuto diverso");
        fs::remove_file(out).unwrap();
    }
    stats
}

#[test]
fn andata_e_ritorno() {
    let lab = Lab::new(6, 4, 2);
    let mut repo = lab.init();
    let samples = load_samples(&lab, &mut repo);
    let stats = check_all(&lab, &lab.open(), &samples);
    assert!(stats.chunks > 20, "pochi blocchi: {}", stats.chunks);
    assert_eq!(stats.degraded, 0);
    // i frammenti sono distribuiti su tutti i backend
    for i in 0..6 {
        assert!(lab.shards_on(i) > 0, "backend {i} vuoto");
    }
}

#[test]
fn perdita_di_due_backend_qualsiasi() {
    // tutte le 15 coppie possibili su 6 backend
    for a in 0..6 {
        for b in a + 1..6 {
            let lab = Lab::new(6, 4, 2);
            let mut repo = lab.init();
            let samples = load_samples(&lab, &mut repo);
            drop(repo);
            lab.lose(a);
            lab.lose(b);
            let stats = check_all(&lab, &lab.open(), &samples);
            assert!(stats.degraded > 0, "persi {a} e {b}: nessun blocco ricostruito con la parità?");
        }
    }
}

#[test]
fn perdita_di_tre_backend_e_errore_pulito() {
    let lab = Lab::new(6, 4, 2);
    let mut repo = lab.init();
    let samples = load_samples(&lab, &mut repo);
    drop(repo);
    for i in [0, 2, 4] {
        lab.lose(i);
    }
    let repo = lab.open();
    let out = lab.out("grande.bin");
    let err = repo.get_file("cartella/grande.bin", &out, &mut ReadStats::default()).unwrap_err();
    assert!(format!("{err:#}").contains("irrecuperabile"), "{err:#}");
    // niente file parziali lasciati in giro
    assert!(!out.exists());
    assert!(!out.parent().unwrap().join("grande.bin.cloudraid-tmp").exists());
    // lo scrub segnala i file persi
    let mut repo = repo;
    let r = repo.scrub(false).unwrap();
    assert!(!r.lost_chunks.is_empty());
    assert!(r.lost_files.contains(&samples[4].0));
}

#[test]
fn frammento_alterato_conta_come_mancante() {
    let lab = Lab::new(6, 4, 2);
    let mut repo = lab.init();
    let samples = load_samples(&lab, &mut repo);
    // altera un byte di tutti i frammenti del backend 1 e perde il backend 3
    let mut altered = 0;
    let data = lab.backend_dir(1).join("data");
    for dir in fs::read_dir(&data).unwrap() {
        for f in fs::read_dir(dir.unwrap().path()).unwrap() {
            let p = f.unwrap().path();
            let mut bytes = fs::read(&p).unwrap();
            bytes[0] ^= 0xff;
            fs::write(&p, bytes).unwrap();
            altered += 1;
        }
    }
    assert!(altered > 0);
    lab.lose(3);
    let repo = lab.open();
    check_all(&lab, &repo, &samples);
    let mut repo = repo;
    let r = repo.scrub(false).unwrap();
    assert_eq!(r.shards_corrupt as usize, altered);
    assert!(r.lost_chunks.is_empty());
}

#[test]
fn riparazione_su_account_nuovi() {
    let lab = Lab::new(6, 4, 2);
    let mut repo = lab.init();
    let samples = load_samples(&lab, &mut repo);
    drop(repo);
    lab.lose(0);
    lab.lose(5);
    lab.replace(0);
    lab.replace(5);

    let mut repo = lab.open();
    let r = repo.scrub(true).unwrap();
    assert!(r.repaired_shards > 0);
    assert_eq!(r.unrepaired_shards, 0);
    repo.save().unwrap();
    drop(repo);

    let mut repo = lab.open();
    let r = repo.scrub(false).unwrap();
    assert_eq!(r.shards_missing + r.shards_corrupt, 0, "dopo la riparazione è tutto integro");
    drop(repo);

    // la ridondanza è tornata piena: si possono perdere altri due backend
    lab.lose(1);
    lab.lose(2);
    check_all(&lab, &lab.open(), &samples);
}

#[test]
fn riparazione_spostando_su_un_backend_libero() {
    // 7 backend per 6 frammenti: uno è sempre libero per ogni blocco
    let lab = Lab::new(7, 4, 2);
    let mut repo = lab.init();
    let samples = load_samples(&lab, &mut repo);
    drop(repo);
    lab.lose(2); // non viene sostituito

    let mut repo = lab.open();
    let r = repo.scrub(true).unwrap();
    assert!(r.repaired_shards > 0);
    assert_eq!(r.unrepaired_shards, 0);
    repo.save().unwrap();
    drop(repo);

    let mut repo = lab.open();
    assert!(repo.status().iter().all(|s| s.name != "acc2" || s.shards == 0), "acc2 non deve più avere frammenti");
    let r = repo.scrub(false).unwrap();
    assert_eq!(r.shards_missing, 0);
    drop(repo);
    lab.lose(4);
    lab.lose(6);
    check_all(&lab, &lab.open(), &samples);
}

#[test]
fn deduplicazione_e_cancellazione() {
    let lab = Lab::new(6, 4, 2);
    let mut repo = lab.init();
    let data = random("dup", 1_500_000);
    let p = lab.src().join("a.bin");
    fs::write(&p, &data).unwrap();
    let first = repo.put_file(&p, "a.bin").unwrap();
    let second = repo.put_file(&p, "copia/a.bin").unwrap();
    assert!(first > 0);
    assert_eq!(second, 0, "la copia non deve scrivere blocchi nuovi");
    repo.save().unwrap();
    let total: usize = (0..6).map(|i| lab.shards_on(i)).sum();
    assert_eq!(total as u64, first * 6);

    // togliere una delle due copie non cancella niente
    repo.remove("a.bin").unwrap();
    repo.save().unwrap();
    assert_eq!((0..6).map(|i| lab.shards_on(i)).sum::<usize>(), total);
    check_all(&lab, &repo, &[("copia/a.bin".into(), data)]);

    // togliere anche l'ultima libera tutti i frammenti
    repo.remove("copia").unwrap();
    repo.save().unwrap();
    assert_eq!((0..6).map(|i| lab.shards_on(i)).sum::<usize>(), 0);
    assert!(repo.index().chunks.is_empty());
}

#[test]
fn sovrascrittura_di_un_file() {
    let lab = Lab::new(6, 4, 2);
    let mut repo = lab.init();
    let p = lab.src().join("f.bin");
    fs::write(&p, random("v1", 500_000)).unwrap();
    repo.put_file(&p, "f.bin").unwrap();
    let v2 = random("v2", 400_000);
    fs::write(&p, &v2).unwrap();
    repo.put_file(&p, "f.bin").unwrap();
    repo.save().unwrap();
    assert_eq!(repo.index().files.len(), 1);
    let expected: u64 = repo.index().files["f.bin"].chunks.len() as u64;
    assert_eq!(repo.index().chunks.len() as u64, expected, "i blocchi della v1 sono stati liberati");
    check_all(&lab, &lab.open(), &[("f.bin".into(), v2)]);
}

#[test]
fn password_errata() {
    let lab = Lab::new(6, 4, 2);
    lab.init();
    let err = Repo::open(lab.cfg.clone(), "sbagliata").err().expect("deve fallire");
    assert!(format!("{err:#}").contains("password errata"));
}

#[test]
fn indice_piu_recente_anche_se_un_backend_era_spento() {
    let lab = Lab::new(6, 4, 2);
    let mut repo = lab.init();
    let samples = load_samples(&lab, &mut repo);
    drop(repo);

    // il backend 0 è spento mentre si aggiunge un file: con 5 backend su 6 il file si scrive con
    // 3 frammenti di dati + 2 di parità, quindi si possono ancora perdere 2 backend qualsiasi
    let off = lab.tmp.path().join("acc0-spento");
    fs::rename(lab.backend_dir(0), &off).unwrap();
    let mut repo = lab.open();
    let p = lab.src().join("nuovo.bin");
    let nuovo = random("nuovo", 200_000);
    fs::write(&p, &nuovo).unwrap();
    repo.put_file(&p, "nuovo.bin").unwrap();
    for id in &repo.index().files["nuovo.bin"].chunks {
        let c = &repo.index().chunks[id];
        assert_eq!((c.k, c.m, c.shards.len()), (3, 2, 5));
        assert!(c.shards.iter().all(|s| s.backend != "acc0"));
    }
    repo.remove("uno.bin").unwrap();
    repo.save().unwrap();
    drop(repo);

    // il backend 0 torna con l'indice vecchio: vince comunque il più recente
    fs::rename(&off, lab.backend_dir(0)).unwrap();
    let repo = lab.open();
    assert!(!repo.index().files.contains_key("uno.bin"));
    let mut rest: Vec<_> = samples.into_iter().filter(|(n, _)| n != "uno.bin").collect();
    rest.push(("nuovo.bin".to_string(), nuovo));
    check_all(&lab, &repo, &rest);
}

#[test]
fn init_rifiuta_un_repository_esistente() {
    let lab = Lab::new(6, 4, 2);
    lab.init();
    assert!(Repo::init(lab.cfg.clone(), PW).is_err());
}

#[test]
fn configurazione_con_pochi_backend() {
    let mut lab = Lab::new(5, 4, 2);
    assert!(lab.cfg.validate().is_err());
    lab.cfg.parity_shards = 1;
    assert!(lab.cfg.validate().is_ok());
}

// ---------------------------------------------------------------------------------------------
// Lock

#[test]
fn lock_esclusivo() {
    let lab = Lab::new(6, 4, 2);
    let mut a = lab.init();
    a.lock().unwrap();
    let mut b = lab.open();
    let err = b.lock().unwrap_err();
    assert!(format!("{err:#}").contains("repository in uso"), "{err:#}");
    assert_eq!(b.locks().len(), 1);
    // unlock senza --all non tocca un lock attivo
    assert!(b.break_locks(false).is_empty());
    drop(a); // rilascia il lock
    b.lock().unwrap();
    assert!(lab.open().locks().len() == 1);
}

#[test]
fn lock_scaduto_ignorato_e_cancellabile() {
    let lab = Lab::new(6, 4, 2);
    lab.init();
    // un lock vecchio di un'ora lasciato da un processo morto
    let old = r#"{"id":"deadbeefdeadbeef","host":"altro","pid":1,"time":1}"#;
    for i in 0..6 {
        let dir = lab.backend_dir(i).join("locks");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("deadbeefdeadbeef.json"), old).unwrap();
    }
    let mut repo = lab.open();
    repo.lock().unwrap(); // scaduto: non blocca
    let removed = repo.break_locks(false);
    assert_eq!(removed.len(), 1);
    assert!(repo.locks().is_empty());
    for i in 0..6 {
        assert!(!lab.backend_dir(i).join("locks/deadbeefdeadbeef.json").exists());
    }
}

#[test]
fn configurazione_di_esempio_valida() {
    let cfg = Config::load(Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/cloudraid.example.toml"))).unwrap();
    assert_eq!(cfg.backends.len(), 6);
    let types: Vec<&str> = cfg.backends.iter().map(|b| b.kind.type_name()).collect();
    assert_eq!(types, ["local", "local", "webdav", "s3", "dropbox", "gdrive"]);
}

#[test]
fn configurazione_con_rclone_errore_chiaro() {
    let old = "data_shards = 1\nparity_shards = 0\n[rclone]\nbinary = \"rclone\"\n[[backend]]\nname = \"a\"\ntype = \"rclone\"\nremote = \"x:y\"\n";
    let err = Config::parse(old).unwrap_err();
    assert!(format!("{err:#}").contains("rclone non è più usato"), "{err:#}");
}

#[test]
fn backend_aggiunto_riceve_la_chiave() {
    let mut lab = Lab::new(3, 2, 1);
    let mut repo = lab.init();
    load_samples(&lab, &mut repo);
    drop(repo);

    // un quarto account, aggiunto alla configurazione dopo init (cartella creata a mano)
    let path = lab.tmp.path().join("acc3");
    fs::create_dir_all(&path).unwrap();
    lab.cfg.backends.push(BackendConfig { name: "acc3".into(), kind: BackendKind::Local { path: path.clone() }, delay_ms: 0, limit_gb: None });
    let mut repo = lab.open();
    assert!(!path.join("cloudraid.key").exists());
    let p = lab.src().join("dopo.bin");
    fs::write(&p, random("dopo", 300_000)).unwrap();
    repo.lock().unwrap();
    repo.put_file(&p, "dopo.bin").unwrap();
    repo.save().unwrap();
    repo.unlock();
    drop(repo);
    assert!(path.join("cloudraid.key").exists(), "chiave copiata sul backend nuovo");

    // ora il repository si apre anche se la chiave sparisce dagli account originali
    for i in 0..3 {
        fs::remove_file(lab.backend_dir(i).join("cloudraid.key")).unwrap();
    }
    let repo = lab.open();
    assert!(repo.index().files.contains_key("dopo.bin"));
}

#[test]
fn chiave_di_un_altro_repository_non_sovrascritta() {
    let mut lab = Lab::new(3, 2, 1);
    let mut repo = lab.init();
    load_samples(&lab, &mut repo);
    drop(repo);
    // un account che contiene già un altro repository
    let altro = Lab::new(3, 2, 1);
    altro.init();
    let path = altro.backend_dir(0);
    let before = fs::read(path.join("cloudraid.key")).unwrap();
    lab.cfg.backends.push(BackendConfig { name: "estraneo".into(), kind: BackendKind::Local { path: path.clone() }, delay_ms: 0, limit_gb: None });
    let mut repo = lab.open();
    repo.lock().unwrap();
    repo.remove("uno.bin").unwrap();
    repo.save().unwrap();
    assert_eq!(fs::read(path.join("cloudraid.key")).unwrap(), before);
}

#[test]
fn rinomina_cartelle_e_date_senza_spostare_dati() {
    let lab = Lab::new(6, 4, 2);
    let mut repo = lab.init();
    let samples = load_samples(&lab, &mut repo);
    let shards_before: usize = (0..6).map(|i| lab.shards_on(i)).sum();

    // file: rinomina e sostituzione di un file esistente
    repo.rename("medio.bin", "archivio/medio.bin").unwrap();
    repo.rename("uno.bin", "piccolo.txt").unwrap();
    assert!(!repo.index().files.contains_key("medio.bin"));
    assert_eq!(repo.index().files["piccolo.txt"].size, 1);
    // cartella intera
    repo.rename("cartella", "altra/dentro").unwrap();
    assert!(repo.index().files.contains_key("altra/dentro/grande.bin"));
    assert!(repo.rename("altra", "altra/sotto").is_err(), "dentro sé stessa");
    assert!(repo.rename("piccolo.txt", "altra").is_err(), "un file su una cartella");
    assert!(repo.rename("non-esiste", "x").is_err());

    // cartelle vuote esplicite
    repo.mkdir("vuota/annidata").unwrap();
    assert!(repo.is_dir("vuota") && repo.is_dir("vuota/annidata"));
    assert!(repo.mkdir("piccolo.txt/x").is_err(), "sotto un file");
    assert!(repo.rmdir("vuota").is_err(), "non vuota");
    repo.rename("vuota", "spostata").unwrap();
    assert!(repo.index().dirs.contains("spostata/annidata"));
    let p = lab.src().join("x");
    fs::write(&p, b"x").unwrap();
    assert!(repo.put_file(&p, "spostata").is_err(), "un file al posto di una cartella");
    repo.rmdir("spostata/annidata").unwrap();
    assert!(repo.rmdir("altra/dentro").is_err(), "contiene file");
    repo.set_mtime("piccolo.txt", 1_600_000_000).unwrap();
    repo.save().unwrap();
    drop(repo);

    // tutto salvato, nessun dato spostato, contenuti intatti; il blocco di "piccolo.txt" vecchio è stato liberato
    let mut repo = lab.open();
    assert!(repo.index().dirs.contains("spostata") && !repo.index().dirs.contains("spostata/annidata"));
    assert_eq!(repo.index().files["piccolo.txt"].mtime, Some(1_600_000_000));
    let renamed: Vec<(String, Vec<u8>)> = samples
        .iter()
        .filter_map(|(n, d)| match n.as_str() {
            "medio.bin" => Some(("archivio/medio.bin".into(), d.clone())),
            "uno.bin" => Some(("piccolo.txt".into(), d.clone())),
            "cartella/grande.bin" => Some(("altra/dentro/grande.bin".into(), d.clone())),
            "vuoto.bin" => Some((n.clone(), d.clone())),
            _ => None,
        })
        .collect();
    check_all(&lab, &repo, &renamed);
    let shards_after: usize = (0..6).map(|i| lab.shards_on(i)).sum();
    assert!(shards_after < shards_before && shards_after > 0);
    // `rm` di una cartella toglie anche le cartelle vuote che contiene
    repo.lock().unwrap();
    assert_eq!(repo.remove("spostata").unwrap(), ["spostata/"]);
    assert!(!repo.is_dir("spostata"));
    repo.save().unwrap();
}

/// Byte di frammenti per backend secondo l'indice.
fn loads(repo: &Repo, n: usize) -> Vec<u64> {
    let mut out = vec![0u64; n];
    for c in repo.index().chunks.values() {
        for loc in &c.shards {
            out[loc.backend.trim_start_matches("acc").parse::<usize>().unwrap()] += c.shard_len;
        }
    }
    out
}

#[test]
fn pulizia_dei_frammenti_orfani() {
    let lab = Lab::new(6, 4, 2);
    let mut repo = lab.init();
    let samples = load_samples(&lab, &mut repo);
    // senza lock la pulizia non parte: un altro processo potrebbe star caricando
    assert!(repo.gc(true, &mut |_, _| {}).is_err());

    // tre tipi di orfani: frammento di un caricamento interrotto, file temporaneo,
    // copia di un frammento rimasta su un backend che non dovrebbe averlo
    let (id, chunk) = repo.index().chunks.iter().next().map(|(i, c)| (i.clone(), c.clone())).unwrap();
    let orphan = lab.backend_dir(0).join("data/zz/zz00interrotto.3");
    fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    fs::write(&orphan, b"frammento mai finito nell'indice").unwrap();
    let tmp = lab.backend_dir(1).join(format!("data/{}/{id}.0.ab12cd34.tmp", &id[..2]));
    fs::create_dir_all(tmp.parent().unwrap()).unwrap();
    fs::write(&tmp, b"scrittura interrotta").unwrap();
    // con 6 backend e 4+2 frammenti ogni backend ha un frammento di ogni blocco: il frammento 0
    // copiato su un altro backend (che per l'indice ha un altro frammento) è un orfano
    let holder = chunk.shards[0].backend.trim_start_matches("acc").parse::<usize>().unwrap();
    let key = format!("data/{}/{id}.0", &id[..2]);
    let copy = lab.backend_dir((holder + 1) % 6).join(&key);
    fs::create_dir_all(copy.parent().unwrap()).unwrap();
    fs::copy(lab.backend_dir(holder).join(&key), &copy).unwrap();
    // cartella vuota lasciata dalla prova delle versioni fino alla 0.9
    let probe = lab.backend_dir(2).join("prova");
    fs::create_dir_all(&probe).unwrap();

    repo.lock().unwrap();
    let dry = repo.gc(true, &mut |_, _| {}).unwrap();
    assert_eq!(dry.orphans, 3, "{dry:?}");
    assert_eq!(dry.deleted, 0);
    assert!(orphan.exists() && tmp.exists() && copy.exists() && probe.exists(), "la simulazione non cancella");
    let mut progress = Vec::new();
    let r = repo.gc(false, &mut |d, t| progress.push((d, t))).unwrap();
    assert_eq!((r.orphans, r.deleted, r.failed), (3, 3, 0));
    assert_eq!(progress.last(), Some(&(3, 3)));
    assert!(!orphan.exists() && !tmp.exists() && !copy.exists());
    assert!(!probe.exists(), "la cartella della prova rimasta viene tolta");
    assert_eq!(repo.gc(true, &mut |_, _| {}).unwrap().orphans, 0);
    // i file sono intatti
    check_all(&lab, &repo, &samples);
    let s = repo.scrub(false).unwrap();
    assert_eq!((s.shards_missing, s.shards_corrupt), (0, 0));
}

#[test]
fn ribilanciamento_dopo_un_backend_aggiunto() {
    let mut lab = Lab::new(6, 4, 2);
    let mut repo = lab.init();
    let samples = load_samples(&lab, &mut repo);
    drop(repo);
    // due account nuovi, vuoti
    for i in 6..8 {
        let path = lab.tmp.path().join(format!("acc{i}"));
        fs::create_dir_all(&path).unwrap();
        lab.cfg.backends.push(BackendConfig { name: format!("acc{i}"), kind: BackendKind::Local { path }, delay_ms: 0, limit_gb: None });
    }
    let mut repo = lab.open();
    repo.lock().unwrap();
    let before = loads(&repo, 8);
    assert_eq!((before[6], before[7]), (0, 0));

    let plan = repo.rebalance(true, &mut |_, _| {}).unwrap();
    assert!(plan.planned > 0);
    assert_eq!((lab.shards_on(6), lab.shards_on(7)), (0, 0), "la simulazione non sposta");
    assert_eq!(loads(&repo, 8), before);

    // si copia solo quello che serve a riempire i backend nuovi, niente giri fra quelli vecchi
    let filled: u64 = plan.after.iter().filter(|(n, _)| n == "acc6" || n == "acc7").map(|(_, b)| *b).sum();
    assert_eq!(plan.planned_bytes, filled, "{plan:?}");
    let r = repo.rebalance(false, &mut |_, _| {}).unwrap();
    assert_eq!((r.moved, r.failed, r.moved_bytes), (plan.planned, 0, plan.planned_bytes));
    let after = loads(&repo, 8);
    let spread = |v: &[u64]| v.iter().max().unwrap() - v.iter().min().unwrap();
    let biggest = repo.index().chunks.values().map(|c| c.shard_len).max().unwrap();
    assert!(spread(&after) <= 2 * biggest, "equilibrato: {after:?}");
    assert!(lab.shards_on(6) > 0 && lab.shards_on(7) > 0);
    // ogni blocco ha ancora i frammenti su backend tutti diversi
    for c in repo.index().chunks.values() {
        let set: std::collections::HashSet<&str> = c.shards.iter().map(|l| l.backend.as_str()).collect();
        assert_eq!(set.len(), c.shards.len());
    }
    // le copie vecchie sono state cancellate: niente orfani, tutto integro e leggibile
    assert_eq!(repo.gc(true, &mut |_, _| {}).unwrap().orphans, 0);
    let total: usize = (0..8).map(|i| lab.shards_on(i)).sum();
    assert_eq!(total, repo.index().chunks.values().map(|c| c.shards.len()).sum::<usize>());
    drop(repo);
    let mut repo = lab.open();
    check_all(&lab, &repo, &samples);
    let s = repo.scrub(false).unwrap();
    assert_eq!((s.shards_missing, s.shards_corrupt), (0, 0));
    // una seconda volta non c'è niente da fare
    repo.lock().unwrap();
    assert_eq!(repo.rebalance(true, &mut |_, _| {}).unwrap().planned, 0, "già equilibrato");
    // e si possono perdere ancora due backend qualsiasi, anche fra i nuovi
    drop(repo);
    lab.lose(6);
    lab.lose(2);
    check_all(&lab, &lab.open(), &samples);
}

/// Un laboratorio con alcuni backend rallentati apposta.
fn slow_lab(slow: &[usize], delay_ms: u64) -> Lab {
    let mut lab = Lab::new(6, 4, 2);
    for &i in slow {
        lab.cfg.backends[i].delay_ms = delay_ms;
    }
    lab
}

#[test]
fn letture_a_gara_un_backend_lento_non_blocca() {
    let lab = slow_lab(&[0], 1500);
    let mut repo = lab.init();
    let samples = load_samples(&lab, &mut repo);
    drop(repo);
    let repo = lab.open();
    let blocks: usize = repo.index().chunks.len();
    assert!(blocks > 20, "servono molti blocchi: {blocks}");
    // senza la gara ogni blocco con un frammento di dati su acc0 aspetterebbe 1,5 s
    let start = std::time::Instant::now();
    let stats = check_all(&lab, &repo, &samples);
    let took = start.elapsed();
    assert!(took < std::time::Duration::from_secs(8), "{blocks} blocchi letti in {took:?}");
    assert_eq!(stats.degraded, 0, "nessun backend guasto: la ridondanza è piena");
    // il backend lento è stato misurato come tale
    std::thread::sleep(std::time::Duration::from_secs(2));
    let st = repo.status();
    let lat = |n: &str| st.iter().find(|b| b.name == n).unwrap().latency_ms;
    // (scartato presto, la sua media non arriva per forza a 1,5 s: basta che sia molto più lento degli altri)
    let (slow, fast) = (lat("acc0").unwrap(), lat("acc1").unwrap());
    assert!(slow > 200.0 && slow > 10.0 * fast, "acc0 {slow} ms, acc1 {fast} ms");
}

#[test]
fn letture_a_gara_con_backend_persi_e_lenti() {
    let lab = slow_lab(&[0], 300);
    let mut repo = lab.init();
    let samples = load_samples(&lab, &mut repo);
    drop(repo);
    // un backend perso e uno lento: bastano i 4 rimasti veloci, ma la ridondanza è ridotta
    lab.lose(3);
    let stats = check_all(&lab, &lab.open(), &samples);
    assert!(stats.degraded > 0);
    // due persi: per forza anche il lento, ma si legge tutto
    lab.lose(4);
    let start = std::time::Instant::now();
    check_all(&lab, &lab.open(), &samples);
    assert!(start.elapsed() > std::time::Duration::from_millis(300), "ha dovuto aspettare il lento");
    // tre persi: errore pulito, senza restare appesi
    lab.lose(5);
    let repo = lab.open();
    let err = repo.get_file("cartella/grande.bin", &lab.out("x"), &mut ReadStats::default()).unwrap_err();
    assert!(format!("{err:#}").contains("irrecuperabile"), "{err:#}");
}

#[test]
fn dismissione_di_un_backend() {
    let lab = Lab::new(7, 4, 2);
    let mut repo = lab.init();
    let samples = load_samples(&lab, &mut repo);
    assert!(lab.shards_on(2) > 0);
    // senza lock niente
    assert!(repo.retire("acc2", &mut |_, _| {}).is_err());
    repo.lock().unwrap();
    assert!(repo.retire("non-esiste", &mut |_, _| {}).is_err());
    let r = repo.retire("acc2", &mut |_, _| {}).unwrap();
    assert_eq!((r.remaining, r.failed), (0, 0), "{r:?}");
    assert!(r.moved > 0);
    assert_eq!(lab.shards_on(2), 0, "copie vecchie cancellate");
    // ogni blocco ha ancora 6 frammenti su backend diversi, nessuno su acc2
    for c in repo.index().chunks.values() {
        let set: std::collections::HashSet<&str> = c.shards.iter().map(|l| l.backend.as_str()).collect();
        assert_eq!(set.len(), 6);
        assert!(!set.contains("acc2"));
    }
    // le scritture nuove e il ribilanciamento lo lasciano stare
    let p = lab.src().join("dopo.bin");
    fs::write(&p, random("dopo", 400_000)).unwrap();
    repo.put_file(&p, "dopo.bin").unwrap();
    repo.save().unwrap();
    assert_eq!(repo.rebalance(true, &mut |_, _| {}).unwrap().after.iter().find(|(n, _)| n == "acc2").unwrap().1, 0);
    assert_eq!(lab.shards_on(2), 0);
    drop(repo);

    // lo stato si salva nell'indice; il backend si può spegnere senza perdere ridondanza
    let mut repo = lab.open();
    assert!(repo.retired().contains("acc2"));
    assert!(repo.status().iter().find(|b| b.name == "acc2").unwrap().retired);
    lab.lose(2);
    lab.lose(5);
    lab.lose(6);
    // persi acc5 e acc6 oltre al dismesso: si legge ancora tutto
    let repo2 = lab.open();
    check_all(&lab, &repo2, &samples);
    drop(repo2);
    lab.replace(2);
    lab.replace(5);
    lab.replace(6);
    // rimesso in servizio, il ribilanciamento gli ridà frammenti
    repo.lock().unwrap();
    repo.scrub(true).unwrap();
    repo.unretire("acc2").unwrap();
    repo.save().unwrap();
    assert!(repo.retired().is_empty());
    repo.rebalance(false, &mut |_, _| {}).unwrap();
    assert!(lab.shards_on(2) > 0);
}

#[test]
fn dismissione_di_un_backend_gia_morto() {
    let lab = Lab::new(7, 4, 2);
    let mut repo = lab.init();
    let samples = load_samples(&lab, &mut repo);
    drop(repo);
    lab.lose(3);
    let mut repo = lab.open();
    repo.lock().unwrap();
    // i suoi frammenti vengono ricostruiti dalla parità sugli altri
    let r = repo.retire("acc3", &mut |_, _| {}).unwrap();
    assert_eq!((r.remaining, r.failed), (0, 0), "{r:?}");
    drop(repo);
    // ridondanza di nuovo piena: si possono perdere altri due backend qualsiasi
    lab.lose(0);
    lab.lose(6);
    check_all(&lab, &lab.open(), &samples);
}

#[test]
fn dismissione_rifiutata_con_pochi_backend() {
    let lab = Lab::new(6, 4, 2);
    let mut repo = lab.init();
    load_samples(&lab, &mut repo);
    repo.lock().unwrap();
    let err = repo.retire("acc1", &mut |_, _| {}).unwrap_err();
    assert!(format!("{err:#}").contains("aggiungi prima"), "{err:#}");
    assert!(repo.retired().is_empty());
}

#[test]
fn prova_di_una_cartella_locale_senza_avanzi() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("disco");
    let bc = BackendConfig { name: "disco".into(), kind: BackendKind::Local { path: root.clone() }, delay_ms: 0, limit_gb: None };
    let keys = std::sync::Arc::new(cloudraid::crypto::Keys::ephemeral());
    let creds = cloudraid::credentials::Credentials::load(None, keys).unwrap();
    let b = cloudraid::backend::open_one(&bc, &AppsConfig::default(), &creds);
    cloudraid::backend::probe(b.as_ref()).unwrap();
    assert!(root.is_dir(), "la cartella del repository resta");
    assert!(!root.join(cloudraid::backend::PROBE_DIR).exists(), "la cartella della prova è stata tolta");
}

/// Controlla che ogni blocco abbia i frammenti su backend tutti diversi e sempre 2 di parità.
fn check_geometry(repo: &Repo) -> std::collections::BTreeSet<(u8, u8)> {
    let mut geoms = std::collections::BTreeSet::new();
    for c in repo.index().chunks.values() {
        let set: std::collections::HashSet<&str> = c.shards.iter().map(|l| l.backend.as_str()).collect();
        assert_eq!(set.len(), c.shards.len(), "frammenti dello stesso blocco sullo stesso backend");
        assert_eq!(c.shards.len(), (c.k + c.m) as usize);
        assert_eq!(c.m, 2, "la parità non cambia mai");
        geoms.insert((c.k, c.m));
    }
    geoms
}

#[test]
fn spazi_piccoli_pieni_e_spazi_grandi_ancora_usati() {
    // sei spazi, due piccolissimi: con la distribuzione uguale il repository si fermerebbe a
    // 4 volte il più piccolo (1,2 MB); qui si caricano 3 MB
    let mut lab = Lab::new(6, 4, 2);
    lab.cfg.backends[0].limit_gb = Some(0.000_3);
    lab.cfg.backends[1].limit_gb = Some(0.000_6);
    let mut repo = lab.init();
    let mut samples = Vec::new();
    for i in 0..6 {
        let data = random(&format!("pezzo{i}"), 500_000);
        let p = lab.src().join(format!("f{i}"));
        fs::write(&p, &data).unwrap();
        repo.put_file(&p, &format!("f{i}.bin")).unwrap();
        samples.push((format!("f{i}.bin"), data));
    }
    repo.save().unwrap();
    let l = loads(&repo, 6);
    assert!(l[0] <= 300_000 && l[1] <= 600_000, "limiti rispettati: {l:?}");
    assert!(l[0] > 200_000 && l[1] > 500_000, "spazi piccoli usati fino in fondo: {l:?}");
    // quando gli spazi con posto sono 5 i blocchi diventano 3 + 2, poi 2 + 2
    let geoms = check_geometry(&repo);
    assert_eq!(geoms, [(2, 2), (3, 2), (4, 2)].into_iter().collect(), "{geoms:?}");
    drop(repo);
    check_all(&lab, &lab.open(), &samples);
    // si possono sempre perdere 2 spazi qualsiasi: anche i due più carichi
    lab.lose(4);
    lab.lose(5);
    check_all(&lab, &lab.open(), &samples);
}

#[test]
fn piu_spazi_i_grandi_ricevono_di_piu() {
    // otto spazi di capienza diversa: ogni blocco va sui 6 con più posto, tutti 4 + 2 finché
    // possibile, e nessuno supera il suo limite
    let mut lab = Lab::new(8, 4, 2);
    let limits = [0.000_4, 0.000_8, 0.001_5, 0.002, 0.002, 0.003, 0.003, 0.003];
    for (b, gb) in lab.cfg.backends.iter_mut().zip(limits) {
        b.limit_gb = Some(gb);
    }
    let mut repo = lab.init();
    let before = repo.capacity_left().unwrap();
    // con la distribuzione uguale: 4 volte il più piccolo (1,6 MB)
    assert!(before > 8_000_000, "capacità stimata {before}");
    let mut samples = Vec::new();
    for i in 0..10 {
        let data = random(&format!("blocco{i}"), 600_000);
        let p = lab.src().join(format!("g{i}"));
        fs::write(&p, &data).unwrap();
        repo.put_file(&p, &format!("g{i}.bin")).unwrap();
        samples.push((format!("g{i}.bin"), data));
    }
    repo.save().unwrap();
    let l = loads(&repo, 8);
    for (i, gb) in limits.iter().enumerate() {
        assert!(l[i] as f64 <= gb * 1e9, "acc{i} oltre il limite: {l:?}");
    }
    assert!(l[7] > 3 * l[0], "i grandi lavorano di più: {l:?}");
    let geoms = check_geometry(&repo);
    assert!(geoms.contains(&(4, 2)), "{geoms:?}");
    let left = repo.capacity_left().unwrap();
    assert!(left < before - 5_000_000, "la stima scende: {before} -> {left}");
    drop(repo);
    lab.lose(6);
    lab.lose(7);
    check_all(&lab, &lab.open(), &samples);
}

#[test]
fn limite_abbassato_e_ribilanciamento() {
    let mut lab = Lab::new(7, 4, 2);
    let mut repo = lab.init();
    let samples = load_samples(&lab, &mut repo);
    let before = loads(&repo, 7)[0];
    assert!(before > 0);
    drop(repo);
    // acc0 ora può tenere solo metà di quello che ha: il ribilanciamento sposta il resto
    lab.cfg.backends[0].limit_gb = Some(before as f64 / 2.0 / 1e9);
    let mut repo = lab.open();
    repo.lock().unwrap();
    let r = repo.rebalance(false, &mut |_, _| {}).unwrap();
    assert!(r.moved > 0 && r.failed == 0, "{r:?}");
    let after = loads(&repo, 7)[0];
    assert!(after <= before / 2, "{before} -> {after}");
    check_geometry(&repo);
    assert_eq!(repo.gc(true, &mut |_, _| {}).unwrap().orphans, 0);
    drop(repo);
    check_all(&lab, &lab.open(), &samples);
}

#[test]
fn al_massimo_due_frammenti_sullo_stesso_disco() {
    // due dischi con tre cartelle ciascuno: quello delle cartelle temporanee e quello di cargo (nel
    // container dei test sono filesystem diversi; se coincidono il test non ha senso e si salta).
    // Con la regola "al massimo 2 frammenti di un blocco sullo stesso disco" i blocchi diventano
    // 2 + 2, due frammenti per disco: si sopravvive alla perdita di un disco intero
    use std::os::unix::fs::MetadataExt;
    let other = tempfile::TempDir::new_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let mut lab = Lab::new(3, 4, 2);
    if fs::metadata(lab.tmp.path()).unwrap().dev() == fs::metadata(other.path()).unwrap().dev() {
        eprintln!("un solo filesystem disponibile: test saltato");
        return;
    }
    for i in 3..6 {
        let path = other.path().join(format!("svc{i}"));
        lab.cfg.backends.push(BackendConfig { name: format!("acc{i}"), kind: BackendKind::Local { path }, delay_ms: 0, limit_gb: None });
    }
    let repo = lab.init();
    drop(repo);
    let mut repo = lab.open();
    let samples = load_samples(&lab, &mut repo);
    for c in repo.index().chunks.values() {
        let on = |range: std::ops::Range<usize>| {
            c.shards.iter().filter(|l| range.contains(&l.backend.trim_start_matches("acc").parse::<usize>().unwrap())).count()
        };
        assert!(on(0..3) <= 2 && on(3..6) <= 2, "{:?}", c.shards.iter().map(|l| &l.backend).collect::<Vec<_>>());
    }
    let s = repo.status();
    assert_eq!(s[0].same_device, ["acc1", "acc2"]);
    let rel = repo.reliability();
    assert_eq!((rel.crowded_chunks, rel.writes_survive_disk_loss, rel.active), (0, true, 6), "{rel:?}");
    drop(repo);
    // perso un intero "disco" (tre cartelle), i file si leggono ancora
    for i in 0..3 {
        lab.lose(i);
    }
    check_all(&lab, &lab.open(), &samples);
}

#[test]
fn backend_di_riserva_rispetta_il_disco() {
    // tre cartelle sul disco dei test e cinque su quello di cargo; una di queste rifiuta le
    // scritture: il frammento va su una riserva che non mette un terzo frammento sul primo disco
    use std::os::unix::fs::MetadataExt;
    let other = tempfile::TempDir::new_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let mut lab = Lab::new(3, 4, 2);
    if fs::metadata(lab.tmp.path()).unwrap().dev() == fs::metadata(other.path()).unwrap().dev() {
        eprintln!("un solo filesystem disponibile: test saltato");
        return;
    }
    for i in 3..8 {
        let path = other.path().join(format!("svc{i}"));
        lab.cfg.backends.push(BackendConfig { name: format!("acc{i}"), kind: BackendKind::Local { path }, delay_ms: 0, limit_gb: None });
    }
    drop(lab.init());
    // acc3 c'è ma non accetta frammenti: al posto della cartella data/ c'è un file
    fs::write(other.path().join("svc3").join("data"), b"non una cartella").unwrap();
    let mut repo = lab.open();
    let samples = load_samples(&lab, &mut repo);
    for c in repo.index().chunks.values() {
        let first_disk = c
            .shards
            .iter()
            .filter(|l| l.backend.trim_start_matches("acc").parse::<usize>().unwrap() < 3)
            .count();
        assert!(first_disk <= 2, "{:?}", c.shards.iter().map(|l| &l.backend).collect::<Vec<_>>());
        assert!(c.shards.iter().all(|l| l.backend != "acc3"));
    }
    drop(repo);
    check_all(&lab, &lab.open(), &samples);
}

#[test]
fn scritture_ferme_quando_finisce_lo_spazio_protetto() {
    // tre cartelle grandi su un disco, tre piccole (300 kB) su un altro: finché le piccole hanno
    // posto i blocchi sono 2 + 2 (2 pezzi per disco); poi le scritture si fermano invece di finire
    // tutte sul primo disco, e la stima della capienza conta solo lo spazio protetto
    use std::os::unix::fs::MetadataExt;
    let other = tempfile::TempDir::new_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let mut lab = Lab::new(3, 4, 2);
    if fs::metadata(lab.tmp.path()).unwrap().dev() == fs::metadata(other.path()).unwrap().dev() {
        eprintln!("un solo filesystem disponibile: test saltato");
        return;
    }
    for i in 3..6 {
        let path = other.path().join(format!("svc{i}"));
        lab.cfg.backends.push(BackendConfig { name: format!("acc{i}"), kind: BackendKind::Local { path }, delay_ms: 0, limit_gb: Some(0.000_3) });
    }
    drop(lab.init());
    let mut repo = lab.open();
    let left = repo.capacity_left().unwrap();
    // 3 × 300 kB sul secondo disco, metà dei pezzi di ogni blocco: circa 900 kB di file
    assert!((700_000..=1_000_000).contains(&left), "capienza protetta stimata: {left}");
    let mut stored = 0usize;
    let mut err = None;
    for i in 0..20 {
        let p = lab.src().join(format!("p{i}"));
        fs::write(&p, random(&format!("p{i}"), 150_000)).unwrap();
        match repo.put_file(&p, &format!("p{i}.bin")) {
            Ok(_) => stored += 150_000,
            Err(e) => {
                err = Some(format!("{e:#}"));
                break;
            }
        }
    }
    let err = err.expect("le scritture devono fermarsi");
    assert!(err.contains("spazio protetto esaurito"), "{err}");
    assert!(stored as u64 >= left / 2, "salvati {stored} su {left} stimati");
    for c in repo.index().chunks.values() {
        let first = c.shards.iter().filter(|l| l.backend.trim_start_matches("acc").parse::<usize>().unwrap() < 3).count();
        assert!(first <= 2, "un blocco con {first} pezzi sul primo disco");
    }
}
