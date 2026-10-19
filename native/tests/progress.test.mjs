import test from 'node:test';
import assert from 'node:assert/strict';
import { scanProgress, progressJob, scanScope } from '../../public/native-progress.js';
const createdAt='2026-10-18T00:00:00Z';
test('restored scope follows the latest scan and survives an unrelated task',()=>{
  assert.equal(scanScope({jobs:[{kind:'scan',status:'running',stats:{scanScope:'disk'}}]}),'disk');
  assert.equal(scanScope({scan:{root:'/System/Volumes/Data'},jobs:[{kind:'cleanup'}]}),'disk');
  assert.equal(scanScope({scan:{root:'/System/Volumes/Data'},jobs:[{kind:'scan',stats:{scanScope:'home'}}]}),'home');
  assert.equal(scanScope({scan:{root:'/Users/example'}}),'home');
  assert.equal(scanScope(null),'home');
});
test('walking has no invented percentage and retains factual counters',()=>{
  const p=scanProgress({status:'running',createdAt,progressAt:createdAt,stats:{stage:'disk',files:1200,bytes:1024,currentPath:'/example'}},Date.parse(createdAt)+9000);
  assert.equal(p.fraction,null);assert.equal(p.step,1);assert.equal(p.waiting,true);assert.equal(p.stats.files,1200);assert.equal(p.elapsedMs,9000);
});
test('known folder totals have exact bounded progress',()=>{
  const p=scanProgress({status:'running',createdAt,stats:{stage:'storage',completed:50,total:200}},Date.parse(createdAt)+1000);
  assert.equal(p.fraction,.25);assert.equal(p.step,3);assert.equal(p.waiting,false);
});
test('disk full remains an explicit failure and stops the elapsed timer',()=>{
  const p=scanProgress({status:'failed',createdAt,finishedAt:'2026-10-18T00:00:07Z',error:'database or disk is full',stats:{stage:'preparing'}},Date.parse(createdAt)+90000);
  assert.equal(p.status,'failed');assert.equal(p.elapsedMs,7000);assert.match(p.error,/磁盘空间不足/);assert.equal(p.waiting,false);
});

test('a stopped scan does not display a cleanup warning',()=>{
  const p=scanProgress({status:'cancelled',createdAt,error:'任务已停止；已发生的移动请查看逐文件记录',stats:{stage:'disk'}});
  assert.equal(p.error,'');assert.equal(p.waiting,false);
});

test('scan estimates bytes from a frozen capacity snapshot and reserves final work',()=>{
  const p=scanProgress({kind:'scan',status:'running',createdAt,stats:{stage:'disk',bytes:250,estimatedBytes:1000,diskSnapshot:{free:200,total:1200}}});
  assert.equal(p.fraction,.225);assert.equal(p.estimated,true);assert.equal(p.stats.diskSnapshot.free,200);
  const over=scanProgress({kind:'scan',status:'running',createdAt,stats:{stage:'disk',bytes:1500,estimatedBytes:1000}});
  assert.equal(over.fraction,.9);assert.notEqual(over.fraction,1);
});
test('overall scan progress continues through real post-scan stages',()=>{
  for(const [stage,min,max] of [['projects',.9,.94],['storage',.94,.98],['harness-analysis',.98,.99],['models',.99,.995]]){
    const p=scanProgress({kind:'scan',status:'running',createdAt,stats:{stage,completed:1,total:2}});
    assert.ok(p.fraction>=min&&p.fraction<=max);assert.equal(p.estimated,true);assert.notEqual(p.fraction,1);
  }
  const done=scanProgress({kind:'scan',status:'complete',createdAt,stats:{stage:'models'}});assert.equal(done.fraction,1);assert.equal(done.estimated,false);
});
test('unknown capacity falls back to activity without making up a denominator',()=>{
  for(const estimatedBytes of [null,0,-1,NaN])assert.equal(scanProgress({kind:'scan',status:'running',createdAt,stats:{stage:'disk',bytes:20,estimatedBytes}}).fraction,null);
});
test('stopping or failing does not report completion',()=>{
  for(const status of ['cancelled','failed']){
    const p=scanProgress({kind:'scan',status,createdAt,stats:{stage:'disk',bytes:500,estimatedBytes:1000}});assert.equal(p.fraction,.45);assert.equal(p.estimated,true);
  }
});


test('restored old scan records can use readable capacity without faking completion',()=>{
  const legacy={kind:'scan',status:'cancelled',createdAt,stats:{stage:'disk',bytes:250}};
  const p=scanProgress(legacy,Date.parse(createdAt)+1000,{used:1000});
  assert.equal(p.fraction,.225);assert.equal(p.estimated,true);
});
test('completed scan progress survives a later task and restart',()=>{
  const scan={id:'saved',status:'complete',createdAt,stats:{elapsedMs:7000,bytes:250}};
  const job=progressJob({jobs:[{id:'summary',kind:'summary',status:'complete'}],scan});
  assert.equal(job.kind,'scan');assert.equal(job.id,'saved');
  const p=scanProgress(job,Date.parse(createdAt)+90000);assert.equal(p.fraction,1);assert.equal(p.elapsedMs,7000);
});
test('running work and a stopped newer scan take precedence over old completed results',()=>{
  const scan={status:'complete',createdAt,stats:{}};
  const active={id:'active',kind:'scan',status:'running'};assert.equal(progressJob({scan,jobs:[active]}),active);
  const stopped={id:'stopped',kind:'scan',status:'cancelled'};assert.equal(progressJob({scan,jobs:[stopped]}),stopped);
  assert.equal(progressJob({jobs:[],scan:null}),null);
});

test('low index volume preserves the failure and exposes recovery without claiming completion',()=>{
  const p=scanProgress({kind:'scan',status:'failed',createdAt,error:'索引磁盘空间不足：剩余 20 MB，需保留 256 MB。',stats:{stage:'projects',bytes:1000,estimatedBytes:1000}});
  assert.equal(p.spaceLimited,true);assert.match(p.error,/回收临时索引/);assert.notEqual(p.fraction,1);
  const recovering=scanProgress({kind:'scan',status:'running',createdAt,stats:{stage:'index-recovery'}});
  assert.ok(recovering.fraction>=.99&&recovering.fraction<1);
});
