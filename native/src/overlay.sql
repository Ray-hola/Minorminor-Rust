CREATE TEMP VIEW entries AS SELECT scan,path,parent,name,directory,bytes,logical,unique_bytes,files,dirs,identity,category,project,tool,skipped,installation FROM main.entries b WHERE NOT EXISTS(SELECT 1 FROM user_state.entry_changes d WHERE d.scan=b.scan AND d.path=b.path)
UNION ALL SELECT scan,path,parent,name,directory,bytes,logical,unique_bytes,files,dirs,identity,category,project,tool,skipped,installation FROM user_state.entry_changes WHERE deleted=0;
CREATE TEMP TRIGGER overlay_entries_insert INSTEAD OF INSERT ON entries BEGIN
 INSERT OR REPLACE INTO entry_changes(scan,path,parent,name,directory,bytes,logical,unique_bytes,files,dirs,identity,category,project,tool,skipped,installation,deleted) VALUES(NEW.scan,NEW.path,NEW.parent,NEW.name,NEW.directory,NEW.bytes,NEW.logical,NEW.unique_bytes,coalesce(NEW.files,0),coalesce(NEW.dirs,0),NEW.identity,NEW.category,NEW.project,NEW.tool,coalesce(NEW.skipped,0),coalesce(NEW.installation,0),0);
END;
CREATE TEMP TRIGGER overlay_entries_update INSTEAD OF UPDATE ON entries BEGIN
 INSERT OR REPLACE INTO entry_changes(scan,path,parent,name,directory,bytes,logical,unique_bytes,files,dirs,identity,category,project,tool,skipped,installation,deleted) VALUES(NEW.scan,NEW.path,NEW.parent,NEW.name,NEW.directory,NEW.bytes,NEW.logical,NEW.unique_bytes,coalesce(NEW.files,0),coalesce(NEW.dirs,0),NEW.identity,NEW.category,NEW.project,NEW.tool,coalesce(NEW.skipped,0),coalesce(NEW.installation,0),0);
END;
CREATE TEMP TRIGGER overlay_entries_delete INSTEAD OF DELETE ON entries BEGIN
 INSERT OR REPLACE INTO entry_changes(scan,path,deleted) VALUES(OLD.scan,OLD.path,1);
END;
CREATE TEMP VIEW projects AS SELECT * FROM main.projects b WHERE NOT EXISTS(SELECT 1 FROM user_state.project_changes d WHERE d.scan=b.scan AND d.id=b.id)
UNION ALL SELECT * FROM user_state.project_changes;
CREATE TEMP TRIGGER overlay_projects_insert INSTEAD OF INSERT ON projects BEGIN
 INSERT OR REPLACE INTO project_changes(scan,id,path,name,markers) VALUES(NEW.scan,NEW.id,NEW.path,NEW.name,NEW.markers);
END;
CREATE TEMP VIEW scans AS SELECT b.id,b.root,b.created,b.status,coalesce(d.stats,b.stats) AS stats FROM main.scans b LEFT JOIN user_state.scan_changes d ON d.id=b.id;
CREATE TEMP TRIGGER overlay_scans_update INSTEAD OF UPDATE OF stats ON scans BEGIN
 INSERT OR REPLACE INTO scan_changes(id,stats) VALUES(NEW.id,NEW.stats);
END;
CREATE TEMP VIEW errors AS SELECT b.scan,b.path,b.code,b.reason FROM main.errors b WHERE NOT EXISTS(SELECT 1 FROM user_state.error_changes d WHERE d.scan=b.scan AND d.path=b.path AND d.code=b.code)
UNION ALL SELECT scan,path,code,reason FROM user_state.error_changes WHERE deleted=0;
CREATE TEMP TRIGGER overlay_errors_insert INSTEAD OF INSERT ON errors BEGIN
 INSERT OR REPLACE INTO error_changes(scan,path,code,reason,deleted) VALUES(NEW.scan,NEW.path,NEW.code,NEW.reason,0);
END;
CREATE TEMP TRIGGER overlay_errors_delete INSTEAD OF DELETE ON errors BEGIN
 INSERT OR REPLACE INTO error_changes(scan,path,code,reason,deleted) VALUES(OLD.scan,OLD.path,OLD.code,OLD.reason,1);
END;
