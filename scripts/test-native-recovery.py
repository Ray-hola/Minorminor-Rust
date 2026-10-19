#!/usr/bin/env python3
"""Verify recovery through the packaged API, using disposable permissions only."""
import json, os, pathlib, signal, subprocess, tempfile, time, urllib.request
from native_index_support import connect_index, resolve_index

ROOT = pathlib.Path(__file__).resolve().parent.parent
APP = pathlib.Path(os.environ.get('MINORMINOR_TEST_BINARY', ROOT / 'dist/Minorminor.app/Contents/MacOS/Minorminor'))
with tempfile.TemporaryDirectory(prefix='minorminor-read-recovery-') as temporary:
    base = pathlib.Path(temporary).resolve()
    home, data = base / 'home', base / 'data'
    denied = home / 'restored'
    denied.mkdir(parents=True)
    (denied / 'recovered.bin').write_bytes(b'fixture' * 4096)
    for n in range(400):
        (home / f'fixture-{n:04}.txt').write_text('fixture')
    denied.chmod(0)
    process = subprocess.Popen([str(APP), '--headless', '--home', str(home), '--data', str(data)], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        url = json.loads(process.stdout.readline())['url']
        token = ''
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        def api(action, body=None):
            headers = {} if body is None else {'content-type':'application/json', 'x-minorminor-token':token}
            request = urllib.request.Request(url+'/api/'+action, data=None if body is None else json.dumps(body).encode(), headers=headers)
            with opener.open(request, timeout=8) as response:
                return json.load(response)
        def wait(job):
            deadline = time.monotonic()+60
            while time.monotonic()<deadline:
                row = next(j for j in api('jobs')['jobs'] if j['id']==job['id'])
                if row['status']!='running':
                    assert row['status']=='complete', row
                    return row
                time.sleep(.1)
            raise AssertionError('Recovery timed out')
        token = api('bootstrap')['token']
        wait(api('scan', {'scope':'home'}))
        boot = api('bootstrap'); scan = boot['scan']['id']
        assert boot['scan']['stats']['skipped']==1, boot['scan']
        report = api('errors', {'scanId':scan, 'status':'permission'})
        assert report['total']==1 and report['rows'][0]['code']=='EACCES', report
        index = resolve_index(data / 'index.sqlite3')
        before = index.stat()
        denied.chmod(0o700)
        result = wait(api('errors/retry', {'scanId':scan}))['result']
        assert result['summary']=={'recovered':1} and result['remaining']==0, result
        boot = api('bootstrap')
        assert boot['scan']['stats']['skipped']==0 and boot['scan']['stats']['files']==401, boot['scan']
        report = api('errors', {'scanId':scan,'status':'recovered'})
        assert report['total']==1 and report['rows'][0]['originalCode']=='EACCES', report
        with connect_index(index) as db:
            assert db.execute('SELECT skipped,files FROM entries WHERE scan=? AND path=?',(scan,str(home))).fetchone()==(0,401)
            assert db.execute('SELECT count(*) FROM errors WHERE scan=?',(scan,)).fetchone()[0]==0
            assert db.execute('SELECT count(*) FROM entries WHERE scan=? AND path=?',(scan,str(denied/'recovered.bin'))).fetchone()[0]==1
        if getattr(before,'st_flags',0)&32:
            assert before.st_blocks==index.stat().st_blocks and before.st_size==index.stat().st_size, 'Recovery expanded immutable main index'
        print(json.dumps({'status':'passed','recoveredFiles':1,'activeErrors':0,'auditPreserved':True,'compressedMain':bool(getattr(before,'st_flags',0)&32)}))
    finally:
        denied.chmod(0o700)
        process.send_signal(signal.SIGINT)
        try:
            process.communicate(timeout=10)
        except subprocess.TimeoutExpired:
            process.kill(); process.communicate()
