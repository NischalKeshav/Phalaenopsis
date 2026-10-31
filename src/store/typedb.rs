//! TypeDB 3 backed graph store. Schema lives in `schema.tql`.

use super::GraphStore;
use crate::model::*;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use typedb_driver::{
    Addresses, Credentials, DriverOptions, DriverTlsConfig, TransactionType, TypeDBDriver,
    answer::ConceptRow,
};

const SCHEMA: &str = include_str!("schema.tql");

#[derive(Clone, Debug)]
pub struct TypeDbConfig {
    pub addr: String,
    pub user: String,
    pub password: String,
    pub database: String,
}

impl TypeDbConfig {
    pub fn from_env() -> Self {
        Self {
            addr: std::env::var("TYPEDB_ADDR").unwrap_or_else(|_| "localhost:1729".into()),
            user: std::env::var("TYPEDB_USER").unwrap_or_else(|_| "admin".into()),
            password: std::env::var("TYPEDB_PASSWORD").unwrap_or_else(|_| "password".into()),
            database: std::env::var("TYPEDB_DATABASE").unwrap_or_else(|_| "phalaenopsis".into()),
        }
    }
}

pub struct TypeDbStore {
    driver: TypeDBDriver,
    db: String,
    /// TypeDB transactions are optimistic: two commits touching the same
    /// concept conflict (STC2). Every mutating operation holds this lock for
    /// its whole read-modify-write, so this process never races itself and
    /// counters never lose updates.
    write_lock: tokio::sync::Mutex<()>,
}

const MAX_COMMIT_ATTEMPTS: u32 = 6;

fn is_conflict(e: &anyhow::Error) -> bool {
    let msg = format!("{e:#}");
    msg.contains("STC2") || msg.contains("isolation conflict")
}

fn te(e: typedb_driver::Error) -> anyhow::Error {
    anyhow!("typedb: {e}")
}

