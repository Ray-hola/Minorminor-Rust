use crate::{
    fail,
    files::{Resource, Resources},
    index, scan, Result,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};
pub fn authorized(resources: &Resources, key: &str, scan: &str) -> Result<Resource> {
    let r = resources
        .issued
        .lock()
        .unwrap()
        .get(key)
        .cloned()
        .ok_or_else(|| fail("目录条目已过期，请重新展开"))?;
    let m = index::checked(&r.path, &r.root, r.identity.dev)?;
    if r.scan != scan
        || !m.is_dir()
        || m.ino() != r.identity.ino
        || r.path == r.root && r.root.components().count() < 3
    {
        return Err(fail("目录身份或范围已变化"));
    }
    Ok(r)
}
struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
pub fn run(db: &mut Connection, resource: Resource, home: &Path, data: &Path) -> Result<Value> {
    index::assert_scan(db, &resource.scan)?;
    let root = &resource.path;
    let before = index::checked(root, &resource.root, resource.identity.dev)?;
    if !before.is_dir() || before.ino() != resource.identity.ino {
        return Err(fail("目录已变化"));
    }
    let temp = Temporary(data.join(format!(".refresh-{}", crate::id())));
    let tempdb = index::initialize(&temp.0)?;
    let measurement = crate::id();
    scan::run(scan::ScanSpec {
        id: measurement.clone(),
        root: root.clone(),
        home: home.into(),
        data: temp.0.clone(),
        disk: false,
        publish: true,
    })?;
    let after = index::checked(root, &resource.root, resource.identity.dev)?;
    if after.dev() != before.dev()
        || after.ino() != before.ino()
        || after.mtime() != before.mtime()
        || after.mtime_nsec() != before.mtime_nsec()
    {
        return Err(fail("计量期间目录内容发生变化，请重试"));
    }
    let source = index::connect(&tempdb)?;
    let info = index::scan_info(&source, &measurement)?;
    let prefix = format!("{}/", root.display());
    let upper = format!("{}0", root.display());
    let project:Option<String>=db.query_row("SELECT id FROM projects WHERE scan=? AND (?=path OR substr(?,1,length(path)+1)=path||'/') ORDER BY length(path) DESC LIMIT 1",params![resource.scan,index::path_string(root),index::path_string(root)],|r|r.get(0)).optional()?;
    let tool:Option<String>=db.query_row("SELECT id FROM tools WHERE scan=? AND (?=path OR substr(?,1,length(path)+1)=path||'/') ORDER BY length(path) DESC LIMIT 1",params![resource.scan,index::path_string(root),index::path_string(root)],|r|r.get(0)).optional()?;
    // All filesystem work has finished before the short, atomic publication.
    let mut stmt=source.prepare("SELECT path,parent,name,directory,bytes,logical,unique_bytes,files,dirs,identity,category,skipped FROM entries WHERE scan=?")?;
    let rows = stmt
        .query_map([&measurement], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, bool>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, i64>(8)?,
                r.get::<_, String>(9)?,
                r.get::<_, String>(10)?,
                r.get::<_, i64>(11)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let tx = db.transaction()?;
    tx.execute(
        "DELETE FROM entries WHERE scan=? AND (path=? OR path>=? AND path<?)",
        params![resource.scan, index::path_string(root), prefix, upper],
    )?;
    for (
        path,
        parent,
        name,
        directory,
        bytes,
        logical,
        unique,
        files,
        dirs,
        identity,
        category,
        skipped,
    ) in rows
    {
        let kind = if tool.is_some() {
            scan::category(Path::new(&path), true)
        } else {
            category.as_str()
        };
        tx.execute("INSERT INTO entries(scan,path,parent,name,directory,bytes,logical,unique_bytes,files,dirs,identity,category,project,tool,skipped) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",params![resource.scan,path,parent,name,directory,bytes,logical,unique,files,dirs,identity,kind,project,tool,skipped])?;
    }
    let mut p = source.prepare(
        "SELECT path,name,markers FROM projects WHERE scan=? ORDER BY length(path) DESC",
    )?;
    let projects = p
        .query_map([&measurement], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for (path, name, markers) in projects {
        let existing: Option<String> = tx
            .query_row(
                "SELECT id FROM projects WHERE scan=? AND path=?",
                params![resource.scan, path],
                |r| r.get(0),
            )
            .optional()?;
        let id = existing.unwrap_or_else(crate::id);
        tx.execute(
            "INSERT OR IGNORE INTO projects(scan,id,path,name,markers) VALUES(?,?,?,?,?)",
            params![resource.scan, id, path, name, markers],
        )?;
        tx.execute("UPDATE entries SET project=? WHERE scan=? AND path>=? AND path<? AND (project IS NULL OR project=?)",params![id,resource.scan,format!("{path}/"),format!("{path}0"),project])?;
    }
    let scan_info = index::scan_info(&tx, &resource.scan)?;
    let physical = scan_info["root"].as_str().unwrap_or("");
    let scope = if physical == "/System/Volumes/Data" {
        PathBuf::from("/")
    } else {
        PathBuf::from(physical)
    };
    for ancestor in root
        .ancestors()
        .skip(1)
        .take_while(|p| p.starts_with(&scope))
    {
        let lower = format!("{}/", ancestor.display());
        let upper = format!("{}0", ancestor.display());
        let(bytes,logical,count):(i64,i64,i64)=tx.query_row("SELECT coalesce(sum(bytes),0),coalesce(sum(logical),0),coalesce(sum(count),0) FROM (SELECT max(bytes) bytes,sum(logical) logical,count(*) count FROM entries WHERE scan=? AND directory=0 AND path>=? AND path<? GROUP BY json_extract(identity,'$.dev'),json_extract(identity,'$.ino'))",params![resource.scan,lower,upper],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        tx.execute(
            "UPDATE entries SET bytes=?,logical=?,files=? WHERE scan=? AND path=? AND directory=1",
            params![
                bytes,
                logical,
                count,
                resource.scan,
                index::path_string(ancestor)
            ],
        )?;
    }
    let mut stats = scan_info["stats"].clone();
    if let Ok((bytes, logical, files)) = tx.query_row(
        "SELECT bytes,logical,files FROM entries WHERE scan=? AND path=?",
        params![resource.scan, index::path_string(&scope)],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        },
    ) {
        stats["bytes"] = json!(bytes);
        stats["logicalBytes"] = json!(logical);
        stats["files"] = json!(files);
    }
    tx.execute(
        "UPDATE scans SET stats=? WHERE id=?",
        params![stats.to_string(), resource.scan],
    )?;
    let directories: i64 = tx.query_row(
        "SELECT count(*) FROM entries WHERE scan=? AND directory=1",
        [&resource.scan],
        |r| r.get(0),
    )?;
    stats["directories"] = json!(directories);
    tx.execute(
        "UPDATE scans SET stats=? WHERE id=?",
        params![stats.to_string(), resource.scan],
    )?;
    let mut dirs=tx.prepare("SELECT path FROM entries WHERE scan=? AND directory=1 AND (path=? OR substr(?2,1,length(path)+1)=path||'/')")?;
    let ancestors = dirs
        .query_map(params![resource.scan, index::path_string(root)], |r| {
            r.get::<_, String>(0)
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(dirs);
    for ancestor in ancestors {
        tx.execute("UPDATE entries SET dirs=(SELECT count(*) FROM entries e WHERE e.scan=?1 AND e.directory=1 AND e.path>=?3 AND e.path<?4) WHERE scan=?1 AND path=?2",params![resource.scan,ancestor,format!("{ancestor}/"),format!("{ancestor}0")])?;
    }
    tx.commit()?;
    Ok(json!({"path":root,"scanId":resource.scan,"stats":info["stats"],"updated":true}))
}
