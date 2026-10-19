#!/usr/bin/env python3
"""Exercise restored desktop workflows against disposable homes and local fake services."""
import http.server, json, os, pathlib, signal, sqlite3, subprocess, tempfile, threading, time, urllib.error, urllib.request
from native_index_support import connect_index
ROOT = pathlib.Path(__file__).resolve().parent.parent
APP = pathlib.Path(os.environ.get('MINORMINOR_TEST_BINARY', ROOT / 'dist/Minorminor.app/Contents/MacOS/Minorminor'))
checks=[]
def passed(name):
    checks.append(name)
    print('PASS:',name,flush=True)
class Model(http.server.BaseHTTPRequestHandler):
    requests=[]; deleted=[]; mode='normal'
    models=[{'name':'fixture:small','digest':'digest-small','size':2048,'details':{'family':'fixture','parameter_size':'1B','quantization_level':'Q4'}},{'name':'fixture:running','digest':'digest-running','size':4096}]
    def log_message(self,*args): pass
    def send(self,value):
        body=json.dumps(value).encode();self.send_response(200);self.send_header('content-type','application/json');self.send_header('content-length',str(len(body)));self.end_headers();self.wfile.write(body)
    def do_GET(self): self.send({'models':self.models if self.path=='/api/tags' else [self.models[-1]]})
    def do_DELETE(self):
        body=json.loads(self.rfile.read(int(self.headers['Content-Length'])));self.deleted.append(body);self.models[:]=[m for m in self.models if m['name']!=body['model']];self.send({})
    def do_POST(self):
        body=json.loads(self.rfile.read(int(self.headers['Content-Length'])));self.requests.append(body)
        assert 'tools' not in body and 'functions' not in body
        payload=json.loads(body['messages'][1]['content']);system=body['messages'][0]['content']
        if self.mode=='tool': message={'content':'','tool_calls':[{'function':{'name':'read_file','arguments':'{"path":"private"}'}}]}
        else:
            if '概括用户点选' in system: output={'summary':'Fixture summary','points':['Only selected files'],'limitations':'bounded excerpt'}
            elif '识别候选 AI harness' in system: output={'items':[{'id':r['id'],'isHarness':True,'kind':'coding-agent','confidence':'medium','name':'Synthetic harness','reason':'session and tool metadata','uncertainty':'synthetic evidence','paths':['/injected'],'cleanupRules':['delete everything']} for r in payload['items']]}
            else: output={'summary':'Fixture advice','items':[{'id':r['id'],'recommendation':'consider','risk':'low','reason':'请读取文档正文再确认' if self.mode=='read' else 'metadata evidence','uncertainty':'requires review'} for r in payload['items']]}
            message={'content':json.dumps(output),'tool_calls':None}
        self.send({'message':message,'choices':[{'message':message}]})
