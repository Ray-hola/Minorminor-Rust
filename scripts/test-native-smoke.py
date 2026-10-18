#!/usr/bin/env python3
"""Exercise the packaged Rust process with disposable files only."""
import json, os, pathlib, signal, subprocess, tempfile, time, urllib.request

ROOT = pathlib.Path(__file__).resolve().parent.parent
APP = ROOT / 'dist/Minorminor.app/Contents/MacOS/Minorminor'
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
        assert boot['native'] and boot['config']['permissions']['analysisDocumentRead'] is False
        api('preferences', {'locale':'en'})
        assert api('bootstrap')['preferences']['locale'] == 'en'
        result, boot = wait(api('scan', {'scope':'home'}))
        scan = boot['scan']['id']
        assert scan == result['scanId'] and boot['models']['scanId'] == scan
        projects = api('projects', {'scanId':scan})['rows']
        demo = next(p for p in projects if p['name'] == 'Demo')
        assert demo['bytes'] > 0 and demo['sessions'] == 1
        tools = api('tools', {'scanId':scan})['rows']
        assert any(t['name'] == 'unknown-agent' and t['uncertain'] for t in tools)
        page = api('files', {'scanId':scan,'project':demo['id']})
        folder = next(row for row in page['rows'] if row['name'] == '.cache')
        assert folder['directory'] and folder['bytes'] > 0
        nested = api('files', {'scanId':scan,'project':demo['id'],'directory':folder['id']})
        assert {row['name'] for row in nested['rows']} == {'old.bin','fresh.bin'}
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
        moved = next(row for row in receipt['rows'] if row['status'] == 'moved')
        assert pathlib.Path(moved['target']).read_bytes() == b'cache' * 2048
        assert api('bootstrap')['stale'] is True
        assert not any(b'PRIVATE_TRANSCRIPT_MARKER' in p.read_bytes() for p in data.glob('index.sqlite3*'))
        wait(api('scan', {'scope':'home'}))
        assert api('bootstrap')['stale'] is False
        print('PASS: packaged Rust worker, atomic scan, project association, folder size, frozen selection, Mole guard, Trash receipt, rescan and language persistence')
    finally:
        process.send_signal(signal.SIGINT)
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
