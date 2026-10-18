use crate::{
    fail, id,
    index::{self, Identity},
    now, scan, Result,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    ffi::CString,
    fs,
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};
pub const TTL: i64 = 30 * 60;

pub fn block(
    path: &Path,
    m: &fs::Metadata,
    kind: &str,
    home: &Path,
    personal: bool,
) -> Option<String> {
    if !path.starts_with(home) || path == home {
        return Some("超出当前用户文件范围".into());
    }
    if scan::protected(path) {
        return Some("凭据、代码依赖、数据库、模型、备份或运行资源保留".into());
    }
    if !m.is_file() || m.file_type().is_symlink() {
        return Some("只整理已审阅的普通文件".into());
    }
    if m.nlink() != 1 {
        return Some("共享硬链接文件保留".into());
    }
    if !["cache", "log", "session", "memory"].contains(&kind) {
        return Some("此类文件保留".into());
    }
    if ["session", "memory"].contains(&kind) && !personal {
        return Some("会话与记忆需要单独启用".into());
    }
    if chrono::Utc::now().timestamp() - m.mtime() < 24 * 3600 {
        return Some("最近 24 小时仍有写入，保留".into());
    }
    None
}
fn guard_root(path: &Path, kind: &str) -> PathBuf {
    if kind == "cache" {
        let mut accumulated = PathBuf::new();
        let mut cache = false;
        for part in path.components() {
            accumulated.push(part.as_os_str());
            let p = part.as_os_str().to_string_lossy();
            if cache {
                return accumulated;
            }
            if [
                "Caches",
                ".cache",
                "__pycache__",
                ".pytest_cache",
                ".mypy_cache",
                ".ruff_cache",
                "Code Cache",
                "GPUCache",
                "cache",
            ]
            .contains(&p.as_ref())
            {
                cache = true;
            }
        }
    }
    path.to_owned()
}
pub fn prepare(db: &mut Connection, scan_id: &str, body: &Value, home: &Path) -> Result<Value> {
    index::assert_scan(db, scan_id)?;
    if index::setting(db, "dirty")?.is_some_and(|s| !s.is_null()) {
        return Err(fail("文件已整理，请重新扫描后生成预览"));
    }
    let personal = body["personal"].as_bool().unwrap_or(false);
    let kinds: Vec<String> = body["categories"]
        .as_array()
        .ok_or_else(|| fail("请选择类别"))?
        .iter()
        .filter_map(|s| s.as_str().map(str::to_owned))
        .collect();
    if kinds.is_empty()
        || kinds
            .iter()
            .any(|s| !["cache", "log", "session", "memory"].contains(&s.as_str()))
    {
        return Err(fail("无效的文件类别"));
    }
    if kinds
        .iter()
        .any(|s| ["session", "memory"].contains(&s.as_str()))
        && !personal
    {
        return Err(fail("请先单独启用会话和记忆审阅"));
    }
    let projects: HashSet<String> = body["projects"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str().map(str::to_owned))
        .collect();
    let tools: HashSet<String> = body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str().map(str::to_owned))
        .collect();
    if projects.is_empty() && tools.is_empty() && !body["allCaches"].as_bool().unwrap_or(false) {
        return Err(fail("请勾选要预览的项目或工具"));
    }
    let mut stmt=db.prepare("SELECT path,category,identity,project,tool,bytes FROM entries WHERE scan=? AND directory=0 AND category<>'other'")?;
    let rows = stmt
        .query_map([scan_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(stmt);
    let plan = id();
    let created = now();
    let expires = chrono::Utc::now().timestamp() + TTL;
    let tx = db.transaction()?;
    tx.execute(
        "INSERT INTO plans(id,scan,created,expires,personal) VALUES(?,?,?,?,?)",
        params![plan, scan_id, created, expires, personal],
    )?;
    let mut kept = Vec::new();
    let mut files = 0;
    let mut bytes = 0;
    for (path, kind, saved, project, tool, size) in rows {
        if !kinds.contains(&kind) {
            continue;
        }
        let selected = project
            .as_ref()
            .map(|p| projects.contains(p))
            .unwrap_or(false)
            || tool.as_ref().map(|p| tools.contains(p)).unwrap_or(false)
            || body["allCaches"].as_bool().unwrap_or(false)
                && ["cache", "log"].contains(&kind.as_str());
        if !selected {
            continue;
        }
        let path = PathBuf::from(path);
        let outcome = (|| -> Result<()> {
            let m = index::checked(&path, home, fs::metadata(home)?.dev())?;
            if let Some(reason) = block(&path, &m, &kind, home, personal) {
                return Err(fail(reason));
            }
            let saved: Identity = serde_json::from_str(&saved)?;
            if Identity::of(&m) != saved {
                return Err(fail("扫描后文件已变化，请重新扫描"));
            }
            let managed: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM associations WHERE scan=? AND path=? AND managed=1)",
                params![scan_id, index::path_string(&path)],
                |r| r.get(0),
            )?;
            if managed {
                return Err(fail("由共享数据库管理，请在所属工具内处理"));
            }
            let parent = Identity::of(&fs::symlink_metadata(
                path.parent().ok_or_else(|| fail("无上级目录"))?,
            )?);
            tx.execute("INSERT INTO plan_files(plan,id,path,root,category,bytes,identity,parent_identity) VALUES(?,?,?,?,?,?,?,?)",params![plan,id(),index::path_string(&path),index::path_string(&guard_root(&path,&kind)),kind,size,serde_json::to_string(&saved)?,serde_json::to_string(&parent)?])?;
            files += 1;
            bytes += size;
            Ok(())
        })();
        if let Err(error) = outcome {
            if kept.len() < 100 {
                kept.push(json!({"path":path,"reason":error.to_string()}));
            }
        }
    }
    tx.commit()?;
    Ok(
        json!({"id":plan,"scanId":scan_id,"createdAt":created,"expiresAt":expires,"files":files,"bytes":bytes,"method":"移到废纸篓","personal":personal,"retained":kept}),
    )
}
pub fn plan_files(db: &Connection, plan: &str, offset: u64) -> Result<Value> {
    let mut stmt=db.prepare("SELECT id,path,category,bytes FROM plan_files WHERE plan=? ORDER BY path LIMIT 100 OFFSET ?")?;
    let rows=stmt.query_map(params![plan,offset.min(i64::MAX as u64) as i64],|r|Ok(json!({"id":r.get::<_,String>(0)?,"path":r.get::<_,String>(1)?,"kind":r.get::<_,String>(2)?,"bytes":r.get::<_,i64>(3)?})))?.collect::<std::result::Result<Vec<_>,_>>()?;
    let total: i64 = db.query_row(
        "SELECT count(*) FROM plan_files WHERE plan=?",
        [plan],
        |r| r.get(0),
    )?;
    Ok(json!({"rows":rows,"offset":offset,"total":total,"hasMore":offset+100<total as u64}))
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Frozen {
    pub path: PathBuf,
    pub root: PathBuf,
    pub kind: String,
    pub bytes: i64,
    pub identity: Identity,
    pub parent: Identity,
}
pub fn claimed_files(db: &Connection, body: &Value) -> Result<Vec<Frozen>> {
    let plan = body["planId"].as_str().ok_or_else(|| fail("缺少预览 ID"))?;
    let used: bool = db.query_row("SELECT used FROM plans WHERE id=?", [plan], |r| r.get(0))?;
    if !used {
        return Err(fail("预览尚未确认"));
    }
    let picked: HashSet<String> = body["ids"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    let mut stmt = db.prepare(
        "SELECT id,path,root,category,bytes,identity,parent_identity FROM plan_files WHERE plan=?",
    )?;
    let rows = stmt.query_map([plan], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, String>(6)?,
        ))
    })?;
    let mut files = Vec::new();
    for row in rows {
        let (id, path, root, kind, bytes, identity, parent) = row?;
        if body["all"].as_bool().unwrap_or(false) || picked.contains(&id) {
            files.push(Frozen {
                path: PathBuf::from(path),
                root: PathBuf::from(root),
                kind,
                bytes,
                identity: serde_json::from_str(&identity)?,
                parent: serde_json::from_str(&parent)?,
            });
        }
    }
    Ok(files)
}
pub fn claim(db: &mut Connection, body: &Value) -> Result<Vec<Frozen>> {
    if body["confirmation"] != "TRASH_REVIEWED_FILES" {
        return Err(fail("请预览后明确确认移到废纸篓"));
    }
    let plan = body["planId"].as_str().ok_or_else(|| fail("缺少预览 ID"))?;
    let tx = db.transaction()?;
    let header: Option<(String, i64, bool, bool)> = tx
        .query_row(
            "SELECT scan,expires,used,personal FROM plans WHERE id=?",
            [plan],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let (scan, expires, used, personal) = header.ok_or_else(|| fail("预览已失效"))?;
    index::assert_scan(&tx, &scan)?;
    if used || expires < chrono::Utc::now().timestamp() {
        return Err(fail("预览已使用或过期，请重新生成"));
    }
    if personal && body["personalConfirmation"] != "ACKNOWLEDGE_HISTORY_LOSS" {
        return Err(fail("请确认处理会话与记忆可能影响历史和长期记忆"));
    }
    let picked: HashSet<String> = body["ids"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str().map(str::to_owned))
        .collect();
    let all = body["all"].as_bool().unwrap_or(false);
    if !all && picked.is_empty() {
        return Err(fail("请选择预览内的文件"));
    }
    let mut stmt = tx.prepare(
        "SELECT id,path,root,category,bytes,identity,parent_identity FROM plan_files WHERE plan=?",
    )?;
    let raw = stmt
        .query_map([plan], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut files = Vec::new();
    for (id, path, root, kind, bytes, identity, parent) in raw {
        if all || picked.contains(&id) {
            files.push(Frozen {
                path: PathBuf::from(path),
                root: PathBuf::from(root),
                kind,
                bytes,
                identity: serde_json::from_str(&identity)?,
                parent: serde_json::from_str(&parent)?,
            });
        }
    }
    drop(stmt);
    if files.is_empty() || !all && files.len() != picked.len() {
        return Err(fail("所选文件不属于当前预览"));
    }
    tx.execute("UPDATE plans SET used=1 WHERE id=? AND used=0", [plan])?;
    index::save_setting(&tx, "dirty", &json!({"scanId":scan,"createdAt":now()}))?;
    tx.commit()?;
    Ok(files)
}
fn directory(path: &Path) -> Result<fs::File> {
    Ok(fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?)
}
fn same_parent(a: &Identity, b: &Identity) -> bool {
    a.dev == b.dev && a.ino == b.ino
}
fn rename_exclusive(from: &fs::File, name: &str, to: &fs::File, target: &str) -> Result<()> {
    let source = CString::new(name)?;
    let target = CString::new(target)?;
    #[cfg(target_os = "macos")]
    let code = unsafe {
        libc::renameatx_np(
            from.as_raw_fd(),
            source.as_ptr(),
            to.as_raw_fd(),
            target.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    #[cfg(not(target_os = "macos"))]
    let code = unsafe {
        libc::renameat2(
            from.as_raw_fd(),
            source.as_ptr(),
            to.as_raw_fd(),
            target.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if code != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
pub fn move_one<F: Fn(&Frozen) -> Result<()>>(
    db: &Connection,
    job: &str,
    file: &Frozen,
    home: &Path,
    guard: F,
) -> Result<(String, Option<PathBuf>)> {
    let validate = || -> Result<()> {
        let m = index::checked(&file.path, home, file.identity.dev)?;
        if Identity::of(&m) != file.identity {
            return Err(fail("预览后文件身份发生变化，已保留"));
        }
        if let Some(reason) = block(&file.path, &m, &file.kind, home, true) {
            return Err(fail(reason));
        }
        Ok(())
    };
    validate()?;
    guard(file)?;
    validate()?;
    let parent_path = file.path.parent().ok_or_else(|| fail("无上级目录"))?;
    let parent = directory(parent_path)?;
    if !same_parent(&Identity::of(&parent.metadata()?), &file.parent) {
        return Err(fail("上级目录已被替换"));
    }
    let source = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&file.path)?;
    if Identity::of(&source.metadata()?) != file.identity {
        return Err(fail("文件在移动前发生变化"));
    }
    let trash = home.join(".Trash");
    index::secure_directory(&trash)?;
    let trash_dir = directory(&trash)?;
    if trash_dir.metadata()?.uid() != unsafe { libc::geteuid() }
        || trash_dir.metadata()?.dev() != file.identity.dev
    {
        return Err(fail("废纸篓所有者或卷不匹配"));
    }
    let name = file
        .path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| fail("文件名无法表示"))?;
    let target_name = format!(
        "Minorminor-{}-{}",
        id(),
        name.chars().take(100).collect::<String>()
    );
    let target = trash.join(&target_name);
    db.execute(
        "INSERT INTO receipts(job,created,path,target,status,bytes,reason) VALUES(?,?,?,?,?,?,?)",
        params![
            job,
            now(),
            index::path_string(&file.path),
            index::path_string(&target),
            "moving",
            file.bytes,
            ""
        ],
    )?;
    validate()?;
    rename_exclusive(&parent, name, &trash_dir, &target_name)?;
    let moved = fs::symlink_metadata(&target)?;
    if moved.dev() != file.identity.dev
        || moved.ino() != file.identity.ino
        || moved.len() != file.identity.size
        || moved.mtime() != file.identity.mtime
        || moved.mtime_nsec() != file.identity.mtime_ns
        || moved.nlink() != file.identity.nlink
    {
        let restored = rename_exclusive(&trash_dir, &target_name, &parent, name).is_ok();
        let status = if restored {
            "restored-stale"
        } else {
            "needs-review"
        };
        db.execute("INSERT INTO receipts(job,created,path,target,status,bytes,reason) VALUES(?,?,?,?,?,?,?)",params![job,now(),index::path_string(&file.path),index::path_string(&target),status,0,"移动瞬间身份变化；请核对原路径和废纸篓"])?;
        return Err(fail(if restored {
            "移动瞬间文件变化，已放回原路径"
        } else {
            "移动瞬间文件变化，需要根据记录核对原路径和废纸篓"
        }));
    }
    db.execute(
        "INSERT INTO receipts(job,created,path,target,status,bytes,reason) VALUES(?,?,?,?,?,?,?)",
        params![
            job,
            now(),
            index::path_string(&file.path),
            index::path_string(&target),
            "moved",
            file.bytes,
            ""
        ],
    )?;
    Ok(("moved".into(), Some(target)))
}
pub fn record_skip(db: &Connection, job: &str, file: &Frozen, error: &str) -> Result<()> {
    db.execute(
        "INSERT INTO receipts(job,created,path,status,bytes,reason) VALUES(?,?,?,?,?,?)",
        params![
            job,
            now(),
            index::path_string(&file.path),
            "skipped",
            0,
            error
        ],
    )?;
    Ok(())
}
pub fn receipt(db: &Connection, job: &str, offset: i64) -> Result<Value> {
    let mut stmt=db.prepare("SELECT created,path,target,status,bytes,reason FROM receipts WHERE job=? ORDER BY rowid LIMIT 100 OFFSET ?")?;
    let rows=stmt.query_map(params![job,offset.max(0)],|r|Ok(json!({"createdAt":r.get::<_,String>(0)?,"path":r.get::<_,String>(1)?,"target":r.get::<_,Option<String>>(2)?,"status":r.get::<_,String>(3)?,"bytes":r.get::<_,i64>(4)?,"reason":r.get::<_,String>(5)?})))?.collect::<std::result::Result<Vec<_>,_>>()?;
    let total: i64 = db.query_row("SELECT count(*) FROM receipts WHERE job=?", [job], |r| {
        r.get(0)
    })?;
    Ok(json!({"rows":rows,"total":total,"hasMore":offset+100<total}))
}
