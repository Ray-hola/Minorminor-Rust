use crate::{
    cleanup,
    files::{Resource, Resources},
    index::{self, Identity},
    Result,
};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::os::unix::fs::MetadataExt;
use std::{
    fs,
    path::{Path, PathBuf},
};
pub fn list(
    db: &Connection,
    resources: &Resources,
    scan: &str,
    home: &Path,
    offset: i64,
) -> Result<Value> {
    index::assert_scan(db, scan)?;
    let mut stmt=db.prepare("SELECT path,identity,category,bytes,logical FROM entries WHERE scan=? AND directory=0 AND ((parent=? AND category='installer') OR (parent=? AND category='log')) ORDER BY bytes DESC LIMIT 100 OFFSET ?")?;
    let rows = stmt
        .query_map(
            params![
                scan,
                index::path_string(&home.join("Downloads")),
                index::path_string(&home.join("Library/Logs")),
                offset.max(0)
            ],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            },
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut result = Vec::new();
    for (path, saved, kind, bytes, logical) in rows {
        let identity: Identity = serde_json::from_str(&saved)?;
        let path = PathBuf::from(path);
        let live = fs::symlink_metadata(&path);
        let reason = match live {
            Ok(m) => {
                if m.len() == 0 {
                    Some("空文件无需整理".into())
                } else if Identity::of(&m) != identity {
                    Some("扫描后文件已变化".into())
                } else if kind == "log"
                    && (chrono::Utc::now().timestamp()
                        - m.mtime().max(std::os::unix::fs::MetadataExt::ctime(&m))
                        < 14 * 86400
                        || !regex::Regex::new(r"(?i)\.log(?:\.\d+)?(?:\.gz)?$")
                            .unwrap()
                            .is_match(&path.file_name().unwrap_or_default().to_string_lossy()))
                {
                    Some("仅整理14天前的历史日志".into())
                } else {
                    cleanup::block(&path, &m, &kind, home, false)
                }
            }
            Err(_) => Some("文件不可读或已失效".into()),
        };
        let id = resources.issue(Resource {
            path: path.clone(),
            root: home.into(),
            scan: scan.into(),
            identity,
            directory: false,
        });
        result.push(json!({"id":id,"name":path.file_name().unwrap_or_default().to_string_lossy(),"path":path,"kind":kind,"bytes":bytes,"logicalBytes":logical,"selectable":reason.is_none(),"reason":reason}));
    }
    let total:i64=db.query_row("SELECT count(*) FROM entries WHERE scan=? AND directory=0 AND ((parent=? AND category='installer') OR (parent=? AND category='log'))",params![scan,index::path_string(&home.join("Downloads")),index::path_string(&home.join("Library/Logs"))],|r|r.get(0))?;
    Ok(json!({"rows":result,"total":total,"offset":offset,"hasMore":offset+100<total}))
}