/// Quote a string as a TypeQL literal.
fn q(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn dt(t: &DateTime<Utc>) -> String {
    t.naive_utc().format("%Y-%m-%dT%H:%M:%S%.3f").to_string()
}

fn get_str(row: &ConceptRow, var: &str) -> Result<String> {
    row.get(var)
        .map_err(te)?
        .and_then(|c| c.try_get_string().map(str::to_string))
        .ok_or_else(|| anyhow!("missing string ${var}"))
}

fn get_int(row: &ConceptRow, var: &str) -> Result<i64> {
    row.get(var).map_err(te)?.and_then(|c| c.try_get_integer()).ok_or_else(|| anyhow!("missing integer ${var}"))
}

fn get_dt(row: &ConceptRow, var: &str) -> Result<DateTime<Utc>> {
    row.get(var)
        .map_err(te)?
        .and_then(|c| c.try_get_datetime())
        .map(|n| DateTime::<Utc>::from_naive_utc_and_offset(n, Utc))
        .ok_or_else(|| anyhow!("missing datetime ${var}"))
}

/// Shared `match` fragment binding all observation attributes of `$o`.
const OBS_ATTRS: &str = "$o has id $id, has host $h, has url_pattern $u, has fingerprint $f, has title $t, has fields_json $fj, has seen_count $n, has created_at $c;";
/// Shared `match` fragment binding all script attributes of `$s`.
const SCRIPT_ATTRS: &str = "$s has id $sid, has name $sn, has kind $sk, has body $sb, has origin $so, has created_at $sc;";
const DOC_ATTRS: &str = "$d has id $did, has name $dn, has mime $dm, has scope $ds, has doc_text $dt, has created_at $dc;";
const APPLIES_ATTRS: &str = "$a has successes $su, has failures $fa, has last_run $lr;";

fn row_observation(row: &ConceptRow) -> Result<Observation> {
    Ok(Observation {
        id: get_str(row, "id")?,
        host: get_str(row, "h")?,
        url_pattern: get_str(row, "u")?,
        fingerprint: get_str(row, "f")?,
        title: get_str(row, "t")?,
        fields: serde_json::from_str(&get_str(row, "fj")?)?,
        seen_count: get_int(row, "n")?,
        created_at: get_dt(row, "c")?,
    })
}

fn row_script(row: &ConceptRow) -> Result<ToolScript> {
    Ok(ToolScript {
        id: get_str(row, "sid")?,
        name: get_str(row, "sn")?,
        body: ScriptBody::from_parts(&get_str(row, "sk")?, &get_str(row, "sb")?)?,
        origin: Origin::parse(&get_str(row, "so")?),
        created_at: get_dt(row, "sc")?,
    })
}

fn row_context(row: &ConceptRow) -> Result<ContextDoc> {
    Ok(ContextDoc {
        id: get_str(row, "did")?,
        name: get_str(row, "dn")?,
        mime: get_str(row, "dm")?,
        scope: get_str(row, "ds")?,
        text: get_str(row, "dt")?,
        created_at: get_dt(row, "dc")?,
    })
}

fn row_applies(row: &ConceptRow, observation_id: &str, script_id: &str) -> Result<AppliesEdge> {
    Ok(AppliesEdge {
        observation_id: observation_id.into(),
        script_id: script_id.into(),
        successes: get_int(row, "su")?,
        failures: get_int(row, "fa")?,
        last_run: get_dt(row, "lr")?,
    })
}

impl TypeDbStore {
    pub async fn connect(cfg: &TypeDbConfig) -> Result<Self> {
        let driver = TypeDBDriver::new(
            Addresses::try_from_address_str(&cfg.addr).map_err(te)?,
            Credentials::new(&cfg.user, &cfg.password),
            DriverOptions::new(DriverTlsConfig::disabled()),
        )
        .await
        .map_err(te)?;
        if !driver.databases().contains(&cfg.database).await.map_err(te)? {
            driver.databases().create(&cfg.database).await.map_err(te)?;
        }
        let store = Self { driver, db: cfg.database.clone(), write_lock: tokio::sync::Mutex::new(()) };
        store.ensure_schema().await?;
        Ok(store)
    }

    async fn ensure_schema(&self) -> Result<()> {
        let tx = self.driver.transaction(&self.db, TransactionType::Schema).await.map_err(te)?;
        tx.query(SCHEMA).await.map_err(te)?;
        tx.commit().await.map_err(te)?;
        Ok(())
    }

    async fn read(&self, query: &str) -> Result<Vec<ConceptRow>> {
        let tx = self.driver.transaction(&self.db, TransactionType::Read).await.map_err(te)?;
        let answer = tx.query(query).await.map_err(te)?;
        let mut rows = Vec::new();
        if answer.is_row_stream() {
            let mut stream = answer.into_rows();
            while let Some(row) = stream.next().await {
                rows.push(row.map_err(te)?);
            }
        }
        Ok(rows)
    }

    /// Run write queries in one transaction and commit, retrying on isolation
    /// conflicts with another process. Returns the rows of the last query.
    async fn write(&self, queries: &[String]) -> Result<Vec<ConceptRow>> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.write_once(queries).await {
                Err(e) if is_conflict(&e) && attempt < MAX_COMMIT_ATTEMPTS => {
                    let backoff = 15u64 * (1 << attempt);
                    tracing::debug!("commit conflict, retry {attempt} in {backoff}ms");
                    tokio::time::sleep(std::time::Duration::from_millis(backoff)).await;
                }
                other => return other,
            }
        }
    }

    async fn write_once(&self, queries: &[String]) -> Result<Vec<ConceptRow>> {
        let tx = self.driver.transaction(&self.db, TransactionType::Write).await.map_err(te)?;
        let mut last = Vec::new();
        for query in queries {
            let answer = tx.query(query).await.map_err(te)?;
            last.clear();
            if answer.is_row_stream() {
                let mut stream = answer.into_rows();
                while let Some(row) = stream.next().await {
                    last.push(row.map_err(te)?);
                }
            }
        }
        tx.commit().await.map_err(te)?;
        Ok(last)
    }

    async fn link_applies_unlocked(&self, observation_id: &str, script_id: &str) -> Result<AppliesEdge> {
        let existing = format!(
            "match $o isa observation, has id {}; $s isa tool_script, has id {}; $a isa applies, links (page: $o, script: $s); {APPLIES_ATTRS}",
            q(observation_id), q(script_id)
        );
        if let Some(row) = self.read(&existing).await?.first() {
            return row_applies(row, observation_id, script_id);
        }
        let now = Utc::now();
        let insert = format!(
            "match $o isa observation, has id {}; $s isa tool_script, has id {}; insert $a isa applies, links (page: $o, script: $s), has successes 0, has failures 0, has last_run {};",
            q(observation_id), q(script_id), dt(&now)
        );
        let rows = self.write(&[insert]).await?;
        if rows.is_empty() {
            return Err(anyhow!("observation {observation_id} or script {script_id} not found"));
        }
        Ok(AppliesEdge { observation_id: observation_id.into(), script_id: script_id.into(), successes: 0, failures: 0, last_run: now })
    }

    async fn observations_where(&self, constraint: &str) -> Result<Vec<Observation>> {
        let query = format!("match $o isa observation{constraint}; {OBS_ATTRS}");
        self.read(&query).await?.iter().map(row_observation).collect()
    }
}

