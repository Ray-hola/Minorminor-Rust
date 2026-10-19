use crate::{
    fail,
    files::{Resource, Resources},
    index, scan, Result,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::Instant,
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
impl Temporary {
    fn new(data: &Path) -> Result<Self> {
        use std::{io::Write, os::unix::fs::OpenOptionsExt};
        let id = crate::id();
        let temp = Self(data.join(format!(".refresh-{id}")));
        index::secure_directory(&temp.0)?;
        let mut owner = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(temp.0.join(".owner.json"))?;
        owner.write_all(json!({"format":"minorminor-scan-work-v1","kind":"measurement","job":id,"pid":std::process::id()}).to_string().as_bytes())?;
        owner.sync_all()?;
        fs::File::open(&temp.0)?.sync_all()?;
        Ok(temp)
    }
}
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = remove_temporary(&self.0, false);
    }
}
fn remove_temporary(path: &Path, dead_only: bool) -> Result<bool> {
    use std::{ffi::CString, os::fd::AsRawFd};
    let Some(id) = path
        .file_name()
        .and_then(|s| s.to_str())
        .and_then(|s| s.strip_prefix(".refresh-"))
    else {
        return Ok(false);
    };
    if id.is_empty()
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Ok(false);
    }
    let owner = crate::snapshot::read_owner(&path.join(".owner.json"))?;
    if owner["kind"] != "measurement" || owner["job"] != id {
        return Ok(false);
    }
    let Some(pid) = owner["pid"]
        .as_u64()
        .filter(|p| *p > 0 && *p < i32::MAX as u64)
    else {
        return Ok(false);
    };
    if dead_only {
        if unsafe { libc::kill(pid as i32, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
        {
            return Ok(false);
        }
    } else if pid != std::process::id() as u64 {
        return Ok(false);
    }
    let metadata = crate::cloud::metadata(path)?;
    let dir = crate::directory::Handle::root(path, &metadata)?;
    let mut names = Vec::new();
    for entry in dir.entries(path)? {
        let name = entry?.file_name();
        let Some(text) = name.to_str() else {
            return Ok(false);
        };
        if ![
            ".owner.json",
            "app.lock",
            "index-v4.sqlite3",
            "index-v4.sqlite3-wal",
            "index-v4.sqlite3-shm",
            "index-v4.sqlite3-journal",
            "index.sqlite3",
            "index.sqlite3-wal",
            "index.sqlite3-shm",
            "index.sqlite3-journal",
            "user-state.sqlite3",
            "user-state.sqlite3-wal",
            "user-state.sqlite3-shm",
            "user-state.sqlite3-journal",
        ]
        .contains(&text)
        {
            return Ok(false);
        }
        let native = CString::new(text)?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                dir.as_raw_fd(),
                native.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Ok(false);
        }
        let stat = unsafe { stat.assume_init() };
        if stat.st_mode & libc::S_IFMT != libc::S_IFREG
            || stat.st_uid != unsafe { libc::geteuid() }
            || stat.st_nlink != 1
        {
            return Ok(false);
        }
        names.push(native);
    }
    // Keep the manifest until all data files are gone, so interrupted GC retries.
    names.sort_by_key(|name| name.to_bytes() == b".owner.json");
    // Relative unlink never resolves a replaced ancestor or follows a symlink.
    for name in names {
        if unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    fs::remove_dir(path)?;
    Ok(true)
}
pub fn cleanup_orphans(data: &Path) -> Result<u64> {
    let mut removed = 0;
    for entry in fs::read_dir(data)? {
        let entry = entry?;
        let metadata = entry.file_type()?;
        if !metadata.is_dir() || metadata.is_symlink() {
            continue;
        }
        if remove_temporary(&entry.path(), true).unwrap_or(false) {
            removed += 1;
        }
    }
    Ok(removed)
}
pub fn run(db: &mut Connection, resource: Resource, home: &Path, data: &Path) -> Result<Value> {
    let root = resource.path.clone();
    run_mapped(db, resource, &root, home, data)
}
pub fn run_mapped(
    db: &mut Connection,
    resource: Resource,
    root: &Path,
    home: &Path,
    data: &Path,
) -> Result<Value> {
    let started = Instant::now();
    let mut phase = Instant::now();
    let mut timings = std::collections::BTreeMap::new();
    index::assert_scan(db, &resource.scan)?;
    let before = index::checked(&resource.path, &resource.root, resource.identity.dev)?;
    if !before.is_dir() || before.ino() != resource.identity.ino {
        return Err(fail("目录已变化"));
    }
    let temp = Temporary::new(data)?;
    let tempdb = index::initialize(&temp.0)?;
    let measurement = crate::id();
    scan::run(scan::ScanSpec {
        id: measurement.clone(),
        root: resource.path.clone(),
        home: home.into(),
        data: temp.0.clone(),
        disk: false,
        publish: true,
        index_path: None,
    })?;
    let after = index::checked(&resource.path, &resource.root, resource.identity.dev)?;
    if after.dev() != before.dev()
        || after.ino() != before.ino()
        || after.mtime() != before.mtime()
        || after.mtime_nsec() != before.mtime_nsec()
    {
        return Err(fail("计量期间目录内容发生变化，请重试"));
    }
    let source = index::connect(&tempdb)?;
    if resource.path != root {
        let physical = index::path_string(&resource.path);
        let logical = index::path_string(root);
        source.execute("UPDATE entries SET path=?1||substr(path,length(?2)+1),parent=CASE WHEN path=?2 THEN ?3 ELSE ?1||substr(parent,length(?2)+1) END WHERE scan=?4 AND (path=?2 OR substr(path,1,length(?2)+1)=?2||'/')",params![logical,physical,index::path_string(root.parent().unwrap_or(root)),measurement])?;
        for table in ["projects", "errors"] {
            source.execute(&format!("UPDATE {table} SET path=?1||substr(path,length(?2)+1) WHERE scan=?3 AND (path=?2 OR substr(path,1,length(?2)+1)=?2||'/')"),params![logical,physical,measurement])?;
        }
    }
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
    let scan_info = index::scan_info(db, &resource.scan)?;
    let physical = scan_info["root"].as_str().unwrap_or("");
    let scope = if physical == "/System/Volumes/Data" {
        PathBuf::from("/")
    } else {
        PathBuf::from(physical)
    };
    let mut own_skips = HashMap::new();
    for ancestor in root
        .ancestors()
        .skip(1)
        .take_while(|p| p.starts_with(&scope))
    {
        let previous: i64 = db
            .query_row(
                "SELECT skipped FROM entries WHERE scan=? AND path=? AND directory=1",
                params![resource.scan, index::path_string(ancestor)],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let children = child_totals(db, &resource.scan, ancestor)?.4;
        own_skips.insert(
            ancestor.to_owned(),
            previous.saturating_sub(children).max(0),
        );
    }
    let (mut affected_projects, mut affected_tools) =
        affected_owners(db, &resource.scan, root, &prefix, &upper)?;
    affected_projects.extend(project.iter().cloned());
    affected_tools.extend(tool.iter().cloned());
    timings.insert("prepare", phase.elapsed().as_millis());
    phase = Instant::now();
    let tx = db.transaction()?;
    tx.execute(
        "DELETE FROM errors WHERE scan=? AND code NOT IN ('SESSION_READ_ERROR','DB_READ_ERROR','UNSUPPORTED_DATABASE') AND (path=? OR (path>=? AND path<?))",
        params![resource.scan, index::path_string(root), prefix, upper],
    )?;
    let mut source_errors = source.prepare("SELECT path,code,reason FROM errors WHERE scan=?")?;
    for row in source_errors.query_map([&measurement], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })? {
        let (path, code, reason) = row?;
        tx.execute(
            "DELETE FROM errors WHERE scan=? AND path=? AND code=?",
            params![resource.scan, path, code],
        )?;
        tx.execute(
            "INSERT INTO errors(scan,path,code,reason) VALUES(?,?,?,?)",
            params![resource.scan, path, code, reason],
        )?;
    }
    replace_subtree(&tx, &resource.scan, root, &prefix, &upper)?;
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
        // Workspace registrations may refer to ancestors or unrelated folders.
        // A partial measurement only imports projects inside its selected root.
        if !Path::new(&path).starts_with(root) {
            continue;
        }
        let existing: Option<String> = tx
            .query_row(
                "SELECT id FROM projects WHERE scan=? AND path=?",
                params![resource.scan, path],
                |r| r.get(0),
            )
            .optional()?;
        let id = existing.unwrap_or_else(crate::id);
        affected_projects.insert(id.clone());
        tx.execute(
            "INSERT OR IGNORE INTO projects(scan,id,path,name,markers) VALUES(?,?,?,?,?)",
            params![resource.scan, id, path, name, markers],
        )?;
        tx.execute("UPDATE entries SET project=? WHERE scan=? AND path>=? AND path<? AND (project IS NULL OR project=?)",params![id,resource.scan,format!("{path}/"),format!("{path}0"),project])?;
    }
    timings.insert("replace", phase.elapsed().as_millis());
    phase = Instant::now();
    crate::scan::print_event(
        json!({"event":"progress","stats":{"stage":"storage","measurePhase":"links","completed":0,"total":root.ancestors().skip(1).take_while(|p|p.starts_with(&scope)).count()}}),
    );
    let links = related_links(&tx, &source, &resource.scan, &measurement)?;
    timings.insert("links", phase.elapsed().as_millis());
    phase = Instant::now();
    for (completed, ancestor) in root
        .ancestors()
        .skip(1)
        .take_while(|p| p.starts_with(&scope))
        .enumerate()
    {
        let (summed, logical, files, dirs, skipped) = child_totals(&tx, &resource.scan, ancestor)?;
        let bytes = summed.saturating_sub(link_overlap(&links, ancestor));
        tx.execute("UPDATE entries SET bytes=?,logical=?,files=?,dirs=?,skipped=? WHERE scan=? AND path=? AND directory=1",params![bytes,logical,files,dirs,skipped+own_skips.get(ancestor).copied().unwrap_or(0),resource.scan,index::path_string(ancestor)])?;
        crate::scan::print_event(
            json!({"event":"progress","stats":{"stage":"storage","measurePhase":"ancestors","currentPath":ancestor,"completed":completed+1,"total":own_skips.len()}}),
        );
    }
    let mut stats = scan_info["stats"].clone();
    stats["skipped"] = json!(tx.query_row(
        "SELECT count(*) FROM errors WHERE scan=?",
        [&resource.scan],
        |r| r.get::<_, i64>(0)
    )?);
    if let Ok((bytes, logical, files, dirs)) = tx.query_row(
        "SELECT bytes,logical,files,dirs FROM entries WHERE scan=? AND path=?",
        params![resource.scan, index::path_string(&scope)],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        },
    ) {
        stats["bytes"] = json!(bytes);
        stats["logicalBytes"] = json!(logical);
        stats["files"] = json!(files);
        stats["directories"] = json!(dirs + 1);
    }
    tx.execute(
        "UPDATE scans SET stats=? WHERE id=?",
        params![stats.to_string(), resource.scan],
    )?;
    timings.insert("ancestors", phase.elapsed().as_millis());
    phase = Instant::now();
    for (project, ids) in [(true, &affected_projects), (false, &affected_tools)] {
        for id in ids {
            tx.execute(
                "DELETE FROM settings WHERE key=?",
                [format!(
                    "{}-{}-{}",
                    if project {
                        "project-storage-cache"
                    } else {
                        "tool-storage-cache"
                    },
                    resource.scan,
                    id
                )],
            )?;
        }
    }
    tx.commit()?;
    timings.insert("commit", phase.elapsed().as_millis());
    Ok(
        json!({"path":root,"scanId":resource.scan,"stats":info["stats"],"updated":true,"timingsMs":timings,"elapsedMs":started.elapsed().as_millis()}),
    )
}

