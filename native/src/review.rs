use crate::{
    cleanup::{self, Frozen},
    fail, files,
    index::{self, Identity},
    mole, now, scan, Result,
};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    fs,
    io::Write,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};
pub fn initialize(db: &Connection) -> Result<()> {
    crate::user_state::initialize(db)
}
pub fn supported(path: &Path, home: &Path) -> bool {
    if !path.starts_with(home) || path == home || scan::protected(path) {
        return false;
    }
    let relative = path.strip_prefix(home).unwrap().to_string_lossy();
    regex::Regex::new(r"^(?:Library/(?:Caches|Logs)/[^/]+|\.npm/_cacache/[^/]+|\.cache/(?:pip|uv|go-build)/[^/]+|Library/Containers/[^/]+/Data/(?:tmp|Library/Caches)/[^/]+|Library/Application Support/CrashReporter/[^/]+|Library/Application Support/[^/]+/(?:[^/]+/)*(?:Cache|Code Cache|GPUCache|DawnCache|DawnGraphiteCache|DawnWebGPUCache))(?:/.*)?$").unwrap().is_match(&relative)
}
pub fn parse_ledger(contents: &str) -> Vec<Value> {
    let mut category = "其他".to_owned();
    let mut seen = HashSet::new();
    let mut rows = Vec::new();
    for line in contents.lines() {
        if line.starts_with("=== ") && line.ends_with(" ===") {
            category = line[4..line.len() - 4].into();
        } else if line.starts_with('/') {
            if let Some((path, estimate)) = line.rsplit_once("  # ") {
                if seen.insert(path.to_owned()) {
                    rows.push(json!({"id":crate::id(),"path":path,"name":Path::new(path).file_name().unwrap_or_default().to_string_lossy(),"category":category,"estimate":estimate}));
                }
            }
        }
    }
    rows
}
pub fn run_preview(
    db: &Connection,
    resources: &Path,
    home: &Path,
    command: &str,
    job: &str,
) -> Result<Value> {
    let ledger_path = home.join(".config/mole/clean-list.txt");
    let previous = fs::symlink_metadata(&ledger_path)
        .ok()
        .map(|m| Identity::of(&m));
    let started = chrono::Utc::now().timestamp();
    let mut result = mole::run_stream(resources, home, command, false, |stats| {
        println!("{}", json!({"event":"progress","stats":stats}));
        let _ = std::io::stdout().flush();
    })?;
    result["id"] = json!(job);
    result["expires"] = json!(chrono::Utc::now().timestamp() + 1800);
    result["used"] = json!(false);
    result["scriptIdentity"] = json!(Identity::of(&fs::metadata(
        resources.join("vendor/mole/mole")
    )?));
    if command == "clean" && result["success"] == true {
        let ledger = home.join(".config/mole/clean-list.txt");
        if let Ok(m) = index::checked(&ledger, home, fs::metadata(home)?.dev()) {
            if m.is_file()
                && m.mtime() >= started - 2
                && previous.as_ref() != Some(&Identity::of(&m))
            {
                let (bytes, _) = files::read_prefix(&ledger, home, 16 * 1024 * 1024, false)?;
                result["ledger"] = json!(String::from_utf8_lossy(&bytes));
            }
        }
    }
    index::save_setting(db, "mole-preview", &result)?;
    if result["success"] != true {
        return Err(fail(format!("Mole 预览未成功：{}", result["output"])));
    }
    Ok(result)
}
pub fn preview(db: &Connection, id: &str, resources: &Path) -> Result<Value> {
    let p = index::setting(db, "mole-preview")?.ok_or_else(|| fail("请先完成 Mole clean 预览"))?;
    if p["id"] != id
        || p["command"] != "clean"
        || p["success"] != true
        || p["used"] == true
        || p["expires"].as_i64().unwrap_or(0) < chrono::Utc::now().timestamp()
        || p["output"].as_str().is_none_or(|s| s.trim().is_empty())
    {
        return Err(fail("预览不匹配、已过期或使用"));
    }
    let identity: Identity = serde_json::from_value(p["scriptIdentity"].clone())?;
    if identity != Identity::of(&fs::metadata(resources.join("vendor/mole/mole"))?) {
        return Err(fail("Mole 程序已变化，需重新预览"));
    }
    Ok(p)
}
pub fn claim_mole(db: &Connection, id: &str, confirmation: &str, resources: &Path) -> Result<()> {
    if confirmation != "PERMANENT_MOLE_CLEAN" {
        return Err(fail("请明确确认整次 Mole 永久清理"));
    }
    db.execute_batch("BEGIN IMMEDIATE")?;
    let outcome = (|| {
        let mut p = preview(db, id, resources)?;
        p["used"] = json!(true);
        index::save_setting(db, "mole-preview", &p)?;
        index::save_setting(db, "dirty", &json!({"createdAt":now(),"previewId":id}))?;
        Ok(())
    })();
    match outcome {
        Ok(()) => {
            db.execute_batch("COMMIT")?;
            Ok(())
        }
        Err(e) => {
            let _ = db.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}
pub fn prepare(db: &Connection, home: &Path, resources: &Path, preview_id: &str) -> Result<Value> {
    initialize(db)?;
    let preview = preview(db, preview_id, resources)?;
    let ledger = preview["ledger"]
        .as_str()
        .ok_or_else(|| fail("本次预览未产生具体清单，请重新生成"))?;
    let mut groups = parse_ledger(ledger);
    let roots: Vec<PathBuf> = groups
        .iter()
        .filter_map(|g| g["path"].as_str().map(PathBuf::from))
        .filter(|p| supported(p, home))
        .collect();
    let id = crate::id();
    db.execute(
        "INSERT INTO reviews VALUES(?,?,?,0,?,?)",
        params![
            id,
            now(),
            chrono::Utc::now().timestamp() + 1800,
            preview_id,
            "[]"
        ],
    )?;
    for group in &mut groups {
        let path = PathBuf::from(group["path"].as_str().unwrap());
        let gid = group["id"].as_str().unwrap().to_owned();
        println!(
            "{}",
            json!({"event":"progress","stats":{"currentPath":path}})
        );
        let _ = std::io::stdout().flush();
        let outcome = (|| -> Result<(i64, usize)> {
            if !supported(&path, home) {
                return Err(fail("超出可选缓存和日志范围，保留"));
            }
            if roots
                .iter()
                .any(|other| other != &path && path.starts_with(other))
            {
                return Err(fail("已包含在上级清单中"));
            }
            let device = fs::metadata(home)?.dev();
            let mut pending = vec![path.clone()];
            let (mut bytes, mut files) = (0, 0);
            let mut nodes = Vec::new();
            while let Some(current) = pending.pop() {
                let m = index::checked(&current, home, device)?;
                if scan::protected(&current) {
                    return Err(fail("包含受保护内容，保留该范围"));
                }
                let parent = current.parent().ok_or_else(|| fail("缺少父目录"))?;
                let node = json!({"path":current,"root":path,"kind":if current.to_string_lossy().contains("/Logs/"){"log"}else{"cache"},"bytes":if m.is_file(){m.blocks()*512}else{0},"identity":Identity::of(&m),"parent":Identity::of(&fs::symlink_metadata(parent)?),"directory":m.is_dir()});
                if m.is_dir() {
                    for entry in fs::read_dir(&current)? {
                        pending.push(entry?.path());
                    }
                } else {
                    if m.nlink() != 1 {
                        return Err(fail("包含硬链接，保留该范围"));
                    }
                    bytes += (m.blocks() * 512) as i64;
                    files += 1;
                }
                nodes.push(node);
            }
            for chunk in nodes.chunks(1000) {
                let tx = db.unchecked_transaction()?;
                for node in chunk {
                    tx.execute(
                        "INSERT INTO review_nodes VALUES(?,?,?,?)",
                        params![id, crate::id(), gid, node.to_string()],
                    )?;
                }
                tx.commit()?;
                println!(
                    "{}",
                    json!({"event":"progress","stats":{"files":files,"bytes":bytes,"currentPath":path}})
                );
                let _ = std::io::stdout().flush();
            }
            Ok((bytes, files))
        })();
        match outcome {
            Ok((bytes, files)) => {
                group["selectable"] = json!(true);
                group["bytes"] = json!(bytes);
                group["files"] = json!(files);
            }
            Err(e) => {
                group["selectable"] = json!(false);
                group["blocked"] = json!(e.to_string());
            }
        }
    }
    db.execute(
        "UPDATE reviews SET groups_json=? WHERE id=?",
        params![json!(groups).to_string(), id],
    )?;
    Ok(json!({"id":id,"previewId":preview_id,"createdAt":now(),"groups":groups}))
}
pub fn detail(db: &Connection, plan: &str, group: Option<&str>, offset: i64) -> Result<Value> {
    let mut stmt=db.prepare("SELECT id,group_id,data FROM review_nodes WHERE plan=? AND (? IS NULL OR group_id=?) ORDER BY rowid LIMIT 100 OFFSET ?")?;
    let rows = stmt
        .query_map(params![plan, group, group, offset.max(0)], |r| {
            let s: String = r.get(2)?;
            let mut v: Value = serde_json::from_str(&s).unwrap();
            v["id"] = json!(r.get::<_, String>(0)?);
            v["groupId"] = json!(r.get::<_, String>(1)?);
            Ok(v)
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let total: i64 = db.query_row(
        "SELECT count(*) FROM review_nodes WHERE plan=? AND (? IS NULL OR group_id=?)",
        params![plan, group, group],
        |r| r.get(0),
    )?;
    Ok(json!({"rows":rows,"total":total,"offset":offset,"hasMore":offset+100<total}))
}
pub fn claim(
    db: &Connection,
    plan: &str,
    ids: &[Value],
    permanent: bool,
    confirmation: &str,
    resources: &Path,
) -> Result<()> {
    if confirmation
        != if permanent {
            "PERMANENT_REVIEWED_FILES"
        } else {
            "TRASH_REVIEWED_FILES"
        }
    {
        return Err(fail("请审阅清单并确认处理方式"));
    }
    initialize(db)?;
    db.execute_batch("BEGIN IMMEDIATE")?;
    let outcome = (|| -> Result<()> {
        let (expires, used, p, groups): (i64, bool, String, String) = db.query_row(
            "SELECT expires,used,preview,groups_json FROM reviews WHERE id=?",
            [plan],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        if used || expires < chrono::Utc::now().timestamp() {
            return Err(fail("清单已使用或过期"));
        }
        let mut source_preview = preview(db, &p, resources)?;
        source_preview["used"] = json!(true);
        index::save_setting(db, "mole-preview", &source_preview)?;
        let groups: Value = serde_json::from_str(&groups)?;
        let unique: HashSet<_> = ids.iter().filter_map(|v| v.as_str()).collect();
        if ids.is_empty()
            || ids.len() != unique.len()
            || ids.iter().any(|id| {
                !groups
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|g| g["id"] == *id && g["selectable"] == true)
            })
        {
            return Err(fail("所选分组不属于可清理范围"));
        }
        db.execute("UPDATE reviews SET used=1 WHERE id=?", [plan])?;
        index::save_setting(db, "dirty", &json!({"createdAt":now(),"plan":plan}))?;
        Ok(())
    })();
    match outcome {
        Ok(()) => {
            db.execute_batch("COMMIT")?;
            Ok(())
        }
        Err(e) => {
            let _ = db.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}
pub fn execute(
    db: &Connection,
    home: &Path,
    resources: &Path,
    plan: &str,
    groups: &[Value],
    permanent: bool,
    job: &str,
) -> Result<Value> {
    let used: bool = db.query_row("SELECT used FROM reviews WHERE id=?", [plan], |r| r.get(0))?;
    if !used {
        return Err(fail("清单未确认"));
    }
    let mut stmt = db.prepare("SELECT group_id,data FROM review_nodes WHERE plan=?")?;
    let selected: HashSet<_> = groups.iter().filter_map(|v| v.as_str()).collect();
    let nodes = stmt
        .query_map([plan], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut rows: Vec<Value> = nodes
        .into_iter()
        .filter(|(g, _)| selected.contains(g.as_str()))
        .filter_map(|(group, s)| {
            let mut row: Value = serde_json::from_str(&s).ok()?;
            row["groupId"] = json!(group);
            Some(row)
        })
        .collect();
    let mut refused = std::collections::HashMap::new();
    for group in &selected {
        let group_rows: Vec<_> = rows.iter().filter(|r| r["groupId"] == **group).collect();
        let check = || -> Result<()> {
            for row in &group_rows {
                let path = Path::new(row["path"].as_str().unwrap());
                let identity: Identity = serde_json::from_value(row["identity"].clone())?;
                let live = index::checked(path, home, identity.dev)?;
                if Identity::of(&live) != identity {
                    return Err(fail("预览后目录内容或文件已变化，保留整个分组"));
                }
            }
            Ok(())
        };
        let outcome = (|| {
            check()?;
            if let Some(root) = group_rows.first().and_then(|r| r["root"].as_str()) {
                mole::guard(resources, home, Path::new(root))?;
            }
            check()
        })();
        if let Err(error) = outcome {
            refused.insert((*group).to_owned(), error.to_string());
        }
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r["path"].as_str().unwrap_or("").len()));
    let (mut moved, mut skipped, mut bytes) = (0, 0, 0);
    for (i, row) in rows.iter().enumerate() {
        let path = PathBuf::from(row["path"].as_str().unwrap());
        let result = (|| -> Result<()> {
            if let Some(reason) = refused.get(row["groupId"].as_str().unwrap_or("")) {
                return Err(fail(reason));
            }
            if row["directory"] == true {
                if !permanent {
                    return Ok(());
                }
                let identity: Identity = serde_json::from_value(row["identity"].clone())?;
                let m = index::checked(&path, home, identity.dev)?;
                if m.dev() != identity.dev || m.ino() != identity.ino {
                    return Err(fail("目录已变化"));
                }
                if path == PathBuf::from(row["root"].as_str().unwrap_or("")) && path == home {
                    return Err(fail("根目录保留"));
                }
                mole::guard(resources, home, &path)?;
                remove_reviewed_directory(
                    &path,
                    home,
                    &identity,
                    &serde_json::from_value(row["parent"].clone())?,
                )?;
                db.execute("INSERT INTO receipts(job,created,path,status,bytes,reason) VALUES(?,?,?,'deleted-directory',0,'')",params![job,now(),index::path_string(&path)])?;
                return Ok(());
            }
            let file: Frozen = serde_json::from_value(row.clone())?;
            cleanup::process_reviewed(db, job, &file, home, true, permanent, |f| {
                mole::guard(resources, home, &f.root)?;
                mole::unused(&f.path)
            })?;
            moved += 1;
            bytes += file.bytes;
            Ok(())
        })();
        if let Err(e) = result {
            skipped += 1;
            db.execute("INSERT INTO receipts(job,created,path,status,bytes,reason) VALUES(?,?,?,'skipped',0,?)",params![job,now(),index::path_string(&path),e.to_string()])?;
        }
        println!(
            "{}",
            json!({"event":"progress","stats":{"completed":i+1,"total":rows.len(),"moved":moved,"skipped":skipped,"bytes":bytes,"currentPath":path}})
        );
        let _ = std::io::stdout().flush();
    }
    let result = json!({"jobId":job,"moved":moved,"skipped":skipped,"bytes":bytes,"method":if permanent{"永久删除"}else{"移到废纸篓"},"createdAt":now()});
    index::save_setting(db, "last-receipt", &result)?;
    Ok(result)
}

fn remove_reviewed_directory(
    path: &Path,
    home: &Path,
    identity: &Identity,
    parent: &Identity,
) -> Result<()> {
    use std::{
        ffi::CString,
        os::{
            fd::AsRawFd,
            unix::{ffi::OsStrExt, fs::OpenOptionsExt},
        },
    };
    let parent_path = path.parent().ok_or_else(|| fail("无上级目录"))?;
    let live_parent = index::checked(parent_path, home, parent.dev)?;
    if live_parent.dev() != parent.dev || live_parent.ino() != parent.ino {
        return Err(fail("上级目录已变化"));
    }
    let handle = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(parent_path)?;
    let opened = handle.metadata()?;
    if opened.dev() != parent.dev || opened.ino() != parent.ino {
        return Err(fail("上级目录身份不匹配"));
    }
    let name = CString::new(path.file_name().ok_or_else(|| fail("无目录名"))?.as_bytes())?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            handle.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    let stat = unsafe { stat.assume_init() };
    if stat.st_dev as u64 != identity.dev
        || stat.st_ino as u64 != identity.ino
        || stat.st_mode & libc::S_IFMT != libc::S_IFDIR
    {
        return Err(fail("待删除目录已变化"));
    }
    // AT_REMOVEDIR never recurses: newly added children remain untouched.
    if unsafe { libc::unlinkat(handle.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