#[async_trait]
impl GraphStore for TypeDbStore {
    fn kind(&self) -> &'static str {
        "typedb"
    }

    async fn insert_observation(&self, o: &Observation) -> Result<()> {
        let _guard = self.write_lock.lock().await;
        let query = format!(
            "insert $o isa observation, has id {}, has host {}, has url_pattern {}, has fingerprint {}, has title {}, has fields_json {}, has seen_count {}, has created_at {};",
            q(&o.id), q(&o.host), q(&o.url_pattern), q(&o.fingerprint), q(&o.title),
            q(&serde_json::to_string(&o.fields)?), o.seen_count, dt(&o.created_at)
        );
        self.write(&[query]).await?;
        Ok(())
    }

    async fn update_observation(&self, o: &Observation) -> Result<()> {
        let _guard = self.write_lock.lock().await;
        let query = format!(
            "match $o isa observation, has id {}; update $o has title {}, has fields_json {}, has seen_count {};",
            q(&o.id), q(&o.title), q(&serde_json::to_string(&o.fields)?), o.seen_count
        );
        self.write(&[query]).await?;
        Ok(())
    }

    async fn get_observation(&self, id: &str) -> Result<Option<Observation>> {
        Ok(self.observations_where(&format!(", has id {}", q(id))).await?.pop())
    }

    async fn find_by_fingerprint(&self, fingerprint: &str) -> Result<Option<Observation>> {
        Ok(self.observations_where(&format!(", has fingerprint {}", q(fingerprint))).await?.pop())
    }

    async fn observations_for_host(&self, host: &str) -> Result<Vec<Observation>> {
        self.observations_where(&format!(", has host {}", q(host))).await
    }

    async fn all_observations(&self) -> Result<Vec<Observation>> {
        let mut v = self.observations_where("").await?;
        v.sort_by_key(|a| a.created_at);
        Ok(v)
    }

    async fn insert_script(&self, s: &ToolScript) -> Result<()> {
        let _guard = self.write_lock.lock().await;
        let query = format!(
            "insert $s isa tool_script, has id {}, has name {}, has kind {}, has body {}, has origin {}, has created_at {};",
            q(&s.id), q(&s.name), q(s.body.kind()), q(&s.body.body_string()), q(s.origin.as_str()), dt(&s.created_at)
        );
        self.write(&[query]).await?;
        Ok(())
    }

    async fn get_script(&self, id: &str) -> Result<Option<ToolScript>> {
        let query = format!("match $s isa tool_script, has id {}; {SCRIPT_ATTRS}", q(id));
        self.read(&query).await?.first().map(row_script).transpose()
    }

    async fn delete_script(&self, id: &str) -> Result<()> {
        let _guard = self.write_lock.lock().await;
        let edges = format!(
            "match $s isa tool_script, has id {}; $a isa applies, links (script: $s); delete $a;",
            q(id)
        );
        let node = format!("match $s isa tool_script, has id {}; delete $s;", q(id));
        self.write(&[edges, node]).await?;
        Ok(())
    }

    async fn all_scripts(&self) -> Result<Vec<ToolScript>> {
        let query = format!("match $s isa tool_script; {SCRIPT_ATTRS}");
        let mut v: Vec<ToolScript> = self.read(&query).await?.iter().filter_map(lenient_script).collect();
        v.sort_by_key(|a| a.created_at);
        Ok(v)
    }

    async fn link_applies(&self, observation_id: &str, script_id: &str) -> Result<AppliesEdge> {
        let _guard = self.write_lock.lock().await;
        self.link_applies_unlocked(observation_id, script_id).await
    }

    async fn applies_for(&self, observation_id: &str) -> Result<Vec<(ToolScript, AppliesEdge)>> {
        let query = format!(
            "match $o isa observation, has id {}; $a isa applies, links (page: $o, script: $s); {APPLIES_ATTRS} {SCRIPT_ATTRS}",
            q(observation_id)
        );
        self.read(&query)
            .await?
            .iter()
            .filter_map(|row| {
                let script = lenient_script(row)?;
                let edge = row_applies(row, observation_id, &script.id).ok()?;
                Some((script, edge))
            })
            .collect::<Vec<_>>()
            .pipe(Ok)
    }

    async fn all_applies(&self) -> Result<Vec<AppliesEdge>> {
        let query = format!(
            "match $a isa applies, links (page: $o, script: $s); $o has id $oid; $s has id $sid; {APPLIES_ATTRS}"
        );
        self.read(&query)
            .await?
            .iter()
            .map(|row| row_applies(row, &get_str(row, "oid")?, &get_str(row, "sid")?))
            .collect()
    }

    async fn record_run(&self, observation_id: &str, script_id: &str, ok: bool) -> Result<AppliesEdge> {
        let _guard = self.write_lock.lock().await;
        let mut edge = self.link_applies_unlocked(observation_id, script_id).await?;
        if ok {
            edge.successes += 1;
        } else {
            edge.failures += 1;
        }
        edge.last_run = Utc::now();
        let query = format!(
            "match $o isa observation, has id {}; $s isa tool_script, has id {}; $a isa applies, links (page: $o, script: $s); update $a has successes {}, has failures {}, has last_run {};",
            q(observation_id), q(script_id), edge.successes, edge.failures, dt(&edge.last_run)
        );
        self.write(&[query]).await?;
        Ok(edge)
    }

    async fn add_transition(&self, from: &str, to: &str, via: Option<&str>) -> Result<Transition> {
        let _guard = self.write_lock.lock().await;
        let via_id = via.unwrap_or("");
        let existing = format!(
            "match $f isa observation, has id {}; $t isa observation, has id {}; $tr isa transition, links (source: $f, target: $t), has via_id {}, has hits $h;",
            q(from), q(to), q(via_id)
        );
        let hits = match self.read(&existing).await?.first() {
            Some(row) => {
                let hits = get_int(row, "h")? + 1;
                let update = format!(
                    "match $f isa observation, has id {}; $t isa observation, has id {}; $tr isa transition, links (source: $f, target: $t), has via_id {}; update $tr has hits {};",
                    q(from), q(to), q(via_id), hits
                );
                self.write(&[update]).await?;
                hits
            }
            None => {
                let insert = format!(
                    "match $f isa observation, has id {}; $t isa observation, has id {}; insert $tr isa transition, links (source: $f, target: $t), has via_id {}, has hits 1;",
                    q(from), q(to), q(via_id)
                );
                if self.write(&[insert]).await?.is_empty() {
                    return Err(anyhow!("observation {from} or {to} not found"));
                }
                1
            }
        };
        Ok(Transition { from: from.into(), to: to.into(), via: via.map(str::to_string), hits })
    }

    async fn transitions_from(&self, observation_id: &str) -> Result<Vec<Transition>> {
        let query = format!(
            "match $f isa observation, has id {}; $tr isa transition, links (source: $f, target: $t), has via_id $v, has hits $h; $t has id $tid;",
            q(observation_id)
        );
        self.read(&query).await?.iter().map(|row| row_transition(row, observation_id.into())).collect()
    }

    async fn all_transitions(&self) -> Result<Vec<Transition>> {
        let query = "match $tr isa transition, links (source: $f, target: $t), has via_id $v, has hits $h; $f has id $fid; $t has id $tid;";
        self.read(query).await?.iter().map(|row| row_transition(row, get_str(row, "fid")?)).collect()
    }

    async fn insert_context(&self, d: &ContextDoc) -> Result<()> {
        let _guard = self.write_lock.lock().await;
        let query = format!(
            "insert $d isa context_doc, has id {}, has name {}, has mime {}, has scope {}, has doc_text {}, has created_at {};",
            q(&d.id), q(&d.name), q(&d.mime), q(&d.scope), q(&d.text), dt(&d.created_at)
        );
        self.write(&[query]).await?;
        Ok(())
    }

    async fn get_context(&self, id: &str) -> Result<Option<ContextDoc>> {
        let query = format!("match $d isa context_doc, has id {}; {DOC_ATTRS}", q(id));
        self.read(&query).await?.first().map(row_context).transpose()
    }

    async fn context_for(&self, host: &str) -> Result<Vec<ContextDoc>> {
        let mut v = Vec::new();
        for scope in ["*", host] {
            let query = format!("match $d isa context_doc, has scope {}; {DOC_ATTRS}", q(scope));
            for row in self.read(&query).await? {
                v.push(row_context(&row)?);
            }
        }
        v.sort_by_key(|d| d.created_at);
        v.dedup_by(|a, b| a.id == b.id);
        Ok(v)
    }

    async fn all_context(&self) -> Result<Vec<ContextDoc>> {
        let query = format!("match $d isa context_doc; {DOC_ATTRS}");
        let mut v: Vec<ContextDoc> = self.read(&query).await?.iter().map(row_context).collect::<Result<_>>()?;
        v.sort_by_key(|d| d.created_at);
        Ok(v)
    }

    async fn delete_context(&self, id: &str) -> Result<()> {
        let _guard = self.write_lock.lock().await;
        self.write(&[format!("match $d isa context_doc, has id {}; delete $d;", q(id))]).await?;
        Ok(())
    }

    async fn profile_all(&self) -> Result<Profile> {
        let rows = self.read("match $p isa profile_fact, has fact_key $k, has fact_value $v;").await?;
        rows.iter().map(|row| Ok((get_str(row, "k")?, get_str(row, "v")?))).collect()
    }

    async fn profile_set(&self, key: &str, value: &str) -> Result<()> {
        let _guard = self.write_lock.lock().await;
        let delete = format!("match $p isa profile_fact, has fact_key {}; delete $p;", q(key));
        let insert = format!("insert $p isa profile_fact, has fact_key {}, has fact_value {};", q(key), q(value));
        self.write(&[delete, insert]).await?;
        Ok(())
    }

    async fn profile_delete(&self, key: &str) -> Result<()> {
        let _guard = self.write_lock.lock().await;
        let delete = format!("match $p isa profile_fact, has fact_key {}; delete $p;", q(key));
        self.write(&[delete]).await?;
        Ok(())
    }
}

