"""Inspect an owned native index; mutable state is a separate SQLite database."""
import pathlib
import sqlite3


def resolve_index(path):
    path = pathlib.Path(path).resolve()
    modern = path.with_name('index-v4.sqlite3')
    return modern if path.name == 'index.sqlite3' and modern.exists() else path


def connect_index(path, *, writable=False):
    path = resolve_index(path)
    if writable:
        db = sqlite3.connect(path)
    else:
        wal = pathlib.Path(str(path) + '-wal')
        immutable = bool(getattr(path.stat(), 'st_flags', 0) & 32) and not (wal.exists() and wal.stat().st_size)
        db = sqlite3.connect(path.as_uri() + '?mode=ro' + ('&immutable=1' if immutable else ''), uri=True)
    state = path.parent / 'user-state.sqlite3'
    if state.exists():
        db.execute('ATTACH DATABASE ? AS user_state', (state.as_uri() + '?mode=rwc',))
    if bool(getattr(path.stat(), 'st_flags', 0) & 32) and state.exists():
        exists = db.execute("SELECT count(*) FROM user_state.sqlite_master WHERE name='entry_changes'").fetchone()[0]
        error_delta = db.execute("SELECT EXISTS(SELECT 1 FROM user_state.sqlite_master WHERE name='error_changes')").fetchone()[0]
        if exists and db.execute('SELECT EXISTS(SELECT 1 FROM user_state.entry_changes) OR EXISTS(SELECT 1 FROM user_state.project_changes) OR EXISTS(SELECT 1 FROM user_state.scan_changes)' + (' OR EXISTS(SELECT 1 FROM user_state.error_changes)' if error_delta else '')).fetchone()[0]:
            sql = (pathlib.Path(__file__).resolve().parent.parent / 'native/src/overlay.sql').read_text()
            db.executescript(sql if error_delta else sql.split('CREATE TEMP VIEW errors AS')[0])
    return db


def index_bytes(data):
    return sum(path.stat().st_blocks * 512 for name in
               ('index.sqlite3', 'index.sqlite3-wal', 'index-v4.sqlite3', 'index-v4.sqlite3-wal', 'user-state.sqlite3', 'user-state.sqlite3-wal')
               if (path := pathlib.Path(data) / name).is_file())
