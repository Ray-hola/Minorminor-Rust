// Byte-based overall progress is explicitly an estimate; stage counts stay factual.
export function scanProgress(job, now = Date.now(), capacity = null) {
  const stats = job.stats || {};
  const stages = ['disk', 'projects', 'storage', 'models'];
  const stage = stats.stage || 'preparing';
  const status = job.status;
  const measuredElapsed = Number(stats.elapsedMs);
  const elapsedMs = job.kind === 'scan' && stats.elapsedMs != null && Number.isFinite(measuredElapsed) && measuredElapsed >= 0
    ? measuredElapsed : Math.max(0, (Date.parse(job.finishedAt) || now) - (Date.parse(job.createdAt) || now));
  const idleMs = Math.max(0, now - (Date.parse(job.progressAt || job.createdAt) || now));
  const total = Number(stats.total) || 0;
  const completed = Number(stats.completed) || 0;
  const stageFraction = total > 0 ? Math.min(1, Math.max(0, completed / total)) : null;
  const estimate = Number(stats.estimatedBytes ?? stats.diskSnapshot?.used ?? capacity?.used);
  const isScan = job.kind === 'scan';
  let fraction = stageFraction;
  if (isScan) {
    const ranges = { preparing: [0, 0], disk: [0, .90], projects: [.90, .04], storage: [.94, .04], 'read-recovery': [.98, 0], summaries: [.98, .005], 'harness-analysis': [.985, .005], models: [.99, 0], 'index-recovery': [.99, .005], 'index-compress': [.99, .004], 'index-verify': [.994, .001] };
    const range = ranges[stage];
    if (status === 'complete') fraction = 1;
    else if (range) {
      const part = stage === 'disk' ? Number.isFinite(estimate) && estimate > 0 ? Math.min(1, Math.max(0, Number(stats.bytes) || 0) / estimate) : null : stageFraction || 0;
      fraction = part == null ? null : Math.min(.995, range[0] + range[1] * part);
    } else fraction = null;
  }
  return {
    stage, status, stats, elapsedMs, idleMs,
    spaceLimited: status === 'failed' && /database or disk is full|disk full|no space left|索引磁盘空间不足/i.test(job.error || ''),
    step: status === 'complete' ? 5 : ['summaries','harness-analysis','read-recovery'].includes(stage) ? 3 : ['index-recovery','index-compress','index-verify'].includes(stage) ? 5 : Math.max(0, stages.indexOf(stage) + 1),
    steps: 5,
    fraction, stageFraction, estimated: isScan && status !== 'complete' && fraction != null,
    waiting: status === 'running' && idleMs >= 8000,
    error: status !== 'failed' ? '' : /database or disk is full|disk full|no space left|索引磁盘空间不足/i.test(job.error || '')
      ? '磁盘空间不足。可先回收临时索引，或重建索引后重试。' : job.error || '',
  };
}

// A later non-scan task can replace the persisted last-job record. The completed
// scan snapshot remains authoritative for the compact result after a restart.
export function progressJob(boot) {
  const jobs = boot?.jobs || [];
  const existing = jobs.find(j => j.status === 'running') || [...jobs].reverse().find(j => j.kind === 'scan');
  if (existing) return existing.kind==='scan' && existing.status==='complete' && existing.id===boot?.scan?.id
    ? {...existing,stats:{...existing.stats,...boot.scan.stats}} : existing;
  const scan = boot?.scan;
  if (scan?.status !== 'complete') return null;
  const started = Date.parse(scan.createdAt);
  const elapsed = Math.max(0, Number(scan.stats?.elapsedMs) || 0);
  return {...scan, kind:'scan', finishedAt:Number.isFinite(started) ? new Date(started + elapsed).toISOString() : scan.createdAt};
}

export function scanScope(boot) {
  const job = [...(boot?.jobs || [])].reverse().find(j => j.kind === 'scan');
  const scope = job?.stats?.scanScope;
  if (scope === 'disk' || scope === 'home') return scope;
  return ['/', '/System/Volumes/Data'].includes(boot?.scan?.root) ? 'disk' : 'home';
}