/// Parse a script row, logging and skipping rows whose body no longer parses.
fn lenient_script(row: &ConceptRow) -> Option<ToolScript> {
    match row_script(row) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!("skipping unreadable tool_script {:?}: {e:#}", get_str(row, "sid").ok());
            None
        }
    }
}

trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}
impl<T> Pipe for T {}

fn row_transition(row: &ConceptRow, from: String) -> Result<Transition> {
    let via = get_str(row, "v")?;
    Ok(Transition {
        from,
        to: get_str(row, "tid")?,
        via: if via.is_empty() { None } else { Some(via) },
        hits: get_int(row, "h")?,
    })
}

/// Live-server tests. Run with: cargo test typedb_live -- --ignored
#[cfg(test)]
mod live_tests {
    use super::*;
    use crate::fingerprint;

    fn obs(id: &str, host: &str, pattern: &str) -> Observation {
        let fields = vec![FieldSig {
            selector: "#first".into(),
            tag: "input".into(),
            input_type: "text".into(),
            name: "first".into(),
            id: "first".into(),
            label: "First \"name\"\nline2 \\ end".into(),
            placeholder: String::new(),
            autocomplete: "given-name".into(),
        }];
        Observation {
            id: id.into(),
            host: host.into(),
            url_pattern: pattern.into(),
            fingerprint: fingerprint::fingerprint(host, pattern, &fields),
            title: "Title with \"quotes\"".into(),
            fields,
            seen_count: 1,
            created_at: Utc::now(),
        }
    }

