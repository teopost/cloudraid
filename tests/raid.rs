//! Test di integrazione: cartelle locali al posto degli account cloud.

use cloudraid::{BackendConfig, BackendKind, ChunkConfig, Config, ReadStats, Repo};
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
            backends: (0..backends)
                .map(|i| BackendConfig {
                    name: format!("acc{i}"),
                    kind: BackendKind::Local { path: tmp.path().join(format!("acc{i}")) },
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

    // il backend 0 è spento mentre si aggiunge un file (con 6 frammenti su 6 backend
    // la scrittura deve essere rifiutata, non fatta a metà)
    let off = lab.tmp.path().join("acc0-spento");
    fs::rename(lab.backend_dir(0), &off).unwrap();
    let mut repo = lab.open();
    let p = lab.src().join("nuovo.bin");
    fs::write(&p, random("nuovo", 200_000)).unwrap();
    let err = repo.put_file(&p, "nuovo.bin").unwrap_err();
    assert!(format!("{err:#}").contains("servono 6 backend"), "{err:#}");
    repo.remove("uno.bin").unwrap();
    repo.save().unwrap();
    drop(repo);

    // il backend 0 torna con l'indice vecchio: vince comunque il più recente
    fs::rename(&off, lab.backend_dir(0)).unwrap();
    let repo = lab.open();
    assert!(!repo.index().files.contains_key("uno.bin"));
    assert!(!repo.index().files.contains_key("nuovo.bin"));
    let rest: Vec<_> = samples.into_iter().filter(|(n, _)| n != "uno.bin").collect();
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
