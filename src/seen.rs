//! seen.db (P2.1): registro persistente por candidato da última atividade
//! "vista" pelo daemon. Cada ciclo de poll compara o mtime atual com o do
//! ciclo anterior: mudou → houve atividade → `last_seen` avança para agora;
//! não mudou → `last_seen` fica no passado (candidato frio, sai primeiro na
//! ordem LRU). Candidato sem registro usa mtime como fallback. O registro
//! (e o timestamp da última evicção, para o rate-limit) sobrevive a restart.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::walk::Candidate;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeenEntry {
    /// Último ciclo em que o candidato mostrou atividade (mtime mudou).
    pub last_seen: SystemTime,
    /// Fingerprint do ciclo anterior (mtime mais novo visto).
    pub last_mtime: SystemTime,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SeenDb {
    /// Última evicção executada pelo daemon (rate-limit entre ciclos).
    pub last_eviction: Option<SystemTime>,
    /// path → registro.
    pub entries: HashMap<String, SeenEntry>,
}

fn to_nanos(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

fn from_nanos(n: u64) -> SystemTime {
    UNIX_EPOCH + std::time::Duration::from_nanos(n)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SeenEntryJson {
    last_seen: u64,
    last_mtime: u64,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SeenDbJson {
    version: u32,
    last_eviction: Option<u64>,
    entries: HashMap<String, SeenEntryJson>,
}

impl SeenDb {
    pub fn load(path: &Path) -> SeenDb {
        let Ok(raw) = fs::read_to_string(path) else {
            return SeenDb::default();
        };
        let Ok(j) = serde_json::from_str::<SeenDbJson>(&raw) else {
            return SeenDb::default(); // corrompido → recomeça frio (fallback mtime)
        };
        SeenDb {
            last_eviction: j.last_eviction.map(from_nanos),
            entries: j
                .entries
                .into_iter()
                .map(|(k, v)| {
                    (
                        k,
                        SeenEntry {
                            last_seen: from_nanos(v.last_seen),
                            last_mtime: from_nanos(v.last_mtime),
                        },
                    )
                })
                .collect(),
        }
    }

    pub fn persist(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let j = SeenDbJson {
            version: 1,
            last_eviction: self.last_eviction.map(to_nanos),
            entries: self
                .entries
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        SeenEntryJson {
                            last_seen: to_nanos(v.last_seen),
                            last_mtime: to_nanos(v.last_mtime),
                        },
                    )
                })
                .collect(),
        };
        // escrita atômica: temp + rename no mesmo diretório
        let tmp = path.with_extension("db.tmp");
        fs::write(&tmp, serde_json::to_string(&j).expect("seen.db serializa"))?;
        fs::rename(&tmp, path)
    }

    /// Atualiza o registro com os candidatos vistos neste ciclo: mtime mudou
    /// desde o ciclo anterior → atividade (last_seen = agora); candidato novo
    /// → last_seen nasce no próprio mtime (fallback conservador).
    pub fn refresh(&mut self, cands: &[Candidate], now: SystemTime) {
        for c in cands {
            let key = c.path.display().to_string();
            let e = self.entries.get_mut(&key);
            match e {
                Some(e) if e.last_mtime == c.newest_mtime => {} // frio: mantém last_seen
                Some(e) => {
                    e.last_seen = now;
                    e.last_mtime = c.newest_mtime;
                }
                None => {
                    self.entries.insert(
                        key,
                        SeenEntry { last_seen: c.newest_mtime, last_mtime: c.newest_mtime },
                    );
                }
            }
        }
    }

    /// Chave de ordenação LRU do candidato: último-visto se houver registro,
    /// mtime como fallback.
    pub fn last_used(&self, cand: &Candidate) -> SystemTime {
        self.entries
            .get(&cand.path.display().to_string())
            .map(|e| e.last_seen)
            .unwrap_or(cand.newest_mtime)
    }

    pub fn mark_eviction(&mut self, now: SystemTime) {
        self.last_eviction = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::walk::Class;
    use std::time::Duration;

    fn cand(path: &str, age_secs: u64) -> Candidate {
        Candidate {
            path: path.into(),
            class: Class::Artifact,
            bytes: 10,
            newest_mtime: SystemTime::now() - Duration::from_secs(age_secs),
            contains_git: false,
            entries: 1,
        }
    }

    #[test]
    fn round_trip_survives_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("seen.db");
        let mut db = SeenDb::default();
        let now = SystemTime::now();
        let c = cand("/x/nm", 100);
        db.refresh(std::slice::from_ref(&c), now);
        db.mark_eviction(now);
        db.persist(&p).unwrap();

        let back = SeenDb::load(&p);
        assert_eq!(back.last_eviction, Some(now), "nanos preservados no round-trip");
        assert_eq!(back.entries.len(), 1);
        let e = &back.entries["/x/nm"];
        assert_eq!(e.last_seen, c.newest_mtime, "novo nasce no mtime");
        assert_eq!(e.last_mtime, c.newest_mtime);
    }

    #[test]
    fn refresh_marks_activity_and_keeps_cold_cold() {
        let mut db = SeenDb::default();
        let cold = cand("/a/nm", 500_000);
        let hot = cand("/b/nm", 500_000);
        let cycle1 = SystemTime::now() - Duration::from_secs(3600);
        db.refresh(&[cold.clone(), hot.clone()], cycle1);
        // nascem no próprio mtime (fallback conservador)
        assert_eq!(db.last_used(&cold), cold.newest_mtime);
        assert_eq!(db.last_used(&hot), hot.newest_mtime);

        // um ciclo depois: /b foi escrito (mtime novo), /a não
        let later = SystemTime::now();
        let hot2 = Candidate { newest_mtime: later - Duration::from_secs(5), ..hot.clone() };
        db.refresh(&[cold.clone(), hot2.clone()], later);

        assert_eq!(db.last_used(&cold), cold.newest_mtime, "frio mantém seen");
        assert_eq!(db.last_used(&hot2), later, "atividade avança o seen");
    }

    #[test]
    fn unknown_candidate_falls_back_to_mtime() {
        let db = SeenDb::default();
        let c = cand("/nunca/visto", 999);
        assert_eq!(db.last_used(&c), c.newest_mtime);
    }

    #[test]
    fn corrupt_db_starts_cold() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("seen.db");
        std::fs::write(&p, "{not json").unwrap();
        assert_eq!(SeenDb::load(&p), SeenDb::default());
    }

    #[test]
    fn seen_wins_over_mtime_and_unseen_falls_back() {
        let mut db = SeenDb::default();
        let old = SystemTime::now() - Duration::from_secs(90 * 86400);

        // `restored`: mtime VELHO (90d, extraído de arquivo), mas o daemon o
        // viu mudando AGORA (arquivo chegou ontem, mtimes vieram antigos):
        // o seen recente protege — é o caso onde seen ≠ mtime.
        let mut restored = cand("/restored", 0);
        restored.newest_mtime = old;
        let cycle1 = SystemTime::now() - Duration::from_secs(86400);
        db.refresh(std::slice::from_ref(&restored), cycle1);
        let mut changed = restored.clone();
        changed.newest_mtime = SystemTime::now() - Duration::from_secs(86300);
        let cycle2 = SystemTime::now();
        db.refresh(std::slice::from_ref(&changed), cycle2);
        db.refresh(std::slice::from_ref(&restored), cycle2); // mtime voltou a 90d

        // `virgem`: mtime 90d, nunca visto → fallback mtime
        let virgem = cand("/virgem", 90 * 86400);

        let mut keyed = vec![
            ("/restored", db.last_used(&restored)),
            ("/virgem", db.last_used(&virgem)),
        ];
        keyed.sort_by_key(|(_, k)| *k);
        assert_eq!(keyed[0].0, "/virgem", "nunca-visto (mtime 90d) sai primeiro");
        assert!(keyed[1].0 == "/restored" && db.last_used(&restored) > old);
    }
}