    #[tokio::test]
    #[ignore]
    async fn typedb_live_round_trip() {
        let mut cfg = TypeDbConfig::from_env();
        cfg.database = "phalaenopsis_test".into();
        let driver = TypeDBDriver::new(
            Addresses::try_from_address_str(&cfg.addr).unwrap(),
            Credentials::new(&cfg.user, &cfg.password),
            DriverOptions::new(DriverTlsConfig::disabled()),
        )
        .await
        .expect("typedb reachable");
        if driver.databases().contains(&cfg.database).await.unwrap() {
            driver.databases().get(&cfg.database).await.unwrap().delete().await.unwrap();
        }
        let store = TypeDbStore::connect(&cfg).await.expect("connect + schema");
        // schema definition must be idempotent
        store.ensure_schema().await.expect("second define");

        let a = obs("obs-a", "apply.mit.edu", "/app/*/profile");
        let b = obs("obs-b", "apply.mit.edu", "/app/*/family");
        store.insert_observation(&a).await.unwrap();
        store.insert_observation(&b).await.unwrap();
        let got = store.get_observation("obs-a").await.unwrap().expect("obs-a");
        assert_eq!(got.fields, a.fields, "string escaping round trip");
        assert_eq!(got.title, a.title);
        assert_eq!(got.created_at.timestamp_millis(), a.created_at.timestamp_millis());
        assert_eq!(store.find_by_fingerprint(&a.fingerprint).await.unwrap().unwrap().id, "obs-a");
        assert_eq!(store.observations_for_host("apply.mit.edu").await.unwrap().len(), 2);
        assert!(store.get_observation("missing").await.unwrap().is_none());

        let mut updated = got.clone();
        updated.seen_count = 7;
        updated.title = "New title".into();
        store.update_observation(&updated).await.unwrap();
        let got = store.get_observation("obs-a").await.unwrap().unwrap();
        assert_eq!(got.seen_count, 7);
        assert_eq!(got.title, "New title");

        let js = ToolScript {
            id: "s-js".into(),
            name: "js one".into(),
            body: ScriptBody::Js("document.querySelector('#first').value = profile.firstName;\n// \"quoted\" \\ done".into()),
            origin: Origin::Manual,
            created_at: Utc::now(),
        };
        let steps = ToolScript {
            id: "s-steps".into(),
            name: "steps one".into(),
            body: ScriptBody::Steps(vec![Step::Fill { selector: "#first".into(), value: ValueRef::Profile { profile: "firstName".into() } }]),
            origin: Origin::Llm,
            created_at: Utc::now(),
        };
        store.insert_script(&js).await.unwrap();
        store.insert_script(&steps).await.unwrap();
        assert_eq!(store.get_script("s-js").await.unwrap().unwrap().body, js.body);
        let got_steps = store.get_script("s-steps").await.unwrap().unwrap();
        assert_eq!(got_steps.body, steps.body);
        assert_eq!(got_steps.origin, Origin::Llm);
        assert_eq!(got_steps.created_at.timestamp_millis(), steps.created_at.timestamp_millis());
        assert_eq!(store.all_scripts().await.unwrap().len(), 2);

        let e = store.link_applies("obs-a", "s-js").await.unwrap();
        assert_eq!((e.successes, e.failures), (0, 0));
        let e = store.link_applies("obs-a", "s-js").await.unwrap(); // idempotent
        assert_eq!((e.successes, e.failures), (0, 0));
        store.link_applies("obs-a", "s-steps").await.unwrap();
        assert!(store.link_applies("obs-a", "nope").await.is_err());
        let e = store.record_run("obs-a", "s-js", true).await.unwrap();
        let e2 = store.record_run("obs-a", "s-js", false).await.unwrap();
        assert_eq!((e.successes, e.failures), (1, 0));
        assert_eq!((e2.successes, e2.failures), (1, 1));
        // Concurrent writers on the same edge: no conflict errors, no lost updates.
        let runs = (0..12).map(|_| store.record_run("obs-a", "s-steps", true));
        for r in futures::future::join_all(runs).await {
            r.expect("concurrent record_run must not fail");
        }
        let store_ref = &store;
        let obs_updates = (0..8).map(|i| {
            let mut o = a.clone();
            o.seen_count = i;
            async move { store_ref.update_observation(&o).await }
        });
        for r in futures::future::join_all(obs_updates).await {
            r.expect("concurrent update_observation must not fail");
        }

        let applies = store.applies_for("obs-a").await.unwrap();
        assert_eq!(applies.len(), 2);
        let steps_edge = applies.iter().find(|(s, _)| s.id == "s-steps").unwrap();
        assert_eq!(steps_edge.1.successes, 12, "every concurrent run counted");
        let js_edge = applies.iter().find(|(s, _)| s.id == "s-js").unwrap();
        assert_eq!((js_edge.1.successes, js_edge.1.failures), (1, 1));
        assert_eq!(store.all_applies().await.unwrap().len(), 2);

        let t = store.add_transition("obs-a", "obs-b", Some("s-js")).await.unwrap();
        assert_eq!(t.hits, 1);
        let t = store.add_transition("obs-a", "obs-b", Some("s-js")).await.unwrap();
        assert_eq!(t.hits, 2);
        let t = store.add_transition("obs-a", "obs-b", None).await.unwrap();
        assert_eq!(t.hits, 1);
        let from_a = store.transitions_from("obs-a").await.unwrap();
        assert_eq!(from_a.len(), 2);
        assert!(from_a.iter().any(|t| t.via.as_deref() == Some("s-js") && t.hits == 2));
        assert!(from_a.iter().any(|t| t.via.is_none() && t.hits == 1));
        assert_eq!(store.all_transitions().await.unwrap().len(), 2);

        store.profile_set("firstName", "Ada").await.unwrap();
        store.profile_set("firstName", "Grace").await.unwrap();
        store.profile_set("email", "g@example.com").await.unwrap();
        let p = store.profile_all().await.unwrap();
        assert_eq!(p.get("firstName").map(String::as_str), Some("Grace"));
        assert_eq!(p.len(), 2);
        store.profile_delete("email").await.unwrap();
        assert_eq!(store.profile_all().await.unwrap().len(), 1);

        let doc = ContextDoc {
            id: "doc-1".into(),
            name: "transcript.txt".into(),
            mime: "text/plain".into(),
            scope: "apply.mit.edu".into(),
            text: "Grade 9 | Algebra I | A\nGrade 10 | \"Geometry\" | A-".into(),
            created_at: Utc::now(),
        };
        let global = ContextDoc { id: "doc-2".into(), scope: "*".into(), name: "notes.md".into(), ..doc.clone() };
        store.insert_context(&doc).await.unwrap();
        store.insert_context(&global).await.unwrap();
        assert_eq!(store.get_context("doc-1").await.unwrap().unwrap().text, doc.text);
        assert_eq!(store.context_for("apply.mit.edu").await.unwrap().len(), 2);
        assert_eq!(store.context_for("other.edu").await.unwrap().len(), 1);
        store.delete_context("doc-2").await.unwrap();
        assert_eq!(store.all_context().await.unwrap().len(), 1);

        store.delete_script("s-js").await.unwrap();
        assert!(store.get_script("s-js").await.unwrap().is_none());
        assert_eq!(store.applies_for("obs-a").await.unwrap().len(), 1);

        let export = store.export().await.unwrap();
        assert_eq!(export.observations.len(), 2);
        assert_eq!(export.scripts.len(), 1);
        assert_eq!(export.applies.len(), 1);
        assert_eq!(export.transitions.len(), 2);
        assert_eq!(export.profile_keys, vec!["firstName".to_string()]);
    }
}
