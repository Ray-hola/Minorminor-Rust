#!/usr/bin/env python3
"""Benchmark full scans on owned files; optionally compare two native binaries."""
import argparse
import hashlib
import json
import os
import pathlib
import signal
import sqlite3
import statistics
import subprocess
import tempfile
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent


def fixture(home, count, sessions, directories):
    for i in range(directories):
        (home / f'Data/group-{i:05}').mkdir(parents=True, exist_ok=True)
    for i in range(count):
        path = home / f'Data/group-{i % directories:05}/file-{i:06}.bin'
        path.write_bytes(b'synthetic data' if i % 100 == 0 else b'')
    project = home / 'Projects/Example'
    project.mkdir(parents=True)
    (project / 'package.json').write_text('{}')
    cache = project / '.cache'
    cache.mkdir()
    (cache / 'one.bin').write_bytes(b'cache')
    os.link(cache / 'one.bin', cache / 'two.bin')
    os.link(cache / 'one.bin', project / 'same.bin')
    (home / 'shortcut').symlink_to(project, target_is_directory=True)
    session_dir = home / '.claude/sessions'
    session_dir.mkdir(parents=True)
    for i in range(sessions):
        (session_dir / f'run-{i:05}.jsonl').write_text(json.dumps({
            'type': 'session', 'cwd': str(project), 'role': 'assistant',
            'title': f'Synthetic session {i}',
            'content': 'SYNTHETIC_BODY_NOT_FOR_INDEX',
        }))


def fingerprint(db):
    digest = hashlib.sha256()
    queries = [
        '''SELECT e.path,e.parent,e.name,e.directory,e.bytes,e.logical,e.unique_bytes,
           e.files,e.dirs,e.identity,e.category,p.path,t.path,e.skipped
           FROM entries e LEFT JOIN projects p ON p.scan=e.scan AND p.id=e.project
           LEFT JOIN tools t ON t.scan=e.scan AND t.id=e.tool ORDER BY e.path''',
        '''SELECT a.path,a.indexed_path,p.path,t.path,a.kind,a.title,a.status,a.reason,a.managed
           FROM associations a LEFT JOIN projects p ON p.scan=a.scan AND p.id=a.project
           LEFT JOIN tools t ON t.scan=a.scan AND t.id=a.tool ORDER BY 1,2,3,4,5,6,7,8,9''',
        'SELECT path,name,uncertain FROM tools ORDER BY 1,2,3',
        'SELECT path,name,markers FROM projects ORDER BY 1,2,3',
        'SELECT path,code,reason FROM errors ORDER BY 1,2,3',
    ]
    for query in queries:
        for row in db.execute(query):
            digest.update(json.dumps(row, ensure_ascii=False).encode() + b'\n')
    return digest.hexdigest()


def run(binary, home, data, expected, expected_dirs, sessions):
    # Use the real initializer, then run the scan worker without a UI or network AI.
    app = subprocess.Popen([str(binary), '--headless', '--home', str(home),
                            '--data', str(data), '--resources', str(ROOT)],
                           stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        json.loads(app.stdout.readline())['url']
    finally:
        app.send_signal(signal.SIGINT)
        app.communicate(timeout=15)
    payload = dict(action='scan', jobId='benchmark', home=str(home), data=str(data),
                   resources=str(ROOT), root=str(home), disk=False,
                   ollamaBase='http://127.0.0.1:1')
    started = time.monotonic()
    worker = subprocess.Popen([str(binary), '--worker'], stdin=subprocess.PIPE,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    worker.stdin.write(json.dumps(payload))
    worker.stdin.close()
    stages = {}
    for line in worker.stdout:
        event = json.loads(line)
        stage = event.get('stats', {}).get('stage')
        if stage:
            stages.setdefault(stage, round(time.monotonic() - started, 3))
    error = worker.stderr.read()
    assert worker.wait() == 0, error
    elapsed = time.monotonic() - started
    with sqlite3.connect(data / 'index.sqlite3') as db:
        rows = dict(db.execute('SELECT path,logical FROM entries WHERE directory=0'))
        assert rows == expected, 'Full traversal lost files or changed logical sizes'
        directories = {row[0] for row in db.execute('SELECT path FROM entries WHERE directory=1')}
        assert directories == expected_dirs, 'Full traversal lost directories'
        associated = db.execute('SELECT count(*) FROM associations WHERE project IS NOT NULL').fetchone()[0]
        assert associated == sessions, (associated, sessions)
        assert db.execute('SELECT status FROM scans').fetchone()[0] == 'complete'
        result = dict(seconds=round(elapsed, 3), stages=stages, files=len(rows),
                      directories=db.execute('SELECT count(*) FROM entries WHERE directory=1').fetchone()[0],
                      associations=associated, fingerprint=fingerprint(db))
    print(json.dumps(dict(binary=str(binary), **result)), flush=True)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=pathlib.Path, default=ROOT / 'dist/Minorminor.app/Contents/MacOS/Minorminor')
    parser.add_argument('--baseline', type=pathlib.Path)
    parser.add_argument('--files', type=int, default=60000)
    parser.add_argument('--sessions', type=int, default=1200)
    parser.add_argument('--directories', type=int, default=2000)
    parser.add_argument('--runs', type=int, default=3)
    args = parser.parse_args()
    assert args.files > 0 and args.directories > 0 and args.sessions > 0 and args.runs > 0
    binaries = [p.resolve() for p in [args.baseline, args.binary] if p]
    with tempfile.TemporaryDirectory(prefix='minorminor-scan-speed-') as temporary:
        base = pathlib.Path(temporary).resolve()
        home = base / 'home'
        fixture(home, args.files, args.sessions, args.directories)
        walked = list(os.walk(home, followlinks=False))
        expected_dirs = {parent for parent, _, _ in walked}
        expected = {str(pathlib.Path(parent) / name): (pathlib.Path(parent) / name).stat().st_size
                    for parent, _, names in walked for name in names
                    if not (pathlib.Path(parent) / name).is_symlink()}
        results = {binary: [] for binary in binaries}
        reference = None
        for iteration in range(args.runs):
            for position, binary in enumerate(binaries if iteration % 2 == 0 else reversed(binaries)):
                result = run(binary, home, base / f'index-{iteration}-{position}', expected, expected_dirs, args.sessions)
                reference = reference or result['fingerprint']
                assert result['fingerprint'] == reference, 'Scan results differ across binaries/runs'
                results[binary].append(result['seconds'])
        print(json.dumps({'medianSeconds': {str(binary): statistics.median(times)
                                           for binary, times in results.items()},
                          'identicalIndexes': True}), flush=True)


if __name__ == '__main__':
    main()