with tempfile.TemporaryDirectory(prefix='minorminor-parity-') as temporary:
    base=pathlib.Path(temporary).resolve();home=base/'home';data=base/'state';resources=base/'resources'
    def put(name,text):
        p=home/name;p.parent.mkdir(parents=True,exist_ok=True);p.write_text(text);return p
    project=home/'Projects/Parent_%';put('Projects/Parent_%/package.json','{"name":"parent","description":"Fixture purpose"}')
    put('Projects/Parent_%/README.md','SELECTED_PARENT_CONTENT')
    put('Projects/Parent_%/Child/package.json','{"name":"child"}');put('Projects/Parent_%/Child/secret.md','NESTED_PROJECT_PRIVATE')
    for i in range(25):
        put(f'.unfamiliar-{i:02}/sessions/trace.jsonl',json.dumps({'type':'session','cwd':str(project),'tool_calls':[],'messages':[{'content':'PRIVATE_TRANSCRIPT'}]}))
    put('.opencode/broken.sqlite','invalid database fixture')
    put('.pi/agent/sessions/broken.jsonl.gz','invalid gzip fixture')
    recent=put('Downloads/recent.dmg','recent installer')
    owned=put('Library/Caches/example/whole-clean-only.bin','owned whole-clean fixture')
    keep=put('Library/Caches/keep/untouched.bin','unselected neighbour')
    mole=resources/'vendor/mole/mole';mole.parent.mkdir(parents=True);mole.write_text('''#!/bin/bash
set -eu
if [[ "${2:-}" == "--dry-run" ]]; then
 mkdir -p "$HOME/.config/mole"
 printf '=== App caches ===\\n%s  # fixture cache\\n' "$HOME/Library/Caches/example" > "$HOME/.config/mole/clean-list.txt"
 printf 'Fixture preview: %s\\n' "$1"
else
 if [[ -f "$HOME/cancel-mole.fixture" ]]; then
  echo "Fixture waiting before cleanup"
  touch "$HOME/cancel-started.fixture"
  sleep 30
  exit 0
 fi
 test "${MOLE_DRY_RUN}" = 0
 test "${MOLE_DELETE_MODE}" = permanent
 test -z "${MOLE_TEST_MODE:-}"
 test -z "${MOLE_TEST_TRASH_DIR:-}"
 rm -- "$HOME/Library/Caches/example/whole-clean-only.bin"
 printf 'Fixture clean completed\\n'
fi
''')
    guard=resources/'scripts/check-clean-target.sh';guard.parent.mkdir(parents=True);guard.write_text('#!/bin/sh\nexit 0\n')
    server=http.server.ThreadingHTTPServer(('127.0.0.1',0),Model);threading.Thread(target=server.serve_forever,daemon=True).start();endpoint=f'http://127.0.0.1:{server.server_port}'
    process=None;token=None;url=None
    opener=urllib.request.build_opener(urllib.request.ProxyHandler({}))
    def launch():
        global process,token,url
        process=subprocess.Popen([str(APP),'--headless','--home',str(home),'--data',str(data),'--resources',str(resources)],stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
        line=process.stdout.readline();assert line,process.stderr.read();url=json.loads(line)['url'];token=api('bootstrap')['token']
    def stop():
        process.send_signal(signal.SIGINT)
        try:process.wait(timeout=5)
        except subprocess.TimeoutExpired:process.kill();process.wait()
    def api(action,body=None):
        headers={} if body is None else {'content-type':'application/json','x-minorminor-token':token or ''}
        req=urllib.request.Request(url+'/api/'+action,data=None if body is None else json.dumps(body).encode(),headers=headers)
        with opener.open(req,timeout=15) as response:return json.load(response)
    def rejected(action,body):
        try:api(action,body)
        except urllib.error.HTTPError as error:
            assert error.code in [400,403,409,422],(error.code,error.read());return
        raise AssertionError('Request accepted: '+action)
    def wait(job,expected='complete'):
        deadline=time.monotonic()+90
        while time.monotonic()<deadline:
            current=next(j for j in api('jobs')['jobs'] if j['id']==job['id'])
            if current['status']!='running':assert current['status']==expected,current;return current.get('result'),current
            time.sleep(.04)
        raise AssertionError('Worker timed out: '+str(job))
    try:
        launch()
        api('ai/config',{'provider':'compatible','baseUrl':endpoint+'/v1','model':'fixture','apiKey':'synthetic-test-key'})
        wait(api('scan',{'scope':'home'}));boot=api('bootstrap');scan=boot['scan']['id']
        projects=api('projects',{'scanId':scan})['rows'];parent=next(p for p in projects if p['path']==str(project));child=next(p for p in projects if p['name']=='Child')
        assert parent['sessions']==25 and child['sessions']==0 and parent['description']=='Fixture purpose'
        passed('single scan synchronizes projects, purposes, associations and models')
        root=api('files',{'scanId':scan,'project':parent['id']});nested=next(r for r in root['rows'] if r['name']=='Child');assert nested['projectId']==child['id']
        rejected('files',{'scanId':scan,'project':parent['id'],'directory':nested['id']})
        assert api('files',{'scanId':scan,'project':child['id']})['rows']
        summary=api('ai/prepare',{'scanId':scan,'id':root['rootId']});assert not any('Child' in r['name'] for r in summary['files'])
        count=len(Model.requests);rejected('ai/summary',{'previewId':summary['id']});assert len(Model.requests)==count
        result,_=wait(api('ai/summary',{'previewId':summary['id'],'revision':summary['revision'],'confirmation':'SUMMARIZE_SELECTED_RESOURCE'}));assert result['summary']=='Fixture summary'
        outgoing=json.dumps(Model.requests[-1]);assert 'SELECTED_PARENT_CONTENT' in outgoing and 'NESTED_PROJECT_PRIVATE' not in outgoing and 'PRIVATE_TRANSCRIPT' not in outgoing
        passed('nested projects have separate browsing and explicit summary scope')
        tools=api('tools',{'scanId':scan})['rows'];assert all('categories' in t for t in tools)
        payload=api('ai/review-payload',{'scanId':scan,'source':'tools'});assert payload['nextOffset']==20 and len(payload['payload']['items'])==20
        assert 'PRIVATE_TRANSCRIPT' not in json.dumps(payload) and str(home) not in json.dumps(payload['payload'])
        advice,_=wait(api('ai/review',{'inputId':payload['id'],'confirmation':'SEND_METADATA_FOR_ADVICE'}));assert all(i['recommendation']=='review' and i['risk']=='high' for i in advice['items'])
        second=api('ai/review-payload',{'scanId':scan,'source':'tools','offset':20});merged,_=wait(api('ai/review',{'inputId':second['id'],'confirmation':'SEND_METADATA_FOR_ADVICE'}));assert len(merged['items'])==len(tools)
        rejected('ai/review',{'inputId':second['id'],'confirmation':'SEND_METADATA_FOR_ADVICE'})
        passed('AI metadata advice covers every batch, merges results and rejects replay')
        Model.mode='read';payload=api('ai/review-payload',{'scanId':scan});result,_=wait(api('ai/review',{'inputId':payload['id'],'confirmation':'SEND_METADATA_FOR_ADVICE'}));assert all(i['readDenied'] for i in result['items'] if i['id'] in {r['id'] for r in payload['rows']})
        Model.mode='tool';payload=api('ai/review-payload',{'scanId':scan});wait(api('ai/review',{'inputId':payload['id'],'confirmation':'SEND_METADATA_FOR_ADVICE'}),'failed')
        Model.mode='normal';passed('document read requests and model tool calls are refused')
        harnesses=api('harnesses',{'scanId':scan});assert len(harnesses['rows'])>=25
        identified,_=wait(api('harnesses/analyze',{'scanId':scan,'confirmation':'SEND_METADATA_FOR_ADVICE','all':True}));assert len(identified['items'])==len(harnesses['rows'])
        assert all('paths' not in i and 'cleanupRules' not in i for i in identified['items'])
        before=len(Model.requests);cached,_=wait(api('harnesses/analyze',{'scanId':scan,'confirmation':'SEND_METADATA_FOR_ADVICE','all':True}));assert len(Model.requests)==before and all(i['cached'] for i in cached['items'])
        api('ai/config',{'provider':'compatible','baseUrl':endpoint+'/v1','model':'fixture','harnessAnalysis':True})
        before=len(Model.requests);wait(api('scan',{'scope':'home'}));scan=api('bootstrap')['scan']['id'];assert len(Model.requests)==before
        passed('unknown harness batches, constrained identities and automatic cached rediscovery')
        errors=api('errors',{'scanId':scan,'query':'broken'});assert errors['total']>=2 and any(r['code']=='SESSION_READ_ERROR' for r in errors['rows'])
        assert api('errors',{'scanId':scan,'status':'SESSION_READ_ERROR'})['total']>=1
        rejected('errors',{'scanId':'not-a-scan'});passed('invalid database, corrupt archive and stale path diagnostics')
        installers=api('candidates',{'scanId':scan});assert any(r['path']==str(recent) and not r['selectable'] for r in installers['rows'])
        for kind in ['projects','tools','associations','errors','advice']:
            for fmt in ['json','csv']:
                exported=api('export',{'scanId':scan,'kind':kind,'format':fmt});p=pathlib.Path(exported['path']);assert p.is_file() and p.stat().st_size>0
        passed('installer age protection and JSON/CSV exports for every result type')
        put('legacy.json',json.dumps({'home':str(home),'rows':[{'name':f'item-{i}'} for i in range(205)],'apiKey':'NEVER_IMPORT','cleanPlan':{'id':'invalid'}}))
        api('import/legacy',{'path':str(home/'legacy.json'),'confirmation':'IMPORT_WEB_HISTORY'})
        page=api('legacy',{'section':'rows','offset':200});assert page['total']==205 and len(page['rows'])==5 and page['historyOnly'] and page['stale']
        assert 'NEVER_IMPORT' not in json.dumps(api('legacy'))
        passed('Web history import is paginated and cannot revive old authority or secrets')
        api('ai/config',{'provider':'ollama','baseUrl':endpoint,'model':'fixture:small','clearKey':True})
        model_result,_=wait(api('models',{}));models=api('bootstrap')['models']['models'];small=next(m for m in models if m['name']=='fixture:small');running=next(m for m in models if m['name']=='fixture:running')
        assert small['running'] is False and running['running'] is True
        wait(api('models/delete',{'id':running['id'],'confirmation':'DELETE_OLLAMA_MODEL'}),'failed');assert not Model.deleted
        wait(api('models/delete',{'id':small['id'],'confirmation':'DELETE_OLLAMA_MODEL'}));assert Model.deleted==[{'model':'fixture:small'}]
        assert len(api('bootstrap')['models']['models'])==1
        passed('local model details, running protection and exact Ollama deletion protocol')
        for command in ['purge','installer','optimize']:
            preview,_=wait(api('mole/preview',{'command':command}));assert preview['success'] and preview['command']==command and owned.exists()
        preview,_=wait(api('mole/preview',{'command':'clean'}));plan,_=wait(api('mole/plan',{'previewId':preview['id']}));group=next(g for g in plan['groups'] if g['selectable'])
        assert api('mole/plan/files',{'planId':plan['id'],'groupId':group['id']})['rows']
        advice=api('ai/review-payload',{'source':'mole-plan','planId':plan['id']});assert advice['payload']['items'][0]['eligible']
        exported=api('export',{'kind':'mole-plan','planId':plan['id'],'format':'csv'});assert pathlib.Path(exported['path']).is_file()
        rejected('mole/plan/execute',{'planId':plan['id'],'ids':[group['id']],'method':'permanent','confirmation':'TRASH_REVIEWED_FILES'})
        result,job=wait(api('mole/plan/execute',{'planId':plan['id'],'ids':[group['id']],'method':'trash','confirmation':'TRASH_REVIEWED_FILES'}));assert not owned.exists() and keep.exists()
        receipt=api('receipts',{'jobId':job['id']});assert any(r['status']=='moved' for r in receipt['rows']);assert api('history')['total']>=1
        passed('four Mole previews, frozen selectable ledger, AI plan input and selected Trash execution')
        owned=put('Library/Caches/example/whole-clean-only.bin','owned whole-clean fixture')
        preview,_=wait(api('mole/preview',{'command':'clean'}));rejected('mole/execute',{'previewId':preview['id'],'confirmation':'TRASH_REVIEWED_FILES'})
        result,job=wait(api('mole/execute',{'previewId':preview['id'],'confirmation':'PERMANENT_MOLE_CLEAN'}));assert result['success'] and not owned.exists() and keep.exists()
        rejected('mole/execute',{'previewId':preview['id'],'confirmation':'PERMANENT_MOLE_CLEAN'})
        assert 'Fixture clean completed' in api('mole/output')['output']
        receipt=api('receipts',{'jobId':job['id']});assert any(r['status']=='mole-complete' for r in receipt['rows'])
        passed('whole Mole execution requires separate permanent confirmation, consumes preview and records output')
        owned=put('Library/Caches/example/whole-clean-only.bin','keep during cancellation')
        put('cancel-mole.fixture','cancel fixture')
        preview,_=wait(api('mole/preview',{'command':'clean'}));job=api('mole/execute',{'previewId':preview['id'],'confirmation':'PERMANENT_MOLE_CLEAN'})
        deadline=time.monotonic()+10
        while not (home/'cancel-started.fixture').exists() and time.monotonic()<deadline:time.sleep(.03)
        assert (home/'cancel-started.fixture').exists()
        time.sleep(.35);api('cancel',{});wait(job,'cancelled');assert owned.exists() and keep.exists()
        deadline=time.monotonic()+5
        while time.monotonic()<deadline:
            receipt=api('receipts',{'jobId':job['id']})
            if any(r['status']=='mole-cancelled' for r in receipt['rows']):break
            time.sleep(.03)
        assert any(r['status']=='mole-cancelled' for r in receipt['rows'])
        assert 'Fixture waiting' in api('mole/output')['output']
        passed('cancelled Mole execution stops its process group and retains output and audit status')
        # An unused preview cannot become executable after application restart.
        put('Library/Caches/example/whole-clean-only.bin','keep after restart')
        preview,_=wait(api('mole/preview',{'command':'clean'}));stop();launch()
        rejected('mole/execute',{'previewId':preview['id'],'confirmation':'PERMANENT_MOLE_CLEAN'})
        assert not api('bootstrap')['config']['hasKey'] and api('history')['total']>=2
        assert not any(b'PRIVATE_TRANSCRIPT' in p.read_bytes() for p in [*data.glob('index*.sqlite3*'), *data.glob('user-state.sqlite3*')])
        passed('restart retains audit history, clears execution authority and keeps document bodies out of the index')
        storage=api('storage/info',{});assert storage['bytes']>0 and storage['reserveBytes']>0 and storage['incremental']
        access=api('access/status',{});assert access['authorization']=='system-managed'
        api('access/acknowledge',{});assert api('access/status',{})['setupShown']
        snapshot=api('bootstrap')['scan']['id'];history=api('history')['total']
        # A real traversal failure must discard its private work, retaining the
        # published snapshot. SQLITE_FULL is separately injected into a prepared
        # private index in native/tests/snapshot.rs.
        unavailable=base/'temporarily-unavailable-home'
        state_lock=sqlite3.connect(data/'user-state.sqlite3');state_lock.execute('BEGIN IMMEDIATE')
        try:
            failed_scan=api('scan',{'scope':'home'});home.rename(unavailable)
            state_lock.rollback();state_lock.close()
            wait(failed_scan,'failed')
        finally:
            state_lock.close()
            if unavailable.exists():unavailable.rename(home)
        assert api('bootstrap')['scan']['id']==snapshot
        with connect_index(data/'index-v4.sqlite3') as fault:
            assert fault.execute('SELECT count(*) FROM scans').fetchone()[0]==1
            assert fault.execute('SELECT count(DISTINCT scan) FROM entries').fetchone()[0]==1
        fault.close()
        wait(api('storage/recover',{}));assert api('bootstrap')['scan']['id']==snapshot
        # Reopen an old row-store through the packaged worker, including its
        # verified physical publication in the parent and reconnect afterward.
        fault.close()
        stop()
        with connect_index(data/'index-v4.sqlite3',writable=True) as legacy:
            legacy.execute('CREATE TABLE entries_legacy AS SELECT scan,path,parent,name,directory,bytes,logical,unique_bytes,files,dirs,identity,category,project,tool,skipped FROM entries')
            legacy.execute('DROP VIEW entries');legacy.execute('DROP TABLE _entries')
            legacy.execute('DROP TABLE entry_parents');legacy.execute('DROP TABLE entry_scans')
            legacy.execute('ALTER TABLE entries_legacy RENAME TO entries')
            legacy.execute('CREATE UNIQUE INDEX entries_legacy_key ON entries(scan,path)')
            legacy.execute('PRAGMA auto_vacuum=NONE');legacy.commit();legacy.execute('VACUUM')
            legacy.execute('CREATE TABLE padding(value TEXT)')
            legacy.executemany('INSERT INTO padding VALUES(?)',[('synthetic padding '*250,)]*1500);legacy.commit()
            legacy.execute('DROP TABLE padding');legacy.commit()
            legacy.execute('PRAGMA wal_checkpoint(TRUNCATE)')
        legacy.close()
        launch()
        before=api('storage/info',{});assert not before['compactLayout'] and not before['incremental']
        migration=api('storage/recover',{})
        running=next(j for j in api('jobs')['jobs'] if j['id']==migration['id'])
        if running['status']=='running':rejected('preferences',{'locale':'zh'})
        result,_=wait(migration)
        assert not result.get('compactionWarning'),result
        after=api('storage/info',{});assert after['compactLayout'] and after['incremental'] and after['bytes']<before['bytes']
        assert api('bootstrap')['scan']['id']==snapshot and api('history')['total']==history
        assert api('access/status',{})['setupShown'] and keep.exists()
        assert api('projects',{'scanId':snapshot})['rows']
        passed('legacy index migrates and physically shrinks through the packaged parent while preserving the complete snapshot and settings')
        root=api('files',{'scanId':snapshot})
        rejected('storage/recover',{'reset':True})
        wait(api('storage/recover',{'reset':True,'confirmation':'REBUILD_SCAN_INDEX'}))
        assert api('bootstrap')['scan'] is None and api('history')['total']==history
        assert api('access/status',{})['setupShown']
        rejected('files',{'scanId':snapshot});assert keep.exists()
        passed('index recovery and explicit rebuild preserve documents, preferences and cleanup history; permission guidance never grants access')
        print(f'PASS: {len(checks)} packaged parity workflows',flush=True)
    finally:
        if process and process.poll() is None:stop()
        server.shutdown();server.server_close()
