#!/usr/bin/env python3
"""Exercise the packaged Rust process with disposable files only."""
import json, os, pathlib, sqlite3, signal, subprocess, tempfile, time, urllib.request
from contextlib import closing
from native_index_support import connect_index

ROOT = pathlib.Path(__file__).resolve().parent.parent
APP = pathlib.Path(os.environ.get('MINORMINOR_TEST_BINARY', ROOT / 'dist/Minorminor.app/Contents/MacOS/Minorminor'))
with tempfile.TemporaryDirectory(prefix='minorminor-native-smoke-') as temporary:
    base = pathlib.Path(temporary).resolve()
    home, data = base / 'home', base / 'index'
    project = home / 'Projects/Demo'
    project.mkdir(parents=True)
    (project / 'package.json').write_text('{"name":"demo"}')
    (project / 'README.md').write_text('Synthetic project for native application tests.')
    cache = project / '.cache'
    cache.mkdir()
    old = cache / 'old.bin'
    old.write_bytes(b'cache' * 2048)
    os.utime(old, (time.time() - 172800,) * 2)
    fresh = cache / 'fresh.bin'
    fresh.write_bytes(b'keep recent data')
    (home / '.unknown-agent/sessions').mkdir(parents=True)
    (home / '.unknown-agent/config.toml').write_text('enabled=true')
    (home / '.unknown-agent/memory').mkdir()
    (home / '.unknown-agent/memory/MEMORY.md').write_text('Synthetic memory')
    (home / '.unknown-agent/sessions/run.jsonl').write_text(json.dumps({'type':'session', 'cwd':str(project), 'messages':[{'content':'PRIVATE_TRANSCRIPT_MARKER'}]}))
    process = subprocess.Popen([str(APP), '--headless', '--home', str(home), '--data', str(data)], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        url = json.loads(process.stdout.readline())['url']
        token = None
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        def api(action, body=None):
            headers = {} if body is None else {'content-type':'application/json','x-minorminor-token':token or ''}
            request = urllib.request.Request(url + '/api/' + action, data=None if body is None else json.dumps(body).encode(), headers=headers)
            with opener.open(request, timeout=8) as response:
                return json.load(response)
        def wait(job):
            deadline = time.monotonic() + 60
            while time.monotonic() < deadline:
                boot = api('bootstrap')
                current = next(j for j in boot['jobs'] if j['id'] == job['id'])
                if current['status'] != 'running':
                    assert current['status'] == 'complete', current
                    return current.get('result'), boot
                time.sleep(.1)
            raise AssertionError('Worker timed out')
        boot = api('bootstrap')
        token = boot['token']
        for asset in ['native.js','native-progress.js','motion.js','i18n.js']:
            with opener.open(url+'/'+asset,timeout=8) as response:
                assert response.read()==(ROOT/'public'/asset).read_bytes(), 'Packaged UI is stale: '+asset
        assert boot['native'] and boot['config']['permissions']['analysisDocumentRead'] is False
        api('preferences', {'locale':'en'})
        assert api('bootstrap')['preferences']['locale'] == 'en'
        # Hold mutable-state writes: private scan construction must preserve
        # responsive task status and cancellation under a locked database.
        lock = sqlite3.connect(data / 'user-state.sqlite3')
        lock.execute('BEGIN IMMEDIATE')
        started = time.monotonic()
        blocked = api('scan', {'scope':'home'})
        running = next(j for j in api('jobs')['jobs'] if j['id'] == blocked['id'])
        assert running['status'] == 'running', running
        capacity=running['stats']['diskSnapshot']
        assert capacity['total']>0 and capacity['free']>=0 and capacity['capturedAt']
        assert running['stats']['estimatedBytes']==capacity['used']
        assert running['stats']['estimateSource']=='volume-used'
        api('cancel', {})
        assert time.monotonic() - started < 2, 'Status or cancel waited on the database'
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline:
            cancelled = next(j for j in api('jobs')['jobs'] if j['id'] == blocked['id'])
            if cancelled['status'] == 'cancelled': break
            time.sleep(.02)
        assert cancelled['status'] == 'cancelled', cancelled
        lock.rollback()
        lock.close()
        result, boot = wait(api('scan', {'scope':'home'}))
        completed_scan = next(j for j in boot['jobs'] if j['id'] == boot['scan']['id'])
        assert completed_scan['stats']['elapsedMs'] == result['elapsedMs'] > 0, 'Completed scan duration must include publication work'
        scan = boot['scan']['id']
        assert scan == result['scanId'] and boot['models']['scanId'] == scan
        projects = api('projects', {'scanId':scan})['rows']
        demo = next(p for p in projects if p['name'] == 'Demo')
        assert demo['bytes'] > 0 and demo['sessions'] == 1
        with closing(connect_index(data / 'index-v4.sqlite3')) as db:
            assert db.execute('SELECT 1 FROM settings WHERE key=?', ('project-storage-cache-'+scan+'-'+demo['id'],)).fetchone()
        tools = api('tools', {'scanId':scan})['rows']
        assert any(t['name'] == 'unknown-agent' and t['uncertain'] for t in tools)
        page = api('files', {'scanId':scan,'project':demo['id']})
        folder = next(row for row in page['rows'] if row['name'] == '.cache')
        assert folder['directory'] and folder['bytes'] > 0
        nested = api('files', {'scanId':scan,'project':demo['id'],'directory':folder['id']})
        assert {row['name'] for row in nested['rows']} == {'old.bin','fresh.bin'}
        # A partial refresh must not expand/rewrite the compressed full snapshot.
        snapshot_blocks = (data / 'index-v4.sqlite3').stat().st_blocks
        added = old.parent / 'added.bin'
        added.write_bytes(b'new fixture data')
        wait(api('files/measure', {'scanId':scan, 'id':folder['id']}))
        assert (data / 'index-v4.sqlite3').stat().st_blocks == snapshot_blocks
        assert getattr((data / 'index-v4.sqlite3').stat(), 'st_flags', 0) & 32
        refreshed = api('files', {'scanId':scan, 'project':demo['id']})
        refreshed_folder = next(row for row in refreshed['rows'] if row['name'] == '.cache')
        refreshed_children = api('files', {'scanId':scan,'project':demo['id'],'directory':refreshed_folder['id']})
        assert {row['name'] for row in refreshed_children['rows']} == {'old.bin','fresh.bin','added.bin'}
        with closing(connect_index(data / 'index-v4.sqlite3')) as db:
            assert db.execute('SELECT logical FROM entries WHERE path=?', (str(added),)).fetchone()[0] == added.stat().st_size
        associated = api('files', {'scanId':scan, 'project':demo['id'], 'kind':'session'})
        excerpt = api('inspect', {'scanId':scan, 'id':associated['rows'][0]['id']})
        assert excerpt['mode'] == 'content' and 'PRIVATE_TRANSCRIPT_MARKER' not in excerpt['excerpt']
        plan = api('preview', {'scanId':scan,'projects':[demo['id']],'categories':['cache']})
        assert plan['files'] == 1, plan
        listing = api('preview/files', {'planId':plan['id']})
        assert listing['rows'][0]['path'] == str(old)
        result, boot = wait(api('execute', {'planId':plan['id'],'ids':[listing['rows'][0]['id']],'confirmation':'TRASH_REVIEWED_FILES'}))
        assert result['moved'] == 1 and result['skipped'] == 0, result
        assert not old.exists() and fresh.exists()
        receipt = api('receipts', {'jobId':result['jobId']})
        assert any(row['status'] == 'moved' for row in receipt['rows']), receipt
        moved = next(row for row in receipt['rows'] if row['status'] == 'moved')
        assert pathlib.Path(moved['target']).read_bytes() == b'cache' * 2048
        assert api('bootstrap')['stale'] is True
        assert not any(b'PRIVATE_TRANSCRIPT_MARKER' in p.read_bytes() for p in [*data.glob('index*.sqlite3*'), *data.glob('user-state.sqlite3*')])
        previous_bytes=api('bootstrap')['scan']['stats']['bytes']
        recalibrated=api('scan', {'scope':'home'})
        wait(recalibrated)
        with closing(connect_index(data / 'index-v4.sqlite3')) as db:
            assert not db.execute('SELECT 1 FROM settings WHERE key LIKE ?', ('project-storage-cache-'+scan+'-%',)).fetchone(), 'Old snapshot aggregates survived a new scan'
        recalibrated_job=next(j for j in api('jobs')['jobs'] if j['id']==recalibrated['id'])
        assert recalibrated_job['stats']['estimateSource']=='previous-scan'
        assert recalibrated_job['stats']['estimatedBytes']==previous_bytes
        assert api('bootstrap')['stale'] is False
        # Exercise the actual pinned Mole whitelist policy, using only an owned cache.
        protected = home / 'Library/Caches/WhitelistFixture/keep.bin'
        protected.parent.mkdir(parents=True)
        protected.write_bytes(b'whitelisted cache')
        os.utime(protected, (time.time() - 172800,) * 2)
        whitelist = home / '.config/mole/whitelist'
        whitelist.parent.mkdir(parents=True, exist_ok=True)
        whitelist.write_text(str(protected.parent) + '\n')
        wait(api('scan', {'scope':'home'}))
        scan = api('bootstrap')['scan']['id']
        plan = api('preview', {'scanId':scan, 'categories':['cache'], 'allCaches':True})
        rows = api('preview/files', {'planId':plan['id']})['rows']
        selected = next(r for r in rows if r['path'] == str(protected))
        result, _ = wait(api('execute', {'planId':plan['id'], 'ids':[selected['id']], 'confirmation':'TRASH_REVIEWED_FILES'}))
        assert result['moved'] == 0 and result['skipped'] == 1 and protected.exists()
        print('PASS: packaged Rust worker, atomic scan, project association, folder size, frozen selection, Mole guard and whitelist, Trash receipt, rescan and language persistence; progress and cancellation under a locked index')
    finally:
        process.send_signal(signal.SIGINT)
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
