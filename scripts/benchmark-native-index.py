#!/usr/bin/env python3
"""Compare index reclamation using disposable metadata, never user files."""
import argparse
import contextlib
import json
import pathlib
import signal
import sqlite3
import subprocess
import tempfile
import time
from native_index_support import connect_index, resolve_index

ROOT = pathlib.Path(__file__).resolve().parent.parent


def run(binary, base, pages):
    home, data = base / 'home', base / 'state'
    home.mkdir(parents=True)
    app = subprocess.Popen([str(binary), '--headless', '--home', str(home),
                            '--data', str(data), '--resources', str(ROOT)],
                           stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        json.loads(app.stdout.readline())['url']
    finally:
        app.send_signal(signal.SIGINT)
        app.communicate(timeout=15)
    path = resolve_index(data / 'index.sqlite3')
    with contextlib.closing(connect_index(path, writable=True)) as db:
        db.execute("INSERT INTO scans VALUES('keep','/synthetic','today','complete','{}')")
        db.execute("INSERT INTO receipts VALUES('keep','today','/synthetic',NULL,'skipped',0,'keep')")
        db.execute("INSERT INTO settings VALUES('ui-preferences','{\"locale\":\"en\"}')")
        db.execute('CREATE TABLE ballast(id INTEGER PRIMARY KEY,value BLOB)')
        db.executemany('INSERT INTO ballast VALUES(?,zeroblob(3000))', ((i,) for i in range(pages)))
        db.commit()
        db.execute('DROP TABLE ballast')
        db.commit()
        unused = db.execute('PRAGMA freelist_count').fetchone()[0]
    started = time.monotonic()
    worker = subprocess.run([str(binary), '--worker'], input=json.dumps(dict(
        action='index-recover', jobId='benchmark', home=str(home), data=str(data),
        resources=str(ROOT), reset=False)), capture_output=True, text=True, timeout=180)
    elapsed = time.monotonic() - started
    assert worker.returncode == 0, worker.stderr
    events = [json.loads(line) for line in worker.stdout.splitlines()]
    assert events[-1]['event'] == 'complete', events[-1]
    with contextlib.closing(connect_index(path)) as db:
        assert db.execute('PRAGMA freelist_count').fetchone()[0] == 0
        assert db.execute('PRAGMA quick_check').fetchone()[0] == 'ok'
        assert db.execute('SELECT id,status FROM scans').fetchall() == [('keep', 'complete')]
        assert db.execute('SELECT job,status FROM receipts').fetchall() == [('keep', 'skipped')]
        assert json.loads(db.execute("SELECT value FROM settings WHERE key='ui-preferences'").fetchone()[0]) == {'locale': 'en'}
    return dict(binary=str(binary), seconds=round(elapsed, 3), reclaimedPages=unused,
                preservedSettingsAndHistory=True, quickCheck='ok')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline', type=pathlib.Path, required=True)
    parser.add_argument('--binary', type=pathlib.Path, required=True)
    parser.add_argument('--pages', type=int, default=6000)
    args = parser.parse_args()
    assert args.pages > 2048
    with tempfile.TemporaryDirectory(prefix='minorminor-index-speed-') as temporary:
        root = pathlib.Path(temporary).resolve()
        results = [run(binary.resolve(), root / str(i), args.pages)
                   for i, binary in enumerate([args.baseline, args.binary])]
    print(json.dumps(dict(results=results, speedup=round(results[0]['seconds'] / results[1]['seconds'], 2))), flush=True)


if __name__ == '__main__':
    main()
