#!/usr/bin/env python3
"""Run the Rust UI with disposable sample projects for README screenshots."""
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time
import urllib.request

ROOT = Path(__file__).resolve().parents[2]
BINARY = Path(os.environ.get('MINORMINOR_DEMO_BINARY', ROOT / 'dist/Minorminor.app/Contents/MacOS/Minorminor'))
if not BINARY.is_file():
    raise SystemExit('Build the desktop app with ./scripts/build-macos.sh first.')

with tempfile.TemporaryDirectory(prefix='minorminor-rust-readme-', dir='/tmp') as temporary:
    base = Path(temporary).resolve()
    home = base / 'Home'
    for index, (name, description) in enumerate([('Atlas', '资料检索与知识库'), ('Paper', '轻量写作工作台'), ('Notes', '个人笔记项目')]):
        project = home / 'Projects' / name
        project.mkdir(parents=True)
        (project / 'package.json').write_text(json.dumps({'name': name.lower(), 'description': description}))
        (project / 'README.md').write_text(f'# {name}\n\n{description}。\n')
        (project / '.cache').mkdir()
        cache = project / '.cache/build.bin'
        cache.write_bytes(b'a' * ((8 - index * 2) * 1024 * 1024))
        (project / 'logs').mkdir()
        log = project / 'logs/development.log'
        log.write_text('[INFO] Sample development server\n' * (3000 - index * 1000))
        for file in [cache, log]:
            os.utime(file, (time.time() - 172800,) * 2)
        sessions = home / '.claude/projects' / str(project).replace('/', '-')
        sessions.mkdir(parents=True)
        (sessions / 'sample.jsonl').write_text(json.dumps({'type':'session','cwd':str(project),'messages':[{'content':f'Sample project {name}'}]}) + '\n')
        (sessions / 'memory').mkdir()
        (sessions / 'memory/overview.md').write_text(f'# {name}\n\nSample project conventions.\n')
    process = subprocess.Popen([str(BINARY), '--headless', '--home', str(home), '--data', str(base / 'Index'), '--resources', str(ROOT), '--port', '4787'], stdout=subprocess.PIPE, text=True)
    def stop(*_):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    try:
        url = json.loads(process.stdout.readline())['url']
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        token = ''
        def api(action, body=None):
            request = urllib.request.Request(url + '/api/' + action, data=None if body is None else json.dumps(body).encode(), headers={} if body is None else {'content-type':'application/json','x-minorminor-token':token})
            with opener.open(request, timeout=10) as response:
                return json.load(response)
        token = api('bootstrap')['token']
        job = api('scan', {'scope':'home'})
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            boot = api('bootstrap')
            status = next(row for row in boot['jobs'] if row['id'] == job['id'])
            if status['status'] != 'running':
                if status['status'] != 'complete':
                    raise RuntimeError(status.get('error', 'Sample scan failed'))
                break
            time.sleep(.1)
        else:
            raise TimeoutError('Sample scan timed out')
        print(json.dumps({'url':url + '/#projects','temporaryHome':str(home)}), flush=True)
        while True:
            signal.pause()
    except KeyboardInterrupt:
        pass
    finally:
        process.send_signal(signal.SIGINT)
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