type Link = (PathBuf, index::Identity, i64);
fn read_identity(text: &str) -> Result<index::Identity> {
    if text.starts_with('[') {
        let (dev, ino, size, mtime, mtime_ns, ctime, ctime_ns, nlink) =
            serde_json::from_str::<(u64, u64, u64, i64, i64, i64, i64, u64)>(text)?;
        Ok(index::Identity {
            dev,
            ino,
            size,
            mtime,
            mtime_ns,
            ctime,
            ctime_ns,
            nlink,
        })
    } else {
        Ok(serde_json::from_str(text)?)
    }
}
fn related_links(
    db: &Connection,
    source: &Connection,
    scan: &str,
    measurement: &str,
) -> Result<Vec<Link>> {
    let overlay = crate::overlay::active(db)?;
    let mut replaced = HashSet::new();
    if overlay {
        let mut stmt = db.prepare("SELECT path FROM entry_changes WHERE scan=?")?;
        for path in stmt.query_map([scan], |r| r.get::<_, String>(0))? {
            replaced.insert(PathBuf::from(path?));
        }
    }
    let mut result = Vec::new();
    let mut stmt = db.prepare(crate::compact::hardlinks_query(db)?)?;
    for row in stmt.query_map([scan], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })? {
        let (path, identity, bytes) = row?;
        let path = PathBuf::from(path);
        if !replaced.contains(&path) {
            result.push((path, read_identity(&identity)?, bytes));
        }
    }
    if overlay {
        let mut stmt=db.prepare("SELECT path,identity,bytes FROM entry_changes WHERE scan=? AND directory=0 AND deleted=0 AND json_extract(identity,'$.nlink')>1")?;
        for row in stmt.query_map([scan], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })? {
            let (path, identity, bytes) = row?;
            result.push((PathBuf::from(path), read_identity(&identity)?, bytes));
        }
    }
    let mut known: HashSet<PathBuf> = result.iter().map(|r| r.0.clone()).collect();
    let mut aliases = db.prepare("SELECT path,dev,ino FROM entry_aliases WHERE scan=?")?;
    for row in aliases.query_map([scan], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })? {
        let (path, dev, ino) = row?;
        let path = PathBuf::from(path);
        if known.contains(&path) {
            continue;
        }
        let found: Option<(String, i64)> = if overlay && replaced.contains(&path) {
            db.query_row("SELECT identity,bytes FROM entry_changes WHERE scan=? AND path=? AND deleted=0 AND directory=0",params![scan,index::path_string(&path)],|r|Ok((r.get(0)?,r.get(1)?))).optional()?
        } else if crate::compact::is_legacy(db)? {
            db.query_row(
                "SELECT identity,bytes FROM main.entries WHERE scan=? AND path=? AND directory=0",
                params![scan, index::path_string(&path)],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
        } else {
            db.query_row("SELECT identity_bits,bytes FROM main._entries WHERE scan=(SELECT id FROM entry_scans WHERE value=?) AND path=? AND directory=0",params![scan,index::path_string(&path)],|r|Ok((r.get(0)?,r.get(1)?))).optional()?
        };
        if let Some((identity, bytes)) = found {
            let identity = read_identity(&identity)?;
            if identity.dev.to_string() == dev
                && identity.ino.to_string() == ino
                && known.insert(path.clone())
            {
                result.push((path, identity, bytes));
            }
        }
    }
    // A newly created hard link can point at an old indexed single-link file.
    // Only this uncommon case needs one metadata-only pass through old identities.
    let mut expected = HashMap::new();
    let mut stmt = source.prepare(crate::compact::hardlinks_query(source)?)?;
    for row in stmt.query_map([measurement], |r| r.get::<_, String>(1))? {
        let identity = read_identity(&row?)?;
        expected.insert((identity.dev, identity.ino), identity.nlink);
    }
    let mut observed = HashMap::<(u64, u64), u64>::new();
    for (_, identity, _) in &result {
        *observed.entry((identity.dev, identity.ino)).or_default() += 1;
    }
    expected.retain(|key, n| observed.get(key).copied().unwrap_or(0) < *n);
    if !expected.is_empty() {
        let sql = if crate::compact::is_legacy(db)? {
            "SELECT path,identity,bytes FROM main.entries WHERE scan=? AND directory=0"
        } else {
            "SELECT path,identity_bits,bytes FROM main._entries WHERE scan=(SELECT id FROM entry_scans WHERE value=?) AND directory=0"
        };

        let mut stmt = db.prepare(sql)?;
        for row in stmt.query_map([scan], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })? {
            let (path, identity, bytes) = row?;
            let identity = read_identity(&identity)?;
            if expected.contains_key(&(identity.dev, identity.ino)) {
                let path = PathBuf::from(path);
                if !replaced.contains(&path) && known.insert(path.clone()) {
                    result.push((path, identity, bytes));
                }
            }
        }
    }
    for (path, identity, _) in &result {
        if expected.contains_key(&(identity.dev, identity.ino)) {
            db.execute(
                "INSERT OR REPLACE INTO entry_aliases(scan,path,dev,ino) VALUES(?,?,?,?)",
                params![
                    scan,
                    index::path_string(path),
                    identity.dev.to_string(),
                    identity.ino.to_string()
                ],
            )?;
        }
    }
    Ok(result)
}
fn link_overlap(links: &[Link], ancestor: &Path) -> i64 {
    let mut branches = HashMap::<(u64, u64), HashMap<PathBuf, i64>>::new();
    for (path, identity, bytes) in links {
        let Ok(relative) = path.strip_prefix(ancestor) else {
            continue;
        };
        let Some(first) = relative.components().next() else {
            continue;
        };
        let branch = if path.parent() == Some(ancestor) {
            path.clone()
        } else {
            ancestor.join(first)
        };
        let size = branches
            .entry((identity.dev, identity.ino))
            .or_default()
            .entry(branch)
            .or_default();
        *size = (*size).max(*bytes);
    }
    branches
        .values()
        .map(|group| group.values().sum::<i64>() - group.values().max().copied().unwrap_or(0))
        .sum()
}
fn child_totals(db: &Connection, scan: &str, parent: &Path) -> Result<(i64, i64, i64, i64, i64)> {
    let rows = if crate::compact::is_legacy(db)? {
        "SELECT bytes,logical,files,dirs,directory,skipped FROM entries WHERE scan=?1 AND parent=?2 AND path<>?2"
    } else if crate::overlay::active(db)? {
        "SELECT e.bytes,e.logical,e.files,e.dirs,e.directory,e.skipped FROM main._entries e WHERE e.scan=(SELECT id FROM entry_scans WHERE value=?1) AND e.parent=(SELECT id FROM entry_parents WHERE path=?2) AND e.path<>?2 AND NOT EXISTS(SELECT 1 FROM entry_changes d WHERE d.scan=?1 AND d.path=e.path) UNION ALL SELECT bytes,logical,files,dirs,directory,skipped FROM entry_changes WHERE scan=?1 AND parent=?2 AND path<>?2 AND deleted=0"
    } else {
        "SELECT bytes,logical,files,dirs,directory,skipped FROM _entries WHERE scan=(SELECT id FROM entry_scans WHERE value=?1) AND parent=(SELECT id FROM entry_parents WHERE path=?2) AND path<>?2"
    };
    Ok(db.query_row(&format!("SELECT coalesce(sum(bytes),0),coalesce(sum(logical),0),coalesce(sum(files),0),coalesce(sum(dirs+directory),0),coalesce(sum(skipped),0) FROM ({rows})"),params![scan,index::path_string(parent)],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?)
}

fn affected_owners(
    db: &Connection,
    scan: &str,
    root: &Path,
    lower: &str,
    upper: &str,
) -> Result<(HashSet<String>, HashSet<String>)> {
    let legacy = crate::compact::is_legacy(db)?;
    let table = if legacy {
        "main.entries"
    } else {
        "main._entries"
    };
    let scope = if legacy {
        "e.scan=?1"
    } else {
        "e.scan=(SELECT id FROM entry_scans WHERE value=?1)"
    };
    let overlay = crate::overlay::active(db)?;
    let guard = if overlay {
        "AND NOT EXISTS(SELECT 1 FROM entry_changes d WHERE d.scan=?1 AND d.path=e.path)"
    } else {
        ""
    };
    let mut sql=format!("SELECT project,tool FROM {table} e WHERE {scope} AND path=?2 {guard} UNION ALL SELECT project,tool FROM {table} e WHERE {scope} AND path>=?3 AND path<?4 {guard}");
    if overlay {
        sql.push_str(" UNION ALL SELECT project,tool FROM entry_changes WHERE scan=?1 AND (path=?2 OR path>=?3 AND path<?4) AND deleted=0");
    }
    let mut projects = HashSet::new();
    let mut tools = HashSet::new();
    let mut stmt = db.prepare(&sql)?;
    for row in stmt.query_map(params![scan, index::path_string(root), lower, upper], |r| {
        Ok((
            r.get::<_, Option<String>>(0)?,
            r.get::<_, Option<String>>(1)?,
        ))
    })? {
        let (p, t) = row?;
        projects.extend(p);
        tools.extend(t);
    }
    Ok((projects, tools))
}
fn replace_subtree(
    db: &Connection,
    scan: &str,
    root: &Path,
    lower: &str,
    upper: &str,
) -> Result<()> {
    let path = index::path_string(root);
    if crate::overlay::active(db)? {
        db.execute(
            "DELETE FROM entry_changes WHERE scan=? AND (path=? OR path>=? AND path<?)",
            params![scan, path, lower, upper],
        )?;
        let legacy = crate::compact::is_legacy(db)?;
        let table = if legacy {
            "main.entries"
        } else {
            "main._entries"
        };
        let scope = if legacy {
            "scan=?1"
        } else {
            "scan=(SELECT id FROM entry_scans WHERE value=?1)"
        };
        db.execute(&format!("INSERT OR REPLACE INTO entry_changes(scan,path,deleted) SELECT ?1,path,1 FROM {table} WHERE {scope} AND path=?2 UNION ALL SELECT ?1,path,1 FROM {table} WHERE {scope} AND path>=?3 AND path<?4"),params![scan,path,lower,upper])?;
    } else if !crate::compact::is_legacy(db)? {
        db.execute(
            "DELETE FROM _entries WHERE scan=(SELECT id FROM entry_scans WHERE value=?) AND path=?",
            params![scan, path],
        )?;
        db.execute("DELETE FROM _entries WHERE scan=(SELECT id FROM entry_scans WHERE value=?) AND path>=? AND path<?",params![scan,lower,upper])?;
    } else {
        db.execute(
            "DELETE FROM entries WHERE scan=? AND (path=? OR path>=? AND path<?)",
            params![scan, path, lower, upper],
        )?;
    }
    Ok(())
}
